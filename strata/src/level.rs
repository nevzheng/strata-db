use std::ops::{Bound, RangeBounds};

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
}

impl Level {
    pub fn new(config: LevelConfig) -> Self {
        Self {
            runs: Vec::new(),
            config,
        }
    }

    /// Add a run to this level. Newest runs are pushed to the end.
    pub fn add_run(&mut self, run: Run) -> Result<(), StorageError> {
        if self.runs.len() >= self.config.max_runs {
            return Err(StorageError::InternalError(format!(
                "level full: {} runs exceeds max {}",
                self.runs.len() + 1,
                self.config.max_runs
            )));
        }
        self.runs.push(run);
        Ok(())
    }

    /// Return all entries within the given user-key range across all runs.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> Vec<(InternalKey, Vec<u8>)> {
        let mut result = Vec::new();
        for run in &self.runs {
            for table in &run.tables {
                let entries =
                    table.scan((range.start_bound().cloned(), range.end_bound().cloned()));
                result.extend(entries.iter().map(|(ik, v)| (ik.clone(), v.clone())));
            }
        }
        result.sort_by(|(a, _), (b, _)| a.cmp(b));
        result
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

impl ReadStore for Level {
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError> {
        let key_vec = key.to_vec();
        for run in self.runs.iter().rev() {
            for table in run.tables.iter().rev() {
                if let Some((ik, value)) = table
                    .scan_at(key_vec.clone()..=key_vec.clone(), max_seq)
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
        range: impl std::ops::RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_ {
        self.scan(range)
            .into_iter()
            .filter(move |(ik, _)| ik.seq <= max_seq)
            .map(Ok)
    }
}

/// A sorted run of SSTables.
pub struct Run {
    pub tables: Vec<SsTableRef>,
    pub size_bytes: usize,
}

impl Run {
    /// Build a single-table run from a memtable flush.
    ///
    /// The entries must already be sorted by `InternalKey` order.
    pub fn from_entries(id: u64, mut entries: Vec<(InternalKey, Vec<u8>)>) -> Self {
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let size_bytes: usize = entries.iter().map(|(ik, v)| ik.key.len() + v.len()).sum();
        let min_key = entries
            .first()
            .map(|(ik, _)| ik.key.clone())
            .unwrap_or_default();
        let max_key = entries
            .last()
            .map(|(ik, _)| ik.key.clone())
            .unwrap_or_default();
        let table = SsTableRef {
            id,
            min_key,
            max_key,
            entries: Some(entries),
        };
        Self {
            tables: vec![table],
            size_bytes,
        }
    }
}

/// A reference to an SSTable, optionally holding its entries in memory.
pub struct SsTableRef {
    pub id: u64,
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
    pub entries: Option<Vec<(InternalKey, Vec<u8>)>>,
}

impl SsTableRef {
    /// Return all entries within the given user-key range, in `InternalKey` order.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> &[(InternalKey, Vec<u8>)] {
        let entries = match self.entries.as_ref() {
            Some(e) => e,
            None => return &[],
        };
        let start_idx = match range.start_bound() {
            Bound::Included(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: u64::MAX,
                    op: OpType::Put,
                };
                entries.partition_point(|(ik, _)| ik < &probe)
            }
            Bound::Excluded(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: 0,
                    op: OpType::Put,
                };
                entries.partition_point(|(ik, _)| ik <= &probe)
            }
            Bound::Unbounded => 0,
        };
        let end_idx = match range.end_bound() {
            Bound::Included(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: 0,
                    op: OpType::Put,
                };
                entries.partition_point(|(ik, _)| ik <= &probe)
            }
            Bound::Excluded(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: u64::MAX,
                    op: OpType::Put,
                };
                entries.partition_point(|(ik, _)| ik < &probe)
            }
            Bound::Unbounded => entries.len(),
        };
        &entries[start_idx..end_idx]
    }
}

impl ReadStore for SsTableRef {
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError> {
        let entries = match self.entries.as_ref() {
            Some(e) => e,
            None => return Ok(None),
        };
        let probe = InternalKey {
            key: key.to_vec(),
            seq: max_seq,
            op: OpType::Put,
        };
        let idx = entries.partition_point(|(ik, _)| ik < &probe);
        if idx < entries.len() && entries[idx].0.key == key {
            return match entries[idx].0.op {
                OpType::Put => Ok(Some(entries[idx].1.clone())),
                OpType::Delete => Ok(None),
            };
        }
        Ok(None)
    }

    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_ {
        self.scan(range)
            .iter()
            .filter(move |(ik, _)| ik.seq <= max_seq)
            .map(|(ik, v)| Ok((ik.clone(), v.clone())))
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

    fn delete_entry(key: &[u8], seq: u64) -> (InternalKey, Vec<u8>) {
        (
            InternalKey {
                key: key.to_vec(),
                seq,
                op: OpType::Delete,
            },
            Vec::new(),
        )
    }

    fn test_config() -> LevelConfig {
        LevelConfig {
            max_runs: 64,
            max_run_size_bytes: 64 * 1024 * 1024,
        }
    }

    // --- SsTableRef tests ---

    #[test]
    fn sstable_get_at_returns_latest_put() {
        let run = Run::from_entries(
            1,
            vec![put_entry(b"a", b"v1", 1), put_entry(b"a", b"v2", 2)],
        );
        let table = &run.tables[0];
        assert_eq!(table.get_at(b"a", u64::MAX).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn sstable_get_at_returns_none_for_tombstone() {
        let run = Run::from_entries(1, vec![put_entry(b"a", b"v1", 1), delete_entry(b"a", 2)]);
        let table = &run.tables[0];
        assert_eq!(table.get_at(b"a", u64::MAX).unwrap(), None);
    }

    #[test]
    fn sstable_get_at_returns_none_for_missing_key() {
        let run = Run::from_entries(1, vec![put_entry(b"a", b"v1", 1)]);
        let table = &run.tables[0];
        assert_eq!(table.get_at(b"missing", u64::MAX).unwrap(), None);
    }

    #[test]
    fn sstable_scan_returns_range() {
        let run = Run::from_entries(
            1,
            vec![
                put_entry(b"a", b"1", 1),
                put_entry(b"b", b"2", 2),
                put_entry(b"c", b"3", 3),
                put_entry(b"d", b"4", 4),
            ],
        );
        let table = &run.tables[0];
        let results = table.scan(b"b".to_vec()..=b"c".to_vec());
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.key, b"b");
        assert_eq!(results[1].0.key, b"c");
    }

    #[test]
    fn sstable_scan_unbounded_returns_all() {
        let run = Run::from_entries(1, vec![put_entry(b"a", b"1", 1), put_entry(b"b", b"2", 2)]);
        let table = &run.tables[0];
        let results = table.scan(..);
        assert_eq!(results.len(), 2);
    }

    // --- Run tests ---

    #[test]
    fn run_from_entries_sorts_and_computes_bounds() {
        let run = Run::from_entries(1, vec![put_entry(b"c", b"3", 3), put_entry(b"a", b"1", 1)]);
        assert_eq!(run.tables.len(), 1);
        assert_eq!(run.tables[0].min_key, b"a");
        assert_eq!(run.tables[0].max_key, b"c");
        assert!(run.size_bytes > 0);
    }

    // --- Level tests ---

    #[test]
    fn level_get_at_returns_latest_from_newest_run() {
        let mut level = Level::new(test_config());
        level
            .add_run(Run::from_entries(1, vec![put_entry(b"a", b"old", 1)]))
            .unwrap();
        level
            .add_run(Run::from_entries(2, vec![put_entry(b"a", b"new", 2)]))
            .unwrap();
        assert_eq!(level.get_at(b"a", u64::MAX).unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn level_scan_returns_range() {
        let mut level = Level::new(test_config());
        level
            .add_run(Run::from_entries(
                1,
                vec![
                    put_entry(b"a", b"1", 1),
                    put_entry(b"b", b"2", 2),
                    put_entry(b"c", b"3", 3),
                    put_entry(b"d", b"4", 4),
                ],
            ))
            .unwrap();

        let results = level.scan(b"b".to_vec()..=b"c".to_vec());
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.key, b"b");
        assert_eq!(results[1].0.key, b"c");
    }

    #[test]
    fn level_scan_unbounded_returns_all() {
        let mut level = Level::new(test_config());
        level
            .add_run(Run::from_entries(
                1,
                vec![put_entry(b"a", b"1", 1), put_entry(b"b", b"2", 2)],
            ))
            .unwrap();
        let results = level.scan(..);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn level_add_run_returns_error_when_full() {
        let mut level = Level::new(LevelConfig {
            max_runs: 2,
            max_run_size_bytes: 64 * 1024 * 1024,
        });
        level
            .add_run(Run::from_entries(1, vec![put_entry(b"a", b"1", 1)]))
            .unwrap();
        level
            .add_run(Run::from_entries(2, vec![put_entry(b"b", b"2", 2)]))
            .unwrap();

        let err = level
            .add_run(Run::from_entries(3, vec![put_entry(b"c", b"3", 3)]))
            .unwrap_err();
        assert!(
            matches!(err, StorageError::InternalError(ref msg) if msg.contains("level full")),
            "expected level full, got: {err:?}"
        );
    }

    #[test]
    fn level_scan_merges_across_runs() {
        let mut level = Level::new(test_config());
        level
            .add_run(Run::from_entries(
                1,
                vec![put_entry(b"a", b"1", 1), put_entry(b"c", b"3", 3)],
            ))
            .unwrap();
        level
            .add_run(Run::from_entries(2, vec![put_entry(b"b", b"2", 4)]))
            .unwrap();

        let results = level.scan(..);
        let keys: Vec<&[u8]> = results.iter().map(|(ik, _)| ik.key.as_slice()).collect();
        assert_eq!(keys, vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
    }
}
