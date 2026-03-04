pub mod memstore;

use std::path::Path;

use memstore::{
    MemStore, ReadError, WriteError,
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
}

impl<M: MemStore> StorageEngine<M> {
    pub fn new(dir: &Path, mut mem: M) -> Result<Self, StorageError> {
        let wal = WriteAheadLog::new(&dir.join("wal"))?;
        for op in wal.replay()? {
            match op {
                WalOp::Put { key, value } => mem.put(&key, &value)?,
                WalOp::Delete { key } => mem.delete(&key)?,
            }
        }
        Ok(Self { mem, wal })
    }

    /// Insert a key-value pair.
    ///
    /// Writes to the WAL first (blocking until durable), then inserts into the memstore.
    #[instrument(skip(self, key, value), fields(key_len = key.len(), value_len = value.len()))]
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        let op = WalOp::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        };
        self.wal.append(&op)?;
        info!("wal ok");
        self.mem.put(key, value)?;
        info!("memstore ok");
        Ok(())
    }

    /// Delete a key.
    ///
    /// Writes to the WAL first (blocking until durable), then deletes from the memstore.
    #[instrument(skip(self, key), fields(key_len = key.len()))]
    pub fn delete(&mut self, key: &[u8]) -> Result<(), StorageError> {
        let op = WalOp::Delete { key: key.to_vec() };
        self.wal.append(&op)?;
        info!("wal ok");
        self.mem.delete(key)?;
        info!("memstore ok");
        Ok(())
    }

    /// Retrieve the value for a given key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.mem.get(key)?)
    }

    /// Return key-value pairs within the given range, sorted by key ascending.
    pub fn scan(&self, start: &[u8], end: &[u8]) -> Result<Vec<memstore::KVPair>, StorageError> {
        Ok(self.mem.scan(start, end)?)
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

        let results = engine.scan(b"key:a", b"key:c").unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, vec![&b"key:a"[..], &b"key:b"[..], &b"key:c"[..]]);
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
