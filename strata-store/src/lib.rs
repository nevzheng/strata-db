//! Strata's storage engine.
//!
//! [`StorageEngine`] coordinates the reusable LSM building blocks from
//! the [`lsm`] crate — the write-ahead log, in-memory store, and on-disk
//! levels — into a single read/write store. This crate is the
//! composition point where higher-level storage subcrates are
//! integrated over time.

pub mod engine;
pub mod iterator;

pub use engine::StorageEngine;
pub use iterator::ScanIterator;

// Re-export the lsm surface so dependents can keep importing storage
// types from a single crate (`strata_store::…`).
pub use lsm::memstore;
pub use lsm::{KVPair, LevelConfig, LsmError, MergeIterator, ReadStore};

use lsm::memstore::{ReadError, WriteError};
use thiserror::Error;

/// Errors returned by [`StorageEngine`] operations.
///
/// Anything that goes wrong in the underlying LSM tree bubbles up here
/// as [`StorageError::Lsm`]. Future engine-level concerns (transactions,
/// catalog integration, …) add their own variants alongside it.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A failure originating in the LSM building blocks.
    #[error(transparent)]
    Lsm(#[from] LsmError),
}

// Convenience conversions so the engine can `?` the leaf LSM errors
// directly; each funnels through [`LsmError`].
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
