//! Strata's storage engine.
//!
//! [`StorageEngine`] coordinates the reusable LSM building blocks from
//! the [`lsm`] crate — the write-ahead log, in-memory store, and on-disk
//! levels — into a single read/write store. This crate is the
//! composition point where higher-level storage subcrates are
//! integrated over time.

pub mod engine;

pub use engine::StorageEngine;

// Re-export the lsm surface so dependents can keep importing storage
// types from a single crate (`strata_store::…`).
pub use lsm::memstore;
pub use lsm::{KVPair, LevelConfig, MergeIterator, ReadStore, ScanIterator, StorageError};
