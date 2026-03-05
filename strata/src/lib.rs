pub mod engine;
pub mod level;
pub mod memstore;

pub use engine::StorageEngine;

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

    /// Return entries within the given user-key range where `seq <= max_seq`.
    ///
    /// For each user key, only the latest version with `seq <= max_seq` is returned.
    /// Tombstoned keys are excluded.
    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> Result<Vec<(InternalKey, Vec<u8>)>, ReadError>;
}

/// Errors returned by [`StorageEngine`] operations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error(transparent)]
    WriteError(WriteError),
    #[error("internal error: {0}")]
    InternalError(String),
}

impl From<WriteError> for StorageError {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Internal(msg) => StorageError::InternalError(msg),
            other => StorageError::WriteError(other),
        }
    }
}

impl From<ReadError> for StorageError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Internal(msg) => StorageError::InternalError(msg),
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::InternalError(e.to_string())
    }
}
