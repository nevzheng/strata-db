//! Memtables — in-memory write buffers implementing [`MemStore`](crate::MemStore).

mod btree;

pub use btree::BTreeMemtable;
