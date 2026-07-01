//! End-to-end tests for the filesystem: the cache over a real file VFS, eviction
//! under a small pool, and the two page types round-tripping through it.

use filesystem::page::finalize_checksum;
use filesystem::page::types::TUPLE_PAGE;
use filesystem::{
    BlockId, DiskBlockStore, MemBlockStore, PAGE_SIZE, PageCache, PageHeader, TuplePage,
    TuplePageMut,
};
use filesystem::{BlockJournal, JournalOp, read_text, write_text};

/// Allocate a page, write it, flush, drop the whole cache, reopen the file, and
/// read the page back — the durability path end to end.
#[test]
fn page_survives_flush_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");

    let id = {
        let cache = PageCache::new(DiskBlockStore::open(&path).unwrap(), 8);
        let (id, page) = cache.allocate().unwrap();
        page.write_header(&PageHeader::new(TUPLE_PAGE, 1));
        page.payload_mut()[..5].copy_from_slice(b"hello");
        drop(page);
        cache.flush().unwrap();
        id
    };

    // Fresh cache + freshly reopened file: the page must come back from disk.
    let cache = PageCache::new(DiskBlockStore::open(&path).unwrap(), 8);
    let page = cache.read(id).unwrap();
    assert_eq!(page.header().unwrap().page_type, TUPLE_PAGE);
    assert_eq!(&page.payload()[..5], b"hello");
}

/// A reopened store must not re-issue an id that already named durable data.
#[test]
fn ids_are_not_reused_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");

    let first = {
        let cache = PageCache::new(DiskBlockStore::open(&path).unwrap(), 4);
        let (id, _page) = cache.allocate().unwrap();
        cache.flush().unwrap();
        id
    };
    let cache = PageCache::new(DiskBlockStore::open(&path).unwrap(), 4);
    let (second, _page) = cache.allocate().unwrap();
    assert_ne!(first, second, "reopened store reissued a live id");
}

/// More distinct pages than frames: eviction must write dirty victims back and
/// reload them on demand, with the data intact.
#[test]
fn eviction_preserves_data_under_small_pool() {
    let cache = PageCache::new(MemBlockStore::new(), 4);

    // Write 32 single-tuple pages through a 4-frame pool.
    let mut ids = Vec::new();
    for i in 0..32u32 {
        let (id, page) = cache.allocate().unwrap();
        {
            let mut buf = page.bytes_mut();
            let mut tp = TuplePageMut::init(&mut buf);
            tp.insert(&i.to_be_bytes()).unwrap();
        }
        drop(page);
        ids.push(id);
    }
    assert!(cache.resident() <= cache.frame_count());

    // Every page reads back correctly despite having been evicted.
    for (i, id) in ids.into_iter().enumerate() {
        let page = cache.read(id).unwrap();
        let buf = page.bytes();
        let tp = TuplePage::open(&buf).unwrap();
        assert_eq!(tp.get(0), Some(&(i as u32).to_be_bytes()[..]));
    }
}

/// A writer and a reader cannot hold the same page at once.
#[test]
fn write_excludes_concurrent_read() {
    let cache = PageCache::new(MemBlockStore::new(), 4);
    let (id, writer) = cache.allocate().unwrap();
    writer.write_header(&PageHeader::new(TUPLE_PAGE, 1));

    assert!(
        cache.read(id).is_err(),
        "read should conflict with held writer"
    );
    drop(writer);
    assert!(
        cache.read(id).is_ok(),
        "read should succeed once writer is gone"
    );
}

/// A TEXT value far larger than one page round-trips through its chain, driven
/// by a pool too small to hold the whole chain at once.
#[test]
fn text_chain_roundtrips_through_cache() {
    let cache = PageCache::new(MemBlockStore::new(), 3);
    let value: String = ('a'..='z').cycle().take(50_000).collect();
    let head = write_text(&cache, &value).unwrap();
    cache.flush().unwrap();
    assert_eq!(read_text(&cache, head).unwrap(), value);
}

/// A journaled cache: write + flush, drop everything, reopen with the same VFS
/// and journal, and read the page back.
#[test]
fn journaled_flush_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let vfs_path = dir.path().join("store.db");
    let journal_path = dir.path().join("pager.journal");

    let id = {
        let cache =
            PageCache::with_journal(DiskBlockStore::open(&vfs_path).unwrap(), 8, &journal_path)
                .unwrap();
        let (id, page) = cache.allocate().unwrap();
        page.write_header(&PageHeader::new(TUPLE_PAGE, 1));
        page.payload_mut()[..3].copy_from_slice(b"hey");
        drop(page);
        cache.flush().unwrap();
        id
    };

    let cache = PageCache::with_journal(DiskBlockStore::open(&vfs_path).unwrap(), 8, &journal_path)
        .unwrap();
    let page = cache.read(id).unwrap();
    assert_eq!(&page.payload()[..3], b"hey");
}

/// The honest recovery test: a committed write sits in the journal but never
/// reached the VFS (crash after the commit marker, before the page write).
/// Recovery must replay it into the VFS — proving the journal, not the VFS, is
/// what made the change durable — and must advance the allocator past it.
#[test]
fn recovery_replays_committed_write_the_vfs_never_had() {
    let dir = tempfile::tempdir().unwrap();
    let vfs_path = dir.path().join("store.db");
    let journal_path = dir.path().join("pager.journal");

    // Build a valid TuplePage image for page 1 (checksum finalized, as a real
    // flush would log it).
    let mut image = vec![0u8; PAGE_SIZE];
    {
        let mut tp = TuplePageMut::init(&mut image);
        tp.insert(b"recovered").unwrap();
    }
    finalize_checksum(&mut image);

    // Pre-seed the journal with that committed write. The VFS stays untouched.
    {
        let mut journal = BlockJournal::open(&journal_path).unwrap();
        journal
            .append(&JournalOp::Write {
                page_id: 1,
                image: image.clone(),
            })
            .unwrap();
        journal.append(&JournalOp::Commit).unwrap();
    }

    let cache = PageCache::with_journal(DiskBlockStore::open(&vfs_path).unwrap(), 8, &journal_path)
        .unwrap();

    // The page is back, sourced purely from the journal.
    let page = cache.read(BlockId(1)).unwrap();
    let buf = page.bytes();
    let tp = TuplePage::open(&buf).unwrap();
    assert_eq!(tp.get(0), Some(&b"recovered"[..]));
    drop(buf);
    drop(page);

    // And the allocator skips the recovered id rather than reusing it.
    let (next, _page) = cache.allocate().unwrap();
    assert_eq!(next, BlockId(2));
}

/// Writes after the last commit marker are a torn flush and must be discarded.
#[test]
fn recovery_discards_uncommitted_writes() {
    let dir = tempfile::tempdir().unwrap();
    let vfs_path = dir.path().join("store.db");
    let journal_path = dir.path().join("pager.journal");

    let mut image = vec![0u8; PAGE_SIZE];
    {
        let mut tp = TuplePageMut::init(&mut image);
        tp.insert(b"doomed").unwrap();
    }
    finalize_checksum(&mut image);

    // A write with no following commit — an interrupted flush.
    {
        let mut journal = BlockJournal::open(&journal_path).unwrap();
        journal
            .append(&JournalOp::Write { page_id: 1, image })
            .unwrap();
    }

    let cache = PageCache::with_journal(DiskBlockStore::open(&vfs_path).unwrap(), 8, &journal_path)
        .unwrap();

    // Page 1 was never committed, so it isn't there...
    assert!(cache.read(BlockId(1)).is_err());
    // ...and its id was never consumed, so the next allocation reuses it.
    let (next, _page) = cache.allocate().unwrap();
    assert_eq!(next, BlockId(1));
}
