//! `filesystem` — strata-db's storage foundation: the virtual file system and
//! the primitives that sit on it. Everything between the backing file and the
//! rest of the engine lives here, exposed as a small set of nouns:
//!
//! - **`vfs`** ([`Vfs`], [`FileVfs`], [`MemVfs`], [`journal`]) — raw, fixed-size
//!   *block* I/O over a backing store, plus the redo [`journal`] that makes its
//!   writes durable. Nothing above it touches `std::fs`.
//! - **Codec** ([`Encode`], [`Decode`]) — the on-disk serialization vocabulary,
//!   shared by page types and the LSM. A sibling of the Vfs, not part of it.
//! - **`cache`** — the generic read-through [`Cache`] (memoizes immutable values,
//!   hands out owned handles) and the [`PageCache`] buffer pool (the mutable,
//!   pinned, journaled read/write path). Eviction [`policies`] live alongside.
//! - **`page`** ([`PageHeader`], …) — a block reinterpreted: a 21-byte
//!   self-describing header (magic, type, CRC32c) plus a typed payload.
//! - **`tuple`** ([`TuplePage`], [`Heap`], [`TupleLoc`]) — the tuple record
//!   layer: the slotted page format, the heap access method over it, and the
//!   address an index stores. The one place with database (record) semantics.
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
pub mod page;
mod tuple;
mod vfs;

// Virtual file system — raw block I/O plus the redo journal that makes its
// writes durable.
pub use vfs::journal::{PageJournal, PageOp};
pub use vfs::{BLOCK_SIZE, FileVfs, MemVfs, Vfs, journal};

// Codec — the on-disk serialization vocabulary (used by page types and the LSM).
pub use codec::{
    Decode, DecodeError, Encode, get_bytes, get_u8, get_u16, get_u32, get_u64, put_bytes, take,
};

// Caches & buffers — the read/write paths over a Vfs. Eviction policies live in
// their own namespace: `filesystem::policies::{EvictionPolicy, Lru, LruK, Clock, Lfu}`.
pub use cache::policies;
pub use cache::{Budget, Cache, PageCache, ReadPage, Weight, WritePage};

// Pages — typed views over a block.
pub use page::{HEADER_LEN, PAGE_SIZE, PageHeader, read_text, write_text};

// Tuples — the tuple page format, the heap over it, and the tuple address.
pub use tuple::{Heap, PageTuples, TupleLoc, TupleMut, TuplePage, TuplePageMut, TupleRef, TupleView};

// Errors.
pub use error::{PageError, Result};

/// Logical identity of a page — stable, unique, and never reused (see the
/// Storage Foundations design). Opaque: it encodes nothing about the page's
/// type, contents, or physical location. The `Vfs` resolves it to bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(pub u64);
