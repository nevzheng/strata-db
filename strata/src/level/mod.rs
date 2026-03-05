mod manifest;
mod sstable;

pub use manifest::{Manifest, ManifestEntry, ManifestOp};
pub use sstable::{SsTable, SsTableRef, read_sstable_ref};

use sstable::write_sstables;

use std::ops::{Bound, RangeBounds};
use std::path::PathBuf;

use itertools::Itertools;

use crate::memstore::{InternalKey, OpType, ReadError};
use crate::{ReadStore, StorageError};

/// Configuration for a single level in the LSM tree.
pub struct LevelConfig {
    pub max_runs: usize,
    pub max_run_size_bytes: usize,
}

/// A level in the LSM tree, containing a set of sorted runs.
pub struct Level {
    pub runs: Vec<Run>,
    pub config: LevelConfig,
    dir: PathBuf,
    next_sst_id: u64,
}

impl Level {
    pub fn new(dir: PathBuf, config: LevelConfig) -> Self {
        Self {
            runs: Vec::new(),
            config,
            dir,
            next_sst_id: 0,
        }
    }

    /// Write entries to disk as SSTables and add the resulting run to this level.
    ///
    /// Entries must be sorted by `InternalKey` (user key asc, seq desc).
    /// Returns `true` if the level has reached its compaction threshold.
    pub fn add_run(
        &mut self,
        entries: impl IntoIterator<Item = (InternalKey, Vec<u8>)>,
    ) -> Result<bool, StorageError> {
        let sst_id = self.next_sst_id;
        self.next_sst_id += 1;
        let refs = write_sstables(&self.dir, sst_id, usize::MAX, entries)?;
        self.runs.push(Run::from_refs(refs));
        Ok(self.runs.len() >= self.config.max_runs)
    }

    /// Open all SSTables and return a merged iterator over their entries,
    /// sorted by `InternalKey` order.
    pub fn merge_iter(
        &self,
    ) -> Result<impl Iterator<Item = (InternalKey, Vec<u8>)> + use<>, ReadError> {
        let tables: Vec<SsTable> = self
            .runs
            .iter()
            .flat_map(|run| &run.tables)
            .map(|r| SsTable::open(r.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ReadError::Internal(e.to_string()))?;
        Ok(tables.into_iter().map(|t| t.entries.into_iter()).kmerge())
    }

    /// Return all entries within the given user-key range across all runs.
    ///
    /// Prunes SSTables by min/max key before opening.
    pub fn scan(
        &self,
        range: impl RangeBounds<Vec<u8>>,
    ) -> Result<Vec<(InternalKey, Vec<u8>)>, ReadError> {
        let tables: Vec<SsTable> = self
            .runs
            .iter()
            .flat_map(|run| &run.tables)
            .filter(|r| overlaps(r, &range))
            .map(|r| SsTable::open(r.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ReadError::Internal(e.to_string()))?;
        let result = tables
            .into_iter()
            .map(|t| t.entries.into_iter())
            .kmerge()
            .filter(|(ik, _)| in_range(&ik.key, &range))
            .collect();
        Ok(result)
    }

    pub fn clear(&mut self) {
        self.runs.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.runs.len() >= self.config.max_runs
    }
}

/// Check whether an SSTable's key range overlaps a query range.
fn overlaps(table_ref: &SsTableRef, range: &impl RangeBounds<Vec<u8>>) -> bool {
    let after_max = match range.start_bound() {
        Bound::Included(k) => table_ref.max_key < *k,
        Bound::Excluded(k) => table_ref.max_key <= *k,
        Bound::Unbounded => false,
    };
    let before_min = match range.end_bound() {
        Bound::Included(k) => table_ref.min_key > *k,
        Bound::Excluded(k) => table_ref.min_key >= *k,
        Bound::Unbounded => false,
    };
    !after_max && !before_min
}

/// Check whether a user key falls within a range.
fn in_range(key: &[u8], range: &impl RangeBounds<Vec<u8>>) -> bool {
    let start_ok = match range.start_bound() {
        Bound::Included(k) => key >= k.as_slice(),
        Bound::Excluded(k) => key > k.as_slice(),
        Bound::Unbounded => true,
    };
    let end_ok = match range.end_bound() {
        Bound::Included(k) => key <= k.as_slice(),
        Bound::Excluded(k) => key < k.as_slice(),
        Bound::Unbounded => true,
    };
    start_ok && end_ok
}

impl ReadStore for Level {
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError> {
        for run in self.runs.iter().rev() {
            for table_ref in run.tables.iter().rev() {
                if key < table_ref.min_key.as_slice() || key > table_ref.max_key.as_slice() {
                    continue;
                }
                let table = SsTable::open(table_ref.clone())
                    .map_err(|e| ReadError::Internal(e.to_string()))?;
                if let Some((ik, value)) = table
                    .scan_at(key.to_vec()..=key.to_vec(), max_seq)
                    .next()
                    .transpose()?
                {
                    return match ik.op {
                        OpType::Put => Ok(Some(value)),
                        OpType::Delete => Ok(None),
                    };
                }
            }
        }
        Ok(None)
    }

    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_ {
        match self.scan(range) {
            Ok(entries) => {
                let iter = entries
                    .into_iter()
                    .filter(move |(ik, _)| ik.seq <= max_seq)
                    .map(Ok);
                Box::new(iter)
                    as Box<dyn Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))),
        }
    }
}

/// A sorted run of SSTables.
pub struct Run {
    pub tables: Vec<SsTableRef>,
    pub size_bytes: usize,
}

impl Run {
    /// Build a run from a set of SSTable references.
    pub fn from_refs(refs: Vec<SsTableRef>) -> Self {
        let size_bytes = refs.iter().map(|r| r.data_size).sum();
        Self {
            tables: refs,
            size_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_entry(key: &[u8], value: &[u8], seq: u64) -> (InternalKey, Vec<u8>) {
        (
            InternalKey {
                key: key.to_vec(),
                seq,
                op: OpType::Put,
            },
            value.to_vec(),
        )
    }

    fn test_level(tmp: &std::path::Path) -> Level {
        Level::new(
            tmp.to_path_buf(),
            LevelConfig {
                max_runs: 64,
                max_run_size_bytes: 64 * 1024 * 1024,
            },
        )
    }

    #[test]
    fn level_add_run_writes_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut level = test_level(tmp.path());
        level
            .add_run(vec![put_entry(b"a", b"1", 1), put_entry(b"c", b"3", 3)])
            .unwrap();
        assert_eq!(level.runs.len(), 1);
        assert_eq!(level.runs[0].tables[0].min_key, b"a");
        assert_eq!(level.runs[0].tables[0].max_key, b"c");
        assert!(level.runs[0].size_bytes > 0);
    }

    #[test]
    fn level_get_at_returns_latest_from_newest_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut level = test_level(tmp.path());
        level.add_run(vec![put_entry(b"a", b"old", 1)]).unwrap();
        level.add_run(vec![put_entry(b"a", b"new", 2)]).unwrap();
        assert_eq!(level.get_at(b"a", u64::MAX).unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn level_scan_returns_range() {
        let tmp = tempfile::tempdir().unwrap();
        let mut level = test_level(tmp.path());
        level
            .add_run(vec![
                put_entry(b"a", b"1", 1),
                put_entry(b"b", b"2", 2),
                put_entry(b"c", b"3", 3),
                put_entry(b"d", b"4", 4),
            ])
            .unwrap();

        let results = level.scan(b"b".to_vec()..=b"c".to_vec()).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.key, b"b");
        assert_eq!(results[1].0.key, b"c");
    }

    #[test]
    fn level_scan_unbounded_returns_all() {
        let tmp = tempfile::tempdir().unwrap();
        let mut level = test_level(tmp.path());
        level
            .add_run(vec![put_entry(b"a", b"1", 1), put_entry(b"b", b"2", 2)])
            .unwrap();
        let results = level.scan(..).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn level_add_run_signals_compaction_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut level = Level::new(
            tmp.path().to_path_buf(),
            LevelConfig {
                max_runs: 2,
                max_run_size_bytes: 64 * 1024 * 1024,
            },
        );
        assert!(!level.add_run(vec![put_entry(b"a", b"1", 1)]).unwrap());
        assert!(level.add_run(vec![put_entry(b"b", b"2", 2)]).unwrap());
        // Can still add beyond threshold.
        assert!(level.add_run(vec![put_entry(b"c", b"3", 3)]).unwrap());
    }

    #[test]
    fn level_scan_merges_across_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut level = test_level(tmp.path());
        level
            .add_run(vec![put_entry(b"a", b"1", 1), put_entry(b"c", b"3", 3)])
            .unwrap();
        level.add_run(vec![put_entry(b"b", b"2", 4)]).unwrap();

        let results = level.scan(..).unwrap();
        let keys: Vec<&[u8]> = results.iter().map(|(ik, _)| ik.key.as_slice()).collect();
        assert_eq!(keys, vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
    }
}
