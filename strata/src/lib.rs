pub mod memstore;

use std::ops::RangeBounds;
use std::path::Path;

use memstore::{
    InternalKey, MemStore, OpType, ReadError, WriteError,
    wal::{WalOp, WriteAheadLog},
};
use thiserror::Error;
use tracing::{info, instrument};

/// Errors returned by [`StorageEngine`] operations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error(transparent)]
    WriteError(WriteError),
    #[error("internal error: {0}")]
    InternalError(String),
}

impl From<WriteError> for StorageError {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Internal(msg) => StorageError::InternalError(msg),
            other => StorageError::WriteError(other),
        }
    }
}

impl From<ReadError> for StorageError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Internal(msg) => StorageError::InternalError(msg),
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::InternalError(e.to_string())
    }
}

/// Core engine coordinating between storage components.
pub struct StorageEngine<M: MemStore> {
    mem: M,
    wal: WriteAheadLog,
    seq: u64,
}

impl<M: MemStore> StorageEngine<M> {
    pub fn new(dir: &Path, mut mem: M) -> Result<Self, StorageError> {
        let wal = WriteAheadLog::new(&dir.join("wal"))?;
        let mut seq = 0u64;
        for op in wal.replay()? {
            seq = op.seq();
            match op {
                WalOp::Put {
                    seq: s, key, value, ..
                } => mem.put(
                    InternalKey {
                        key,
                        seq: s,
                        op: OpType::Put,
                    },
                    value,
                )?,
                WalOp::Delete { seq: s, key, .. } => mem.delete(InternalKey {
                    key,
                    seq: s,
                    op: OpType::Delete,
                })?,
            }
        }
        Ok(Self { mem, wal, seq })
    }

    /// Insert a key-value pair.
    ///
    /// Writes to the WAL first (blocking until durable), then inserts into the memstore.
    #[instrument(skip(self, key, value), fields(key_len = key.len(), value_len = value.len()))]
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        let next_seq = self.seq + 1;
        let op = WalOp::Put {
            seq: next_seq,
            key: key.to_vec(),
            value: value.to_vec(),
        };
        self.wal.append(&op)?;
        info!("wal ok");
        self.mem.put(
            InternalKey {
                key: key.to_vec(),
                seq: next_seq,
                op: OpType::Put,
            },
            value.to_vec(),
        )?;
        self.seq = next_seq;
        info!(seq = self.seq, "memstore ok");
        Ok(())
    }

    /// Delete a key.
    ///
    /// Writes to the WAL first (blocking until durable), then deletes from the memstore.
    #[instrument(skip(self, key), fields(key_len = key.len()))]
    pub fn delete(&mut self, key: &[u8]) -> Result<(), StorageError> {
        let next_seq = self.seq + 1;
        let op = WalOp::Delete {
            seq: next_seq,
            key: key.to_vec(),
        };
        self.wal.append(&op)?;
        info!("wal ok");
        self.mem.delete(InternalKey {
            key: key.to_vec(),
            seq: next_seq,
            op: OpType::Delete,
        })?;
        self.seq = next_seq;
        info!(seq = self.seq, "memstore ok");
        Ok(())
    }

    /// Current monotonic write sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Retrieve the value for a given key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.mem.get(key)?)
    }

    /// Return key-value pairs within the given range, sorted by key ascending.
    pub fn scan(
        &self,
        range: impl RangeBounds<Vec<u8>>,
    ) -> Result<Vec<memstore::KVPair>, StorageError> {
        Ok(self.mem.scan(range)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memstore::BTreeMapStore;

    #[test]
    fn put_get_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();

        engine.put(b"user:alice", b"admin").unwrap();
        assert_eq!(engine.get(b"user:alice").unwrap(), Some(b"admin".to_vec()));

        engine.delete(b"user:alice").unwrap();
        assert_eq!(engine.get(b"user:alice").unwrap(), None);
    }

    #[test]
    fn scan_returns_sorted_results() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();

        engine.put(b"key:c", b"3").unwrap();
        engine.put(b"key:a", b"1").unwrap();
        engine.put(b"key:b", b"2").unwrap();

        let results = engine.scan(b"key:a".to_vec()..=b"key:c".to_vec()).unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, vec![&b"key:a"[..], &b"key:b"[..], &b"key:c"[..]]);
    }

    #[test]
    #[ignore = "TODO: engine should flush memtable before it fills up"]
    fn put_returns_error_when_memtable_full() {
        let tmp = tempfile::tempdir().unwrap();
        // Tiny capacity: 64 bytes
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();

        // Fill the memtable with small entries until it rejects a write.
        let mut i = 0u32;
        let err = loop {
            let key = format!("k:{i}");
            let val = format!("v:{i}");
            if let Err(e) = engine.put(key.as_bytes(), val.as_bytes()) {
                break e;
            }
            i += 1;
            assert!(i < 1000, "expected StoreFull but wrote 1000 entries");
        };

        // Should surface as a WriteError::StoreFull through StorageError.
        assert!(
            matches!(err, StorageError::WriteError(WriteError::StoreFull)),
            "expected StoreFull, got: {err:?}"
        );

        // Earlier keys should still be readable.
        assert!(engine.get(b"k:0").unwrap().is_some());
    }

    #[test]
    #[ignore = "TODO: engine should flush memtable before it fills up"]
    fn reopen_with_full_memtable_fails_on_replay() {
        let tmp = tempfile::tempdir().unwrap();

        // Fill a tiny memtable until it's full, then drop the engine.
        {
            let mut engine =
                StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();
            let mut i = 0u32;
            loop {
                let key = format!("k:{i}");
                let val = format!("v:{i}");
                if engine.put(key.as_bytes(), val.as_bytes()).is_err() {
                    break;
                }
                i += 1;
            }
        }

        // Reopen with the same tiny capacity — WAL replay should hit StoreFull.
        let result = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64));
        assert!(result.is_err(), "expected replay to fail, but it succeeded");
    }

    #[test]
    fn seq_increments_on_writes_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();

        assert_eq!(engine.seq(), 0);
        engine.put(b"a", b"1").unwrap();
        assert_eq!(engine.seq(), 1);
        engine.put(b"b", b"2").unwrap();
        assert_eq!(engine.seq(), 2);
        engine.delete(b"a").unwrap();
        assert_eq!(engine.seq(), 3);
    }

    #[test]
    fn seq_restored_from_wal_replay() {
        let tmp = tempfile::tempdir().unwrap();

        {
            let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
            engine.put(b"x", b"1").unwrap();
            engine.put(b"y", b"2").unwrap();
            engine.delete(b"x").unwrap();
            assert_eq!(engine.seq(), 3);
        }

        let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        assert_eq!(engine.seq(), 3);
    }

    #[test]
    fn data_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();

        // Write and drop.
        {
            let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
            engine.put(b"config:theme", b"dark").unwrap();
            engine.put(b"config:lang", b"en").unwrap();
            engine.delete(b"config:lang").unwrap();
        }

        // Reopen with a fresh memstore — WAL replay should restore state.
        let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        assert_eq!(engine.get(b"config:theme").unwrap(), Some(b"dark".to_vec()));
        assert_eq!(engine.get(b"config:lang").unwrap(), None);
    }
}
