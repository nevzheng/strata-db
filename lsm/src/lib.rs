//! A basic, configurable log-structured merge-tree library.
//!
//! Provides the reusable building blocks of an LSM tree — an in-memory
//! store ([`memstore`]) backed by a write-ahead log, on-disk sorted
//! runs organized into [`level`]s, and merge [`iterator`]s — plus the
//! [`ReadStore`] trait that ties them together. The coordinating engine
//! lives in the consuming crate.

pub mod iterator;
pub mod level;
pub mod memstore;

pub use iterator::MergeIterator;
pub use level::LevelConfig;

use std::ops::RangeBounds;

use memstore::{InternalKey, ReadError, WriteError};
use thiserror::Error;

/// A resolved key-value pair of owned byte vectors.
pub type KVPair = (Vec<u8>, Vec<u8>);

/// Read interface shared by memstores and levels.
///
/// All reads require an explicit sequence number to support
/// point-in-time queries. The engine provides convenience methods
/// (`get`, `scan`) that pass the current sequence number.
pub trait ReadStore {
    /// Retrieve the value for a given user key at a specific sequence number.
    ///
    /// Returns the most recent version with `seq <= max_seq`.
    /// Returns `None` if no such entry exists or the matching entry is a
    /// tombstone.
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError>;

    /// Return all entries within the given user-key range where `seq <= max_seq`,
    /// sorted by `InternalKey` order (user key ascending, seq descending).
    ///
    /// Returns all versions of each key, including tombstones.
    /// Version resolution is the caller's responsibility.
    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_;
}

/// The unified error type for the LSM library.
///
/// Every fallible operation across the building blocks funnels into
/// this type; the leaf errors ([`WriteError`], [`ReadError`], and I/O
/// failures) convert into it. Consuming crates typically wrap it in
/// their own engine-level error.
#[derive(Debug, Error)]
pub enum LsmError {
    #[error(transparent)]
    WriteError(WriteError),
    #[error("internal error: {0}")]
    InternalError(String),
}

impl From<WriteError> for LsmError {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Internal(msg) => LsmError::InternalError(msg),
            other => LsmError::WriteError(other),
        }
    }
}

impl From<ReadError> for LsmError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Internal(msg) => LsmError::InternalError(msg),
        }
    }
}

impl From<std::io::Error> for LsmError {
    fn from(e: std::io::Error) -> Self {
        LsmError::InternalError(e.to_string())
    }
}
