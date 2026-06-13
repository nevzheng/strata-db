//! `lsm` — a log-structured merge tree.
//!
//! The crate root holds the tree itself ([`Lsm`]). Tree *structure* — which
//! runs live in which level — is the [`manifest`]'s [`Version`]; physical detail
//! (on-disk format, bloom filters, paging) lives in `storage`, resolved from an
//! [`SsTableId`]; durability lives in the memtable journal (`memstore`) and the
//! manifest.

mod compaction;
mod config;
mod error;
mod iterator;
mod key;
mod layout;
mod manifest;
mod memstore;
mod storage;
mod store;

pub use compaction::{CompactionJob, CompactionKind};
pub use config::{
    BloomConfig, CachePolicy, LevelConfig, LsmConfig, PageCacheConfig, PageConfig, RunConfig,
    SizeConfig, TableConfig,
};
pub use error::{LsmError, ReadError, WriteError};
pub use iterator::{KvStream, MergeIterator, ScanIterator};
pub use key::{InternalKey, KVPair, KeyRange, KeyValue, OpType};
pub use manifest::{ManifestEdit, ManifestOp, RunDescriptor, RunId, Version};
pub use memstore::BTreeMemtable;
pub use storage::{
    BloomFilter, DataBlock, Decode, DecodeError, Encode, Header, Page, PageId, SsTable,
    SstPageCache,
};
pub use store::{MemStore, ReadStore, WriteStore};

use std::ops::{Bound, RangeBounds};
use std::path::PathBuf;

use layout::Layout;
use manifest::ManifestManager;
use memstore::Journaled;

/// Globally-unique identity of an SSTable. The manifest and tree hold these;
/// the filesystem and page cache resolve an id to the physical table — its
/// header (range, bloom, size) and its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SsTableId(pub u64);

/// The LSM tree — the live store you set and read values on.
///
/// Writes are logged to the memtable journal, then land in the in-memory
/// memtable; [`flush`](Lsm::flush) seals it into an on-disk L0 SSTable and
/// records it in the manifest. Reads merge the memtable with the levels the
/// manifest reports, resolved from their ids through the page cache. On open
/// the manifest rebuilds the structure and the journal replays the unflushed
/// tail. Generic over the memtable.
pub struct Lsm<M: MemStore = BTreeMemtable> {
    config: LsmConfig,
    layout: Layout,
    cache: SstPageCache,
    mem: Journaled<M>,
    manifest: ManifestManager,
    seq: u64,
}

impl<M: MemStore + Default> Lsm<M> {
    /// Open a tree rooted at `dir` with a default memtable.
    pub fn new(dir: impl Into<PathBuf>, config: LsmConfig) -> Result<Self, LsmError> {
        Self::with_memtable(dir, config, M::default())
    }
}

impl<M: MemStore> Lsm<M> {
    /// Open a tree rooted at `dir` using `mem` as its memtable. Rebuilds the
    /// structure from the manifest, replays the memtable journal for unflushed
    /// writes, and garbage-collects any SSTables an interrupted operation orphaned.
    pub fn with_memtable(
        root: impl Into<PathBuf>,
        config: LsmConfig,
        mem: M,
    ) -> Result<Self, LsmError> {
        let layout = Layout::new(root);
        let cache = SstPageCache::new(config.page_cache);
        let manifest = ManifestManager::open(&layout.manifests())?;

        // The memstore journals itself and replays its unflushed tail on open.
        let mem = Journaled::open(layout.memtable_journal(), mem)?;
        // Resume seq past both the flushed watermark and the recovered tail.
        let mem_seq = mem
            .scan_at(.., u64::MAX)
            .filter_map(Result::ok)
            .map(|(key, _)| key.seq)
            .max()
            .unwrap_or(0);
        let seq = manifest.version().last_seq().max(mem_seq);

        // Drop SSTables no committed run references (orphaned flush/compaction output).
        manifest.garbage_collect(&layout.sstables())?;

        Ok(Self {
            config,
            layout,
            cache,
            mem,
            manifest,
            seq,
        })
    }

    /// Insert or overwrite a value.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), LsmError> {
        self.write(key, value, OpType::Put)
    }

    /// Delete a key, writing a tombstone.
    pub fn delete(&mut self, key: &[u8]) -> Result<(), LsmError> {
        self.write(key, &[], OpType::Delete)
    }

    fn write(&mut self, key: &[u8], value: &[u8], op: OpType) -> Result<(), LsmError> {
        let ikey = InternalKey {
            user_key: key.to_vec(),
            seq: self.seq + 1,
            op,
        };
        // The memstore logs the write durably before applying it.
        self.mem.put(ikey, value)?;
        self.seq += 1;
        Ok(())
    }

    /// Seal the memtable into a new on-disk L0 SSTable, record it in the
    /// manifest, and clear the memtable (which truncates its journal).
    ///
    /// Order is what makes it crash-safe: write the file, then commit the
    /// manifest edit (the commit point), then clear. A crash before the commit
    /// leaves the file as an orphan that open's GC sweeps.
    pub fn flush(&mut self) -> Result<(), LsmError> {
        // All versions (including tombstones), in InternalKey order.
        let entries: Vec<KeyValue> = self
            .mem
            .scan_at(.., u64::MAX)
            .map(|r| r.map(|(key, value)| KeyValue { key, value }))
            .collect::<Result<_, _>>()?;
        if entries.is_empty() {
            return Ok(());
        }

        let id = SsTableId(self.manifest.version().next_sst_id());
        let table_cfg = self
            .config
            .levels
            .first()
            .map(|l| l.table.clone())
            .unwrap_or_default();
        SsTable::write(id, &self.layout.sstables(), &table_cfg, entries)?;

        // A run is named by its (only) file id; record it as a new L0 run.
        let edit = ManifestEdit::new()
            .add_run(RunDescriptor {
                level: 0,
                run: RunId(id.0),
                files: vec![id],
            })
            .set_next_sst_id(id.0 + 1)
            .set_last_seq(self.seq);
        self.manifest.commit(edit)?;

        self.mem.clear()?;
        Ok(())
    }

    /// Compact the manifest to a fresh snapshot, bounding its size and the work
    /// to replay on the next open.
    pub fn checkpoint(&mut self) -> Result<(), LsmError> {
        self.manifest.checkpoint()
    }

    /// Latest value for `key`, or `None` if absent or deleted.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, LsmError> {
        self.get_at(key, self.seq)
    }

    /// Point lookup as of `max_seq`. Checks the memtable first, then on-disk
    /// levels newest-first, skipping tables by range + bloom. Stops at the
    /// first layer that holds the key (a tombstone there means "deleted").
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, LsmError> {
        let probe = key.to_vec();
        if let Some(r) = self.mem.scan_at(probe.clone()..=probe, max_seq).next() {
            let (ikey, value) = r?;
            return Ok(resolve(ikey.op, value));
        }
        let version = self.manifest.version();
        let sstables = self.layout.sstables();
        for level in 0..self.config.num_levels() as u32 {
            // Within a level, newest run first (a newer run shadows older ones).
            let runs: Vec<_> = version.runs_in(level).collect();
            for run in runs.into_iter().rev() {
                for &id in &run.files {
                    let table = SsTable::open(id, &sstables, &self.cache)?;
                    if let Some(kv) = table.get(key, max_seq, &self.cache)? {
                        return Ok(resolve(kv.key.op, kv.value));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Scan a key range, newest version per key, tombstones dropped.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> ScanIterator<'_> {
        self.scan_at(range, self.seq)
    }

    /// Scan as of `max_seq` (point-in-time).
    pub fn scan_at(&self, range: impl RangeBounds<Vec<u8>>, max_seq: u64) -> ScanIterator<'_> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();

        // Lazy sources, merged on demand: the memtable, then every on-disk file
        // whose range overlaps. Each file streams one block at a time, so peak
        // memory is one block per source, not the whole result.
        let mut sources: Vec<KvStream<'_>> = Vec::new();

        sources.push(Box::new(
            self.mem
                .scan_at((start.clone(), end.clone()), max_seq)
                .map(|r| r.map(|(key, value)| KeyValue { key, value })),
        ));

        // Version resolution handles ordering by seq, so run order doesn't
        // matter here — every overlapping file across all levels is a source.
        let version = self.manifest.version();
        let sstables = self.layout.sstables();
        for level in 0..self.config.num_levels() as u32 {
            for run in version.runs_in(level) {
                for &id in &run.files {
                    let table = match SsTable::open(id, &sstables, &self.cache) {
                        Ok(table) => table,
                        Err(e) => {
                            sources.push(Box::new(std::iter::once(Err(read_err(e)))));
                            continue;
                        }
                    };
                    if !overlaps(table.range(), &start, &end) {
                        continue;
                    }
                    sources.push(table.scan((start.clone(), end.clone()), max_seq, &self.cache));
                }
            }
        }

        let merged = MergeIterator::new(sources, |a, b| match (a, b) {
            (Ok(x), Ok(y)) => x.key.cmp(&y.key),
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => std::cmp::Ordering::Equal,
        });
        let resolved = iterator::VersionResolver::new(merged).map(|r| r.map_err(LsmError::from));
        ScanIterator::new(resolved)
    }

    /// The tree's configuration.
    pub fn config(&self) -> &LsmConfig {
        &self.config
    }
}

fn read_err(e: LsmError) -> ReadError {
    ReadError::Internal(e.to_string())
}

/// Apply a record's op: a put yields its value, a tombstone yields `None`.
fn resolve(op: OpType, value: Vec<u8>) -> Option<Vec<u8>> {
    match op {
        OpType::Put => Some(value),
        OpType::Delete => None,
    }
}

/// Whether a table's `[min, max]` could hold any key in the query range.
fn overlaps(range: &KeyRange, start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> bool {
    let below = match end {
        Bound::Included(e) => range.min.as_slice() > e.as_slice(),
        Bound::Excluded(e) => range.min.as_slice() >= e.as_slice(),
        Bound::Unbounded => false,
    };
    let above = match start {
        Bound::Included(s) => range.max.as_slice() < s.as_slice(),
        Bound::Excluded(s) => range.max.as_slice() <= s.as_slice(),
        Bound::Unbounded => false,
    };
    !(below || above)
}
