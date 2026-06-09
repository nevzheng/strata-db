//! The SSTable file: a [`Header`] plus a [`DataBlock`].
//!
//! For now a file is exactly one header and one data block holding all its
//! entries, with a single file-level bloom in the header. Later the header
//! grows per-block metadata (key range, tuple count, size, bloom) kept in
//! their own paged index/filter sections — not crammed inline — so large
//! files can be navigated a block at a time.

use std::ops::{Bound, RangeBounds};

use super::data::DataBlock;
use super::header::Header;
use crate::iterator::{KvStream, Scan};
use crate::key::{KeyRange, KeyValue};
use crate::{BloomConfig, BloomFilter, ReadError, SsTableId};

/// An opened SSTable file: its [`Header`] plus its [`DataBlock`].
pub struct SsTable {
    pub header: Header,
    pub data: DataBlock,
}

impl SsTable {
    /// Build a table from sorted entries: derives the header (id, key range,
    /// a bloom over the user keys, data size) and packs the entries into a
    /// single data block.
    pub fn build(sst_id: SsTableId, bloom_cfg: BloomConfig, entries: Vec<KeyValue>) -> Self {
        let min = entries
            .first()
            .map(|e| e.key.user_key.clone())
            .unwrap_or_default();
        let max = entries
            .last()
            .map(|e| e.key.user_key.clone())
            .unwrap_or_default();
        let size_bytes = entries
            .iter()
            .map(|e| (e.key.user_key.len() + e.value.len()) as u64)
            .sum();
        let bloom = BloomFilter::build(
            bloom_cfg,
            entries.len(),
            entries.iter().map(|e| e.key.user_key.as_slice()),
        );
        let header = Header {
            sst_id,
            range: KeyRange { min, max },
            bloom,
            size_bytes,
        };
        Self {
            header,
            data: DataBlock(entries),
        }
    }
}

impl Scan for SsTable {
    fn scan(&self, range: impl RangeBounds<Vec<u8>>, max_seq: u64) -> KvStream<'_> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();
        let iter = self
            .data
            .0
            .iter()
            .filter(move |e| in_bounds(&e.key.user_key, &start, &end) && e.key.seq <= max_seq)
            .map(|e| Ok::<KeyValue, ReadError>(e.clone()));
        Box::new(iter)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{InternalKey, OpType};

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

    fn table() -> SsTable {
        SsTable::build(
            SsTableId(1),
            BloomConfig { bits_per_key: 10 },
            vec![kv(b"a", 1, b"1"), kv(b"b", 2, b"2"), kv(b"c", 3, b"3")],
        )
    }

    fn keys(stream: KvStream<'_>) -> Vec<Vec<u8>> {
        stream.map(|r| r.unwrap().key.user_key).collect()
    }

    #[test]
    fn scans_full_range() {
        assert_eq!(
            keys(table().scan(.., u64::MAX)),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
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
    fn respects_max_seq() {
        assert_eq!(
            keys(table().scan(.., 2)),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn header_carries_range_and_bloom() {
        let t = table();
        assert_eq!(t.header.range.min, b"a");
        assert_eq!(t.header.range.max, b"c");
        assert!(t.header.bloom.contains(b"b"));
        assert!(!t.header.bloom.contains(b"missing"));
    }
}
