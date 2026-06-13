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
pub use iterator::{Scan, ScanRow};

// Re-export the lsm surface so dependents can keep importing storage types
// from a single crate (`strata_store::…`).
pub use lsm::{KVPair, LevelConfig, LsmConfig, LsmError, MergeIterator, ReadStore};

// Re-export the heap's view types so dependents can name engine read results.
pub use filesystem::{FileBlockStore, TupleRef, TupleView};

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
    /// A failure originating in the LSM building blocks (the index).
    #[error(transparent)]
    Lsm(LsmError),

    /// A failure in the page heap (the tuple store).
    #[error(transparent)]
    Block(filesystem::Error),

    /// The index and heap disagree: a key maps to a tuple location that is
    /// malformed or no longer present. Indicates corruption or a bug.
    #[error("storage corruption: {0}")]
    Corruption(String),

    /// A bounded resource ran out (buffer pool, memtable, or backing
    /// store) — every layer's exhaustion funnels here. A write/allocate
    /// failure only: reads never allocate, so they keep serving. (The
    /// journal is the exception — a journal-write failure is fail-stop;
    /// see [`filesystem::BlockJournal`].)
    #[error("storage exhausted: {0}")]
    Exhausted(String),
}

impl StorageError {
    /// Whether this is a bounded-resource exhaustion — writes fail with
    /// it while reads, which never allocate, keep serving.
    pub fn is_exhausted(&self) -> bool {
        matches!(self, StorageError::Exhausted(_))
    }
}

// Leaf errors funnel here, routing their exhaustion cases into the single
// `Exhausted` variant so callers recognize "ran out of room" uniformly.
impl From<LsmError> for StorageError {
    fn from(e: LsmError) -> Self {
        if e.is_exhausted() {
            StorageError::Exhausted(e.to_string())
        } else {
            StorageError::Lsm(e)
        }
    }
}

impl From<filesystem::Error> for StorageError {
    fn from(e: filesystem::Error) -> Self {
        if e.is_exhausted() {
            StorageError::Exhausted(e.to_string())
        } else {
            StorageError::Block(e)
        }
    }
}

// Convenience conversions so the engine can `?` the leaf LSM errors directly;
// each funnels through [`LsmError`] (and thus the exhaustion routing above).
impl From<WriteError> for StorageError {
    fn from(e: WriteError) -> Self {
        LsmError::from(e).into()
    }
}

impl From<ReadError> for StorageError {
    fn from(e: ReadError) -> Self {
        LsmError::from(e).into()
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        LsmError::from(e).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhaustion_funnels_into_one_variant() {
        // LSM memtable-full and filesystem pool-exhaustion both land in Exhausted.
        let from_lsm: StorageError = LsmError::from(WriteError::StoreFull).into();
        assert!(from_lsm.is_exhausted());
        let from_pager: StorageError = filesystem::Error::PoolExhausted(8).into();
        assert!(from_pager.is_exhausted());

        // A non-exhaustion error keeps its own variant.
        let internal: StorageError = LsmError::Internal("boom".into()).into();
        assert!(!internal.is_exhausted());
        assert!(matches!(internal, StorageError::Lsm(_)));
    }
}
