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

mod disk;
pub mod journal;
mod mem;

pub use disk::DiskBlockStore;
pub use mem::MemBlockStore;

use crate::{BlockId, Result};

/// The size of a block — and therefore a page. A fixed system constant: the
/// addressing, the cache, and the block store all assume it, so it is never encoded in
/// a page. 8 KiB matches PostgreSQL; the value is a tuning knob pending
/// benchmarks, not a format commitment.
pub const BLOCK_SIZE: usize = 8 * 1024;

/// Alignment (bytes) a [`Block`] guarantees. 4 KiB covers every mainstream
/// device's logical *and* physical sector size, which is what direct I/O
/// requires of both the buffer address and the transfer offset/length. Our
/// [`BLOCK_SIZE`] is a multiple of it, so block-granular I/O is always aligned.
pub const BLOCK_ALIGN: usize = 4096;

/// A [`BLOCK_SIZE`]-byte buffer aligned to [`BLOCK_ALIGN`] — the unit of I/O
/// between the cache and a [`BlockStore`].
///
/// The alignment exists so the buffer is a legal direct-I/O target: `O_DIRECT`
/// (Linux) and `F_NOCACHE` (macOS) demand a sector-aligned address, and this
/// type carries that invariant in its layout instead of asking every call site
/// to prove it. `Deref<Target = [u8]>` keeps the byte-slice code above
/// (headers, checksums, tuple pages) working unchanged.
#[repr(align(4096))]
#[derive(Clone, PartialEq, Eq)]
pub struct Block([u8; BLOCK_SIZE]);

impl Block {
    /// A zero-filled block.
    pub fn zeroed() -> Self {
        Self([0u8; BLOCK_SIZE])
    }
}

impl Default for Block {
    fn default() -> Self {
        Self::zeroed()
    }
}

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

impl AsRef<[u8]> for Block {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AsMut<[u8]> for Block {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

// The array's own Debug would dump 8 KiB; a size summary is all a block is.
impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Block([{BLOCK_SIZE} bytes])")
    }
}

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

    /// Read the block for `id` into `block`. Its size and alignment are fixed
    /// by the [`Block`] type, so there is no length to check.
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
