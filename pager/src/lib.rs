//! `pager` — strata-db's page-based storage foundation.
//!
//! The pager is the layer between the backing file and the rest of the engine:
//! it owns pages and the buffer cache — the role SQLite's "pager" plays.
//!
//! Three layers, lowest first:
//!
//! - [`vfs`] — the **Virtual File System** ([`Vfs`]). It reads and writes raw,
//!   fixed-size *blocks* and owns the logical→physical mapping. Nothing above it
//!   touches `std::fs`. [`FileVfs`] backs blocks with a local file; [`MemVfs`]
//!   keeps them in memory (tests, ephemeral stores).
//! - [`page`] — **Pages**. A page is a block reinterpreted: a 21-byte
//!   self-describing [`PageHeader`] (magic, type, CRC32c) plus a payload. Page
//!   *types* ([`TuplePage`](page::TuplePage), [`TextPage`](page::text)) layer
//!   structure onto the payload.
//! - [`cache`] — the **Page Cache** ([`PageCache`]): a fixed pool of frames over
//!   a `Vfs`, with pinning, dirty writeback, and LRU-K eviction. It is the read
//!   *and write* path everything above the VFS goes through.
//!
//! This is a distinct component from `lsm`'s internal `SstPageCache`, which only
//! memoizes immutable SSTable blocks. This cache owns mutable, pinned, written-
//! back pages over the VFS abstraction.
//!
//! The v1 cache is single-threaded (`Rc`/`RefCell`); concurrency is deferred.

mod cache;
mod error;
pub mod journal;
pub mod page;
mod vfs;

pub use cache::{EvictionPolicy, LruK, PageCache, ReadPage, WritePage};
pub use error::{PageError, Result};
pub use journal::{PageJournal, PageOp};
pub use page::{HEADER_LEN, PAGE_SIZE, PageHeader, TuplePage, TuplePageMut, read_text, write_text};
pub use vfs::{BLOCK_SIZE, FileVfs, MemVfs, Vfs};

/// Logical identity of a page — stable, unique, and never reused (see the
/// Storage Foundations design). Opaque: it encodes nothing about the page's
/// type, contents, or physical location. The `Vfs` resolves it to bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(pub u64);
