//! Pages — the unit the page cache loads and serves.

use std::sync::Arc;

use super::codec::{Decode, DecodeError, Encode};
use crate::SsTableId;

/// Identifies one data block tree-wide: which file ([`SsTableId`]) and the
/// block's byte `offset` in it. The offset is a stable, unique identity that
/// holds whether the block index is inline or sharded across child headers, so
/// one shared block cache can key every table's blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    pub table: SsTableId,
    pub offset: u64,
}

/// Identifies one *header* page tree-wide, so a single shared header cache can
/// hold the root and any child chunk of many tables at once. A table has one
/// [`Root`](HeaderId::Root) (loaded from the file tail) and, when large, child
/// chunks keyed by their byte `offset` in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaderId {
    Root(SsTableId),
    Child { table: SsTableId, offset: u64 },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ids_distinguish_table_and_offset() {
        let a = PageId {
            table: SsTableId(1),
            offset: 0,
        };
        assert_ne!(
            a,
            PageId {
                table: SsTableId(1),
                offset: 64
            },
            "same table, different block offset"
        );
        assert_ne!(
            a,
            PageId {
                table: SsTableId(2),
                offset: 0
            },
            "different table, same offset"
        );
        assert_eq!(
            a,
            PageId {
                table: SsTableId(1),
                offset: 0
            }
        );
    }

    #[test]
    fn header_ids_distinguish_role_table_and_offset() {
        let table = SsTableId(1);
        let root = HeaderId::Root(table);
        let child0 = HeaderId::Child { table, offset: 0 };
        // A root and a child of the same table are distinct cache keys...
        assert_ne!(root, child0);
        // ...two shards of one table differ by offset...
        assert_ne!(
            child0,
            HeaderId::Child {
                table,
                offset: 4096
            }
        );
        // ...and roots of different tables are distinct.
        assert_ne!(root, HeaderId::Root(SsTableId(2)));
        assert_eq!(child0, HeaderId::Child { table, offset: 0 });
    }
}
