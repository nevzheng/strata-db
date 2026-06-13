//! Error types for the LSM library.

use thiserror::Error;

/// `errno` for "no space left on device" — same value on Linux, macOS, and BSD.
const ENOSPC: i32 = 28;

/// The unified error type for the library. Leaf errors funnel into it.
#[derive(Debug, Error)]
pub enum LsmError {
    #[error(transparent)]
    Write(WriteError),
    /// A bounded resource ran out — the memtable filled or the backing
    /// store could not grow (`ENOSPC`). A write-path failure; reads never
    /// allocate, so they continue. See [`is_exhausted`](Self::is_exhausted).
    #[error("lsm exhausted: {0}")]
    Exhausted(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl LsmError {
    /// Whether this is a bounded-resource exhaustion (full memtable or
    /// backing store), so the layer above can funnel it into one
    /// resource-exhausted error. Only arises on the write path.
    pub fn is_exhausted(&self) -> bool {
        matches!(
            self,
            LsmError::Exhausted(_) | LsmError::Write(WriteError::StoreFull)
        )
    }
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
        // A full disk is resource exhaustion, not an internal fault.
        if e.raw_os_error() == Some(ENOSPC) {
            LsmError::Exhausted(format!("backing store full: {e}"))
        } else {
            LsmError::Internal(e.to_string())
        }
    }
}

impl From<journal::JournalError> for LsmError {
    fn from(e: journal::JournalError) -> Self {
        LsmError::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_exhaustion() {
        assert!(LsmError::from(WriteError::StoreFull).is_exhausted());
        assert!(LsmError::Exhausted("x".into()).is_exhausted());
        assert!(!LsmError::Internal("x".into()).is_exhausted());
        // A full disk (ENOSPC) is exhaustion; other I/O is not.
        assert!(LsmError::from(std::io::Error::from_raw_os_error(ENOSPC)).is_exhausted());
        assert!(!LsmError::from(std::io::Error::from_raw_os_error(13)).is_exhausted());
    }
}
