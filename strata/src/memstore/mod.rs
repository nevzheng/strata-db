use thiserror::Error;

/// A key-value pair of owned byte vectors.
pub type KVPair = (Vec<u8>, Vec<u8>);

/// Errors that can occur during write operations.
#[derive(Debug, Error)]
pub enum WriteError {
    /// Store has reached capacity and cannot accept writes.
    ///
    /// Flush the store to disk to reclaim space before retrying.
    #[error("store is full")]
    StoreFull,

    /// The caller provided invalid input.
    ///
    /// Fix the input and retry.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// An unexpected internal error occurred.
    ///
    /// This indicates a bug or unrecoverable state.
    /// A developer should investigate.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Errors that can occur during read operations.
#[derive(Debug, Error)]
pub enum ReadError {
    /// An unexpected internal error occurred.
    ///
    /// This indicates a bug or unrecoverable state.
    /// A developer should investigate.
    #[error("internal error: {0}")]
    Internal(String),
}

/// An interface for in-memory storage backends in an LSM tree.
///
/// A `MemStore` is a mutable, sorted, in-memory data structure that
/// buffers writes before they are flushed to disk as SSTables.
/// Implementations may use skip lists, B-trees, or other sorted
/// structures.
///
/// # What this does
///
/// - Stores key-value pairs in sorted byte order
/// - Supports point reads, range scans, inserts, and tombstone deletes
/// - Tracks its own size to signal when it should be flushed
///
/// # What this does not do
///
/// - Persistence — data lives only in memory
/// - Concurrency — this interface assumes single-threaded access
/// - Compression or encoding — keys and values are raw bytes
/// - Flushing — the caller is responsible for serializing to disk
///
/// # Design Notes
///
/// Keys and values are raw bytes (`&[u8]` / `Vec<u8>`). This keeps
/// the interface general — type-safe wrappers can be layered on top.
///
/// This API copies data on read and write. Methods like `get` and
/// `scan` return owned `Vec<u8>` values. This is simple and correct
/// but not zero-copy.
pub trait MemStore {
    /// Insert or update a key-value pair.
    ///
    /// Copies both key and value into the store.
    ///
    /// # Errors
    /// - `WriteError::StoreFull` — store has reached capacity
    /// - `WriteError::InvalidArgument` — key or value rejected by implementation
    /// - `WriteError::Internal` — unexpected error
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), WriteError>;

    /// Retrieve the value for a given key.
    ///
    /// Returns a copy of the value, or `None` if the key does not exist.
    ///
    /// # Errors
    /// - `ReadError::Internal` — unexpected error
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, ReadError>;

    /// Delete a key from the store.
    ///
    /// After deletion, `get` for this key should return `None`.
    /// How the deletion is represented internally is implementation-defined.
    ///
    /// # Errors
    /// - `WriteError::StoreFull` — deletion may require additional storage
    /// - `WriteError::InvalidArgument` — key rejected by implementation
    /// - `WriteError::Internal` — unexpected error
    fn delete(&mut self, key: &[u8]) -> Result<(), WriteError>;
    /// Return key-value pairs within the given range, sorted by key ascending.
    ///
    /// Both bounds are inclusive. Returns owned copies of each pair.
    ///
    /// # Errors
    /// - `ReadError::Internal` — unexpected error
    fn scan(&self, start: &[u8], end: &[u8]) -> Result<Vec<KVPair>, ReadError>;

    /// Current size in bytes of the store's contents.
    fn size(&self) -> usize;

    /// Whether the store has reached its capacity threshold.
    fn is_full(&self) -> bool;
}
