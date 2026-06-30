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

mod direct;
mod file;
pub mod journal;
mod mem;

pub use direct::DirectFile;
pub use file::FileBlockStore;
pub use mem::MemBlockStore;

use crate::{BlockId, Result};

/// The size of a block — and therefore a page. A fixed system constant: the
/// addressing, the cache, and the block store all assume it, so it is never encoded in
/// a page. 8 KiB matches PostgreSQL; the value is a tuning knob pending
/// benchmarks, not a format commitment.
pub const BLOCK_SIZE: usize = 8 * 1024;

/// A fixed-size, page-aligned block of [`BLOCK_SIZE`] bytes. The alignment
/// guarantees a direct-I/O DMA target — no bounce buffer is needed.
///
/// Derefs to `[u8]` so all existing byte-slice code (page headers, checksums,
/// tuple pages) works unchanged.
#[repr(align(4096))]
pub struct Block([u8; BLOCK_SIZE]);

impl Block {
    /// A zero-filled block.
    pub fn zeroed() -> Self {
        Self([0u8; BLOCK_SIZE])
    }

    /// The base pointer, for alignment checks.
    pub fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }
}

impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Block")
            .field(&format_args!("[{} bytes]", self.0.len()))
            .finish()
    }
}

impl Clone for Block {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Default for Block {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Block {}

impl std::ops::Deref for Block {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for Block {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

/// Raw block storage. Reads and writes whole [`Block`]s by [`BlockId`];
/// allocates new blocks; makes writes durable. Implementations choose their
/// own physical addressing — the rest of the system only ever holds opaque
/// `BlockId`s.
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

    /// Read the block for `id` into `block`. The block's size and alignment are
    /// fixed by the type — no runtime size check is needed.
    fn read(&self, id: BlockId, block: &mut Block) -> Result<()>;

    /// Write `block` to the block for `id`. Not durable until
    /// [`sync`](BlockStore::sync) returns.
    fn write(&mut self, id: BlockId, block: &Block) -> Result<()>;

    /// Flush all prior writes (and the allocation high-water mark) to stable
    /// storage. This is the only durability point.
    fn sync(&mut self) -> Result<()>;

    /// The number of blocks ever allocated — the high-water mark, including any
    /// reserved bookkeeping block.
    fn block_count(&self) -> u64;
}
