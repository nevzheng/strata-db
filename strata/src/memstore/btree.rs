use std::collections::BTreeMap;
use std::ops::{Bound, RangeBounds};

use super::{InternalKey, MemStore, OpType, ReadError, WriteError};

const DEFAULT_CAPACITY: usize = 4 * 1024 * 1024; // 4 MB

/// A [`MemStore`] backed by [`BTreeMap`].
pub struct BTreeMapStore {
    store: BTreeMap<InternalKey, Box<[u8]>>,
    capacity: usize,
    current_size: usize,
}

impl Default for BTreeMapStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BTreeMapStore {
    pub fn new() -> Self {
        Self {
            store: BTreeMap::new(),
            capacity: DEFAULT_CAPACITY,
            current_size: 0,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            store: BTreeMap::new(),
            capacity,
            current_size: 0,
        }
    }
}

impl MemStore for BTreeMapStore {
    fn put(&mut self, key: InternalKey, value: &[u8]) -> Result<(), WriteError> {
        self.current_size += key.key.len() + value.len();
        self.store.insert(key, value.into());
        Ok(())
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, ReadError> {
        // Probe with max seq so we land just before the first entry for this user key.
        let probe = InternalKey {
            key: key.to_vec(),
            seq: u64::MAX,
            op: OpType::Put,
        };
        if let Some((ikey, value)) = self.store.range(probe..).next()
            && ikey.key == key
        {
            return match ikey.op {
                OpType::Put => Ok(Some(value.to_vec())),
                OpType::Delete => Ok(None),
            };
        }
        Ok(None)
    }

    fn scan(
        &self,
        range: impl RangeBounds<Vec<u8>>,
    ) -> Result<Vec<(InternalKey, Vec<u8>)>, ReadError> {
        let start = match range.start_bound() {
            Bound::Included(k) => Bound::Included(InternalKey {
                key: k.clone(),
                seq: u64::MAX,
                op: OpType::Put,
            }),
            Bound::Excluded(k) => Bound::Excluded(InternalKey {
                key: k.clone(),
                seq: 0,
                op: OpType::Put,
            }),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end = match range.end_bound() {
            Bound::Included(k) => Bound::Included(InternalKey {
                key: k.clone(),
                seq: 0,
                op: OpType::Put,
            }),
            Bound::Excluded(k) => Bound::Excluded(InternalKey {
                key: k.clone(),
                seq: u64::MAX,
                op: OpType::Put,
            }),
            Bound::Unbounded => Bound::Unbounded,
        };

        let results = self
            .store
            .range((start, end))
            .map(|(ikey, value)| (ikey.clone(), value.to_vec()))
            .collect();
        Ok(results)
    }

    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError> {
        let probe = InternalKey {
            key: key.to_vec(),
            seq: max_seq,
            op: OpType::Put,
        };
        if let Some((ikey, value)) = self.store.range(probe..).next()
            && ikey.key == key
        {
            return match ikey.op {
                OpType::Put => Ok(Some(value.to_vec())),
                OpType::Delete => Ok(None),
            };
        }
        Ok(None)
    }

    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> Result<Vec<(InternalKey, Vec<u8>)>, ReadError> {
        let start = match range.start_bound() {
            Bound::Included(k) => Bound::Included(InternalKey {
                key: k.clone(),
                seq: max_seq,
                op: OpType::Put,
            }),
            Bound::Excluded(k) => Bound::Excluded(InternalKey {
                key: k.clone(),
                seq: 0,
                op: OpType::Put,
            }),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end = match range.end_bound() {
            Bound::Included(k) => Bound::Included(InternalKey {
                key: k.clone(),
                seq: 0,
                op: OpType::Put,
            }),
            Bound::Excluded(k) => Bound::Excluded(InternalKey {
                key: k.clone(),
                seq: max_seq,
                op: OpType::Put,
            }),
            Bound::Unbounded => Bound::Unbounded,
        };

        let mut results = Vec::new();
        let mut last_key: Option<&[u8]> = None;
        for (ikey, value) in self.store.range((start, end)) {
            if ikey.seq > max_seq {
                continue;
            }
            // InternalKey ordering: user key asc, seq desc.
            // First entry per user key with seq <= max_seq is the latest visible version.
            if last_key == Some(ikey.key.as_slice()) {
                continue;
            }
            last_key = Some(&ikey.key);
            if ikey.op == OpType::Put {
                results.push((ikey.clone(), value.to_vec()));
            }
        }
        Ok(results)
    }

    fn size(&self) -> usize {
        self.current_size
    }

    fn is_full(&self) -> bool {
        self.current_size >= self.capacity
    }

    fn fits(&self, key: &InternalKey, value_len: usize) -> bool {
        self.current_size + key.key.len() + value_len <= self.capacity
    }

    fn clear(&mut self) {
        self.store.clear();
        self.current_size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_key(store: &mut BTreeMapStore, key: &[u8], value: &[u8], seq: u64) {
        store
            .put(
                InternalKey {
                    key: key.to_vec(),
                    seq,
                    op: OpType::Put,
                },
                value,
            )
            .unwrap();
    }

    fn delete_key(store: &mut BTreeMapStore, key: &[u8], seq: u64) {
        store
            .put(
                InternalKey {
                    key: key.to_vec(),
                    seq,
                    op: OpType::Delete,
                },
                &[],
            )
            .unwrap();
    }

    // --- scan returns sorted order ---

    #[test]
    fn scan_returns_results_in_sorted_order() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"user:charlie", b"admin", 1);
        put_key(&mut store, b"user:alice", b"viewer", 2);
        put_key(&mut store, b"user:bob", b"editor", 3);

        let results = store
            .scan(b"user:alice".to_vec()..=b"user:charlie".to_vec())
            .unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(ik, _)| ik.key.as_slice()).collect();
        assert_eq!(
            keys,
            vec![&b"user:alice"[..], &b"user:bob"[..], &b"user:charlie"[..]]
        );
    }

    #[test]
    fn scan_with_integer_keys_preserves_big_endian_order() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, &100u64.to_be_bytes(), b"hundred", 1);
        put_key(&mut store, &1u64.to_be_bytes(), b"one", 2);
        put_key(&mut store, &42u64.to_be_bytes(), b"forty-two", 3);

        let results = store
            .scan(1u64.to_be_bytes().to_vec()..=100u64.to_be_bytes().to_vec())
            .unwrap();
        let keys: Vec<u64> = results
            .iter()
            .map(|(ik, _)| u64::from_be_bytes(ik.key.as_slice().try_into().unwrap()))
            .collect();
        assert_eq!(keys, vec![1, 42, 100]);
    }

    // --- capacity enforcement ---

    #[test]
    fn fits_returns_false_when_capacity_exceeded() {
        let mut store = BTreeMapStore::with_capacity(30);
        // "order:1001" (10) + "pending" (7) = 17 bytes
        put_key(&mut store, b"order:1001", b"pending", 1);
        // "order:1002" (10) + "shipped" (7) = 17 more, total 34 > 30
        let ikey = InternalKey {
            key: b"order:1002".to_vec(),
            seq: 2,
            op: OpType::Put,
        };
        assert!(!store.fits(&ikey, b"shipped".len()));
    }

    #[test]
    fn is_full_returns_true_at_capacity() {
        let mut store = BTreeMapStore::with_capacity(20);
        put_key(&mut store, b"session:xyz789", b"active", 1); // exactly 20
        assert!(store.is_full());
    }

    // --- delete correctness ---

    #[test]
    fn get_returns_none_after_delete() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"config:feature_flags", b"enabled", 1);
        delete_key(&mut store, b"config:feature_flags", 2);
        assert_eq!(store.get(b"config:feature_flags").unwrap(), None);
    }

    #[test]
    fn scan_includes_tombstones() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"a", b"1", 1);
        put_key(&mut store, b"b", b"2", 2);
        delete_key(&mut store, b"b", 3);

        let results = store.scan(b"a".to_vec()..=b"b".to_vec()).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0.key, b"a");
        // b has seq 3 (delete) before seq 2 (put) due to InternalKey ordering
        assert_eq!(results[1].0.key, b"b");
        assert_eq!(results[1].0.op, OpType::Delete);
        assert_eq!(results[2].0.key, b"b");
        assert_eq!(results[2].0.op, OpType::Put);
    }

    // --- versioning ---

    #[test]
    fn get_returns_latest_version() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"key:a", b"v1", 1);
        put_key(&mut store, b"key:a", b"v2", 2);
        put_key(&mut store, b"key:a", b"v3", 3);
        assert_eq!(store.get(b"key:a").unwrap(), Some(b"v3".to_vec()));
    }

    #[test]
    fn scan_returns_all_versions() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"k:a", b"old", 1);
        put_key(&mut store, b"k:a", b"new", 2);
        put_key(&mut store, b"k:b", b"only", 3);

        let results = store.scan(b"k:a".to_vec()..=b"k:b".to_vec()).unwrap();
        assert_eq!(results.len(), 3);
        // k:a seq 2 (newest) first, then k:a seq 1
        assert_eq!(results[0].0.seq, 2);
        assert_eq!(results[0].1, b"new");
        assert_eq!(results[1].0.seq, 1);
        assert_eq!(results[1].1, b"old");
        assert_eq!(results[2].0.key, b"k:b");
    }

    // --- get_at ---

    #[test]
    fn get_at_returns_version_at_seq() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"key", b"v1", 1);
        put_key(&mut store, b"key", b"v2", 2);
        put_key(&mut store, b"key", b"v3", 3);

        assert_eq!(store.get_at(b"key", 1).unwrap(), Some(b"v1".to_vec()));
        assert_eq!(store.get_at(b"key", 2).unwrap(), Some(b"v2".to_vec()));
        assert_eq!(store.get_at(b"key", 3).unwrap(), Some(b"v3".to_vec()));
        assert_eq!(store.get_at(b"key", 0).unwrap(), None);
    }

    #[test]
    fn get_at_respects_tombstones() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"key", b"val", 1);
        delete_key(&mut store, b"key", 2);
        put_key(&mut store, b"key", b"revived", 3);

        assert_eq!(store.get_at(b"key", 1).unwrap(), Some(b"val".to_vec()));
        assert_eq!(store.get_at(b"key", 2).unwrap(), None);
        assert_eq!(store.get_at(b"key", 3).unwrap(), Some(b"revived".to_vec()));
    }

    // --- scan_at ---

    #[test]
    fn scan_at_returns_entries_at_seq() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"a", b"v1", 1);
        put_key(&mut store, b"a", b"v2", 3);
        put_key(&mut store, b"b", b"v1", 2);
        put_key(&mut store, b"b", b"v2", 4);

        let results = store.scan_at(b"a".to_vec()..=b"b".to_vec(), 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.key, b"a");
        assert_eq!(results[0].1, b"v1");
        assert_eq!(results[1].0.key, b"b");
        assert_eq!(results[1].1, b"v1");
    }

    #[test]
    fn scan_at_excludes_tombstones() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"a", b"val", 1);
        delete_key(&mut store, b"a", 2);
        put_key(&mut store, b"b", b"val", 3);

        let results = store.scan_at(b"a".to_vec()..=b"b".to_vec(), 3).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.key, b"b");
    }

    #[test]
    fn scan_at_skips_future_versions() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"a", b"v1", 1);
        put_key(&mut store, b"b", b"v1", 5); // only version, but seq > max_seq

        let results = store.scan_at(b"a".to_vec()..=b"b".to_vec(), 3).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.key, b"a");
    }

    // --- InternalKey ordering ---

    #[test]
    fn internal_key_orders_by_user_key_asc_then_seq_desc() {
        let k1 = InternalKey {
            key: b"a".to_vec(),
            seq: 3,
            op: OpType::Put,
        };
        let k2 = InternalKey {
            key: b"a".to_vec(),
            seq: 1,
            op: OpType::Put,
        };
        let k3 = InternalKey {
            key: b"b".to_vec(),
            seq: 2,
            op: OpType::Put,
        };
        // Same user key: higher seq comes first.
        assert!(k1 < k2);
        // Different user key: "a" < "b" regardless of seq.
        assert!(k2 < k3);
    }
}
