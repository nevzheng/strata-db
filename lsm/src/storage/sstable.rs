//! The SSTable file: data blocks followed by a header.
//!
//! `write` packs sorted entries into blocks of about `page_size`, pads each to
//! a page boundary, and records every block (key range + offset/len) in the
//! header. Reads go through [`SstPageCache`]: `open` faults in the header,
//! `get`/`scan` fault in only the data blocks they actually need — `scan`
//! lazily, one block at a time, so peak memory is a single block.
//!
//! On-disk layout:
//! ```text
//! | block 0 | (pad) | block 1 | (pad) | ... | header | header_len (4B) |
//! ```

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};

use super::bloom::BloomFilter;
use super::cache::SstPageCache;
use super::codec::{Decode, Encode};
use super::data::{self, DataBlock};
use super::header::{BlockMeta, Header};
use super::page::{Page, PageId};
use crate::iterator::KvStream;
use crate::key::{KeyRange, KeyValue};
use crate::{LsmError, ReadError, SsTableId, TableConfig};

/// A handle to an on-disk SSTable: its [`Header`] (with block index), loaded
/// through the cache, plus the directory its file lives in. Data blocks are
/// faulted in on demand.
pub struct SsTable {
    header: Header,
    dir: PathBuf,
}

impl SsTable {
    /// Block up `entries`, build the header, and write `{dir}/{id}.sst`.
    pub fn write(
        sst_id: SsTableId,
        dir: &Path,
        config: &TableConfig,
        entries: Vec<KeyValue>,
    ) -> io::Result<()> {
        let (data, header) = build(sst_id, config, entries);

        let mut header_bytes = Vec::new();
        header.encode(&mut header_bytes);
        let mut buf = data;
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());

        std::fs::create_dir_all(dir)?;
        let mut file = File::create(sst_path(dir, sst_id))?;
        file.write_all(&buf)?;
        file.sync_all()?;
        Ok(())
    }

    /// Open a table for reading, loading its header through `cache`.
    pub fn open(sst_id: SsTableId, dir: &Path, cache: &SstPageCache) -> Result<SsTable, LsmError> {
        let dir = dir.to_path_buf();
        let page = cache.fetch_header(sst_id, || read_header_page(&dir, sst_id))?;
        let mut bytes = page.bytes();
        let header = Header::decode(&mut bytes).map_err(|e| LsmError::Internal(e.to_string()))?;
        Ok(SsTable { header, dir })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Newest version of `key` at `max_seq` in this table, or `None` if absent.
    ///
    /// Skips the whole table when `key` is outside its range or the bloom says
    /// no; otherwise reads just the one block whose range covers `key`.
    pub fn get(
        &self,
        key: &[u8],
        max_seq: u64,
        cache: &SstPageCache,
    ) -> Result<Option<KeyValue>, LsmError> {
        if !self.header.range.contains(key) || !self.header.bloom.contains(key) {
            return Ok(None);
        }
        // Blocks are sorted and disjoint, so at most one covers `key`.
        let found = self
            .header
            .blocks
            .iter()
            .enumerate()
            .find(|(_, b)| b.min_key.as_slice() <= key && key <= b.max_key.as_slice());
        let Some((idx, block)) = found else {
            return Ok(None);
        };

        let id = self.header.sst_id;
        let page = cache.fetch_block(
            PageId {
                table: id,
                page_index: idx as u32,
            },
            || read_block_page(&self.dir, id, block.offset, block.len),
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

    /// Lazily stream this table's entries in `range` as of `max_seq`.
    ///
    /// Consumes the table — the returned stream owns it and faults one block in
    /// at a time through `cache`, so peak memory is a single block rather than
    /// the whole result.
    pub fn scan(
        self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
        cache: &SstPageCache,
    ) -> KvStream<'_> {
        Box::new(BlockStream {
            table: self,
            cache,
            start: range.start_bound().cloned(),
            end: range.end_bound().cloned(),
            max_seq,
            next_block: 0,
            current: Vec::new().into_iter(),
        })
    }
}

/// A lazy [`KvStream`] over one [`SsTable`]: it owns the table and faults the
/// next overlapping block in only when the current one is drained.
struct BlockStream<'a> {
    table: SsTable,
    cache: &'a SstPageCache,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    max_seq: u64,
    next_block: usize,
    current: std::vec::IntoIter<KeyValue>,
}

impl BlockStream<'_> {
    /// Fault block `idx` in, decode it, and keep only entries in range/seq.
    fn load(&self, idx: usize, offset: u64, len: u32) -> Result<Vec<KeyValue>, ReadError> {
        let id = self.table.header.sst_id;
        let page = self
            .cache
            .fetch_block(
                PageId {
                    table: id,
                    page_index: idx as u32,
                },
                || read_block_page(&self.table.dir, id, offset, len),
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
            // Current block drained — find the next overlapping block.
            let (idx, offset, len) = loop {
                let idx = self.next_block;
                if idx >= self.table.header.blocks.len() {
                    return None;
                }
                self.next_block += 1;
                let b = &self.table.header.blocks[idx];
                if block_overlaps(b, &self.start, &self.end) {
                    break (idx, b.offset, b.len);
                }
            };
            match self.load(idx, offset, len) {
                Ok(entries) => self.current = entries.into_iter(),
                Err(e) => {
                    self.next_block = self.table.header.blocks.len(); // stop after an error
                    return Some(Err(e));
                }
            }
        }
    }
}

/// Block up sorted `entries` into page-aligned blocks, returning the data
/// section bytes and the header describing it.
fn build(sst_id: SsTableId, config: &TableConfig, entries: Vec<KeyValue>) -> (Vec<u8>, Header) {
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
    let header = Header {
        sst_id,
        range,
        bloom,
        size_bytes: data.len() as u64,
        blocks,
    };
    (data, header)
}

fn sst_path(dir: &Path, id: SsTableId) -> PathBuf {
    dir.join(format!("{}.sst", id.0))
}

/// Read just the header (the file's trailing `[header][header_len]`).
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

/// Read one data block's exact bytes (`[offset, offset+len)`), never its padding.
fn read_block_page(dir: &Path, id: SsTableId, offset: u64, len: u32) -> io::Result<Page> {
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

/// Whether a block's `[min, max]` could hold any key in the query range.
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

    /// A tiny block target, to force multiple (padded) blocks.
    fn tiny_blocks() -> TableConfig {
        TableConfig {
            max_file_size_bytes: 1 << 20,
            bloom: BloomConfig { bits_per_key: 10 },
            page: PageConfig {
                page_size_bytes: 24,
            },
        }
    }

    /// Write a table to a fresh dir and open it through a cache.
    fn write_and_open(entries: Vec<KeyValue>) -> (TempDir, SsTable, SstPageCache) {
        let tmp = tempfile::tempdir().unwrap();
        SsTable::write(SsTableId(1), tmp.path(), &tiny_blocks(), entries).unwrap();
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

    #[test]
    fn packs_into_multiple_padded_blocks() {
        let (_tmp, t, _cache) = write_and_open(vec![
            kv(b"a", 1, b"1"),
            kv(b"b", 2, b"2"),
            kv(b"c", 3, b"3"),
        ]);
        assert!(
            t.header().blocks.len() > 1,
            "tiny pages should force >1 block"
        );
        for b in &t.header().blocks {
            assert_eq!(
                b.offset % 24,
                0,
                "block offset {} not page-aligned",
                b.offset
            );
        }
        assert_eq!(t.header().range.min, b"a");
        assert_eq!(t.header().range.max, b"c");
    }

    #[test]
    fn scans_full_range_across_blocks() {
        let (_tmp, t, cache) = write_and_open(four_keys());
        assert_eq!(
            keys(t.scan(.., u64::MAX, &cache)),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn scans_sub_range_only_touches_relevant_blocks() {
        let (_tmp, t, cache) = write_and_open(four_keys());
        assert_eq!(
            keys(t.scan(b"b".to_vec()..=b"c".to_vec(), u64::MAX, &cache)),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        // header + only the two overlapping blocks were faulted in.
        assert_eq!(cache.len(), 1 + 2);
    }

    #[test]
    fn scan_faults_blocks_lazily() {
        let (_tmp, t, cache) = write_and_open(four_keys());
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
        let (_tmp, t, cache) = write_and_open(vec![
            kv(b"a", 1, b"x"),
            kv(b"b", 2, b"y"),
            kv(b"c", 3, b"z"),
        ]);

        let payload: u64 = t.header().blocks.iter().map(|b| b.len as u64).sum();
        assert!(
            t.header().size_bytes > payload,
            "test must actually exercise padding: size {} vs payload {}",
            t.header().size_bytes,
            payload
        );

        let got: Vec<_> = t.scan(.., u64::MAX, &cache).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 3, "padding must not produce extra entries");
        assert_eq!(got[0].value, b"x");
        assert_eq!(got[2].value, b"z");
    }

    #[test]
    fn get_finds_present_key_and_misses_absent() {
        let (_tmp, t, cache) = write_and_open(four_keys());
        let hit = t.get(b"c", u64::MAX, &cache).unwrap();
        assert_eq!(hit.map(|kv| kv.value), Some(b"3".to_vec()));
        assert!(t.get(b"z", u64::MAX, &cache).unwrap().is_none());
    }
}
