pub mod engine;
pub mod level;
pub mod memstore;

pub use engine::StorageEngine;

use memstore::{ReadError, WriteError};
use thiserror::Error;

/// A resolved key-value pair of owned byte vectors.
pub type KVPair = (Vec<u8>, Vec<u8>);

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
