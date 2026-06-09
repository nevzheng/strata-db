//! `lsm` — a log-structured merge tree.
//!
//! The crate root holds the logical data hierarchy — structure and ids only.
//! Physical detail (on-disk format, bloom filters, paging) lives in the
//! `storage` module and is resolved from an [`SsTableId`].
//!
//! ```text
//! KeyValue → Run → Level → Lsm
//! ```

mod config;
mod error;
mod iterator;
mod key;
mod layout;
mod manifest;
mod memstore;
mod storage;
mod store;

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
use memstore::Journaled;

/// Globally-unique identity of an SSTable. The manifest and tree hold these;
/// the filesystem and page cache resolve an id to the physical table — its
/// header (range, bloom, size) and its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SsTableId(pub u64);

/// A sorted run: KV pairs with no key repeated, stored as one or more SSTable
/// files. Holds only their ids — range, bloom, and size are physical and
/// resolved from storage.
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub files: Vec<SsTableId>,
}

/// A level: one or more runs.
///
/// L0 holds multiple runs that *overlap* — one per memtable flush, sharing
/// keys at newer seqs — so it's read newest-first. Leveled compaction keeps
/// L1+ at a single run, so those don't overlap.
#[derive(Debug, Clone, Default)]
pub struct Level {
    pub runs: Vec<Run>,
}

/// The LSM tree — the live store you set and read values on.
///
/// Writes are logged to the memtable journal, then land in the in-memory
/// memtable; [`flush`](Lsm::flush) seals it into an on-disk L0 SSTable. Reads
/// merge the memtable with the on-disk levels — resolved from their ids through
/// the page cache — and return the newest version of each key. On open the
/// journal is replayed to recover unflushed writes. Generic over the memtable.
pub struct Lsm<M: MemStore = BTreeMemtable> {
    config: LsmConfig,
    layout: Layout,
    cache: SstPageCache,
    mem: Journaled<M>,
    levels: Vec<Level>,
    seq: u64,
    next_sst_id: u64,
}

impl<M: MemStore + Default> Lsm<M> {
    /// Open a tree rooted at `dir` with a default memtable.
    pub fn new(dir: impl Into<PathBuf>, config: LsmConfig) -> Result<Self, LsmError> {
        Self::with_memtable(dir, config, M::default())
    }
}

impl<M: MemStore> Lsm<M> {
    /// Open a tree rooted at `dir` using `mem` as its memtable; SSTable files
    /// and the memtable journal live under `dir`. The journal is replayed into
    /// the memtable to recover writes that hadn't been flushed.
    pub fn with_memtable(
        root: impl Into<PathBuf>,
        config: LsmConfig,
        mem: M,
    ) -> Result<Self, LsmError> {
        let layout = Layout::new(root);
        let levels = (0..config.num_levels()).map(|_| Level::default()).collect();
        let cache = SstPageCache::new(config.page_cache);

        // The memstore journals itself and replays on open; the journal is
        // entirely internal to it. We just resume seq numbering past whatever
        // it recovered.
        let mem = Journaled::open(layout.memtable_journal(), mem)?;
        let seq = mem
            .scan_at(.., u64::MAX)
            .filter_map(Result::ok)
            .map(|(key, _)| key.seq)
            .max()
            .unwrap_or(0);

        Ok(Self {
            config,
            layout,
            cache,
            mem,
            levels,
            seq,
            next_sst_id: 0,
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

    /// Seal the memtable into a new on-disk L0 SSTable and clear it.
    ///
    /// Note: the memtable journal is *not* truncated yet. Without a manifest,
    /// flushed SSTables aren't rediscovered on open, so the journal must keep
    /// every write for recovery; truncation lands once the manifest records the
    /// on-disk levels.
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

        let id = SsTableId(self.next_sst_id);
        self.next_sst_id += 1;
        let table_cfg = self
            .config
            .levels
            .first()
            .map(|l| l.table.clone())
            .unwrap_or_default();
        SsTable::write(id, &self.layout.sstables(), &table_cfg, entries)?;

        if self.levels.is_empty() {
            self.levels.push(Level::default());
        }
        // Newest run first within L0.
        self.levels[0].runs.insert(0, Run { files: vec![id] });
        self.mem.clear()?;
        Ok(())
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
        for level in &self.levels {
            for run in &level.runs {
                for &id in &run.files {
                    let table = SsTable::open(id, &self.layout.sstables(), &self.cache)?;
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

        for level in &self.levels {
            for run in &level.runs {
                for &id in &run.files {
                    let table = match SsTable::open(id, &self.layout.sstables(), &self.cache) {
                        Ok(table) => table,
                        Err(e) => {
                            sources.push(Box::new(std::iter::once(Err(read_err(e)))));
                            continue;
                        }
                    };
                    if !overlaps(&table.header().range, &start, &end) {
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

    /// On-disk levels, L0 first.
    pub fn levels(&self) -> &[Level] {
        &self.levels
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
