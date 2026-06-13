//! `pager` — strata-db's storage foundation: the virtual file system and the
//! primitives that sit on it. Everything between the backing file and the rest
//! of the engine lives here, exposed as a small set of nouns:
//!
//! - **Vfs** ([`Vfs`], [`FileVfs`], [`MemVfs`]) — raw, fixed-size *block* I/O
//!   over a backing store. Nothing above it touches `std::fs`. Knows nothing
//!   about page headers, types, or encoding.
//! - **Codec** ([`Encode`], [`Decode`]) — the on-disk serialization vocabulary,
//!   shared by page types and the LSM. A sibling of the Vfs, not part of it.
//! - **Caches** — the generic read-through [`Cache`] (memoizes immutable values,
//!   hands out owned handles) and the [`PageCache`] buffer pool (the mutable,
//!   pinned, journaled read/write path for the heap).
//! - **Policies** ([`Lru`], [`LruK`], [`Clock`], [`Lfu`]) — pluggable
//!   [`EvictionPolicy`], shared by both caches.
//! - **Pages** ([`PageHeader`], [`TuplePage`], …) — a block reinterpreted: a
//!   21-byte self-describing header (magic, type, CRC32c) plus a typed payload.
//! - **Heap** ([`Heap`]) — tuple storage over the buffer pool.
//!
//! The read-through [`Cache`] is the substrate both the engine and the LSM share
//! for immutable blocks; the [`PageCache`] buffer pool stays the mutable-page
//! path. A `Buffer` primitive (memory/placement) and a `ScanBuffer` are the next
//! refactor — both will sit under these caches.
//!
//! The v1 caches are single-threaded (`Rc`/`RefCell`); concurrency is deferred.

mod cache;
mod codec;
mod error;
mod heap;
pub mod journal;
mod loc;
pub mod page;
mod policies;
mod vfs;

// Virtual file system — raw, fixed-size block I/O over a backing store.
pub use vfs::{BLOCK_SIZE, FileVfs, MemVfs, Vfs};

// Codec — the on-disk serialization vocabulary (used by page types and the LSM).
pub use codec::{
    Decode, DecodeError, Encode, get_bytes, get_u8, get_u16, get_u32, get_u64, put_bytes, take,
};

// Caches & buffers — the read/write paths over a Vfs.
pub use cache::{Budget, Cache, PageCache, ReadPage, Weight, WritePage};

// Eviction policies — pluggable, shared by the buffer pool and the read-through cache.
pub use policies::{Clock, EvictionPolicy, Lfu, Lru, LruK};

// Pages — typed views over a block.
pub use page::{HEADER_LEN, PAGE_SIZE, PageHeader, TuplePage, TuplePageMut, read_text, write_text};

// Heap — tuple storage over the cache.
pub use heap::{Heap, PageTuples, TupleMut, TupleRef, TupleView};

// Journal, errors, and ids.
pub use error::{PageError, Result};
pub use journal::{PageJournal, PageOp};
pub use loc::TupleLoc;

/// Logical identity of a page — stable, unique, and never reused (see the
/// Storage Foundations design). Opaque: it encodes nothing about the page's
/// type, contents, or physical location. The `Vfs` resolves it to bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(pub u64);
