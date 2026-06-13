//! The page-type registry — the well-known `page_type` values stamped into a
//! page header.
//!
//! The Storage Foundations design envisions a central registry the block store owns,
//! with ranges reserved for application-specific types. For v1 the well-known
//! types are these constants; a runtime registry can come later without
//! changing the on-disk meaning of these numbers.

/// Slotted NSM row storage — see [`TuplePage`](super::TuplePage).
pub const TUPLE_PAGE: u16 = 1;

/// Per-page MVCC delta chain. Reserved; not implemented in v1.
pub const DELTA_PAGE: u16 = 2;

/// Unbounded `TEXT` storage — see [`text`](super::text).
pub const TEXT_PAGE: u16 = 3;
