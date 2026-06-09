//! The SSTable file: data blocks followed by a header.
//!
//! `write` packs sorted entries into blocks of about `page_size`, pads each to
//! a page boundary, and records every block (key range + offset/len) in the
//! header. Reads go through [`SstPageCache`]: `open` faults in the header,
//! `scan` faults in only the data blocks whose range overlaps the query.
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
use crate::iterator::{KvStream, Scan};
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
            || read_block_page(&self.dir, id, block),
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
}

impl Scan for SsTable {
    fn scan(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
        cache: &SstPageCache,
    ) -> KvStream<'_> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();
        let id = self.header.sst_id;

        // Fault in and decode only the blocks whose range overlaps the query.
        let mut out: Vec<Result<KeyValue, ReadError>> = Vec::new();
        for (idx, block) in self.header.blocks.iter().enumerate() {
            if !block_overlaps(block, &start, &end) {
                continue;
            }
            let page = match cache.fetch_block(
                PageId {
                    table: id,
                    page_index: idx as u32,
                },
                || read_block_page(&self.dir, id, block),
            ) {
                Ok(page) => page,
                Err(e) => {
                    out.push(Err(ReadError::Internal(e.to_string())));
                    continue;
                }
            };
            let mut cursor = page.bytes();
            match DataBlock::decode(&mut cursor) {
                Ok(decoded) => {
                    for kv in decoded.0 {
                        if kv.key.seq <= max_seq && in_bounds(&kv.key.user_key, &start, &end) {
                            out.push(Ok(kv));
                        }
                    }
                }
                Err(e) => out.push(Err(ReadError::Internal(e.to_string()))),
            }
        }
        Box::new(out.into_iter())
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
fn read_block_page(dir: &Path, id: SsTableId, block: &BlockMeta) -> io::Result<Page> {
    let mut file = File::open(sst_path(dir, id))?;
    file.seek(SeekFrom::Start(block.offset))?;
    let mut bytes = vec![0u8; block.len as usize];
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
        let (_tmp, t, cache) = write_and_open(vec![
            kv(b"a", 1, b"1"),
            kv(b"b", 2, b"2"),
            kv(b"c", 3, b"3"),
            kv(b"d", 4, b"4"),
        ]);
        assert_eq!(
            keys(t.scan(.., u64::MAX, &cache)),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn scans_sub_range_only_touches_relevant_blocks() {
        let (_tmp, t, cache) = write_and_open(vec![
            kv(b"a", 1, b"1"),
            kv(b"b", 2, b"2"),
            kv(b"c", 3, b"3"),
            kv(b"d", 4, b"4"),
        ]);
        assert_eq!(
            keys(t.scan(b"b".to_vec()..=b"c".to_vec(), u64::MAX, &cache)),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        // Only the two overlapping blocks were faulted in.
        assert_eq!(cache.len(), 1 /* header */ + 2 /* blocks */);
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

        // Sanity: padding really is present (data section larger than payload).
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
}
