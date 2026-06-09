//! The Virtual File System — the boundary between the page system and raw
//! storage. It deals in fixed-size *blocks* of bytes and knows nothing about
//! page headers, types, or checksums; those belong to the page layer above.
//!
//! A `Vfs` also owns ID assignment: [`allocate`](Vfs::allocate) reserves a
//! block and hands back a fresh [`PageId`]. IDs are unique and never reused,
//! even across restarts — the high-water mark is persisted in the backing store.
//! (A dedicated `Sequencer` generalizing this is deferred; for v1 the VFS is
//! the allocator.)

mod file;
mod mem;

pub use file::FileVfs;
pub use mem::MemVfs;

use crate::{PageId, Result};

/// The size of a block — and therefore a page. A fixed system constant: the
/// addressing, the cache, and the VFS all assume it, so it is never encoded in
/// a page. 8 KiB matches PostgreSQL; the value is a tuning knob pending
/// benchmarks, not a format commitment.
pub const BLOCK_SIZE: usize = 8 * 1024;

/// Raw block storage. Reads and writes whole blocks by [`PageId`]; allocates
/// new blocks; makes writes durable. Implementations choose their own physical
/// addressing — the rest of the system only ever holds opaque `PageId`s.
pub trait Vfs {
    /// Reserve a new block and return its freshly-issued [`PageId`].
    fn allocate(&mut self) -> Result<PageId>;

    /// Recovery primitive: treat `id` as already allocated, growing the backing
    /// store and bumping the high-water mark past it if needed. Used when the
    /// journal replays a write to a page whose allocation never reached the
    /// superblock, so the id is not re-issued later.
    fn ensure_allocated(&mut self, id: PageId) -> Result<()>;

    /// Read the block for `id` into `buf`, which must be exactly
    /// [`BLOCK_SIZE`] bytes.
    fn read(&self, id: PageId, buf: &mut [u8]) -> Result<()>;

    /// Write `buf` (exactly [`BLOCK_SIZE`] bytes) to the block for `id`. Not
    /// durable until [`sync`](Vfs::sync) returns.
    fn write(&mut self, id: PageId, buf: &[u8]) -> Result<()>;

    /// Flush all prior writes (and the allocation high-water mark) to stable
    /// storage. This is the only durability point.
    fn sync(&mut self) -> Result<()>;

    /// The number of blocks ever allocated — the high-water mark, including any
    /// reserved bookkeeping block.
    fn block_count(&self) -> u64;
}
