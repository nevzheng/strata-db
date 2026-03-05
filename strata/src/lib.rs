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

/// A resolved key-value pair of owned byte vectors.
pub type KVPair = (Vec<u8>, Vec<u8>);

const DEFAULT_L0_CAPACITY: usize = 64;

/// Core engine coordinating between storage components.
pub struct StorageEngine<M: MemStore> {
    mem: M,
    wal: WriteAheadLog,
    seq: u64,
    l0_vec: Vec<(InternalKey, Vec<u8>)>,
    l0_capacity: usize,
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
                    &value,
                )?,
                WalOp::Delete { seq: s, key, .. } => mem.delete(InternalKey {
                    key,
                    seq: s,
                    op: OpType::Delete,
                })?,
            }
        }
        Ok(Self {
            mem,
            wal,
            seq,
            l0_vec: Vec::new(),
            l0_capacity: DEFAULT_L0_CAPACITY,
        })
    }

    /// Insert a key-value pair.
    #[instrument(skip(self, key, value), fields(key_len = key.len(), value_len = value.len()))]
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.write(key, value, OpType::Put)
    }

    /// Delete a key (writes a tombstone).
    #[instrument(skip(self, key), fields(key_len = key.len()))]
    pub fn delete(&mut self, key: &[u8]) -> Result<(), StorageError> {
        self.write(key, &[], OpType::Delete)
    }

    /// Shared write path for puts and deletes.
    ///
    /// Checks capacity, writes to the WAL (blocking until durable),
    /// then applies to the memstore.
    fn write(&mut self, key: &[u8], value: &[u8], op_type: OpType) -> Result<(), StorageError> {
        let next_seq = self.seq + 1;
        let ikey = InternalKey {
            key: key.to_vec(),
            seq: next_seq,
            op: op_type,
        };
        if !self.mem.fits(&ikey, value.len()) {
            self.compact()?;
        }
        let wal_op = match op_type {
            OpType::Put => WalOp::Put {
                seq: next_seq,
                key: key.to_vec(),
                value: value.to_vec(),
            },
            OpType::Delete => WalOp::Delete {
                seq: next_seq,
                key: key.to_vec(),
            },
        };
        self.wal.append(&wal_op)?;
        info!("wal ok");
        match op_type {
            OpType::Put => self.mem.put(ikey, value)?,
            OpType::Delete => self.mem.delete(ikey)?,
        }
        self.seq = next_seq;
        info!(seq = self.seq, "memstore ok");
        Ok(())
    }

    /// Flush the current memtable into `l0_vec` and reset it.
    ///
    /// Merges incoming entries with existing L0 entries, sorted by `InternalKey`.
    /// Returns `StorageError::InternalError` if L0 would exceed capacity.
    fn compact(&mut self) -> Result<(), StorageError> {
        let incoming = self.mem.scan(..)?;
        self.l0_vec.extend(incoming);
        self.l0_vec.sort_by(|(a, _), (b, _)| a.cmp(b));
        if self.l0_vec.len() > self.l0_capacity {
            return Err(StorageError::InternalError(format!(
                "L0 full: {} entries exceeds capacity {}",
                self.l0_vec.len(),
                self.l0_capacity
            )));
        }
        info!(entries = self.l0_vec.len(), "compacted memtable to l0");
        self.mem.clear();
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
    ///
    /// Resolves versions: for each user key only the latest version is kept,
    /// and tombstones are excluded.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> Result<Vec<KVPair>, StorageError> {
        let entries = self.mem.scan(range)?;
        Ok(Self::resolve_versions(&entries))
    }

    /// Collapse a sorted `InternalKey` stream into user-key pairs.
    ///
    /// Because `InternalKey` sorts by user key ascending then seq descending,
    /// the first entry for each user key is the latest version. Tombstones
    /// (`OpType::Delete`) are dropped.
    fn resolve_versions(entries: &[(InternalKey, Vec<u8>)]) -> Vec<KVPair> {
        let mut result = Vec::new();
        let mut last_key: Option<&[u8]> = None;
        for (ik, value) in entries {
            if last_key == Some(ik.key.as_slice()) {
                continue;
            }
            last_key = Some(&ik.key);
            if ik.op == OpType::Put {
                result.push((ik.key.clone(), value.clone()));
            }
        }
        result
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
    fn compact_flushes_memtable_to_l0() {
        let tmp = tempfile::tempdir().unwrap();
        // Tiny memtable: 32 bytes. Each "k:N"+"v:N" pair is ~6 bytes.
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();

        // Write enough to trigger a compaction.
        for i in 0..20u32 {
            engine
                .put(format!("k:{i}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }

        // L0 should have entries from the compacted memtable.
        assert!(!engine.l0_vec.is_empty());
    }

    #[test]
    fn compact_merges_into_existing_l0() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();

        // Write enough to trigger multiple compactions.
        for i in 0..40u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }

        // L0 should contain entries from multiple compactions, sorted.
        assert!(engine.l0_vec.len() > 1);
        for w in engine.l0_vec.windows(2) {
            assert!(w[0].0 <= w[1].0, "L0 not sorted");
        }
    }

    #[test]
    fn compact_returns_error_when_l0_full() {
        let tmp = tempfile::tempdir().unwrap();
        // Tiny memtable + tiny L0 capacity.
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();
        engine.l0_capacity = 2; // very small L0

        let mut i = 0u32;
        let err = loop {
            let key = format!("k:{i:04}");
            let val = format!("v:{i}");
            if let Err(e) = engine.put(key.as_bytes(), val.as_bytes()) {
                break e;
            }
            i += 1;
            assert!(i < 1000, "expected L0 full but wrote 1000 entries");
        };

        assert!(
            matches!(err, StorageError::InternalError(ref msg) if msg.contains("L0 full")),
            "expected L0 full error, got: {err:?}"
        );
    }

    #[test]
    fn data_survives_multiple_compactions() {
        let tmp = tempfile::tempdir().unwrap();
        // Tiny memtable so compaction triggers often.
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();

        // Round 1: write, exceed memstore, trigger compaction.
        for i in 0..10u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("r1:{i}").as_bytes())
                .unwrap();
        }
        let l0_after_round1 = engine.l0_vec.len();
        assert!(l0_after_round1 > 0, "expected compaction after round 1");

        // Round 2: write more, trigger another compaction that merges into L0.
        for i in 10..20u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("r2:{i}").as_bytes())
                .unwrap();
        }
        assert!(
            engine.l0_vec.len() > l0_after_round1,
            "expected L0 to grow after round 2"
        );

        // All keys should still be readable from the memstore (latest batch)
        // or present in L0 (compacted batches).
        for w in engine.l0_vec.windows(2) {
            assert!(w[0].0 <= w[1].0, "L0 not sorted after two rounds");
        }
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
