use std::collections::BTreeMap;
use std::ops::RangeBounds;
use std::path::Path;

use itertools::Itertools;
use lsm::iterator::{MergeIterator, ScanIterator};
use lsm::level::{Level, LevelConfig, Manifest, Run, SsTableWriter};
use lsm::memstore::{
    InternalKey, MemStore, OpType, ReadError,
    wal::{WalOp, WriteAheadLog},
};
use lsm::{KVPair, ReadStore, StorageError};
use tracing::{info, instrument};

const DEFAULT_NUM_LEVELS: usize = 7;

/// A boxed iterator over raw entries from one storage source (memtable
/// or level) at a fixed `max_seq`. Same item type as the underlying
/// `ReadStore::scan_at`, just type-erased so we can collect several
/// sources of different concrete iterator types into one `Vec`.
type RawSource<'a> = Box<dyn Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + 'a>;

/// Core engine coordinating between storage components.
pub struct StorageEngine<M: MemStore> {
    mem: M,
    wal: WriteAheadLog,
    seq: u64,
    writer: SsTableWriter,
    levels: Vec<Level>,
}

impl<M: MemStore> StorageEngine<M> {
    pub fn new(dir: &Path, mem: M) -> Result<Self, StorageError> {
        let configs: Vec<LevelConfig> = (0..DEFAULT_NUM_LEVELS)
            .map(|i| {
                if i == 0 {
                    LevelConfig {
                        max_runs: 64,
                        max_run_size_bytes: 64 * 1024 * 1024,
                    }
                } else {
                    LevelConfig {
                        max_runs: 1,
                        max_run_size_bytes: 64 * 1024 * 1024 * (1 << i),
                    }
                }
            })
            .collect();
        Self::with_levels(dir, mem, configs)
    }

    pub fn with_levels(
        dir: &Path,
        mut mem: M,
        level_configs: Vec<LevelConfig>,
    ) -> Result<Self, StorageError> {
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
        let manifest = Manifest::new(&dir.join("MANIFEST"))?;
        let writer = SsTableWriter::new(manifest, dir.to_path_buf());

        // Reconstruct levels from manifest entries.
        let mut levels: Vec<Level> = level_configs.into_iter().map(Level::new).collect();

        // Group manifest entries by (level, run_id), sorted by run_id within each level.
        let mut level_runs: BTreeMap<u16, BTreeMap<u64, Vec<_>>> = BTreeMap::new();
        for entry in writer.tables().values() {
            level_runs
                .entry(entry.level)
                .or_default()
                .entry(entry.run_id)
                .or_default()
                .push(entry.sst_ref.clone());
        }

        for (level_idx, runs_by_id) in level_runs {
            if let Some(level) = levels.get_mut(level_idx as usize) {
                for (_run_id, mut refs) in runs_by_id {
                    refs.sort_by(|a, b| a.min_key.cmp(&b.min_key));
                    level.runs.push(Run::from_refs(refs));
                }
            }
        }

        Ok(Self {
            mem,
            wal,
            seq,
            writer,
            levels,
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
        let incoming: Vec<_> = self.mem.scan_at(.., u64::MAX).collect::<Result<_, _>>()?;
        if let Err(e) = Self::compact_level(
            &mut self.levels,
            &mut self.writer,
            0,
            0,
            Box::new(incoming.into_iter()),
        ) {
            self.writer.rollback();
            return Err(e);
        }
        self.writer.commit()?;
        self.wal.truncate()?;
        info!("compacted memtable to l0");
        self.mem.clear();
        Ok(())
    }

    /// Add entries to the given level. If that level exceeds its compaction
    /// threshold, merge all its runs and push the result into the next level.
    /// At the last level, merges incoming data with existing data in place.
    ///
    /// `abs_level` tracks the absolute level index for manifest ops (since
    /// `levels` is a slice that may start at an offset).
    fn compact_level(
        levels: &mut [Level],
        writer: &mut SsTableWriter,
        level: usize,
        abs_level: u16,
        entries: Box<dyn Iterator<Item = (InternalKey, Vec<u8>)>>,
    ) -> Result<(), StorageError> {
        let is_last = level + 1 >= levels.len();
        if levels[level].is_full() && is_last {
            let merged = levels[level].merge_iter()?.merge(entries);
            levels[level].clear_with_writer(writer, abs_level);
            levels[level].add_run(writer, abs_level, merged)?;
        } else if levels[level].is_full() {
            let merged = levels[level].merge_iter()?;
            Self::compact_level(
                &mut levels[level + 1..],
                writer,
                0,
                abs_level + 1,
                Box::new(merged),
            )?;
            levels[level].clear_with_writer(writer, abs_level);
            levels[level].add_run(writer, abs_level, entries)?;
        } else {
            levels[level].add_run(writer, abs_level, entries)?;
        }
        Ok(())
    }

    /// Current monotonic write sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Number of levels in the LSM tree.
    pub fn num_levels(&self) -> usize {
        self.levels.len()
    }

    /// Number of runs in a given level.
    pub fn level_run_count(&self, level: usize) -> usize {
        self.levels[level].runs.len()
    }

    /// Whether a given level has no runs.
    pub fn level_is_empty(&self, level: usize) -> bool {
        self.levels[level].is_empty()
    }

    /// Retrieve the latest value for a given key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_at(key, u64::MAX)
    }

    /// Retrieve the value for a given key at a specific sequence number.
    ///
    /// Checks the memtable first, then each level from L0 downward.
    /// Stops at the first layer that contains the key — a tombstone in
    /// a newer layer shadows any value in older layers.
    /// Returns the most recent version with `seq <= max_seq`.
    pub fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, StorageError> {
        let key_vec = key.to_vec();
        if let Some((ik, value)) = self
            .mem
            .scan_at(key_vec.clone()..=key_vec, max_seq)
            .next()
            .transpose()?
        {
            return match ik.op {
                OpType::Put => Ok(Some(value.clone())),
                OpType::Delete => Ok(None),
            };
        }
        for level in &self.levels {
            let key_range = key.to_vec()..=key.to_vec();
            if let Some((ik, value)) = level
                .scan_at(key_range, max_seq)
                .next()
                .transpose()
                .map_err(StorageError::from)?
            {
                return match ik.op {
                    OpType::Put => Ok(Some(value)),
                    OpType::Delete => Ok(None),
                };
            }
        }
        Ok(None)
    }

    /// Scan the range, yielding the latest visible version of each
    /// user key in ascending order. Tombstones are skipped.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> ScanIterator<'_> {
        self.scan_at(range, u64::MAX)
    }

    /// Scan the range as of `max_seq`: only entries with `seq <= max_seq`
    /// are visible. For each user key the latest such version is emitted
    /// and tombstones are skipped.
    pub fn scan_at(&self, range: impl RangeBounds<Vec<u8>>, max_seq: u64) -> ScanIterator<'_> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();

        // Each source is already sorted by InternalKey, so merging produces
        // a globally sorted stream without an intermediate Vec.
        let mut sources: Vec<RawSource<'_>> = Vec::with_capacity(1 + self.levels.len());
        sources.push(Box::new(
            self.mem.scan_at((start.clone(), end.clone()), max_seq),
        ));
        for level in &self.levels {
            sources.push(Box::new(
                level.scan_at((start.clone(), end.clone()), max_seq),
            ));
        }

        let merged = MergeIterator::new(sources, |a, b| match (a, b) {
            (Ok((ka, _)), Ok((kb, _))) => ka.cmp(kb),
            // Surface errors before any further data is consumed.
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => std::cmp::Ordering::Equal,
        });

        ScanIterator::new(VersionResolver::new(merged))
    }
}

/// Collapses a sorted internal-entry stream into user-key `KVPair`s.
/// Requires the input to be ordered by user key ascending, seq
/// descending — so the first occurrence of each user key is the
/// latest version. Tombstones are dropped.
struct VersionResolver<I> {
    inner: I,
    last_key: Option<Vec<u8>>,
}

impl<I> VersionResolver<I> {
    fn new(inner: I) -> Self {
        Self {
            inner,
            last_key: None,
        }
    }
}

impl<I> Iterator for VersionResolver<I>
where
    I: Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>>,
{
    type Item = Result<KVPair, StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next()? {
                Err(e) => return Some(Err(e.into())),
                Ok((ik, value)) => {
                    if self.last_key.as_deref() == Some(ik.key.as_slice()) {
                        continue;
                    }
                    self.last_key = Some(ik.key.clone());
                    if ik.op == OpType::Delete {
                        // Set last_key before continuing so older versions of
                        // this tombstoned key are also skipped on subsequent
                        // pulls — order matters here.
                        continue;
                    }
                    return Some(Ok((ik.key, value)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsm::memstore::BTreeMapStore;

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

        let results: Vec<KVPair> = engine
            .scan(b"key:a".to_vec()..=b"key:c".to_vec())
            .collect::<Result<_, _>>()
            .unwrap();
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
    fn compact_cascades_from_l0_to_l1() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(32)).unwrap();
        // Tiny L0: triggers inter-level compaction after 2 runs.
        engine.levels[0] = Level::new(LevelConfig {
            max_runs: 2,
            max_run_size_bytes: 64 * 1024 * 1024,
        });
        // Give L1 plenty of room so it doesn't cascade further.
        engine.levels[1] = Level::new(LevelConfig {
            max_runs: 64,
            max_run_size_bytes: 256 * 1024 * 1024,
        });

        // Write enough to trigger L0 compaction into L1.
        for i in 0..40u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }

        // L0 should have been drained into L1.
        assert!(
            !engine.levels[1].is_empty(),
            "L1 should have runs from L0 compaction"
        );
        // All data should still be readable.
        assert_eq!(engine.get(b"k:0000").unwrap(), Some(b"v:0".to_vec()),);
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
