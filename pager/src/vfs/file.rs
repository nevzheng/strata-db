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

/// Grow the file this many blocks at a time. A run of allocations then
/// costs one `set_len` per chunk instead of one per block. A tuning knob.
const GROWTH_CHUNK_BLOCKS: u64 = 16;

/// A `Vfs` backed by a single local file.
#[derive(Debug)]
pub struct FileVfs {
    file: File,
    /// Next id to hand out. Persisted in the superblock on [`sync`](Vfs::sync).
    next_id: u64,
    /// Blocks the file is physically grown to hold (including the
    /// superblock). Allocation grows this by whole chunks; derived from
    /// the file length on open, not persisted separately.
    capacity_blocks: u64,
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

        let len = file.metadata()?.len();
        let vfs = if len == 0 {
            let mut vfs = Self {
                file,
                next_id: 1,
                capacity_blocks: 0,
            };
            vfs.set_capacity(1)?; // reserve the superblock
            vfs.write_superblock()?;
            vfs.file.sync_all()?;
            vfs
        } else {
            let mut vfs = Self {
                file,
                next_id: 0,
                capacity_blocks: len / BLOCK_SIZE as u64,
            };
            vfs.next_id = vfs.read_superblock()?;
            vfs
        };
        debug_assert!(vfs.next_id >= 1);
        Ok(vfs)
    }

    /// Physically size the file to exactly `blocks` blocks.
    fn set_capacity(&mut self, blocks: u64) -> Result<()> {
        self.file.set_len(blocks * BLOCK_SIZE as u64)?;
        self.capacity_blocks = blocks;
        Ok(())
    }

    /// Ensure the file holds at least `blocks` blocks, growing by whole
    /// chunks when it falls short. The grown high-water mark is durable
    /// only after [`sync`](Vfs::sync) persists the superblock.
    fn ensure_capacity(&mut self, blocks: u64) -> Result<()> {
        if blocks > self.capacity_blocks {
            self.set_capacity(blocks.next_multiple_of(GROWTH_CHUNK_BLOCKS))?;
        }
        Ok(())
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
        // Grow the file (by a chunk) so the block exists before anyone
        // reads it. The new high-water mark is not durable until `sync`.
        self.ensure_capacity(self.next_id)?;
        Ok(id)
    }

    fn ensure_allocated(&mut self, id: PageId) -> Result<()> {
        if id.0 >= self.next_id {
            self.next_id = id.0 + 1;
            self.ensure_capacity(self.next_id)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn allocates_across_chunks_and_survives_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");

        let last = {
            let mut vfs = FileVfs::open(&path).unwrap();
            // Allocate well past one growth chunk.
            let mut last = PageId(0);
            for _ in 0..(GROWTH_CHUNK_BLOCKS * 2 + 3) {
                last = vfs.allocate().unwrap();
            }
            // The file grew by whole chunks and covers every issued id.
            assert!(vfs.capacity_blocks >= vfs.next_id);
            assert_eq!(vfs.capacity_blocks % GROWTH_CHUNK_BLOCKS, 0);
            vfs.sync().unwrap();
            last
        };

        // Reopen: the high-water mark persists, so the next id continues
        // rather than colliding with an already-issued one.
        let mut vfs = FileVfs::open(&path).unwrap();
        assert_eq!(vfs.allocate().unwrap().0, last.0 + 1);
    }

    #[test]
    fn writes_survive_chunked_growth() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut vfs = FileVfs::open(&path).unwrap();

        // Allocate across a chunk boundary, write a marker to the last one.
        let mut id = PageId(0);
        for _ in 0..GROWTH_CHUNK_BLOCKS + 1 {
            id = vfs.allocate().unwrap();
        }
        let mut block = vec![0u8; BLOCK_SIZE];
        block[..4].copy_from_slice(b"MARK");
        vfs.write(id, &block).unwrap();

        let mut read = vec![0u8; BLOCK_SIZE];
        vfs.read(id, &mut read).unwrap();
        assert_eq!(&read[..4], b"MARK");
    }
}
