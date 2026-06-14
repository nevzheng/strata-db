//! Errors for the page system.

use crate::BlockId;
use thiserror::Error;

/// Anything that can go wrong reading, writing, or interpreting a page.
#[derive(Debug, Error)]
pub enum Error {
    /// A failure in the backing store (the `BlockStore`).
    #[error("block i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A page read back from disk failed CRC32c verification — it is corrupt or
    /// was torn by a crash mid-write.
    #[error("page {0:?} failed checksum verification (corrupt or torn)")]
    Checksum(BlockId),

    /// The bytes do not begin with the `STDB` magic — not a strata-db page.
    #[error("not a strata-db page: bad magic")]
    BadMagic,

    /// The page header version is one this build does not understand.
    #[error("unsupported page header version {0}")]
    BadHeaderVersion(u8),

    /// A page-type handler was pointed at a page of a different type.
    #[error("expected page type {expected}, found {got}")]
    BadPageType { expected: u16, got: u16 },

    /// Every frame in the pool is pinned, so no page can be loaded. The caller
    /// is holding too many pages at once for the configured pool size.
    #[error("page cache exhausted: all {0} frames are pinned")]
    PoolExhausted(usize),

    /// A latch conflict: the page is already held in an incompatible mode
    /// (a writer excludes all others; a reader excludes writers).
    #[error("page {0:?} is locked by another handle")]
    Busy(BlockId),

    /// A block buffer handed to the `BlockStore` was not exactly [`BLOCK_SIZE`] bytes.
    ///
    /// [`BLOCK_SIZE`]: crate::BLOCK_SIZE
    #[error("block buffer must be {expected} bytes, got {got}")]
    BadBlockSize { expected: usize, got: usize },

    /// A tuple is larger than an (empty) page can hold; it cannot live in the
    /// heap and needs out-of-line storage.
    #[error("tuple of {len} bytes exceeds the {max}-byte page capacity")]
    TupleTooLarge { len: usize, max: usize },

    /// A `TEXT` page chain did not decode to valid UTF-8.
    #[error("text value is not valid utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    /// A failure in the page journal (append, replay, or recovery).
    #[error("journal error: {0}")]
    Journal(#[from] ::journal::JournalError),

    /// The inline free list outgrew the superblock. It needs to spill to
    /// dedicated free-space-map pages — future work; today the list is
    /// empty (nothing frees pages yet) so this is unreachable.
    #[error("free list of {len} ids overflows the superblock (max {max})")]
    FreeListOverflow { len: usize, max: usize },

    /// The backing store could not grow — out of space (`ENOSPC`). A
    /// write/allocate failure; reads never allocate, so they continue.
    /// See also [`PoolExhausted`](Self::PoolExhausted) (the in-RAM
    /// counterpart) and [`is_exhausted`](Self::is_exhausted).
    #[error("backing store exhausted: {0}")]
    Exhausted(String),

    /// A workspace hit its size bound — an append needed `requested` more bytes
    /// but only `capacity - used` were left. Workspaces are always bounded, so
    /// this is the expected back-pressure signal (memory or disk), not a bug.
    #[error("workspace full: need {requested} more bytes, {used}/{capacity} used")]
    WorkspaceFull {
        requested: usize,
        used: usize,
        capacity: usize,
    },
}

impl Error {
    /// Whether this is a bounded-resource exhaustion — the buffer pool
    /// (RAM) or the backing store (disk) ran out. These arise only on the
    /// allocate/write path; the layer above maps them to a single
    /// resource-exhausted error so a write fails cleanly while reads,
    /// which never allocate, keep serving.
    pub fn is_exhausted(&self) -> bool {
        matches!(
            self,
            Error::PoolExhausted(_) | Error::Exhausted(_) | Error::WorkspaceFull { .. }
        )
    }
}

/// Result alias for the crate.
pub type Result<T> = std::result::Result<T, Error>;
