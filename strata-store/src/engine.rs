use std::ops::RangeBounds;
use std::path::Path;

use crate::iterator::ScanIterator;
use crate::{StorageError, memstore::BTreeMapStore};
use lsm::{LevelConfig, Lsm, LsmConfig, MemStore};
use tracing::{info, instrument};

/// Strata's storage engine: a thin wrapper over the [`Lsm`] tree.
///
/// Writes go to the memtable; [`flush`](Self::flush) seals it into an on-disk
/// L0 SSTable. Reads merge the memtable with the on-disk levels and return the
/// newest visible version of each key.
pub struct StorageEngine<M: MemStore = BTreeMapStore> {
    lsm: Lsm<M>,
}

impl<M: MemStore> StorageEngine<M> {
    /// Open an engine rooted at `dir`, using `mem` as the memtable and the
    /// default level configuration. SSTable files live under `dir`.
    pub fn new(dir: &Path, mem: M) -> Result<Self, StorageError> {
        Ok(Self {
            lsm: Lsm::with_memtable(dir, LsmConfig::default(), mem)?,
        })
    }

    /// Open an engine with explicit per-level configuration.
    pub fn with_levels(dir: &Path, mem: M, levels: Vec<LevelConfig>) -> Result<Self, StorageError> {
        let config = LsmConfig {
            levels,
            ..LsmConfig::default()
        };
        Ok(Self {
            lsm: Lsm::with_memtable(dir, config, mem)?,
        })
    }

    /// Insert a key-value pair.
    #[instrument(skip(self, key, value), fields(key_len = key.len(), value_len = value.len()))]
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.lsm.put(key, value)?;
        Ok(())
    }

    /// Delete a key (writes a tombstone).
    #[instrument(skip(self, key), fields(key_len = key.len()))]
    pub fn delete(&mut self, key: &[u8]) -> Result<(), StorageError> {
        self.lsm.delete(key)?;
        Ok(())
    }

    /// Seal the current memtable into a new on-disk L0 SSTable.
    pub fn flush(&mut self) -> Result<(), StorageError> {
        self.lsm.flush()?;
        info!("flushed memtable to l0");
        Ok(())
    }

    /// Retrieve the latest value for `key`, or `None` if absent or deleted.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.lsm.get(key)?)
    }

    /// Number of levels in the LSM tree.
    pub fn num_levels(&self) -> usize {
        self.lsm.levels().len()
    }

    /// Scan the range, yielding the latest visible version of each user key in
    /// ascending order. Tombstones are skipped.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> ScanIterator<'_> {
        ScanIterator::new(self.lsm.scan(range).map(|r| r.map_err(StorageError::from)))
    }

    /// Scan the range as of `max_seq` (point-in-time).
    pub fn scan_at(&self, range: impl RangeBounds<Vec<u8>>, max_seq: u64) -> ScanIterator<'_> {
        ScanIterator::new(
            self.lsm
                .scan_at(range, max_seq)
                .map(|r| r.map_err(StorageError::from)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KVPair;
    use crate::memstore::BTreeMapStore;

    fn engine() -> (tempfile::TempDir, StorageEngine<BTreeMapStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        (tmp, engine)
    }

    #[test]
    fn put_get_delete_round_trip() {
        let (_tmp, mut engine) = engine();
        engine.put(b"user:alice", b"admin").unwrap();
        assert_eq!(engine.get(b"user:alice").unwrap(), Some(b"admin".to_vec()));

        engine.delete(b"user:alice").unwrap();
        assert_eq!(engine.get(b"user:alice").unwrap(), None);
    }

    #[test]
    fn scan_returns_sorted_results() {
        let (_tmp, mut engine) = engine();
        engine.put(b"key:c", b"3").unwrap();
        engine.put(b"key:a", b"1").unwrap();
        engine.put(b"key:b", b"2").unwrap();

        let results: Vec<KVPair> = engine
            .scan(b"key:a".to_vec()..=b"key:c".to_vec())
            .collect::<Result<_, _>>()
            .unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, vec![&b"key:a"[..], &b"key:b"[..], &b"key:c"[..]]);
    }

    #[test]
    fn reads_merge_memtable_over_flushed_l0() {
        let (_tmp, mut engine) = engine();
        engine.put(b"a", b"1").unwrap();
        engine.put(b"b", b"2").unwrap();
        engine.flush().unwrap();
        assert_eq!(engine.num_levels(), LsmConfig::default().num_levels());

        // Newer writes in the memtable win over the flushed L0.
        engine.put(b"a", b"1b").unwrap();
        assert_eq!(engine.get(b"a").unwrap(), Some(b"1b".to_vec()));
        assert_eq!(engine.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn delete_after_flush_shadows_value() {
        let (_tmp, mut engine) = engine();
        engine.put(b"k", b"v").unwrap();
        engine.flush().unwrap();
        engine.delete(b"k").unwrap();
        assert_eq!(engine.get(b"k").unwrap(), None);
    }
}
