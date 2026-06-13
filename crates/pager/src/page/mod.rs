//! Pages — blocks reinterpreted as typed, self-describing units.
//!
//! Every page starts with a fixed [`PageHeader`]; the bytes after it are the
//! payload, whose shape is defined by the page *type*. The header's checksum
//! covers the whole page and is finalized by the [`cache`](crate::cache) at
//! writeback, so page-type code never touches it.

mod header;
pub mod text;
mod tuple;
pub mod types;

pub use header::{HEADER_LEN, PageHeader, finalize_checksum, verify_checksum};
pub use text::{read_text, write_text};
pub use tuple::{TuplePage, TuplePageMut};

/// The size of a page — identical to the block size, since a page *is* a block
/// with meaning layered on.
pub const PAGE_SIZE: usize = crate::vfs::BLOCK_SIZE;

/// Bytes available for a page type's payload, after the header.
pub const PAYLOAD_CAPACITY: usize = PAGE_SIZE - HEADER_LEN;
