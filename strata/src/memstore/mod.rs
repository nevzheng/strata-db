mod btree;
pub mod wal;
pub use btree::BTreeMapStore;

use std::cmp::Ordering;
use std::io::{self, Read, Write};
use std::ops::RangeBounds;

use thiserror::Error;

/// The type of operation recorded in an internal key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpType {
    Put = 0x01,
    Delete = 0x02,
}

impl OpType {
    fn from_u8(b: u8) -> io::Result<Self> {
        match b {
            0x01 => Ok(OpType::Put),
            0x02 => Ok(OpType::Delete),
            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown op type: {:#x}", unknown),
            )),
        }
    }
}

/// A versioned internal key used by the memstore layer.
///
/// Ordering: user key ascending, then sequence number descending.
/// The operation type is not included in the ordering.
///
/// Wire format (all integers big-endian):
///
/// `| key_len (2B) | key | seq (8B) | op (1B) |`
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InternalKey {
    pub key: Vec<u8>,
    pub seq: u64,
    pub op: OpType,
}

impl InternalKey {
    /// Encode this internal key to the writer.
    pub fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&(self.key.len() as u16).to_be_bytes())?;
        w.write_all(&self.key)?;
        w.write_all(&self.seq.to_be_bytes())?;
        w.write_all(&[self.op as u8])?;
        Ok(())
    }

    /// Decode an internal key from the reader.
    pub fn decode(r: &mut impl Read) -> io::Result<Self> {
        let mut key_len_buf = [0u8; 2];
        r.read_exact(&mut key_len_buf)?;
        let key_len = u16::from_be_bytes(key_len_buf) as usize;

        let mut key = vec![0u8; key_len];
        r.read_exact(&mut key)?;

        let mut seq_buf = [0u8; 8];
        r.read_exact(&mut seq_buf)?;
        let seq = u64::from_be_bytes(seq_buf);

        let mut op_buf = [0u8; 1];
        r.read_exact(&mut op_buf)?;
        let op = OpType::from_u8(op_buf[0])?;

        Ok(Self { key, seq, op })
    }
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then(self.seq.cmp(&other.seq).reverse())
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

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
/// - Supports range scans, inserts, and tombstone deletes
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
    /// Insert a key-value pair or tombstone.
    ///
    /// The [`InternalKey::op`] field determines the operation type:
    /// - [`OpType::Put`] — stores the value
    /// - [`OpType::Delete`] — stores a tombstone (value should be empty)
    ///
    /// # Errors
    /// - `WriteError::StoreFull` — store has reached capacity
    /// - `WriteError::InvalidArgument` — key or value rejected by implementation
    /// - `WriteError::Internal` — unexpected error
    fn put(&mut self, key: InternalKey, value: &[u8]) -> Result<(), WriteError>;

    /// Return all entries within the given user-key range, sorted by
    /// `InternalKey` order (user key ascending, seq descending).
    ///
    /// Returns all versions of each key, including tombstones.
    /// Version resolution is the caller's responsibility.
    ///
    /// # Errors
    /// - `ReadError::Internal` — unexpected error
    fn scan(
        &self,
        range: impl RangeBounds<Vec<u8>>,
    ) -> Result<Vec<(InternalKey, Vec<u8>)>, ReadError>;

    /// Current size in bytes of keys and values in the store.
    fn size(&self) -> usize;

    /// Whether the store has reached its capacity threshold.
    fn is_full(&self) -> bool;

    /// Whether an entry with the given key and value size would fit.
    fn fits(&self, key: &InternalKey, value_len: usize) -> bool;

    /// Remove all entries from the store, resetting its size to zero.
    fn clear(&mut self);
}
