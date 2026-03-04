use std::collections::BTreeMap;

use super::{KVPair, MemStore, ReadError, WriteError};

const DEFAULT_CAPACITY: usize = 4 * 1024 * 1024; // 4 MB

/// A [`MemStore`] backed by [`BTreeMap`].
pub struct BTreeMapStore {
    store: BTreeMap<Vec<u8>, Vec<u8>>,
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
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), WriteError> {
        if self.size() + (key.len() + value.len()) > self.capacity {
            return Err(WriteError::StoreFull);
        }
        self.store.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, ReadError> {
        Ok(self.store.get(key).cloned())
    }

    fn delete(&mut self, key: &[u8]) -> Result<(), WriteError> {
        self.store.remove(key);
        Ok(())
    }

    fn scan(&self, start: &[u8], end: &[u8]) -> Result<Vec<KVPair>, ReadError> {
        let results = self
            .store
            .range(start.to_vec()..=end.to_vec())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(results)
    }

    fn size(&self) -> usize {
        self.store.iter().map(|(k, v)| k.len() + v.len()).sum()
    }

    fn is_full(&self) -> bool {
        self.size() >= self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- scan returns sorted order ---

    #[test]
    fn scan_returns_results_in_sorted_order() {
        let mut store = BTreeMapStore::new();
        // Insert out of order — scan should return sorted by byte order.
        store.put(b"user:charlie", b"admin").unwrap();
        store.put(b"user:alice", b"viewer").unwrap();
        store.put(b"user:bob", b"editor").unwrap();

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
        // Big-endian encoding means lexicographic == numeric order.
        store.put(&100u64.to_be_bytes(), b"hundred").unwrap();
        store.put(&1u64.to_be_bytes(), b"one").unwrap();
        store.put(&42u64.to_be_bytes(), b"forty-two").unwrap();

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
        store.put(b"order:1001", b"pending").unwrap();
        // "order:1002" (10) + "shipped" (7) = 17 more, total 34 > 30
        let result = store.put(b"order:1002", b"shipped");
        assert!(matches!(result, Err(WriteError::StoreFull)));
    }

    #[test]
    fn put_overwrite_does_not_grow_size() {
        let mut store = BTreeMapStore::with_capacity(100);
        store.put(b"session:xyz789", b"active").unwrap(); // 20 bytes
        store.put(b"session:xyz789", b"closed").unwrap(); // still 20
        assert_eq!(store.size(), 20);
    }

    #[test]
    fn is_full_returns_true_at_capacity() {
        let mut store = BTreeMapStore::with_capacity(20);
        store.put(b"session:xyz789", b"active").unwrap(); // exactly 20
        assert!(store.is_full());
    }

    // --- delete correctness ---

    #[test]
    fn get_returns_none_after_delete() {
        let mut store = BTreeMapStore::new();
        store.put(b"config:feature_flags", b"enabled").unwrap();
        store.delete(b"config:feature_flags").unwrap();
        assert_eq!(store.get(b"config:feature_flags").unwrap(), None);
    }

    #[test]
    fn scan_excludes_deleted_keys() {
        let mut store = BTreeMapStore::new();
        store.put(b"metric:cpu_usage", b"72.5").unwrap();
        store.put(b"metric:disk_io", b"150.3").unwrap();
        store.put(b"metric:mem_free", b"2048").unwrap();
        store.delete(b"metric:disk_io").unwrap();

        let results = store.scan(b"metric:cpu_usage", b"metric:mem_free").unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(
            keys,
            vec![&b"metric:cpu_usage"[..], &b"metric:mem_free"[..]]
        );
    }

    #[test]
    fn size_decreases_after_delete() {
        let mut store = BTreeMapStore::new();
        store.put(b"cache:page:/home", b"<html>...</html>").unwrap();
        let size_before = store.size();
        store.delete(b"cache:page:/home").unwrap();
        assert!(store.size() < size_before);
    }
}
