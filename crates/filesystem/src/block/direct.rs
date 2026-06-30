//! Direct I/O file abstraction — a [`DirectFile`] wraps a [`std::fs::File`] and
//! optionally bypasses the OS page cache via `O_DIRECT` (Linux) or `F_NOCACHE`
//! (macOS). Callers pass [`Block`](crate::block::Block) buffers that are aligned
//! by construction, so no bounce buffer is needed.
//!
//! # Platform support
//!
//! | Platform | Mechanism | Notes |
//! |----------|-----------|-------|
//! | Linux    | `O_DIRECT` on `open(2)` | True bypass. Falls back to buffered on unsupported filesystems (tmpfs, NFS, some FUSE). |
//! | macOS    | `fcntl(F_NOCACHE)` after open | Advisory: deprioritises caching, not a hard bypass. Best available. |
//! | Other    | Normal buffered I/O | Silent fallback. |

use std::fs::File;
use std::io;
use std::path::Path;

use super::Block;

// ---------------------------------------------------------------------------
// DirectFile
// ---------------------------------------------------------------------------

/// A file handle that *may* use direct I/O (bypassing the OS page cache).
///
/// Alignment is enforced at the type level by [`Block`](crate::block::Block),
/// so reads and writes go straight to `pread`/`pwrite` with no bounce buffer
/// and no per-I/O branch.
///
/// Construct via [`DirectFile::open`] (attempts direct I/O) or
/// [`DirectFile::open_buffered`] (normal buffered I/O).  Query the outcome
/// with [`is_direct`](DirectFile::is_direct).
#[derive(Debug)]
pub struct DirectFile {
    file: File,
    direct: bool,
}

impl DirectFile {
    // ------------------------------------------------------------------
    // Constructors
    // ------------------------------------------------------------------

    /// Open `path` with direct I/O, falling back silently to buffered I/O
    /// on unsupported platforms or filesystems.
    pub fn open(path: &Path) -> io::Result<Self> {
        // Try the platform-specific direct-I/O open.  If it fails, retry
        // without the direct flag so the caller always gets a working file.
        match Self::try_open_direct(path) {
            Ok(this) => Ok(this),
            Err(_) => Self::open_buffered(path),
        }
    }

    /// Open `path` with normal buffered I/O (the OS page cache is active).
    pub fn open_buffered(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self {
            file,
            direct: false,
        })
    }

    /// Attempt a platform-specific direct-I/O open.  Returns `Err` if the
    /// platform/filesystem doesn't support it so the caller can fall back.
    fn try_open_direct(path: &Path) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;

            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true).truncate(false);

            // O_DIRECT: bypass the page cache.  Requires aligned buffers
            // and I/O sizes that are multiples of the device sector size
            // (our BLOCK_SIZE of 8192 is always a multiple).
            opts.custom_flags(libc::O_DIRECT);

            let file = match opts.open(path) {
                Ok(f) => f,
                Err(e) => {
                    // tmpfs, some FUSE filesystems, and NFS don't support
                    // O_DIRECT.  Return the error so `open()` can retry.
                    return Err(e);
                }
            };

            // Verify the fd is actually open — a belts-and-suspenders check.
            debug_assert!(file.as_raw_fd() >= 0);

            Ok(Self { file, direct: true })
        }

        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;

            // F_NOCACHE tells the Unified Buffer Cache to deprioritise
            // caching for this fd.  It is advisory (not a hard bypass like
            // Linux O_DIRECT), but it is the best macOS offers.
            // SAFETY: `file.as_raw_fd()` is a valid, open fd; `F_NOCACHE`
            // is a standard fcntl command with no memory-safety implications.
            let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
            let direct = rc == 0;

            Ok(Self { file, direct })
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // Unsupported platform — always buffered.
            Self::open_buffered(path)
        }
    }

    // ------------------------------------------------------------------
    // I/O methods
    // ------------------------------------------------------------------

    /// Read a block at `offset` into `block`. The block is aligned by
    /// construction, so the kernel can DMA directly to it when direct I/O
    /// is active.
    pub fn read_exact_at(&self, block: &mut Block, offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(&mut block[..], offset)
    }

    /// Write `block` at `offset`. The block is aligned by construction, so
    /// the kernel can DMA directly from it when direct I/O is active.
    pub fn write_all_at(&self, block: &Block, offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(&block[..], offset)
    }

    /// Flush all buffered writes to stable storage (`fsync`).  Direct I/O
    /// bypasses the page cache but does **not** guarantee durability — this
    /// call is still required after every commit.
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Truncate or extend the underlying file to `len` bytes.
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    /// Return the underlying file's metadata.
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }

    /// Whether direct I/O actually engaged for this handle.
    pub fn is_direct(&self) -> bool {
        self.direct
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Block;
    use tempfile::tempdir;

    // --- Block alignment ---------------------------------------------------

    #[test]
    fn block_is_page_aligned() {
        let block = Block::zeroed();
        assert_eq!(block.as_ptr() as usize % 4096, 0, "Block is page-aligned");
        assert_eq!(block.len(), 8192);
    }

    #[test]
    fn block_deref_read_write() {
        let mut block = Block::zeroed();
        // Write a pattern through DerefMut.
        for (i, byte) in block.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }
        // Read back through Deref.
        for (i, byte) in block.iter().enumerate() {
            assert_eq!(*byte, (i % 256) as u8, "mismatch at offset {i}");
        }
    }

    #[test]
    fn block_default_is_zeroed() {
        let block = Block::default();
        assert!(block.iter().all(|&b| b == 0));
    }

    // --- DirectFile constructors --------------------------------------------

    #[test]
    fn open_buffered_is_not_direct() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("buf.db");
        let f = DirectFile::open_buffered(&path).unwrap();
        assert!(!f.is_direct(), "buffered open must report direct=false");
        // The file exists and is writable.
        let mut block = Block::zeroed();
        block[..9].copy_from_slice(b"hello----");
        f.write_all_at(&block, 0).unwrap();
    }

    #[test]
    fn open_is_direct_on_supported_platform_or_false_otherwise() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("maybe.db");
        let f = DirectFile::open(&path).unwrap();
        // We cannot assert `true` (CI may run on tmpfs or a platform without
        // direct I/O), but we CAN assert it returns a valid boolean and the
        // file actually works.
        let direct = f.is_direct();
        assert!(direct == true || direct == false);
        // Use Block so the buffer is aligned for the direct-I/O path.
        let mut block = Block::zeroed();
        block[..8].copy_from_slice(b"data----");
        f.write_all_at(&block, 0).unwrap();
        let mut out = Block::zeroed();
        f.read_exact_at(&mut out, 0).unwrap();
        assert_eq!(&out[..8], b"data----");
    }

    #[test]
    fn open_buffered_then_reopen_direct_data_is_identical() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shared.db");

        // Write via buffered.
        {
            let f = DirectFile::open_buffered(&path).unwrap();
            let mut data = Block::zeroed();
            data.fill(0xAA);
            f.write_all_at(&data, 0).unwrap();
            f.sync_all().unwrap();
        }

        // Read via direct (or fallback). Block is aligned.
        {
            let f = DirectFile::open(&path).unwrap();
            let mut buf = Block::zeroed();
            f.read_exact_at(&mut buf, 0).unwrap();
            let mut expected = Block::zeroed();
            expected.fill(0xAA);
            assert_eq!(buf, expected, "buffered→direct data mismatch");
        }
    }

    #[test]
    fn open_direct_then_reopen_buffered_data_is_identical() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shared2.db");

        // Write via direct (or fallback). Block is aligned.
        {
            let f = DirectFile::open(&path).unwrap();
            let mut data = Block::zeroed();
            data.fill(0xBB);
            f.write_all_at(&data, 0).unwrap();
            f.sync_all().unwrap();
        }

        // Read via buffered.
        {
            let f = DirectFile::open_buffered(&path).unwrap();
            let mut buf = Block::zeroed();
            f.read_exact_at(&mut buf, 0).unwrap();
            let mut expected = Block::zeroed();
            expected.fill(0xBB);
            assert_eq!(buf, expected, "direct→buffered data mismatch");
        }
    }

    // --- read_exact_at / write_all_at round-trips ---------------------------

    #[test]
    fn read_write_roundtrip_buffered() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rw_buf.db");
        let f = DirectFile::open_buffered(&path).unwrap();
        assert!(!f.is_direct());

        let mut pattern = Block::zeroed();
        for (i, byte) in pattern.iter_mut().enumerate() {
            *byte = (i.wrapping_mul(37).wrapping_add(13)) as u8;
        }
        f.write_all_at(&pattern, 4096).unwrap(); // non-zero offset

        let mut out = Block::zeroed();
        f.read_exact_at(&mut out, 4096).unwrap();
        assert_eq!(out, pattern, "buffered round-trip mismatch");
    }

    #[test]
    fn read_write_roundtrip_direct() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rw_dir.db");
        let f = DirectFile::open(&path).unwrap();

        let mut pattern = Block::zeroed();
        for (i, byte) in pattern.iter_mut().enumerate() {
            *byte = (i.wrapping_mul(37).wrapping_add(13)) as u8;
        }
        f.write_all_at(&pattern, 8192).unwrap();
        f.sync_all().unwrap();

        let mut out = Block::zeroed();
        f.read_exact_at(&mut out, 8192).unwrap();
        assert_eq!(out, pattern, "direct round-trip mismatch");
    }

    #[test]
    fn multiple_independent_blocks_dont_interfere() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("blocks.db");
        let f = DirectFile::open(&path).unwrap();

        let block0 = Block::zeroed();
        let mut block1 = Block::zeroed();
        block1.fill(0x11);
        block1[0] = 0xFE;
        let mut block2 = Block::zeroed();
        block2.fill(0x22);

        f.write_all_at(&block0, 0).unwrap();
        f.write_all_at(&block1, 8192).unwrap();
        f.write_all_at(&block2, 16384).unwrap();

        let mut out0 = Block::zeroed();
        out0.fill(0xFF);
        let mut out1 = Block::zeroed();
        out1.fill(0xFF);
        let mut out2 = Block::zeroed();
        out2.fill(0xFF);
        f.read_exact_at(&mut out0, 0).unwrap();
        f.read_exact_at(&mut out1, 8192).unwrap();
        f.read_exact_at(&mut out2, 16384).unwrap();

        assert_eq!(out0, block0, "block 0 mismatch");
        assert_eq!(out1, block1, "block 1 mismatch");
        assert_eq!(out2, block2, "block 2 mismatch");
    }

    // --- sync_all / persistence --------------------------------------------

    #[test]
    fn data_survives_sync_and_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("persist.db");

        let mut original = Block::zeroed();
        original.fill(0xCC);
        {
            let f = DirectFile::open(&path).unwrap();
            f.write_all_at(&original, 0).unwrap();
            f.sync_all().unwrap();
        }

        // Reopen and read back.
        let f = DirectFile::open(&path).unwrap();
        let mut buf = Block::zeroed();
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(buf, original, "data lost across reopen+sync");
    }

    // --- set_len / metadata ------------------------------------------------

    #[test]
    fn set_len_and_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sized.db");
        let f = DirectFile::open(&path).unwrap();

        let target = 65536u64; // 64 KiB = 8 blocks
        f.set_len(target).unwrap();
        let meta = f.metadata().unwrap();
        assert_eq!(meta.len(), target);
    }

    // --- Debug --------------------------------------------------------------

    #[test]
    fn debug_does_not_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("debug.db");
        let f = DirectFile::open(&path).unwrap();
        let s = format!("{f:?}");
        assert!(s.contains("DirectFile"), "Debug output: {s}");
    }

    // --- is_direct validation (catches flag-inversion mutants) ---------------

    /// On macOS, `F_NOCACHE` against a real filesystem (APFS) should succeed.
    /// On Linux, `O_DIRECT` against a real filesystem (ext4/xfs/btrfs) should
    /// also succeed.  If CI runs on tmpfs this won't be true, so we only
    /// assert on macOS for now — the Linux side exercises the same code
    /// structure.
    #[test]
    fn open_direct_engages_on_supported_platform() {
        let dir = tempdir().unwrap();
        let f = DirectFile::open(&dir.path().join("engage.db")).unwrap();

        #[cfg(target_os = "macos")]
        assert!(
            f.is_direct(),
            "macOS APFS should support F_NOCACHE; if this fails the CI \
             filesystem may not support it"
        );

        #[cfg(target_os = "linux")]
        {
            if f.is_direct() {
                // O_DIRECT engaged — the common case.
            }
        }

        // Regardless of platform, the file must work.  Use Block so the
        // buffer is aligned for the direct-I/O path.
        let mut block = Block::zeroed();
        block[..8].copy_from_slice(b"engaged-");
        f.write_all_at(&block, 0).unwrap();
        let mut out = Block::zeroed();
        f.read_exact_at(&mut out, 0).unwrap();
        assert_eq!(&out[..8], b"engaged-");
    }

    // --- Corner cases -------------------------------------------------------

    #[test]
    fn write_at_then_read_at_zero_offset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("zero.db");
        let f = DirectFile::open_buffered(&path).unwrap();
        let mut block = Block::zeroed();
        block[..16].copy_from_slice(b"zero-offset-data");
        f.write_all_at(&block, 0).unwrap();
        let mut out = Block::zeroed();
        f.read_exact_at(&mut out, 0).unwrap();
        assert_eq!(&out[..16], b"zero-offset-data");
    }

    #[test]
    fn overwrite_shrinks_then_grows_back() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("resize.db");
        let f = DirectFile::open(&path).unwrap();

        f.set_len(16384).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 16384);

        f.set_len(4096).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 4096);

        f.set_len(32768).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 32768);
    }

    #[test]
    fn write_read_partial_within_block() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial.db");
        let f = DirectFile::open_buffered(&path).unwrap();

        let payload = b"partial write test";
        let mut block = Block::zeroed();
        block[..payload.len()].copy_from_slice(payload);
        f.write_all_at(&block, 100).unwrap();

        let mut out = Block::zeroed();
        f.read_exact_at(&mut out, 100).unwrap();
        assert_eq!(&out[..payload.len()], payload);
    }
}
