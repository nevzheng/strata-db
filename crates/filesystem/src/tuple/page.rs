//! The TuplePage — strata-db's first page type: slotted row storage in the
//! N-ary Storage Model (NSM, all of a row's fields packed contiguously).
//!
//! ```text
//! off          field
//! 0            page header (21 bytes)
//! 21    u16    slot_count
//! 23    u16    free_start   — end of the slot array (free space begins here)
//! 25    u16    free_end     — start of tuple data (free space ends here)
//! 27           slot array, grows downward: [offset u16 | length u16 | flags u16 | _ u16]
//!  ⋮           free space
//!              tuple data, packed upward from the bottom
//! ```
//!
//! A tuple's stable logical identity is `(PageId, slot_id)`: `slot_id` is the
//! index into the slot array and never changes for the tuple's lifetime, which
//! is what makes it usable by indexes and any future MVCC chain.
//!
//! The page stores **opaque tuple bytes** — field layout is the schema's
//! concern, encoded by the engine and passed in. (The doc's dedicated VarLen
//! section is an in-place-varchar optimization folded into the opaque blob for
//! v1; `TEXT` is just a [`PageId`](crate::PageId) pointer inside the blob,
//! resolved against a [`TextPage`](crate::page::text).)

use crate::page::{HEADER_LEN, PageHeader};
use crate::page::types::TUPLE_PAGE;
use crate::error::PageError;
use crate::{PAGE_SIZE, Result};

const FORMAT_VERSION: u16 = 1;

// Page-metadata field offsets, just past the header.
const OFF_SLOT_COUNT: usize = HEADER_LEN; // 21
const OFF_FREE_START: usize = HEADER_LEN + 2; // 23
const OFF_FREE_END: usize = HEADER_LEN + 4; // 25
const SLOT_ARRAY_OFFSET: usize = HEADER_LEN + 6; // 27

const SLOT_SIZE: usize = 8;

/// Slot flag: the tuple is logically deleted. Its bytes may linger for older
/// snapshots until compaction reclaims them.
const FLAG_DELETED: u16 = 0x0001;

// u16 offsets cap a page at 64 KiB. Fine for the 8 KiB default; assert it so a
// future page-size bump can't silently corrupt offsets.
const _: () = assert!(PAGE_SIZE <= u16::MAX as usize + 1);

/// A read-only view over a TuplePage's bytes.
pub struct TuplePage<'a>(&'a [u8]);

/// A mutable view for building or updating a TuplePage.
pub struct TuplePageMut<'a>(&'a mut [u8]);

impl<'a> TuplePage<'a> {
    /// Open an existing page for reading, verifying it really is a TuplePage.
    pub fn open(buf: &'a [u8]) -> Result<Self> {
        let header = PageHeader::parse(buf)?;
        if header.page_type != TUPLE_PAGE {
            return Err(PageError::BadPageType {
                expected: TUPLE_PAGE,
                got: header.page_type,
            });
        }
        Ok(Self(buf))
    }

    /// Number of slots ever allocated (including deleted ones — `slot_id`s are
    /// never renumbered).
    pub fn slot_count(&self) -> u16 {
        get_u16(self.0, OFF_SLOT_COUNT)
    }

    /// The live tuple at `slot_id`, or `None` if out of range or deleted.
    pub fn get(&self, slot_id: u16) -> Option<&'a [u8]> {
        slot_bytes(self.0, slot_id)
    }

    /// Iterate `(slot_id, bytes)` over live tuples in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &'a [u8])> {
        let buf = self.0;
        (0..self.slot_count())
            .filter_map(move |slot_id| slot_bytes(buf, slot_id).map(|b| (slot_id, b)))
    }
}

impl<'a> TuplePageMut<'a> {
    /// Initialize a fresh, empty TuplePage over `buf` (stamps the header and
    /// metadata). Use on a newly [`allocate`](crate::PageCache::allocate)d page.
    pub fn init(buf: &'a mut [u8]) -> Self {
        PageHeader::new(TUPLE_PAGE, FORMAT_VERSION).write(buf);
        put_u16(buf, OFF_SLOT_COUNT, 0);
        put_u16(buf, OFF_FREE_START, SLOT_ARRAY_OFFSET as u16);
        put_u16(buf, OFF_FREE_END, PAGE_SIZE as u16);
        Self(buf)
    }

    /// Open an existing TuplePage for mutation, verifying its type.
    pub fn open(buf: &'a mut [u8]) -> Result<Self> {
        let header = PageHeader::parse(buf)?;
        if header.page_type != TUPLE_PAGE {
            return Err(PageError::BadPageType {
                expected: TUPLE_PAGE,
                got: header.page_type,
            });
        }
        Ok(Self(buf))
    }

    /// Free bytes available for one more tuple (after reserving its slot).
    pub fn free_space(&self) -> usize {
        let span = get_u16(self.0, OFF_FREE_END) - get_u16(self.0, OFF_FREE_START);
        (span as usize).saturating_sub(SLOT_SIZE)
    }

    /// Append `tuple`, returning its `slot_id`, or `None` if it doesn't fit.
    pub fn insert(&mut self, tuple: &[u8]) -> Option<u16> {
        let slot_count = get_u16(self.0, OFF_SLOT_COUNT);
        let free_start = get_u16(self.0, OFF_FREE_START);
        let free_end = get_u16(self.0, OFF_FREE_END);

        // Need room for the tuple *and* its 8-byte slot.
        if tuple.len() + SLOT_SIZE > (free_end - free_start) as usize {
            return None;
        }

        let tuple_off = free_end - tuple.len() as u16;
        self.0[tuple_off as usize..free_end as usize].copy_from_slice(tuple);

        let slot_pos = free_start as usize; // == SLOT_ARRAY_OFFSET + slot_count * SLOT_SIZE
        put_u16(self.0, slot_pos, tuple_off);
        put_u16(self.0, slot_pos + 2, tuple.len() as u16);
        put_u16(self.0, slot_pos + 4, 0); // flags
        put_u16(self.0, slot_pos + 6, 0); // reserved

        put_u16(self.0, OFF_SLOT_COUNT, slot_count + 1);
        put_u16(self.0, OFF_FREE_START, free_start + SLOT_SIZE as u16);
        put_u16(self.0, OFF_FREE_END, tuple_off);
        Some(slot_count)
    }

    /// Mark `slot_id` deleted. Returns `false` if out of range. Space is not
    /// reclaimed here — that is compaction's job.
    pub fn delete(&mut self, slot_id: u16) -> bool {
        if slot_id >= get_u16(self.0, OFF_SLOT_COUNT) {
            return false;
        }
        let flags_pos = SLOT_ARRAY_OFFSET + slot_id as usize * SLOT_SIZE + 4;
        let flags = get_u16(self.0, flags_pos);
        put_u16(self.0, flags_pos, flags | FLAG_DELETED);
        true
    }

    /// The live tuple at `slot_id`, or `None` if out of range or deleted.
    pub fn get(&self, slot_id: u16) -> Option<&[u8]> {
        slot_bytes(self.0, slot_id)
    }

    /// Consume the view and return a *mutable* slice of the live tuple at
    /// `slot_id`, or `None` if out of range or deleted. For in-place,
    /// same-length updates; the returned slice borrows the page for `'a`.
    pub fn into_slot_bytes_mut(self, slot_id: u16) -> Option<&'a mut [u8]> {
        let range = slot_range(self.0, slot_id)?;
        Some(&mut self.0[range])
    }
}

/// The byte range of a live slot's tuple within the page, or `None` if the slot
/// is out of range or deleted. Shared by the read and write views so they agree
/// on where a tuple's bytes are.
pub(crate) fn slot_range(buf: &[u8], slot_id: u16) -> Option<core::ops::Range<usize>> {
    if slot_id >= get_u16(buf, OFF_SLOT_COUNT) {
        return None;
    }
    let pos = SLOT_ARRAY_OFFSET + slot_id as usize * SLOT_SIZE;
    if get_u16(buf, pos + 4) & FLAG_DELETED != 0 {
        return None;
    }
    let off = get_u16(buf, pos) as usize;
    let len = get_u16(buf, pos + 2) as usize;
    Some(off..off + len)
}

/// Resolve a slot to its live tuple bytes, or `None` if out of range/deleted.
fn slot_bytes(buf: &[u8], slot_id: u16) -> Option<&[u8]> {
    slot_range(buf, slot_id).map(|r| &buf[r])
}

fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes(buf[off..off + 2].try_into().unwrap())
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_delete() {
        let mut buf = vec![0u8; PAGE_SIZE];
        let mut page = TuplePageMut::init(&mut buf);

        let a = page.insert(b"alice").unwrap();
        let b = page.insert(b"bob").unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(page.get(a), Some(&b"alice"[..]));
        assert_eq!(page.get(b), Some(&b"bob"[..]));

        assert!(page.delete(a));
        assert_eq!(page.get(a), None);
        // bob keeps his slot id even though alice's slot is now a tombstone.
        assert_eq!(page.get(b), Some(&b"bob"[..]));
    }

    #[test]
    fn reopen_reads_back() {
        let mut buf = vec![0u8; PAGE_SIZE];
        {
            let mut page = TuplePageMut::init(&mut buf);
            page.insert(b"x").unwrap();
            page.insert(b"yy").unwrap();
        }
        let page = TuplePage::open(&buf).unwrap();
        assert_eq!(page.slot_count(), 2);
        let live: Vec<_> = page.iter().collect();
        assert_eq!(live, vec![(0u16, &b"x"[..]), (1u16, &b"yy"[..])]);
    }

    #[test]
    fn insert_fails_when_full() {
        let mut buf = vec![0u8; PAGE_SIZE];
        let mut page = TuplePageMut::init(&mut buf);
        let big = vec![0u8; PAGE_SIZE]; // can't possibly fit
        assert_eq!(page.insert(&big), None);
        // The page is still usable for something that fits.
        assert_eq!(page.insert(b"ok"), Some(0));
    }

    #[test]
    fn wrong_type_is_rejected() {
        let mut buf = vec![0u8; PAGE_SIZE];
        PageHeader::new(crate::page::types::TEXT_PAGE, 1).write(&mut buf);
        assert!(matches!(
            TuplePage::open(&buf),
            Err(PageError::BadPageType { .. })
        ));
    }
}
