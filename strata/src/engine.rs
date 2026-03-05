use std::ops::RangeBounds;
use std::path::Path;

use crate::level::{Level, LevelConfig, Lookup, Run};
use crate::memstore::{
    InternalKey, MemStore, OpType,
    wal::{WalOp, WriteAheadLog},
};
use crate::{KVPair, StorageError};
use tracing::{info, instrument};

const DEFAULT_NUM_LEVELS: usize = 7;

/// Core engine coordinating between storage components.
pub struct StorageEngine<M: MemStore> {
    mem: M,
    wal: WriteAheadLog,
    seq: u64,
    levels: Vec<Level>,
    next_sst_id: u64,
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
                WalOp::Delete { seq: s, key, .. } => mem.put(
                    InternalKey {
                        key,
                        seq: s,
                        op: OpType::Delete,
                    },
                    &[],
                )?,
            }
        }
        let levels = Self::default_levels();
        Ok(Self {
            mem,
            wal,
            seq,
            levels,
            next_sst_id: 0,
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
        self.mem.put(ikey, value)?;
        self.seq = next_seq;
        info!(seq = self.seq, "memstore ok");
        Ok(())
    }

    /// Flush the current memtable into L0, truncate the WAL, and reset the memtable.
    fn compact(&mut self) -> Result<(), StorageError> {
        let incoming = self.mem.scan(..)?;
        let sst_id = self.next_sst_id;
        self.next_sst_id += 1;
        let run = Run::from_entries(sst_id, incoming);
        self.levels[0].add_run(run)?;
        self.wal.truncate()?;
        info!(sst_id, "compacted memtable to l0");
        self.mem.clear();
        Ok(())
    }

    fn default_levels() -> Vec<Level> {
        let mut levels = Vec::with_capacity(DEFAULT_NUM_LEVELS);
        for i in 0..DEFAULT_NUM_LEVELS {
            let config = if i == 0 {
                LevelConfig {
                    max_runs: 64,
                    max_run_size_bytes: 64 * 1024 * 1024,
                }
            } else {
                LevelConfig {
                    max_runs: 1,
                    max_run_size_bytes: 64 * 1024 * 1024 * (1 << i),
                }
            };
            levels.push(Level::new(config));
        }
        levels
    }

    /// Current monotonic write sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Retrieve the value for a given key.
    ///
    /// Checks the memtable first, then each level from L0 downward.
    /// Stops at the first layer that contains the key — a tombstone in
    /// a newer layer shadows any value in older layers.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        // Check memtable via single-key scan to distinguish "not found" from "deleted".
        let key_vec = key.to_vec();
        let mem_entries = self.mem.scan(key_vec.clone()..=key_vec)?;
        if let Some((ik, value)) = mem_entries.first() {
            return match ik.op {
                OpType::Put => Ok(Some(value.clone())),
                OpType::Delete => Ok(None),
            };
        }
        for level in &self.levels {
            match level.lookup(key) {
                Lookup::Found(val) => return Ok(Some(val.to_vec())),
                Lookup::Deleted => return Ok(None),
                Lookup::NotFound => {}
            }
        }
        Ok(None)
    }

    /// Retrieve the value for a given key at a specific sequence number.
    ///
    /// Returns the most recent version with `seq <= max_seq`.
    pub fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, StorageError> {
        // Check memtable: get_at returns None for both "not found" and "deleted",
        // so we need to distinguish via raw scan to handle tombstone shadowing.
        let key_vec = key.to_vec();
        let raw_entries = self.mem.scan(key_vec.clone()..=key_vec)?;
        for (ik, value) in &raw_entries {
            if ik.seq <= max_seq {
                return match ik.op {
                    OpType::Put => Ok(Some(value.clone())),
                    OpType::Delete => Ok(None),
                };
            }
        }
        for level in &self.levels {
            match level.lookup_at(key, max_seq) {
                Lookup::Found(val) => return Ok(Some(val.to_vec())),
                Lookup::Deleted => return Ok(None),
                Lookup::NotFound => {}
            }
        }
        Ok(None)
    }

    /// Return key-value pairs within the given range, sorted by key ascending.
    ///
    /// Merges entries from the memtable and all levels, then resolves versions:
    /// for each user key only the latest version is kept and tombstones are excluded.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> Result<Vec<KVPair>, StorageError> {
        let mut entries = self
            .mem
            .scan((range.start_bound().cloned(), range.end_bound().cloned()))?;
        for level in &self.levels {
            let level_entries =
                level.scan((range.start_bound().cloned(), range.end_bound().cloned()));
            entries.extend(level_entries);
        }
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
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
    use crate::memstore::BTreeMapStore;

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

        // L0 should have runs from the compacted memtable.
        assert!(!engine.levels[0].is_empty());
    }

    #[test]
    fn compact_creates_multiple_runs_in_l0() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();

        // Write enough to trigger multiple compactions.
        for i in 0..40u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }

        // L0 should contain multiple runs from separate compactions.
        assert!(engine.levels[0].runs.len() > 1);
    }

    #[test]
    fn compact_returns_error_when_l0_full() {
        let tmp = tempfile::tempdir().unwrap();
        // Tiny memtable + tiny L0 capacity (max 2 runs).
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();
        engine.levels[0] = Level::new(LevelConfig {
            max_runs: 2,
            max_run_size_bytes: 64 * 1024 * 1024,
        });

        let mut i = 0u32;
        let err = loop {
            let key = format!("k:{i:04}");
            let val = format!("v:{i}");
            if let Err(e) = engine.put(key.as_bytes(), val.as_bytes()) {
                break e;
            }
            i += 1;
            assert!(i < 1000, "expected level full but wrote 1000 entries");
        };

        assert!(
            matches!(err, StorageError::InternalError(ref msg) if msg.contains("level full")),
            "expected level full error, got: {err:?}"
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
        let runs_after_round1 = engine.levels[0].runs.len();
        assert!(runs_after_round1 > 0, "expected compaction after round 1");

        // Round 2: write more, trigger another compaction.
        for i in 10..20u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("r2:{i}").as_bytes())
                .unwrap();
        }
        assert!(
            engine.levels[0].runs.len() > runs_after_round1,
            "expected L0 to grow after round 2"
        );
    }

    #[test]
    fn get_reads_from_l0_after_compaction() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();

        engine.put(b"key:a", b"value_a").unwrap();
        engine.put(b"key:b", b"value_b").unwrap();

        // Force compaction by filling memtable.
        for i in 0..20u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }

        // Original keys should still be readable from L0.
        assert_eq!(engine.get(b"key:a").unwrap(), Some(b"value_a".to_vec()));
        assert_eq!(engine.get(b"key:b").unwrap(), Some(b"value_b".to_vec()));
    }

    #[test]
    fn compact_truncates_wal() {
        let tmp = tempfile::tempdir().unwrap();
        // Capacity 16: "k:0"(3) + "v:0"(3) = 6 bytes per entry, fits ~2 entries.
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(16)).unwrap();

        // Fill memtable (2 entries fit).
        engine.put(b"k:0", b"v:0").unwrap();
        engine.put(b"k:1", b"v:1").unwrap();

        // This write triggers compaction (memtable full), then appends itself.
        // After compaction the WAL is truncated, so only this entry remains.
        engine.put(b"k:2", b"v:2").unwrap();
        assert!(!engine.levels[0].is_empty(), "compaction should have fired");

        // Replay the WAL — should contain only the 1 entry written after truncation.
        drop(engine);
        let wal = WriteAheadLog::new(&tmp.path().join("wal")).unwrap();
        let replayed: Vec<_> = wal.replay().unwrap();
        assert_eq!(
            replayed.len(),
            1,
            "WAL should have 1 entry after compaction"
        );
        assert_eq!(replayed[0].seq(), 3);

        // Write more entries after reopening, they accumulate in the WAL.
        let mut engine =
            StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(1024)).unwrap();
        engine.put(b"k:3", b"v:3").unwrap();
        engine.put(b"k:4", b"v:4").unwrap();
        drop(engine);

        let wal = WriteAheadLog::new(&tmp.path().join("wal")).unwrap();
        let replayed: Vec<_> = wal.replay().unwrap();
        assert_eq!(
            replayed.len(),
            3,
            "WAL should have 3 entries (1 from before + 2 new)"
        );
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
