use std::ops::RangeBounds;
use std::path::Path;

use crate::iterator::Scan;
use crate::{StorageError, memstore::BTreeMapStore};
use filesystem::{Heap, HeapOptions, TupleLoc, TupleRef};
use lsm::{LevelConfig, Lsm, LsmConfig, MemStore};
use tracing::{info, instrument};

/// Frames in the heap's buffer pool. 1024 × 8 KiB ≈ 8 MiB. A tuning knob.
const HEAP_FRAMES: usize = 1024;

/// Strata's storage engine: an **index over a heap**.
///
/// The [`Lsm`] tree is the *index* — it maps a row key to a [`TupleLoc`] (the
/// 10-byte address of the tuple in the heap). The [`Heap`] is the *store* — it
/// holds the tuple bytes in pages, behind a journaled page cache.
///
/// Writes insert the tuple into the heap, then record `key → loc` in the index.
/// Reads look the key up in the index, decode the location, and fetch the bytes
/// from the heap.
///
/// # Durability note (v1)
///
/// The index and the heap journal independently: the LSM logs every write, but
/// the heap only becomes durable at [`flush`](Self::flush). So a crash between
/// writes and a `flush` can leave the index referencing heap pages that never
/// reached disk. Cross-journal ordering (one log, or an LSN protocol) is future
/// work; for now, treat `flush` as the durability point.
pub struct StorageEngine<M: MemStore = BTreeMapStore> {
    index: Lsm<M>,
    heap: Heap,
}

/// Builder for [`StorageEngine`].  Construct via [`StorageEngine::builder`].
pub struct EngineBuilder<'a, M: MemStore> {
    dir: &'a Path,
    mem: M,
    heap_frames: usize,
    direct_io: bool,
    levels: Option<Vec<LevelConfig>>,
}

impl<M: MemStore> EngineBuilder<'_, M> {
    /// Set the heap buffer-pool size in frames (8 KiB each).  Default: 1024.
    pub fn heap_frames(mut self, n: usize) -> Self {
        self.heap_frames = n;
        self
    }

    /// Enable direct I/O on the heap.  Default: false.
    pub fn direct_io(mut self, yes: bool) -> Self {
        self.direct_io = yes;
        self
    }

    /// Set explicit per-level LSM configuration.  Default: [`LsmConfig::default`].
    pub fn levels(mut self, levels: Vec<LevelConfig>) -> Self {
        self.levels = Some(levels);
        self
    }

    /// Build the engine.
    pub fn build(self) -> Result<StorageEngine<M>, StorageError> {
        let config = match self.levels {
            Some(levels) => LsmConfig {
                levels,
                ..LsmConfig::default()
            },
            None => LsmConfig::default(),
        };
        Ok(StorageEngine {
            index: Lsm::with_memtable(self.dir, config, self.mem)?,
            heap: Heap::open(
                &self.dir.join("heap"),
                HeapOptions {
                    frames: self.heap_frames,
                    direct_io: self.direct_io,
                },
            )?,
        })
    }
}

impl<M: MemStore> StorageEngine<M> {
    /// Create a builder for an engine rooted at `dir`, using `mem` as the
    /// memtable.
    pub fn builder(dir: &Path, mem: M) -> EngineBuilder<'_, M> {
        EngineBuilder {
            dir,
            mem,
            heap_frames: HEAP_FRAMES,
            direct_io: false,
            levels: None,
        }
    }

    /// Open an engine with default settings: 1024 heap frames, buffered I/O,
    /// default LSM levels. Convenience shortcut for `builder(dir, mem).build()`.
    pub fn new(dir: &Path, mem: M) -> Result<Self, StorageError> {
        Self::builder(dir, mem).build()
    }

    /// Insert a key-value pair: store the value in the heap, index its location.
    ///
    /// Overwriting a key leaves the previous tuple's slot unreferenced in the
    /// heap; that space is reclaimed only by future compaction (no free list yet).
    #[instrument(skip(self, key, value), fields(key_len = key.len(), value_len = value.len()))]
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        let loc = self.heap.insert(value)?;
        self.index.put(key, &loc.encode())?;
        Ok(())
    }

    /// Delete a key (writes a tombstone in the index). The heap slot is left
    /// behind for compaction to reclaim.
    #[instrument(skip(self, key), fields(key_len = key.len()))]
    pub fn delete(&mut self, key: &[u8]) -> Result<(), StorageError> {
        self.index.delete(key)?;
        Ok(())
    }

    /// Commit the heap durably, then seal the memtable into a new on-disk L0
    /// SSTable. Heap first, so the index never becomes durable ahead of the
    /// tuple bytes it points to.
    pub fn flush(&mut self) -> Result<(), StorageError> {
        self.heap.flush()?;
        self.index.flush()?;
        info!("flushed heap and sealed memtable to l0");
        Ok(())
    }

    /// Retrieve a zero-copy view of the latest value for `key`, or `None` if
    /// absent or deleted. The view pins its page until dropped; decode out of it
    /// to materialize a value.
    pub fn get(&self, key: &[u8]) -> Result<Option<TupleRef>, StorageError> {
        let Some(loc_bytes) = self.index.get(key)? else {
            return Ok(None);
        };
        let loc = decode_loc(&loc_bytes)?;
        Ok(Some(self.heap.get(loc)?))
    }

    /// Number of levels in the LSM index.
    pub fn num_levels(&self) -> usize {
        self.index.config().num_levels()
    }

    /// Scan the range, yielding the latest visible version of each user key in
    /// ascending order. Tombstones are skipped. Returns a [`Scan`] — a lending
    /// iterator of tuple views, holding one pinned heap page at a time.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> Scan<'_> {
        Scan::new(
            Box::new(
                self.index
                    .scan(range)
                    .map(|r| r.map_err(StorageError::from)),
            ),
            &self.heap,
        )
    }

    /// Scan the range as of `max_seq` (point-in-time).
    pub fn scan_at(&self, range: impl RangeBounds<Vec<u8>>, max_seq: u64) -> Scan<'_> {
        Scan::new(
            Box::new(
                self.index
                    .scan_at(range, max_seq)
                    .map(|r| r.map_err(StorageError::from)),
            ),
            &self.heap,
        )
    }
}

/// Open the heap under `dir/heap`, behind a journaled page cache.
pub(crate) fn decode_loc(bytes: &[u8]) -> Result<TupleLoc, StorageError> {
    TupleLoc::decode(bytes)
        .ok_or_else(|| StorageError::Corruption(format!("malformed tuple location ({bytes:?})")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memstore::BTreeMapStore;

    fn engine() -> (tempfile::TempDir, StorageEngine<BTreeMapStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        (tmp, engine)
    }

    /// Materialize a point lookup's view into owned bytes (test convenience).
    fn get_bytes(engine: &StorageEngine<BTreeMapStore>, key: &[u8]) -> Option<Vec<u8>> {
        engine
            .get(key)
            .unwrap()
            .map(|view| view.bytes().expect("live tuple").to_vec())
    }

    /// Drain a scan into owned (key, value) pairs (test convenience).
    fn scan_pairs(
        engine: &StorageEngine<BTreeMapStore>,
        range: std::ops::RangeInclusive<Vec<u8>>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut scan = engine.scan(range);
        let mut out = Vec::new();
        while let Some(row) = scan.next() {
            let row = row.unwrap();
            out.push((row.key.clone(), row.tuple.bytes().unwrap().to_vec()));
        }
        out
    }

    #[test]
    fn put_get_delete_round_trip() {
        let (_tmp, mut engine) = engine();
        engine.put(b"user:alice", b"admin").unwrap();
        assert_eq!(get_bytes(&engine, b"user:alice"), Some(b"admin".to_vec()));

        engine.delete(b"user:alice").unwrap();
        assert_eq!(get_bytes(&engine, b"user:alice"), None);
    }

    #[test]
    fn scan_returns_sorted_results() {
        let (_tmp, mut engine) = engine();
        engine.put(b"key:c", b"3").unwrap();
        engine.put(b"key:a", b"1").unwrap();
        engine.put(b"key:b", b"2").unwrap();

        let pairs = scan_pairs(&engine, b"key:a".to_vec()..=b"key:c".to_vec());
        assert_eq!(
            pairs,
            vec![
                (b"key:a".to_vec(), b"1".to_vec()),
                (b"key:b".to_vec(), b"2".to_vec()),
                (b"key:c".to_vec(), b"3".to_vec()),
            ]
        );
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
        assert_eq!(get_bytes(&engine, b"a"), Some(b"1b".to_vec()));
        assert_eq!(get_bytes(&engine, b"b"), Some(b"2".to_vec()));
    }

    #[test]
    fn delete_after_flush_shadows_value() {
        let (_tmp, mut engine) = engine();
        engine.put(b"k", b"v").unwrap();
        engine.flush().unwrap();
        engine.delete(b"k").unwrap();
        assert_eq!(get_bytes(&engine, b"k"), None);
    }

    #[test]
    fn values_survive_flush_and_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
            engine.put(b"durable", b"value").unwrap();
            engine.flush().unwrap();
        }
        // Reopen: the index replays and the heap recovers from its journal.
        let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        assert_eq!(get_bytes(&engine, b"durable"), Some(b"value".to_vec()));
    }
}
