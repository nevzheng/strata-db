use std::ops::{Bound, RangeBounds};

use crate::StorageError;
use crate::memstore::{InternalKey, OpType};

const DEFAULT_L0_CAPACITY: usize = 64;

/// In-memory sorted buffer for recently compacted memtable entries.
pub struct LevelZero {
    entries: Vec<(InternalKey, Vec<u8>)>,
    capacity: usize,
}

impl LevelZero {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            capacity: DEFAULT_L0_CAPACITY,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
        }
    }

    /// Merge incoming entries into the sorted L0 buffer.
    ///
    /// Returns an error if the merged result exceeds capacity.
    pub fn merge(&mut self, incoming: Vec<(InternalKey, Vec<u8>)>) -> Result<(), StorageError> {
        self.entries.extend(incoming);
        self.entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        if self.entries.len() > self.capacity {
            return Err(StorageError::InternalError(format!(
                "L0 full: {} entries exceeds capacity {}",
                self.entries.len(),
                self.capacity
            )));
        }
        Ok(())
    }

    /// Look up the latest version of a user key.
    ///
    /// Returns `Some` with the value if the latest entry is a `Put`,
    /// `None` if it's a `Delete` or the key doesn't exist.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        let probe = InternalKey {
            key: key.to_vec(),
            seq: u64::MAX,
            op: OpType::Put,
        };
        let idx = self.entries.partition_point(|(ik, _)| ik < &probe);
        if idx < self.entries.len() && self.entries[idx].0.key == key {
            let (ik, value) = &self.entries[idx];
            return match ik.op {
                OpType::Put => Some(value.as_slice()),
                OpType::Delete => None,
            };
        }
        None
    }

    /// Return all entries within the given user-key range, in `InternalKey` order.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> &[(InternalKey, Vec<u8>)] {
        let start_idx = match range.start_bound() {
            Bound::Included(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: u64::MAX,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik < &probe)
            }
            Bound::Excluded(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: 0,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik <= &probe)
            }
            Bound::Unbounded => 0,
        };
        let end_idx = match range.end_bound() {
            Bound::Included(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: 0,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik <= &probe)
            }
            Bound::Excluded(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: u64::MAX,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik < &probe)
            }
            Bound::Unbounded => self.entries.len(),
        };
        &self.entries[start_idx..end_idx]
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for LevelZero {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_entry(key: &[u8], value: &[u8], seq: u64) -> (InternalKey, Vec<u8>) {
        (
            InternalKey {
                key: key.to_vec(),
                seq,
                op: OpType::Put,
            },
            value.to_vec(),
        )
    }

    fn delete_entry(key: &[u8], seq: u64) -> (InternalKey, Vec<u8>) {
        (
            InternalKey {
                key: key.to_vec(),
                seq,
                op: OpType::Delete,
            },
            Vec::new(),
        )
    }

    #[test]
    fn get_returns_latest_put() {
        let mut l0 = LevelZero::new();
        l0.merge(vec![put_entry(b"a", b"v1", 1), put_entry(b"a", b"v2", 2)])
            .unwrap();
        assert_eq!(l0.get(b"a"), Some(&b"v2"[..]));
    }

    #[test]
    fn get_returns_none_for_tombstone() {
        let mut l0 = LevelZero::new();
        l0.merge(vec![put_entry(b"a", b"v1", 1), delete_entry(b"a", 2)])
            .unwrap();
        assert_eq!(l0.get(b"a"), None);
    }

    #[test]
    fn get_returns_none_for_missing_key() {
        let l0 = LevelZero::new();
        assert_eq!(l0.get(b"missing"), None);
    }

    #[test]
    fn scan_returns_range() {
        let mut l0 = LevelZero::new();
        l0.merge(vec![
            put_entry(b"a", b"1", 1),
            put_entry(b"b", b"2", 2),
            put_entry(b"c", b"3", 3),
            put_entry(b"d", b"4", 4),
        ])
        .unwrap();

        let results = l0.scan(b"b".to_vec()..=b"c".to_vec());
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.key, b"b");
        assert_eq!(results[1].0.key, b"c");
    }

    #[test]
    fn scan_unbounded_returns_all() {
        let mut l0 = LevelZero::new();
        l0.merge(vec![put_entry(b"a", b"1", 1), put_entry(b"b", b"2", 2)])
            .unwrap();

        let results = l0.scan(..);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn merge_sorts_entries() {
        let mut l0 = LevelZero::new();
        l0.merge(vec![put_entry(b"c", b"3", 3), put_entry(b"a", b"1", 1)])
            .unwrap();
        l0.merge(vec![put_entry(b"b", b"2", 4)]).unwrap();

        let all = l0.scan(..);
        let keys: Vec<&[u8]> = all.iter().map(|(ik, _)| ik.key.as_slice()).collect();
        assert_eq!(keys, vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
    }

    #[test]
    fn merge_returns_error_when_full() {
        let mut l0 = LevelZero::with_capacity(2);
        l0.merge(vec![put_entry(b"a", b"1", 1), put_entry(b"b", b"2", 2)])
            .unwrap();

        let err = l0.merge(vec![put_entry(b"c", b"3", 3)]).unwrap_err();
        assert!(
            matches!(err, StorageError::InternalError(ref msg) if msg.contains("L0 full")),
            "expected L0 full, got: {err:?}"
        );
    }
}
