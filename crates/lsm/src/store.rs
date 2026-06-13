//! Read and write store traits, shared across the tree's layers.

use std::ops::RangeBounds;

use crate::error::{ReadError, WriteError};
use crate::key::InternalKey;

/// Read access, shared by every layer — the memtable and on-disk levels.
/// Reads are sequence-numbered to support point-in-time queries.
pub trait ReadStore {
    /// Latest value for `key` with `seq <= max_seq`; `None` if absent or the
    /// newest matching version is a tombstone.
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError>;

    /// Entries in `range` with `seq <= max_seq`, in `InternalKey` order
    /// (user key ascending, seq descending). Tombstones included; resolving
    /// versions is the caller's job.
    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_;
}

/// Write access — inserting versioned records. Only the memtable implements
/// it; levels are built by compaction, never written directly.
pub trait WriteStore {
    /// Insert a record (put or tombstone). Returns [`WriteError::StoreFull`]
    /// at capacity, signalling the caller to flush and retry.
    fn put(&mut self, key: InternalKey, value: &[u8]) -> Result<(), WriteError>;
}

/// The in-memory write buffer (memtable): readable, writable, and size-aware
/// so the engine can flush it on a size policy.
pub trait MemStore: ReadStore + WriteStore {
    /// Bytes currently buffered (keys plus values).
    fn size(&self) -> usize;

    /// Drop all entries and reset size to zero, after the buffer has been
    /// flushed to an on-disk level. Fallible because a durable memstore must
    /// also discard its journal here.
    fn clear(&mut self) -> Result<(), WriteError>;
}
