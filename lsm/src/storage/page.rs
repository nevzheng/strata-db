//! Pages — the unit the page cache loads and serves.

use std::sync::Arc;

use super::codec::{Decode, DecodeError, Encode};
use crate::SsTableId;

/// Identifies one page within an SSTable file: which file ([`SsTableId`])
/// and which page in it. `(table, page_index)` names a page tree-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    pub table: SsTableId,
    pub page_index: u32,
}

/// A loaded page: immutable bytes shared by `Arc`, so cache hits clone cheaply.
#[derive(Debug, Clone)]
pub struct Page {
    bytes: Arc<[u8]>,
}

impl Page {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Encode a value into a new page.
    pub fn encode<T: Encode>(value: &T) -> Self {
        let mut buf = Vec::new();
        value.encode(&mut buf);
        Page::new(buf)
    }

    /// Decode a value from this page's bytes.
    pub fn decode<T: Decode>(&self) -> Result<T, DecodeError> {
        let mut bytes = self.bytes();
        T::decode(&mut bytes)
    }
}
