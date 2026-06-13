//! Memtables — in-memory write buffers implementing [`MemStore`](crate::MemStore).

mod btree;
mod journal;

pub use btree::BTreeMemtable;
pub(crate) use journal::Journaled;
