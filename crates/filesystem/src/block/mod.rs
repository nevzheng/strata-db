//! The block storage — the boundary between the page system and raw
//! storage. It deals in fixed-size *blocks* of bytes and knows nothing about
//! page headers, types, or checksums; those belong to the page layer above.
//!
//! A `BlockStore` also owns ID assignment: [`allocate`](BlockStore::allocate) reserves a
//! block and hands back a [`BlockId`] — reusing one from the free list if
//! available, otherwise a fresh id past the high-water mark (both persisted in
//! the backing store). [`free`](BlockStore::free) returns a block for reuse. A fresh
//! id is never reused while live; a freed one may be, so only [`free`](BlockStore::free)
//! a page that holds no referenced data. (A dedicated `Sequencer` generalizing
//! this is deferred; for v1 the block store is the allocator.)

mod file;
pub mod journal;
mod mem;

pub use file::FileBlockStore;
pub use mem::MemBlockStore;

use crate::{BlockId, Result};

/// The size of a block — and therefore a page. A fixed system constant: the
/// addressing, the cache, and the block store all assume it, so it is never encoded in
/// a page. 8 KiB matches PostgreSQL; the value is a tuning knob pending
/// benchmarks, not a format commitment.
pub const BLOCK_SIZE: usize = 8 * 1024;

/// Raw block storage. Reads and writes whole blocks by [`BlockId`]; allocates
/// new blocks; makes writes durable. Implementations choose their own physical
/// addressing — the rest of the system only ever holds opaque `BlockId`s.
pub trait BlockStore {
    /// Reserve a block and return its [`BlockId`] — a reused one from the free
    /// list, or a fresh id (growing the store) when the list is empty.
    fn allocate(&mut self) -> Result<BlockId>;

    /// Return `id` to the free list for reuse by a later [`allocate`](BlockStore::allocate).
    ///
    /// **Safety of reuse:** the block must hold no data anything still
    /// references — a later allocation will hand the id out and overwrite it.
    /// In the heap, that means every tuple on the page is dead and no index
    /// entry points at it. Durable once [`sync`](BlockStore::sync) persists.
    fn free(&mut self, id: BlockId);

    /// Recovery primitive: treat `id` as already allocated, growing the backing
    /// store and bumping the high-water mark past it if needed. Used when the
    /// journal replays a write to a page whose allocation never reached the
    /// superblock, so the id is not re-issued later.
    fn ensure_allocated(&mut self, id: BlockId) -> Result<()>;

    /// Read the block for `id` into `buf`, which must be exactly
    /// [`BLOCK_SIZE`] bytes.
    fn read(&self, id: BlockId, buf: &mut [u8]) -> Result<()>;

    /// Write `buf` (exactly [`BLOCK_SIZE`] bytes) to the block for `id`. Not
    /// durable until [`sync`](BlockStore::sync) returns.
    fn write(&mut self, id: BlockId, buf: &[u8]) -> Result<()>;

    /// Flush all prior writes (and the allocation high-water mark) to stable
    /// storage. This is the only durability point.
    fn sync(&mut self) -> Result<()>;

    /// The number of blocks ever allocated — the high-water mark, including any
    /// reserved bookkeeping block.
    fn block_count(&self) -> u64;
}
