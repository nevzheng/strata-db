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
