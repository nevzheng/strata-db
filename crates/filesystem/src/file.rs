//! `File` — a file handle whose I/O policy is a value ([`FileOptions`]) the
//! caller passes, not a constructor baked into each layer. The one `unsafe`
//! FFI call enabling direct I/O lives here instead of leaking into every store.
//!
//! Direct I/O needs sector-aligned buffers, offsets, and lengths. `File` does
//! not enforce that — it stays generic over `&[u8]`; callers that request
//! direct (the block store, via [`Block`](crate::Block)) must pass aligned bytes.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default)]
pub struct FileOptions {
    /// Best-effort: if the platform or filesystem rejects direct I/O the file
    /// opens buffered instead — [`File::is_direct`] reports the outcome.
    pub direct: bool,
}

#[derive(Debug)]
pub struct File {
    inner: std::fs::File,
    direct: bool,
}

impl File {
    pub fn open(path: impl AsRef<Path>, opts: FileOptions) -> io::Result<Self> {
        let path = path.as_ref();
        if opts.direct {
            // A rejected direct open is recoverable, so fall back to buffered.
            if let Ok(file) = Self::open_direct(path) {
                return Ok(file);
            }
        }
        Ok(Self {
            inner: base_opts().open(path)?,
            direct: false,
        })
    }

    pub fn is_direct(&self) -> bool {
        self.direct
    }

    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_exact_at(buf, offset)
    }

    pub fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.inner.write_all_at(buf, offset)
    }

    pub fn size(&self) -> io::Result<u64> {
        Ok(self.inner.metadata()?.len())
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }

    /// Direct I/O bypasses the page cache but does not imply durability; this
    /// is still the sync point.
    pub fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }

    #[cfg(target_os = "linux")]
    fn open_direct(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let inner = base_opts().custom_flags(libc::O_DIRECT).open(path)?;
        Ok(Self {
            inner,
            direct: true,
        })
    }

    #[cfg(target_os = "macos")]
    fn open_direct(path: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let inner = base_opts().open(path)?;
        // F_NOCACHE is advisory; a non-zero return still leaves a usable file.
        // SAFETY: the fd is open and valid; F_NOCACHE has no memory effects.
        let direct = unsafe { libc::fcntl(inner.as_raw_fd(), libc::F_NOCACHE, 1) } == 0;
        Ok(Self { inner, direct })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn open_direct(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: base_opts().open(path)?,
            direct: false,
        })
    }
}

fn base_opts() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    opts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Block;
    use tempfile::tempdir;

    #[test]
    fn buffered_roundtrip_len_and_sync() {
        let dir = tempdir().unwrap();
        let f = File::open(dir.path().join("f"), FileOptions::default()).unwrap();
        assert!(!f.is_direct());

        f.set_len(16).unwrap();
        assert_eq!(f.size().unwrap(), 16);

        f.write_all_at(b"abcd", 0).unwrap();
        f.sync_all().unwrap();
        let mut buf = [0u8; 4];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"abcd");
    }

    #[test]
    fn direct_open_yields_a_working_file() {
        let dir = tempdir().unwrap();
        let f = File::open(dir.path().join("f"), FileOptions { direct: true }).unwrap();

        // Don't assert is_direct — whether it engages is filesystem-dependent.
        // A Block supplies the alignment O_DIRECT requires on Linux.
        let mut w = Block::zeroed();
        w[..5].copy_from_slice(b"hello");
        f.write_all_at(&w, 0).unwrap();
        f.sync_all().unwrap();

        let mut r = Block::zeroed();
        f.read_exact_at(&mut r, 0).unwrap();
        assert_eq!(&r[..5], b"hello");
    }
}
