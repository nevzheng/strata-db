//! Error types for the LSM library.

use thiserror::Error;

/// The unified error type for the library. Leaf errors funnel into it.
#[derive(Debug, Error)]
pub enum LsmError {
    #[error(transparent)]
    Write(WriteError),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Errors from the write path (memtable inserts).
#[derive(Debug, Error)]
pub enum WriteError {
    #[error("store is full")]
    StoreFull,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Errors from the read path.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<WriteError> for LsmError {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Internal(msg) => LsmError::Internal(msg),
            other => LsmError::Write(other),
        }
    }
}

impl From<ReadError> for LsmError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Internal(msg) => LsmError::Internal(msg),
        }
    }
}

impl From<std::io::Error> for LsmError {
    fn from(e: std::io::Error) -> Self {
        LsmError::Internal(e.to_string())
    }
}
