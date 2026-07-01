//! `filesystem` — strata-db's storage foundation: pure storage *mechanism*
//! (store and fetch bytes), never query *policy*. Everything between the backing
//! file and the rest of the engine lives here, organized as a small set of
//! capability modules. Reach for the one that matches what you're doing:
//!
//! - [`memory`] — [`MemoryPool`] hands out [`Slab`]s (raw, owned byte spans)
//!   under one global cap. The **execution engine** asks for a `Slab`, imposes a
//!   view on it (scratch space, a hash table, a heap), and drops it when done.
//! - [`tuple`] — the record layer: the [`Heap`] access method (open one with
//!   [`Heap::open`](tuple::Heap::open) — it owns its block store and page cache
//!   internally), slotted [`TuplePage`]s, and the [`TupleLoc`] an index stores.
//!   The **storage engine** works with tuples here.
//! - [`codec`] — the [`Encode`]/[`Decode`] vocabulary. Exposed for whoever owns
//!   an on-disk format (page types, the LSM); reach for it *when* you serialize.
//! - [`cache`] — the generic read-through [`Cache`] (immutable memo, owned
//!   handles) and the [`PageCache`] buffer pool (mutable, pinned, journaled).
//!   Eviction [`policies`](cache::policies) live alongside. The LSM shares the
//!   `Cache`; the heap is built on the `PageCache`.
//! - [`block`] — the [`BlockStore`] device (fixed-size block I/O over a file or
//!   memory) and its redo [`BlockJournal`]. The bedrock the caches sit on.
//! - [`page`] — a block reinterpreted as a self-describing typed page.
//!
//! `cache`, `block`, and `page` are plumbing the first three sit on — public so
//! consumers can wire them, but not the usual entry point. [`MemoryPool`] /
//! [`Slab`] are the memory primitive (once sketched as `Buffer`); a `ScanBuffer`
//! adapter and wiring the caches onto the pool are the next steps.
//!
//! The v1 caches are single-threaded (`Rc`/`RefCell`); concurrency is deferred.

pub mod block;
pub mod cache;
pub mod codec;
mod error;
mod memory;
pub mod page;
pub mod tuple;
pub mod workspace;

// Block storage — raw block I/O plus the redo journal that makes its writes
// durable.
pub use block::journal::{BlockJournal, JournalOp};
pub use block::{
    BLOCK_ALIGN, BLOCK_SIZE, Block, BlockStore, DiskBlockStore, MemBlockStore, journal,
};

// Memory — the allocator facade and the raw memory unit it hands out. The
// module is interior; consumers reach the types through these re-exports.
pub use memory::{MemoryPool, OutOfMemory, Slab};

// Workspace — bounded, journal-less scratch tuple storage (memory or file
// spill), behind the `Workspace` trait.
pub use workspace::{
    FileWorkspace, FileWorkspaceTuples, MemoryWorkspace, TupleBytes, Workspace, WorkspaceBlock,
    WorkspaceLoc,
};

// Codec — the on-disk serialization vocabulary (used by page types and the LSM).
pub use codec::{
    Decode, DecodeError, Encode, get_bytes, get_u8, get_u16, get_u32, get_u64, put_bytes, take,
};

// Caches & buffers — the read/write paths over a BlockStore. Eviction policies live in
// their own namespace: `filesystem::policies::{EvictionPolicy, Lru, LruK, Clock, Lfu}`.
pub use cache::policies;
pub use cache::{Budget, Cache, PageCache, ReadPage, Weight, WritePage};

// Pages — typed views over a block.
pub use page::{HEADER_LEN, PAGE_SIZE, PageHeader, read_text, write_text};

// Tuples — the tuple page format, the heap over it, and the tuple address.
pub use tuple::{
    Heap, PageTuples, TupleLoc, TupleMut, TuplePage, TuplePageMut, TupleRef, TupleView,
};

// Errors.
pub use error::{Error, Result};

/// Logical identity of a page — stable, unique, and never reused (see the
/// Storage Foundations design). Opaque: it encodes nothing about the page's
/// type, contents, or physical location. The `BlockStore` resolves it to bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);
