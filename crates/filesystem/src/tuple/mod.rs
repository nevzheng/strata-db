//! Tuples — everything you need to work with tuple records, in one place:
//!
//! - [`page`] — the slotted [`TuplePage`] format (tuples packed into a page).
//! - [`heap`] — the [`Heap`], a tuple store over the [`PageCache`](crate::PageCache).
//! - [`loc`] — [`TupleLoc`], the durable address an index stores to find a tuple.
//!
//! These sit one layer above the generic page/cache machinery: the page is a
//! [`page`](crate::page) format, and the heap is the access method over it.

mod heap;
mod loc;
mod page;

pub use heap::{Heap, PageTuples, TupleMut, TupleRef, TupleView};
pub use loc::TupleLoc;
pub use page::{TuplePage, TuplePageMut};
