//! The TextPage — storage for unbounded `TEXT` values, referenced by a
//! [`PageId`] pointer from a tuple.
//!
//! ```text
//! off          field
//! 0            page header (21 bytes)
//! 21    u64    next_page_id    — 0 on the last segment
//! 29    u64    total_length    — whole value's byte length (first page only)
//! 37    u16    segment_length  — content bytes in this page
//! 39           content         — raw UTF-8
//! ```
//!
//! A value longer than one page's content capacity is split across a **chain**
//! of TextPages linked by `next_page_id`. The value is stored as UTF-8 bytes;
//! the split can fall mid–code-point, so a segment is not independently valid
//! UTF-8 — only the reassembled whole is decoded.

use super::header::{HEADER_LEN, PageHeader};
use super::types::TEXT_PAGE;
use crate::error::PageError;
use crate::{PAGE_SIZE, PageCache, PageId, Result, Vfs};

const FORMAT_VERSION: u16 = 1;

const OFF_NEXT: usize = HEADER_LEN; // 21
const OFF_TOTAL: usize = HEADER_LEN + 8; // 29
const OFF_SEG_LEN: usize = HEADER_LEN + 16; // 37
const CONTENT_OFFSET: usize = HEADER_LEN + 18; // 39

/// Bytes of `TEXT` content one page can hold.
pub const CONTENT_CAPACITY: usize = PAGE_SIZE - CONTENT_OFFSET;

/// Write `s` as a chain of TextPages through `cache`, returning the head
/// [`PageId`] — the pointer a tuple stores. The pages are dirty in the cache;
/// they reach disk on [`flush`](PageCache::flush).
pub fn write_text<V: Vfs>(cache: &PageCache<V>, s: &str) -> Result<PageId> {
    let bytes = s.as_bytes();
    // An empty value still needs one page so the pointer resolves to something.
    let chunks: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[]]
    } else {
        bytes.chunks(CONTENT_CAPACITY).collect()
    };
    let total = bytes.len() as u64;

    // Allocate back-to-front: each page links to the one already allocated
    // after it, so we never have to revisit a page to patch its `next` pointer.
    let mut next = 0u64;
    let mut head = PageId(0);
    for (i, chunk) in chunks.iter().enumerate().rev() {
        let (id, page) = cache.allocate()?;
        {
            let mut buf = page.bytes_mut();
            let total = if i == 0 { total } else { 0 };
            write_segment(&mut buf, next, total, chunk);
        }
        drop(page); // mark dirty and release the frame for reuse
        next = id.0;
        head = id;
    }
    Ok(head)
}

/// Read back the `TEXT` value whose chain begins at `head`.
pub fn read_text<V: Vfs>(cache: &PageCache<V>, head: PageId) -> Result<String> {
    let mut out = Vec::new();
    let mut cur = head;
    loop {
        let page = cache.read(cur)?;
        let buf = page.bytes();

        let header = PageHeader::parse(&buf)?;
        if header.page_type != TEXT_PAGE {
            return Err(PageError::BadPageType {
                expected: TEXT_PAGE,
                got: header.page_type,
            });
        }
        let next = get_u64(&buf, OFF_NEXT);
        let seg_len = get_u16(&buf, OFF_SEG_LEN) as usize;
        out.extend_from_slice(&buf[CONTENT_OFFSET..CONTENT_OFFSET + seg_len]);

        drop(buf);
        drop(page);
        if next == 0 {
            break;
        }
        cur = PageId(next);
    }
    Ok(String::from_utf8(out)?)
}

fn write_segment(buf: &mut [u8], next_page_id: u64, total_length: u64, content: &[u8]) {
    PageHeader::new(TEXT_PAGE, FORMAT_VERSION).write(buf);
    put_u64(buf, OFF_NEXT, next_page_id);
    put_u64(buf, OFF_TOTAL, total_length);
    put_u16(buf, OFF_SEG_LEN, content.len() as u16);
    buf[CONTENT_OFFSET..CONTENT_OFFSET + content.len()].copy_from_slice(content);
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(buf[off..off + 8].try_into().unwrap())
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_be_bytes());
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
    use crate::MemVfs;

    #[test]
    fn single_page_roundtrip() {
        let cache = PageCache::new(MemVfs::new(), 4);
        let id = write_text(&cache, "hello, world").unwrap();
        assert_eq!(read_text(&cache, id).unwrap(), "hello, world");
    }

    #[test]
    fn empty_string_roundtrip() {
        let cache = PageCache::new(MemVfs::new(), 4);
        let id = write_text(&cache, "").unwrap();
        assert_eq!(read_text(&cache, id).unwrap(), "");
    }

    #[test]
    fn long_value_chains_across_pages() {
        // A tiny pool forces dirty chain pages to be written back and re-read.
        let cache = PageCache::new(MemVfs::new(), 2);
        let big = "λ".repeat(CONTENT_CAPACITY); // multibyte, so splits fall mid-codepoint
        let id = write_text(&cache, &big).unwrap();
        assert_eq!(read_text(&cache, id).unwrap(), big);
    }
}
