//! The SSTable file: data blocks followed by a header.
//!
//! `build` packs sorted entries into blocks of about `page_size` (splitting
//! when the next entry would overflow), recording each block's key range and
//! byte offset in the header's index. `scan` consults that index to skip
//! blocks that can't overlap the query.
//!
//! On-disk layout:
//! ```text
//! | block 0 | block 1 | ... | header | header_len (4B) |
//! ```
//! (Open is eager/whole-file for now; per-block lazy loading through
//! `SstPageCache` is the next step — the index is what enables it.)

use std::fs::File;
use std::io::{self, Write};
use std::ops::{Bound, RangeBounds};
use std::path::Path;

use super::bloom::BloomFilter;
use super::codec::{Decode, Encode};
use super::data::{self, DataBlock};
use super::header::{BlockMeta, Header};
use crate::iterator::{KvStream, Scan};
use crate::key::{KeyRange, KeyValue};
use crate::{LsmError, ReadError, SsTableId, TableConfig};

/// An SSTable: its [`Header`] (with block index) plus the raw bytes of its
/// data blocks.
pub struct SsTable {
    pub header: Header,
    data: Vec<u8>,
}

impl SsTable {
    /// Build a table from sorted entries, packing them into blocks of about
    /// `config.page.page_size_bytes` and recording each block in the header.
    pub fn build(sst_id: SsTableId, config: &TableConfig, entries: Vec<KeyValue>) -> Self {
        let target = config.page.page_size_bytes;
        let mut data = Vec::new();
        let mut blocks = Vec::new();

        let mut i = 0;
        while i < entries.len() {
            let offset = data.len() as u64;
            let min_key = entries[i].key.user_key.clone();
            let mut max_key = min_key.clone();
            let mut block_size = 0usize;
            // Pack entries until the next would push the block past `target`.
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
        SsTable { header, data }
    }

    /// Serialize to `{dir}/{id}.sst` as `[blocks][header][header_len]`.
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let mut header_bytes = Vec::new();
        self.header.encode(&mut header_bytes);

        let mut buf = self.data.clone();
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());

        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.sst", self.header.sst_id.0));
        let mut file = File::create(&path)?;
        file.write_all(&buf)?;
        file.sync_all()?;
        Ok(())
    }

    /// Read a table back from `{dir}/{id}.sst` (whole-file, eager for now).
    pub fn open(sst_id: SsTableId, dir: &Path) -> Result<SsTable, LsmError> {
        let path = dir.join(format!("{}.sst", sst_id.0));
        let bytes = std::fs::read(&path)?;
        if bytes.len() < 4 {
            return Err(LsmError::Internal("sstable file too small".into()));
        }
        let header_len = u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as usize;
        let header_start = bytes
            .len()
            .checked_sub(4 + header_len)
            .ok_or_else(|| LsmError::Internal("header_len exceeds file size".into()))?;
        let mut header_bytes = &bytes[header_start..bytes.len() - 4];
        let header =
            Header::decode(&mut header_bytes).map_err(|e| LsmError::Internal(e.to_string()))?;
        let data = bytes[..header_start].to_vec();
        Ok(SsTable { header, data })
    }
}

impl Scan for SsTable {
    fn scan(&self, range: impl RangeBounds<Vec<u8>>, max_seq: u64) -> KvStream<'_> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();

        // Decode only the blocks whose key range overlaps the query, in order.
        let mut out: Vec<Result<KeyValue, ReadError>> = Vec::new();
        for block in &self.header.blocks {
            if !block_overlaps(block, &start, &end) {
                continue;
            }
            let mut cursor =
                &self.data[block.offset as usize..block.offset as usize + block.len as usize];
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

    /// A config with a tiny block target, to force multiple blocks.
    fn tiny_blocks() -> TableConfig {
        TableConfig {
            max_file_size_bytes: 1 << 20,
            bloom: BloomConfig { bits_per_key: 10 },
            page: PageConfig {
                page_size_bytes: 24,
            },
        }
    }

    fn table() -> SsTable {
        SsTable::build(
            SsTableId(1),
            &tiny_blocks(),
            vec![
                kv(b"a", 1, b"1"),
                kv(b"b", 2, b"2"),
                kv(b"c", 3, b"3"),
                kv(b"d", 4, b"4"),
            ],
        )
    }

    fn keys(stream: KvStream<'_>) -> Vec<Vec<u8>> {
        stream.map(|r| r.unwrap().key.user_key).collect()
    }

    #[test]
    fn packs_into_multiple_blocks() {
        let t = table();
        assert!(
            t.header.blocks.len() > 1,
            "tiny page size should force >1 block, got {}",
            t.header.blocks.len()
        );
        assert_eq!(t.header.range.min, b"a");
        assert_eq!(t.header.range.max, b"d");
    }

    #[test]
    fn scans_full_range_across_blocks() {
        assert_eq!(
            keys(table().scan(.., u64::MAX)),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn scans_sub_range() {
        assert_eq!(
            keys(table().scan(b"b".to_vec()..=b"c".to_vec(), u64::MAX)),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn write_then_open_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let t = table();
        t.write(tmp.path()).unwrap();

        let opened = SsTable::open(SsTableId(1), tmp.path()).unwrap();
        assert_eq!(opened.header.blocks, t.header.blocks);
        assert_eq!(
            keys(opened.scan(.., u64::MAX)),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }
}
