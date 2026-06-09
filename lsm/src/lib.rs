//! `lsm` — a log-structured merge tree.
//!
//! The crate root holds the top-level data hierarchy; the supporting
//! concepts live in their own modules and are re-exported here.
//!
//! ```text
//! KeyValue → Run → SsTableRef → Level → Lsm
//! ```
//!
//! Every node above the leaf carries a [`KeyRange`] and a [`BloomFilter`]
//! so a lookup or scan can skip a subtree that can't hold the key.

mod bloom;
mod config;
mod error;
mod iterator;
mod key;
mod memstore;
mod storage;
mod store;

pub use bloom::BloomFilter;
pub use config::{
    BloomConfig, CachePolicy, LevelConfig, LsmConfig, PageCacheConfig, PageConfig, RunConfig,
    SizeConfig, TableConfig,
};
pub use error::{LsmError, ReadError, WriteError};
pub use iterator::{KvStream, MergeIterator, Scan, ScanIterator};
pub use key::{InternalKey, KVPair, KeyRange, KeyValue, OpType};
pub use memstore::BTreeMemtable;
pub use storage::{
    DataBlock, Decode, DecodeError, Encode, Header, Page, PageId, SsTable, SstPageCache,
};
pub use store::{MemStore, ReadStore, WriteStore};

use std::ops::RangeBounds;

/// Globally-unique identity of an SSTable file. Shared by [`SsTableRef`]
/// (the logical handle) and [`PageId`] (the page-cache key), so the logical
/// and storage layers name the same file the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SsTableId(pub u64);

/// A reference to an on-disk SSTable: its header (id, range, bloom, size),
/// not its data. Cheap to hold; enough to decide whether to read the file.
#[derive(Debug, Clone)]
pub struct SsTableRef {
    pub id: SsTableId,
    pub range: KeyRange,
    pub bloom: BloomFilter,
    pub size_bytes: u64,
}

impl SsTableRef {
    /// The page-cache key for this table's page `index`.
    pub fn page(&self, index: u32) -> PageId {
        PageId {
            table: self.id,
            page_index: index,
        }
    }
}

/// A sorted run: KV pairs with no key repeated. Stored as SSTable file(s) —
/// one per run today (splitting a large run across files isn't implemented
/// yet), so `files` is a list but currently holds a single [`SsTableRef`].
#[derive(Debug, Clone)]
pub struct Run {
    pub files: Vec<SsTableRef>,
    pub range: KeyRange,
    pub bloom: BloomFilter, // covers every key in the run
    pub size_bytes: u64,
}

/// A level: one or more runs.
///
/// L0 holds multiple runs that *overlap* — one per memtable flush, sharing
/// keys at newer seqs — so it's read newest-first. Leveled compaction keeps
/// L1+ at a single run, so those don't overlap.
#[derive(Debug, Clone)]
pub struct Level {
    pub runs: Vec<Run>,
    pub range: KeyRange,
    pub bloom: BloomFilter, // covers every key in the level
}

/// The LSM tree — the live store you set and read values on.
///
/// Writes land in the in-memory `mem` buffer; reads check it first, then the
/// on-disk `levels` (newest first). Generic over the memtable so it can be
/// swapped (e.g. for a skip list).
pub struct Lsm<M: MemStore = BTreeMemtable> {
    config: LsmConfig,
    mem: M,
    levels: Vec<Level>,
    seq: u64,
}

impl<M: MemStore + Default> Lsm<M> {
    /// Create an empty tree with the given configuration.
    pub fn new(config: LsmConfig) -> Self {
        Self {
            config,
            mem: M::default(),
            levels: Vec::new(),
            seq: 0,
        }
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
        self.seq += 1;
        let ikey = InternalKey {
            user_key: key.to_vec(),
            seq: self.seq,
            op,
        };
        self.mem.put(ikey, value)?;
        Ok(())
    }

    /// Latest value for `key`, or `None` if absent or deleted.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, LsmError> {
        // The memtable holds the newest versions; on-disk levels are searched
        // after it once flushing is in place.
        Ok(self.mem.get_at(key, u64::MAX)?)
    }

    /// Scan a key range, yielding the newest version of each key in order,
    /// with tombstones dropped.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> ScanIterator<'_> {
        self.scan_at(range, self.seq)
    }

    /// Scan as of `max_seq` (point-in-time).
    pub fn scan_at(&self, range: impl RangeBounds<Vec<u8>>, max_seq: u64) -> ScanIterator<'_> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();

        // One source today — the memtable; on-disk level streams merge into
        // this same `Vec` once flushing populates them.
        let mem: KvStream<'_> = Box::new(
            self.mem
                .scan_at((start, end), max_seq)
                .map(|r| r.map(|(key, value)| KeyValue { key, value })),
        );

        let merged = MergeIterator::new(vec![mem], |a, b| match (a, b) {
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
