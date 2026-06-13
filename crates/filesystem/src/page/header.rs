//! The page header — fixed 21 bytes, big-endian, self-describing.
//!
//! ```text
//! off  size  field
//! 0    4     magic            "STDB"
//! 4    1     header version   u8
//! 5    2     page type        u16
//! 7    2     format version   u16
//! 9    8     LSN              u64
//! 17   4     checksum         u32  (CRC32c over the whole page, this field zeroed)
//! ```
//!
//! The checksum covers the entire page, so it lives at a fixed spot and is
//! computed over everything *except* its own four bytes. The page cache
//! finalizes it just before writeback and verifies it on load; nothing else
//! needs to touch it.

use crate::error::Error;
use crate::{BlockId, Result};

const MAGIC: [u8; 4] = *b"STDB";
const HEADER_VERSION: u8 = 1;

/// Total header length in bytes.
pub const HEADER_LEN: usize = 21;

const OFF_CHECKSUM: usize = 17;

/// The parsed, mutable fields of a page header. Magic, header version, and
/// checksum are handled by [`write`](PageHeader::write) / the cache, not stored
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    /// Which page-type handler interprets the payload (see [`types`](super::types)).
    pub page_type: u16,
    /// That page type's own format version.
    pub format_version: u16,
    /// Log sequence number — the engine-level journal position of the change
    /// that produced this page state. `0` until journaling wires it through.
    pub lsn: u64,
}

impl PageHeader {
    /// A header for a given type and format, with `lsn = 0`.
    pub fn new(page_type: u16, format_version: u16) -> Self {
        Self {
            page_type,
            format_version,
            lsn: 0,
        }
    }

    /// Write the header fields into the first [`HEADER_LEN`] bytes of `page`.
    /// Leaves the checksum field untouched — that is finalized at writeback.
    pub fn write(&self, page: &mut [u8]) {
        page[0..4].copy_from_slice(&MAGIC);
        page[4] = HEADER_VERSION;
        page[5..7].copy_from_slice(&self.page_type.to_be_bytes());
        page[7..9].copy_from_slice(&self.format_version.to_be_bytes());
        page[9..17].copy_from_slice(&self.lsn.to_be_bytes());
    }

    /// Parse and validate the header at the start of `page`. Checks magic and
    /// header version; does *not* verify the checksum (that is the cache's job
    /// on load — see [`verify_checksum`]).
    pub fn parse(page: &[u8]) -> Result<Self> {
        if page.len() < HEADER_LEN || page[0..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        if page[4] != HEADER_VERSION {
            return Err(Error::BadHeaderVersion(page[4]));
        }
        Ok(Self {
            page_type: u16::from_be_bytes(page[5..7].try_into().unwrap()),
            format_version: u16::from_be_bytes(page[7..9].try_into().unwrap()),
            lsn: u64::from_be_bytes(page[9..17].try_into().unwrap()),
        })
    }
}

/// Compute and store the page's CRC32c checksum. Call immediately before
/// writing the page to the block store.
pub fn finalize_checksum(page: &mut [u8]) {
    let sum = checksum(page);
    page[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&sum.to_be_bytes());
}

/// Verify the stored checksum against the page's current bytes. `id` is only
/// used to label the error.
pub fn verify_checksum(page: &[u8], id: BlockId) -> Result<()> {
    let stored = u32::from_be_bytes(page[OFF_CHECKSUM..OFF_CHECKSUM + 4].try_into().unwrap());
    if checksum(page) != stored {
        return Err(Error::Checksum(id));
    }
    Ok(())
}

/// CRC32c over the whole page with the 4-byte checksum field excluded, so the
/// result is independent of whatever those bytes currently hold.
fn checksum(page: &[u8]) -> u32 {
    let sum = crc32c::crc32c(&page[..OFF_CHECKSUM]);
    crc32c::crc32c_append(sum, &page[OFF_CHECKSUM + 4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PAGE_SIZE;

    #[test]
    fn write_then_parse_roundtrips() {
        let mut page = vec![0u8; PAGE_SIZE];
        let h = PageHeader {
            page_type: 1,
            format_version: 2,
            lsn: 0xDEAD_BEEF,
        };
        h.write(&mut page);
        assert_eq!(PageHeader::parse(&page).unwrap(), h);
    }

    #[test]
    fn checksum_detects_corruption() {
        let mut page = vec![0u8; PAGE_SIZE];
        PageHeader::new(1, 1).write(&mut page);
        finalize_checksum(&mut page);
        verify_checksum(&page, BlockId(7)).unwrap();

        page[100] ^= 0xFF; // flip a payload bit
        assert!(verify_checksum(&page, BlockId(7)).is_err());
    }

    #[test]
    fn rejects_foreign_bytes() {
        let page = vec![0u8; PAGE_SIZE];
        assert!(matches!(PageHeader::parse(&page), Err(Error::BadMagic)));
    }
}
