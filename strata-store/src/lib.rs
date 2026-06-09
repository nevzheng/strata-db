//! Strata's storage engine.
//!
//! [`StorageEngine`] is a thin wrapper over the [`lsm`] crate's [`Lsm`] tree:
//! writes land in the memtable, [`flush`](StorageEngine::flush) seals it into
//! an on-disk L0 SSTable, and reads merge the memtable with the on-disk levels
//! through the tree's page cache. This crate is the composition point where
//! higher-level storage concerns are integrated over time.
//!
//! Not yet ported from the previous engine: a write-ahead log and a manifest,
//! so the tree currently has no cross-restart recovery — only data that has
//! been flushed and is re-discovered in-process survives. Compaction is also
//! pending; every flush adds a new L0 run.

pub mod engine;
pub mod iterator;

pub use engine::StorageEngine;
pub use iterator::ScanIterator;

// Re-export the lsm surface so dependents can keep importing storage types
// from a single crate (`strata_store::…`).
pub use lsm::{KVPair, LevelConfig, LsmConfig, LsmError, MergeIterator, ReadStore};

/// The memtable types, kept under a `memstore` path for dependents.
pub mod memstore {
    pub use lsm::BTreeMemtable as BTreeMapStore;
    pub use lsm::{MemStore, ReadStore, WriteStore};
}

use lsm::{ReadError, WriteError};
use thiserror::Error;

/// Errors returned by [`StorageEngine`] operations.
///
/// Anything that goes wrong in the underlying LSM tree bubbles up here as
/// [`StorageError::Lsm`]. Future engine-level concerns (transactions, catalog
/// integration, …) add their own variants alongside it.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A failure originating in the LSM building blocks.
    #[error(transparent)]
    Lsm(#[from] LsmError),
}

// Convenience conversions so the engine can `?` the leaf LSM errors directly;
// each funnels through [`LsmError`].
impl From<WriteError> for StorageError {
    fn from(e: WriteError) -> Self {
        StorageError::Lsm(e.into())
    }
}

impl From<ReadError> for StorageError {
    fn from(e: ReadError) -> Self {
        StorageError::Lsm(e.into())
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Lsm(e.into())
    }
}
