//! The page journal — what the cache logs and how it's framed.
//!
//! The pager runs a **redo journal** with **no-steal** semantics: a dirty page
//! is never written to the VFS outside [`flush`](crate::PageCache::flush), and a
//! flush logs the after-image of every dirty page plus a [`Commit`](PageOp::Commit)
//! marker *before* touching the VFS. Recovery replays the after-images of the
//! last fully-committed batch, so a crash mid-flush is atomic: all of that
//! flush's pages come back, or none do.
//!
//! [`PageJournal`] wraps the generic [`journal`] crate (framing, CRC, crash-safe
//! replay live there) with the page record type:
//!
//! ```text
//! Write:  | tag=1 | page_id (8, BE) | after-image (PAGE_SIZE bytes) |
//! Commit: | tag=2 |
//! ```
//!
//! Full-page after-images (rather than byte diffs) keep recovery trivially
//! idempotent and, because the page header carries a CRC, let recovery rewrite a
//! page that a crash tore mid-write. Compact diffs and group commit are future
//! work. We do not log `Free` yet (no deallocation path exists), nor before-
//! images for undo (no transaction rollback yet).
//!
//! The dependency is also named `journal`; this module shadows it, so the two
//! references to the crate itself use the `::journal` extern path.

use std::path::Path;

use ::journal::{Codec, Journal, JournalError};

use crate::Result;

const TAG_WRITE: u8 = 1;
const TAG_COMMIT: u8 = 2;

/// One record in the page journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageOp {
    /// The full after-image of a page, to be redone on recovery.
    Write {
        /// The page this image belongs to.
        page_id: u64,
        /// The page's bytes (`PAGE_SIZE` long, header + payload, checksum final).
        image: Vec<u8>,
    },
    /// Marks the preceding `Write`s as a complete, durable batch. Recovery
    /// applies `Write`s only up to the last `Commit`; anything after is a torn
    /// flush and is discarded.
    Commit,
}

/// The page cache's redo journal: an append-only log of [`PageOp`]s with
/// crash-safe replay. A thin, typed wrapper over the generic journal.
pub struct PageJournal {
    inner: Journal<PageOpCodec>,
}

impl PageJournal {
    /// Open (creating if needed) the journal at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            inner: Journal::open(path)?,
        })
    }

    /// Durably append one record (returns once it is `fsync`'d).
    ///
    /// A journal write is the durability point: if we cannot record a
    /// page image, there is no safe way to proceed — continuing would
    /// risk applying changes we can never recover. So a failure here is
    /// **fail-stop** (panic), unlike pager/LSM allocation exhaustion,
    /// which fails the write but lets reads continue.
    pub fn append(&mut self, op: &PageOp) -> Result<()> {
        self.inner.append(op).unwrap_or_else(|e| {
            panic!("page journal append failed; cannot guarantee durability: {e}")
        });
        Ok(())
    }

    /// Read back every durably-recorded op in append order.
    pub fn replay(&self) -> Result<Vec<PageOp>> {
        Ok(self
            .inner
            .replay()?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// Discard all records — call once they have been applied durably to the VFS.
    pub fn truncate(&mut self) -> Result<()> {
        self.inner.truncate()?;
        Ok(())
    }
}

/// Frames [`PageOp`]s into journal records.
#[derive(Debug, Default, Clone, Copy)]
struct PageOpCodec;

impl Codec for PageOpCodec {
    type Record = PageOp;

    fn encode(&self, record: &PageOp, buf: &mut Vec<u8>) {
        match record {
            PageOp::Write { page_id, image } => {
                buf.push(TAG_WRITE);
                buf.extend_from_slice(&page_id.to_be_bytes());
                buf.extend_from_slice(image);
            }
            PageOp::Commit => buf.push(TAG_COMMIT),
        }
    }

    fn decode(&self, bytes: &[u8]) -> std::result::Result<PageOp, JournalError> {
        match bytes.first() {
            Some(&TAG_WRITE) => {
                if bytes.len() < 9 {
                    return Err(JournalError::Decode(format!(
                        "write record too short: {} bytes",
                        bytes.len()
                    )));
                }
                let page_id = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
                Ok(PageOp::Write {
                    page_id,
                    image: bytes[9..].to_vec(),
                })
            }
            Some(&TAG_COMMIT) => Ok(PageOp::Commit),
            other => Err(JournalError::Decode(format!(
                "unknown record tag: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(op: &PageOp) -> PageOp {
        let codec = PageOpCodec;
        let mut buf = Vec::new();
        codec.encode(op, &mut buf);
        codec.decode(&buf).unwrap()
    }

    #[test]
    fn write_and_commit_roundtrip() {
        let w = PageOp::Write {
            page_id: 42,
            image: vec![7u8; 100],
        };
        assert_eq!(roundtrip(&w), w);
        assert_eq!(roundtrip(&PageOp::Commit), PageOp::Commit);
    }

    #[test]
    fn rejects_garbage() {
        let codec = PageOpCodec;
        assert!(codec.decode(&[]).is_err());
        assert!(codec.decode(&[99]).is_err());
        assert!(codec.decode(&[TAG_WRITE, 0, 0]).is_err()); // truncated page_id
    }
}
