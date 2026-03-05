use std::collections::BTreeMap;

use super::{InternalKey, KVPair, MemStore, OpType, ReadError, WriteError};

const DEFAULT_CAPACITY: usize = 4 * 1024 * 1024; // 4 MB

/// A [`MemStore`] backed by [`BTreeMap`].
pub struct BTreeMapStore {
    store: BTreeMap<InternalKey, Vec<u8>>,
    capacity: usize,
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
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            store: BTreeMap::new(),
            capacity,
        }
    }
}

impl MemStore for BTreeMapStore {
    fn put(&mut self, key: InternalKey, value: Vec<u8>) -> Result<(), WriteError> {
        if self.size() + key.key.len() + value.len() > self.capacity {
            return Err(WriteError::StoreFull);
        }
        self.store.insert(key, value);
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
                OpType::Put => Ok(Some(value.clone())),
                OpType::Delete => Ok(None),
            };
        }
        Ok(None)
    }

    fn delete(&mut self, key: InternalKey) -> Result<(), WriteError> {
        if self.size() + key.key.len() > self.capacity {
            return Err(WriteError::StoreFull);
        }
        self.store.insert(key, Vec::new());
        Ok(())
    }

    fn scan(&self, start: &[u8], end: &[u8]) -> Result<Vec<KVPair>, ReadError> {
        let start_probe = InternalKey {
            key: start.to_vec(),
            seq: u64::MAX,
            op: OpType::Put,
        };
        let end_probe = InternalKey {
            key: end.to_vec(),
            seq: 0,
            op: OpType::Put,
        };

        let mut results = Vec::new();
        let mut last_key: Option<&[u8]> = None;

        for (ikey, value) in self.store.range(start_probe..=end_probe) {
            if last_key == Some(ikey.key.as_slice()) {
                continue;
            }
            last_key = Some(&ikey.key);
            if ikey.op == OpType::Put {
                results.push((ikey.key.clone(), value.clone()));
            }
        }
        Ok(results)
    }

    fn size(&self) -> usize {
        self.store.iter().map(|(k, v)| k.key.len() + v.len()).sum()
    }

    fn is_full(&self) -> bool {
        self.size() >= self.capacity
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
                value.to_vec(),
            )
            .unwrap();
    }

    fn delete_key(store: &mut BTreeMapStore, key: &[u8], seq: u64) {
        store
            .delete(InternalKey {
                key: key.to_vec(),
                seq,
                op: OpType::Delete,
            })
            .unwrap();
    }

    // --- scan returns sorted order ---

    #[test]
    fn scan_returns_results_in_sorted_order() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"user:charlie", b"admin", 1);
        put_key(&mut store, b"user:alice", b"viewer", 2);
        put_key(&mut store, b"user:bob", b"editor", 3);

        let results = store.scan(b"user:alice", b"user:charlie").unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(k, _)| k.as_slice()).collect();
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
            .scan(&1u64.to_be_bytes(), &100u64.to_be_bytes())
            .unwrap();
        let keys: Vec<u64> = results
            .iter()
            .map(|(k, _)| u64::from_be_bytes(k.as_slice().try_into().unwrap()))
            .collect();
        assert_eq!(keys, vec![1, 42, 100]);
    }

    // --- capacity enforcement ---

    #[test]
    fn put_returns_store_full_when_capacity_exceeded() {
        let mut store = BTreeMapStore::with_capacity(30);
        // "order:1001" (10) + "pending" (7) = 17 bytes
        put_key(&mut store, b"order:1001", b"pending", 1);
        // "order:1002" (10) + "shipped" (7) = 17 more, total 34 > 30
        let result = store.put(
            InternalKey {
                key: b"order:1002".to_vec(),
                seq: 2,
                op: OpType::Put,
            },
            b"shipped".to_vec(),
        );
        assert!(matches!(result, Err(WriteError::StoreFull)));
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
    fn scan_excludes_deleted_keys() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"metric:cpu_usage", b"72.5", 1);
        put_key(&mut store, b"metric:disk_io", b"150.3", 2);
        put_key(&mut store, b"metric:mem_free", b"2048", 3);
        delete_key(&mut store, b"metric:disk_io", 4);

        let results = store.scan(b"metric:cpu_usage", b"metric:mem_free").unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(
            keys,
            vec![&b"metric:cpu_usage"[..], &b"metric:mem_free"[..]]
        );
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
    fn scan_returns_latest_version_per_key() {
        let mut store = BTreeMapStore::new();
        put_key(&mut store, b"k:a", b"old", 1);
        put_key(&mut store, b"k:a", b"new", 2);
        put_key(&mut store, b"k:b", b"only", 3);

        let results = store.scan(b"k:a", b"k:b").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], (b"k:a".to_vec(), b"new".to_vec()));
        assert_eq!(results[1], (b"k:b".to_vec(), b"only".to_vec()));
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
