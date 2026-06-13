//! The SSTable file: data blocks, then (for large tables) index shards, then
//! the root header.
//!
//! `write` packs sorted entries into blocks of about `page_size`, pads each to
//! a page boundary, and records every block (key range + offset/len) in the
//! index. A small table keeps that index inline in the root; once it has more
//! than `blocks_per_chunk` blocks the index is partitioned into **child
//! headers** and the root becomes a directory of them, so no single header page
//! grows unbounded.
//!
//! Reads go through [`SstPageCache`]: `open` faults in the root, `get`/`scan`
//! fault in only the child shards and data blocks they actually need — `scan`
//! lazily, one block at a time, so peak memory is a single shard plus a single
//! block.
//!
//! On-disk layout:
//! ```text
//! | block 0 | (pad) | block 1 | ... | child 0 | child 1 | ... | root | root_len (4B) |
//! ```
//! (the child shards are absent when the index is inline.)

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};

use super::bloom::BloomFilter;
use super::cache::SstPageCache;
use super::codec::{Decode, Encode};
use super::data::{self, DataBlock};
use super::header::{BlockMeta, ChunkRef, Header, Index};
use super::page::{HeaderId, Page, PageId};
use crate::iterator::KvStream;
use crate::key::{KeyRange, KeyValue};
use crate::{LsmError, ReadError, SsTableId, TableConfig};

/// A handle to an on-disk SSTable: its decoded root header (whole-table range,
/// bloom, and the block index — inline or a directory of shards), plus the
/// directory its file lives in. Child shards and data blocks are faulted in on
/// demand through the cache.
pub struct SsTable {
    sst_id: SsTableId,
    range: KeyRange,
    bloom: BloomFilter,
    size_bytes: u64,
    index: Index,
    dir: PathBuf,
}

impl SsTable {
    /// Block up `entries`, build the header(s), and write `{dir}/{id}.sst`.
    pub fn write(
        sst_id: SsTableId,
        dir: &Path,
        config: &TableConfig,
        entries: Vec<KeyValue>,
    ) -> io::Result<()> {
        let Built {
            mut buf,
            blocks,
            bloom,
            range,
            size_bytes,
        } = build(config, entries);

        // `buf` holds the data section (file offset 0..size_bytes). Append child
        // shards (if any), then the root, then the root-length footer.
        let blocks_per_chunk = config.page.blocks_per_chunk.max(1);
        let index = if blocks.len() <= blocks_per_chunk {
            Index::Inline(blocks)
        } else {
            Index::Sharded(append_shards(&mut buf, sst_id, &blocks, blocks_per_chunk))
        };

        let root = Header::Root {
            sst_id,
            range,
            bloom,
            size_bytes,
            index,
        };
        let mut root_bytes = Vec::new();
        root.encode(&mut root_bytes);
        buf.extend_from_slice(&root_bytes);
        buf.extend_from_slice(&(root_bytes.len() as u32).to_be_bytes());

        std::fs::create_dir_all(dir)?;
        let mut file = File::create(sst_path(dir, sst_id))?;
        file.write_all(&buf)?;
        file.sync_all()?;
        Ok(())
    }

    /// Open a table for reading, loading its root header through `cache`.
    pub fn open(sst_id: SsTableId, dir: &Path, cache: &SstPageCache) -> Result<SsTable, LsmError> {
        let dir = dir.to_path_buf();
        let page = cache.fetch_header(HeaderId::Root(sst_id), || read_header_page(&dir, sst_id))?;
        let header =
            Header::decode(&mut page.bytes()).map_err(|e| LsmError::Internal(e.to_string()))?;
        match header {
            Header::Root {
                sst_id,
                range,
                bloom,
                size_bytes,
                index,
            } => Ok(SsTable {
                sst_id,
                range,
                bloom,
                size_bytes,
                index,
                dir,
            }),
            Header::Child { .. } => Err(LsmError::Internal(
                "sstable tail is not a root header".into(),
            )),
        }
    }

    pub fn sst_id(&self) -> SsTableId {
        self.sst_id
    }

    pub fn range(&self) -> &KeyRange {
        &self.range
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// The root's block index — inline, or a directory of shard pointers.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Newest version of `key` at `max_seq` in this table, or `None` if absent.
    ///
    /// Skips the whole table when `key` is outside its range or the bloom says
    /// no; otherwise faults the one child shard (if sharded) and the one data
    /// block whose range covers `key`.
    pub fn get(
        &self,
        key: &[u8],
        max_seq: u64,
        cache: &SstPageCache,
    ) -> Result<Option<KeyValue>, LsmError> {
        if !self.range.contains(key) || !self.bloom.contains(key) {
            return Ok(None);
        }
        let Some(block) = self.locate_block(key, cache)? else {
            return Ok(None);
        };

        let id = self.sst_id;
        let page = cache.fetch_block(
            PageId {
                table: id,
                offset: block.offset,
            },
            || read_range(&self.dir, id, block.offset, block.len),
        )?;
        let mut cursor = page.bytes();
        let data = DataBlock::decode(&mut cursor).map_err(|e| LsmError::Internal(e.to_string()))?;
        // Entries sort seq-descending, so the first match ≤ max_seq is newest.
        for kv in data.0 {
            if kv.key.user_key == key && kv.key.seq <= max_seq {
                return Ok(Some(kv));
            }
        }
        Ok(None)
    }

    /// Find the data block whose range covers `key`, faulting one child shard in
    /// when the index is sharded. Blocks and shards are sorted and disjoint, so
    /// at most one covers `key` at each level.
    fn locate_block(
        &self,
        key: &[u8],
        cache: &SstPageCache,
    ) -> Result<Option<BlockMeta>, LsmError> {
        match &self.index {
            Index::Inline(blocks) => Ok(covering(blocks, key).cloned()),
            Index::Sharded(chunks) => {
                let Some(cref) = covering(chunks, key) else {
                    return Ok(None);
                };
                let blocks = self.load_child(cref, cache)?;
                Ok(covering(&blocks, key).cloned())
            }
        }
    }

    /// Fault and decode the child shard `cref` points at, through the (shared)
    /// header cache.
    fn load_child(
        &self,
        cref: &ChunkRef,
        cache: &SstPageCache,
    ) -> Result<Vec<BlockMeta>, LsmError> {
        let id = self.sst_id;
        let page = cache.fetch_header(
            HeaderId::Child {
                table: id,
                offset: cref.offset,
            },
            || read_range(&self.dir, id, cref.offset, cref.len),
        )?;
        match Header::decode(&mut page.bytes()).map_err(|e| LsmError::Internal(e.to_string()))? {
            Header::Child { blocks, .. } => Ok(blocks),
            Header::Root { .. } => {
                Err(LsmError::Internal("expected child header, got root".into()))
            }
        }
    }

    /// Lazily stream this table's entries in `range` as of `max_seq`.
    ///
    /// Consumes the table — the returned stream owns it and faults one child
    /// shard / data block in at a time through `cache`, so peak memory is a
    /// single shard plus a single block rather than the whole result.
    pub fn scan(
        self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
        cache: &SstPageCache,
    ) -> KvStream<'_> {
        let cursor = match &self.index {
            Index::Inline(_) => Cursor::Inline { next: 0 },
            Index::Sharded(_) => Cursor::Sharded {
                next_chunk: 0,
                child: Vec::new(),
                next_in_child: 0,
            },
        };
        Box::new(BlockStream {
            table: self,
            cache,
            start: range.start_bound().cloned(),
            end: range.end_bound().cloned(),
            max_seq,
            cursor,
            current: Vec::new().into_iter(),
            done: false,
        })
    }
}

/// Where a [`BlockStream`] is in its walk down the index — the inline block list
/// or, when sharded, the current shard plus the position within it.
enum Cursor {
    Inline {
        next: usize,
    },
    Sharded {
        next_chunk: usize,
        child: Vec<BlockMeta>,
        next_in_child: usize,
    },
}

/// A lazy [`KvStream`] over one [`SsTable`]: it owns the table and walks the
/// index (root → shard → block) on demand, faulting the next overlapping block
/// in only when the current one is drained.
struct BlockStream<'a> {
    table: SsTable,
    cache: &'a SstPageCache,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    max_seq: u64,
    cursor: Cursor,
    current: std::vec::IntoIter<KeyValue>,
    done: bool,
}

impl BlockStream<'_> {
    /// Advance to the next data block overlapping the query range, returning its
    /// meta — faulting a child shard in when a shard boundary is crossed.
    fn next_block(&mut self) -> Option<Result<BlockMeta, ReadError>> {
        loop {
            match &mut self.cursor {
                Cursor::Inline { next } => {
                    let Index::Inline(blocks) = &self.table.index else {
                        unreachable!("inline cursor over a sharded index")
                    };
                    while *next < blocks.len() {
                        let b = &blocks[*next];
                        *next += 1;
                        if block_overlaps(b, &self.start, &self.end) {
                            return Some(Ok(b.clone()));
                        }
                    }
                    return None;
                }
                Cursor::Sharded {
                    next_chunk,
                    child,
                    next_in_child,
                } => {
                    // Drain the loaded shard first.
                    while *next_in_child < child.len() {
                        let b = &child[*next_in_child];
                        *next_in_child += 1;
                        if block_overlaps(b, &self.start, &self.end) {
                            return Some(Ok(b.clone()));
                        }
                    }
                    // Shard drained — find the next overlapping shard. Copy out
                    // its offset/len so we hold no borrow of the index over I/O.
                    let (offset, len) = {
                        let Index::Sharded(chunks) = &self.table.index else {
                            unreachable!("sharded cursor over an inline index")
                        };
                        loop {
                            if *next_chunk >= chunks.len() {
                                return None;
                            }
                            let c = &chunks[*next_chunk];
                            *next_chunk += 1;
                            if block_overlaps(c, &self.start, &self.end) {
                                break (c.offset, c.len);
                            }
                        }
                    };
                    let id = self.table.sst_id;
                    let page = match self
                        .cache
                        .fetch_header(HeaderId::Child { table: id, offset }, || {
                            read_range(&self.table.dir, id, offset, len)
                        }) {
                        Ok(p) => p,
                        Err(e) => return Some(Err(ReadError::Internal(e.to_string()))),
                    };
                    match Header::decode(&mut page.bytes()) {
                        Ok(Header::Child { blocks, .. }) => {
                            *child = blocks;
                            *next_in_child = 0;
                        }
                        Ok(Header::Root { .. }) => {
                            return Some(Err(ReadError::Internal(
                                "expected child header, got root".into(),
                            )));
                        }
                        Err(e) => return Some(Err(ReadError::Internal(e.to_string()))),
                    }
                    // Loop: drain the freshly loaded shard.
                }
            }
        }
    }

    /// Fault `block` in, decode it, and keep only entries in range/seq.
    fn load_entries(&self, block: &BlockMeta) -> Result<Vec<KeyValue>, ReadError> {
        let id = self.table.sst_id;
        let page = self
            .cache
            .fetch_block(
                PageId {
                    table: id,
                    offset: block.offset,
                },
                || read_range(&self.table.dir, id, block.offset, block.len),
            )
            .map_err(|e| ReadError::Internal(e.to_string()))?;
        let mut cursor = page.bytes();
        let data =
            DataBlock::decode(&mut cursor).map_err(|e| ReadError::Internal(e.to_string()))?;
        Ok(data
            .0
            .into_iter()
            .filter(|kv| {
                kv.key.seq <= self.max_seq && in_bounds(&kv.key.user_key, &self.start, &self.end)
            })
            .collect())
    }
}

impl Iterator for BlockStream<'_> {
    type Item = Result<KeyValue, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(kv) = self.current.next() {
                return Some(Ok(kv));
            }
            if self.done {
                return None;
            }
            match self.next_block() {
                Some(Ok(block)) => match self.load_entries(&block) {
                    Ok(entries) => self.current = entries.into_iter(),
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                },
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(e));
                }
                None => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

/// The data section plus the metadata `write` needs to assemble the header(s).
struct Built {
    /// The data-blocks section (file offset `0..size_bytes`); `write` appends
    /// shards and the root onto it.
    buf: Vec<u8>,
    blocks: Vec<BlockMeta>,
    bloom: BloomFilter,
    range: KeyRange,
    size_bytes: u64,
}

/// Block up sorted `entries` into page-aligned blocks, returning the data
/// section bytes and the metadata describing it.
fn build(config: &TableConfig, entries: Vec<KeyValue>) -> Built {
    let target = config.page.page_size_bytes.max(1);
    let mut data = Vec::new();
    let mut blocks = Vec::new();

    let mut i = 0;
    while i < entries.len() {
        let offset = data.len() as u64;
        let min_key = entries[i].key.user_key.clone();
        let mut max_key = min_key.clone();
        let mut block_size = 0usize;
        while i < entries.len() {
            let esize = data::entry_size(&entries[i]);
            if block_size > 0 && block_size + esize > target {
                break;
            }
            entries[i].encode(&mut data);
            max_key = entries[i].key.user_key.clone();
            block_size += esize;
            i += 1;
        }
        blocks.push(BlockMeta {
            min_key,
            max_key,
            offset,
            len: (data.len() as u64 - offset) as u32,
        });

        // Pad out to a page boundary so the next block starts page-aligned.
        let rem = data.len() % target;
        if rem != 0 {
            data.resize(data.len() + (target - rem), 0);
        }
    }

    let bloom = BloomFilter::build(
        config.bloom,
        entries.len(),
        entries.iter().map(|e| e.key.user_key.as_slice()),
    );
    let range = match (blocks.first(), blocks.last()) {
        (Some(first), Some(last)) => KeyRange {
            min: first.min_key.clone(),
            max: last.max_key.clone(),
        },
        _ => KeyRange {
            min: Vec::new(),
            max: Vec::new(),
        },
    };
    let size_bytes = data.len() as u64;
    Built {
        buf: data,
        blocks,
        bloom,
        range,
        size_bytes,
    }
}

/// Partition `blocks` into child shards of up to `per_chunk` each, append every
/// shard's encoded header to `buf`, and return the root's directory of
/// [`ChunkRef`]s pointing at them.
fn append_shards(
    buf: &mut Vec<u8>,
    sst_id: SsTableId,
    blocks: &[BlockMeta],
    per_chunk: usize,
) -> Vec<ChunkRef> {
    let mut chunks = Vec::new();
    for slice in blocks.chunks(per_chunk) {
        let offset = buf.len() as u64;
        let child = Header::Child {
            sst_id,
            blocks: slice.to_vec(),
        };
        let mut bytes = Vec::new();
        child.encode(&mut bytes);
        let len = bytes.len() as u32;
        buf.extend_from_slice(&bytes);
        chunks.push(ChunkRef {
            min_key: slice.first().unwrap().min_key.clone(),
            max_key: slice.last().unwrap().max_key.clone(),
            offset,
            len,
        });
    }
    chunks
}

/// The meta (block or shard) whose `[min, max]` covers `key`, if any. Entries
/// are sorted and disjoint, so at most one matches.
fn covering<'a>(metas: &'a [BlockMeta], key: &[u8]) -> Option<&'a BlockMeta> {
    metas
        .iter()
        .find(|m| m.min_key.as_slice() <= key && key <= m.max_key.as_slice())
}

fn sst_path(dir: &Path, id: SsTableId) -> PathBuf {
    dir.join(format!("{}.sst", id.0))
}

/// Read just the root header (the file's trailing `[root][root_len]`).
fn read_header_page(dir: &Path, id: SsTableId) -> io::Result<Page> {
    let mut file = File::open(sst_path(dir, id))?;
    let file_len = file.metadata()?.len();
    if file_len < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sstable file too small",
        ));
    }
    file.seek(SeekFrom::End(-4))?;
    let mut footer = [0u8; 4];
    file.read_exact(&mut footer)?;
    let header_len = u32::from_be_bytes(footer) as u64;
    if header_len + 4 > file_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "header_len exceeds file size",
        ));
    }
    file.seek(SeekFrom::End(-(4 + header_len as i64)))?;
    let mut bytes = vec![0u8; header_len as usize];
    file.read_exact(&mut bytes)?;
    Ok(Page::new(bytes))
}

/// Read an exact byte range (`[offset, offset+len)`) — a data block or a child
/// shard — never its padding.
fn read_range(dir: &Path, id: SsTableId, offset: u64, len: u32) -> io::Result<Page> {
    let mut file = File::open(sst_path(dir, id))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; len as usize];
    file.read_exact(&mut bytes)?;
    Ok(Page::new(bytes))
}

fn in_bounds(key: &[u8], start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> bool {
    let after_start = match start {
        Bound::Included(s) => key >= s.as_slice(),
        Bound::Excluded(s) => key > s.as_slice(),
        Bound::Unbounded => true,
    };
    let before_end = match end {
        Bound::Included(e) => key <= e.as_slice(),
        Bound::Excluded(e) => key < e.as_slice(),
        Bound::Unbounded => true,
    };
    after_start && before_end
}

/// Whether a block or shard's `[min, max]` could hold any key in the query range.
fn block_overlaps(b: &BlockMeta, start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> bool {
    let entirely_below = match start {
        Bound::Included(s) => b.max_key.as_slice() < s.as_slice(),
        Bound::Excluded(s) => b.max_key.as_slice() <= s.as_slice(),
        Bound::Unbounded => false,
    };
    let entirely_above = match end {
        Bound::Included(e) => b.min_key.as_slice() > e.as_slice(),
        Bound::Excluded(e) => b.min_key.as_slice() >= e.as_slice(),
        Bound::Unbounded => false,
    };
    !(entirely_below || entirely_above)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{InternalKey, OpType};
    use crate::{BloomConfig, PageConfig};
    use tempfile::TempDir;

    fn kv(key: &[u8], seq: u64, value: &[u8]) -> KeyValue {
        KeyValue {
            key: InternalKey {
                user_key: key.to_vec(),
                seq,
                op: OpType::Put,
            },
            value: value.to_vec(),
        }
    }

    /// A tiny block target, to force multiple (padded) blocks. Big enough
    /// `blocks_per_chunk` keeps the index inline.
    fn tiny_blocks() -> TableConfig {
        TableConfig {
            max_file_size_bytes: 1 << 20,
            bloom: BloomConfig { bits_per_key: 10 },
            page: PageConfig {
                page_size_bytes: 24,
                blocks_per_chunk: 256,
            },
        }
    }

    /// Tiny blocks *and* one block per shard, to force a sharded index.
    fn sharded_blocks() -> TableConfig {
        TableConfig {
            page: PageConfig {
                page_size_bytes: 24,
                blocks_per_chunk: 1,
            },
            ..tiny_blocks()
        }
    }

    /// Write a table to a fresh dir and open it through a cache.
    fn write_and_open(
        config: &TableConfig,
        entries: Vec<KeyValue>,
    ) -> (TempDir, SsTable, SstPageCache) {
        let tmp = tempfile::tempdir().unwrap();
        SsTable::write(SsTableId(1), tmp.path(), config, entries).unwrap();
        let cache = SstPageCache::default();
        let table = SsTable::open(SsTableId(1), tmp.path(), &cache).unwrap();
        (tmp, table, cache)
    }

    fn four_keys() -> Vec<KeyValue> {
        vec![
            kv(b"a", 1, b"1"),
            kv(b"b", 2, b"2"),
            kv(b"c", 3, b"3"),
            kv(b"d", 4, b"4"),
        ]
    }

    fn keys(stream: KvStream<'_>) -> Vec<Vec<u8>> {
        stream.map(|r| r.unwrap().key.user_key).collect()
    }

    fn inline_blocks(t: &SsTable) -> &[BlockMeta] {
        match t.index() {
            Index::Inline(blocks) => blocks,
            Index::Sharded(_) => panic!("expected an inline index"),
        }
    }

    #[test]
    fn packs_into_multiple_padded_blocks() {
        let (_tmp, t, _cache) = write_and_open(
            &tiny_blocks(),
            vec![kv(b"a", 1, b"1"), kv(b"b", 2, b"2"), kv(b"c", 3, b"3")],
        );
        let blocks = inline_blocks(&t);
        assert!(blocks.len() > 1, "tiny pages should force >1 block");
        for b in blocks {
            assert_eq!(
                b.offset % 24,
                0,
                "block offset {} not page-aligned",
                b.offset
            );
        }
        assert_eq!(t.range().min, b"a");
        assert_eq!(t.range().max, b"c");
    }

    #[test]
    fn scans_full_range_across_blocks() {
        let (_tmp, t, cache) = write_and_open(&tiny_blocks(), four_keys());
        assert_eq!(
            keys(t.scan(.., u64::MAX, &cache)),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn scans_sub_range_only_touches_relevant_blocks() {
        let (_tmp, t, cache) = write_and_open(&tiny_blocks(), four_keys());
        assert_eq!(
            keys(t.scan(b"b".to_vec()..=b"c".to_vec(), u64::MAX, &cache)),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        // header + only the two overlapping blocks were faulted in.
        assert_eq!(cache.len(), 1 + 2);
    }

    #[test]
    fn scan_faults_blocks_lazily() {
        let (_tmp, t, cache) = write_and_open(&tiny_blocks(), four_keys());
        assert_eq!(cache.len(), 1, "open faults just the header");

        let mut stream = t.scan(.., u64::MAX, &cache);
        stream.next().unwrap().unwrap(); // pulling one row faults only the first block
        assert_eq!(cache.len(), 2, "header + one data block, not all four");
    }

    #[test]
    fn padding_is_not_decoded_as_data() {
        // Each ~19-byte entry sits in its own 24-byte page, so every block is
        // padded. A scan must return exactly the written entries — never a
        // phantom row decoded from the zero padding.
        let (_tmp, t, cache) = write_and_open(
            &tiny_blocks(),
            vec![kv(b"a", 1, b"x"), kv(b"b", 2, b"y"), kv(b"c", 3, b"z")],
        );

        let payload: u64 = inline_blocks(&t).iter().map(|b| b.len as u64).sum();
        assert!(
            t.size_bytes() > payload,
            "test must actually exercise padding: size {} vs payload {}",
            t.size_bytes(),
            payload
        );

        let got: Vec<_> = t.scan(.., u64::MAX, &cache).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 3, "padding must not produce extra entries");
        assert_eq!(got[0].value, b"x");
        assert_eq!(got[2].value, b"z");
    }

    #[test]
    fn get_finds_present_key_and_misses_absent() {
        let (_tmp, t, cache) = write_and_open(&tiny_blocks(), four_keys());
        let hit = t.get(b"c", u64::MAX, &cache).unwrap();
        assert_eq!(hit.map(|kv| kv.value), Some(b"3".to_vec()));
        assert!(t.get(b"z", u64::MAX, &cache).unwrap().is_none());
    }

    #[test]
    fn large_index_is_sharded() {
        let (_tmp, t, _cache) = write_and_open(&sharded_blocks(), four_keys());
        match t.index() {
            Index::Sharded(chunks) => assert_eq!(chunks.len(), 4, "one block per shard → 4 shards"),
            Index::Inline(_) => panic!("expected a sharded index"),
        }
        assert_eq!(t.range().min, b"a");
        assert_eq!(t.range().max, b"d");
    }

    #[test]
    fn sharded_get_walks_root_to_shard_to_block() {
        let (_tmp, t, cache) = write_and_open(&sharded_blocks(), four_keys());
        // Hit: root → covering shard → covering block.
        assert_eq!(
            t.get(b"c", u64::MAX, &cache).unwrap().map(|kv| kv.value),
            Some(b"3".to_vec())
        );
        // header(root) + one child shard + one data block faulted in.
        assert_eq!(cache.len(), 3);
        // Miss inside the table's range still returns None.
        assert!(t.get(b"zz", u64::MAX, &cache).unwrap().is_none());
    }

    #[test]
    fn sharded_scan_matches_full_and_sub_range() {
        let (_tmp, t, cache) = write_and_open(&sharded_blocks(), four_keys());
        assert_eq!(
            keys(t.scan(.., u64::MAX, &cache)),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
        let (_tmp2, t2, cache2) = write_and_open(&sharded_blocks(), four_keys());
        assert_eq!(
            keys(t2.scan(b"b".to_vec()..=b"c".to_vec(), u64::MAX, &cache2)),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
    }

    /// Tiny blocks with two blocks per shard, to exercise walking *within* a
    /// shard and *across* shards in the same table.
    fn multi_block_shards() -> TableConfig {
        TableConfig {
            page: PageConfig {
                page_size_bytes: 24,
                blocks_per_chunk: 2,
            },
            ..tiny_blocks()
        }
    }

    fn six_keys() -> Vec<KeyValue> {
        vec![
            kv(b"a", 1, b"1"),
            kv(b"b", 2, b"2"),
            kv(b"c", 3, b"3"),
            kv(b"d", 4, b"4"),
            kv(b"e", 5, b"5"),
            kv(b"f", 6, b"6"),
        ]
    }

    #[test]
    fn multi_block_shards_get_reaches_every_shard() {
        let (_tmp, t, cache) = write_and_open(&multi_block_shards(), six_keys());
        match t.index() {
            Index::Sharded(chunks) => assert_eq!(chunks.len(), 3, "6 blocks / 2 per shard"),
            Index::Inline(_) => panic!("expected a sharded index"),
        }
        // Reach a key in the first, middle, and last shard.
        for (k, v) in [(&b"a"[..], &b"1"[..]), (b"c", b"3"), (b"f", b"6")] {
            assert_eq!(t.get(k, u64::MAX, &cache).unwrap().unwrap().value, v);
        }
    }

    #[test]
    fn multi_block_shards_scan_crosses_shard_boundaries() {
        let (_tmp, t, cache) = write_and_open(&multi_block_shards(), six_keys());
        // A sub-range that starts inside shard 0 and ends inside shard 2.
        assert_eq!(
            keys(t.scan(b"b".to_vec()..=b"e".to_vec(), u64::MAX, &cache)),
            vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec(), b"e".to_vec()]
        );
    }

    #[test]
    fn get_respects_max_seq_within_a_block() {
        // Both versions of "a" share one block; an older snapshot sees the older
        // value, the newest snapshot the newer one.
        let cfg = TableConfig {
            page: PageConfig {
                page_size_bytes: 4096,
                blocks_per_chunk: 256,
            },
            ..tiny_blocks()
        };
        // Entries arrive sorted user-key asc, seq desc (the LSM's write order).
        let (_tmp, t, cache) = write_and_open(&cfg, vec![kv(b"a", 2, b"new"), kv(b"a", 1, b"old")]);
        assert_eq!(
            t.get(b"a", u64::MAX, &cache).unwrap().unwrap().value,
            b"new"
        );
        assert_eq!(t.get(b"a", 1, &cache).unwrap().unwrap().value, b"old");
    }

    #[test]
    fn empty_table_has_empty_inline_index_and_no_rows() {
        let (_tmp, t, cache) = write_and_open(&tiny_blocks(), vec![]);
        assert!(
            matches!(t.index(), Index::Inline(blocks) if blocks.is_empty()),
            "no entries → an empty inline index"
        );
        assert!(t.get(b"a", u64::MAX, &cache).unwrap().is_none());
        assert!(keys(t.scan(.., u64::MAX, &cache)).is_empty());
    }
}
