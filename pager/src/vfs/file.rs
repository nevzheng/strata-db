//! A local-file [`Vfs`]: one file, blocks at `id * BLOCK_SIZE`.
//!
//! Block 0 is a **superblock** holding the allocation high-water mark, so issued
//! `PageId`s survive restarts and are never reused. User pages start at id 1.
//!
//! Physical addressing is the identity map `id → id * BLOCK_SIZE`. That is the
//! VFS's private business — callers only ever see `PageId`s — and a future
//! free-list or relocating allocator can change it without touching anything
//! above this layer.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt; // positional read/write — no shared cursor to coordinate
use std::path::Path;

use super::{BLOCK_SIZE, Vfs};
use crate::error::PageError;
use crate::{PageId, Result};

const SUPER_MAGIC: [u8; 4] = *b"SVFS";

/// A `Vfs` backed by a single local file.
#[derive(Debug)]
pub struct FileVfs {
    file: File,
    /// Next id to hand out. Persisted in the superblock on [`sync`](Vfs::sync).
    next_id: u64,
}

impl FileVfs {
    /// Open (creating if absent) the store at `path`. On a fresh file the
    /// superblock is initialized; on an existing one the high-water mark is
    /// recovered from it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let vfs = if file.metadata()?.len() == 0 {
            let vfs = Self { file, next_id: 1 };
            vfs.file.set_len(BLOCK_SIZE as u64)?; // reserve the superblock
            vfs.write_superblock()?;
            vfs.file.sync_all()?;
            vfs
        } else {
            let mut vfs = Self { file, next_id: 0 };
            vfs.next_id = vfs.read_superblock()?;
            vfs
        };
        debug_assert!(vfs.next_id >= 1);
        Ok(vfs)
    }

    fn write_superblock(&self) -> Result<()> {
        let mut block = vec![0u8; BLOCK_SIZE];
        block[0..4].copy_from_slice(&SUPER_MAGIC);
        block[4..12].copy_from_slice(&self.next_id.to_be_bytes());
        self.file.write_all_at(&block, 0)?;
        Ok(())
    }

    fn read_superblock(&self) -> Result<u64> {
        let mut block = vec![0u8; BLOCK_SIZE];
        self.file.read_exact_at(&mut block, 0)?;
        if block[0..4] != SUPER_MAGIC {
            return Err(PageError::BadMagic);
        }
        Ok(u64::from_be_bytes(block[4..12].try_into().unwrap()))
    }

    fn offset(id: PageId) -> u64 {
        id.0 * BLOCK_SIZE as u64
    }
}

impl Vfs for FileVfs {
    fn allocate(&mut self) -> Result<PageId> {
        let id = PageId(self.next_id);
        self.next_id += 1;
        // Grow the file so the block exists before anyone reads it. The new
        // high-water mark is not durable until `sync`.
        self.file.set_len(Self::offset(id) + BLOCK_SIZE as u64)?;
        Ok(id)
    }

    fn ensure_allocated(&mut self, id: PageId) -> Result<()> {
        if id.0 >= self.next_id {
            self.next_id = id.0 + 1;
            self.file.set_len(Self::offset(id) + BLOCK_SIZE as u64)?;
        }
        Ok(())
    }

    fn read(&self, id: PageId, buf: &mut [u8]) -> Result<()> {
        check_len(buf.len())?;
        self.file.read_exact_at(buf, Self::offset(id))?;
        Ok(())
    }

    fn write(&mut self, id: PageId, buf: &[u8]) -> Result<()> {
        check_len(buf.len())?;
        self.file.write_all_at(buf, Self::offset(id))?;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        // Persist the high-water mark, then fsync — so a recovered store never
        // re-issues an id that named durable data.
        self.write_superblock()?;
        self.file.sync_all()?;
        Ok(())
    }

    fn block_count(&self) -> u64 {
        self.next_id
    }
}

fn check_len(got: usize) -> Result<()> {
    if got != BLOCK_SIZE {
        return Err(PageError::BadBlockSize {
            expected: BLOCK_SIZE,
            got,
        });
    }
    Ok(())
}
