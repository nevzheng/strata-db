//! A local-file [`Vfs`]: one file, blocks at `id * BLOCK_SIZE`.
//!
//! Block 0 is a **superblock** holding the allocation high-water mark and the
//! free list, so both survive restarts. User pages start at id 1. A fresh id is
//! never reused while live; freed ids are recycled through the free list.
//!
//! Physical addressing is the identity map `id → id * BLOCK_SIZE`. That is the
//! VFS's private business — callers only ever see `PageId`s — and a future
//! relocating allocator can change it without touching anything above this
//! layer.

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
    /// Freed block ids available for reuse. [`allocate`](Vfs::allocate)
    /// drains this before growing the file. Persisted in the superblock
    /// on [`sync`](Vfs::sync). Empty today — nothing calls
    /// [`free`](Vfs::free) yet (see its safety note).
    free_list: Vec<u64>,
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
                free_list: Vec::new(),
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
                free_list: Vec::new(),
            };
            let (next_id, free_list) = vfs.read_superblock()?;
            vfs.next_id = next_id;
            vfs.free_list = free_list;
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

    /// Superblock layout: `magic(4) | next_id(8) | free_count(8) | free
    /// ids (8 each)`. The free list lives inline, so it is bounded by the
    /// block size; spilling it to dedicated free-space-map pages is
    /// future work.
    fn write_superblock(&self) -> Result<()> {
        let max_free = (BLOCK_SIZE - 20) / 8;
        if self.free_list.len() > max_free {
            return Err(PageError::FreeListOverflow {
                len: self.free_list.len(),
                max: max_free,
            });
        }
        let mut block = vec![0u8; BLOCK_SIZE];
        block[0..4].copy_from_slice(&SUPER_MAGIC);
        block[4..12].copy_from_slice(&self.next_id.to_be_bytes());
        block[12..20].copy_from_slice(&(self.free_list.len() as u64).to_be_bytes());
        let mut off = 20;
        for &id in &self.free_list {
            block[off..off + 8].copy_from_slice(&id.to_be_bytes());
            off += 8;
        }
        self.file.write_all_at(&block, 0)?;
        Ok(())
    }

    fn read_superblock(&self) -> Result<(u64, Vec<u64>)> {
        let mut block = vec![0u8; BLOCK_SIZE];
        self.file.read_exact_at(&mut block, 0)?;
        if block[0..4] != SUPER_MAGIC {
            return Err(PageError::BadMagic);
        }
        let next_id = u64::from_be_bytes(block[4..12].try_into().unwrap());
        let free_count = u64::from_be_bytes(block[12..20].try_into().unwrap()) as usize;
        let mut free_list = Vec::with_capacity(free_count);
        let mut off = 20;
        for _ in 0..free_count {
            free_list.push(u64::from_be_bytes(block[off..off + 8].try_into().unwrap()));
            off += 8;
        }
        Ok((next_id, free_list))
    }

    fn offset(id: PageId) -> u64 {
        id.0 * BLOCK_SIZE as u64
    }
}

impl Vfs for FileVfs {
    fn allocate(&mut self) -> Result<PageId> {
        // Reuse a freed block first — it is already within the file's
        // grown capacity, so no `set_len` is needed.
        if let Some(id) = self.free_list.pop() {
            return Ok(PageId(id));
        }
        let id = PageId(self.next_id);
        self.next_id += 1;
        // Grow the file (by a chunk) so the block exists before anyone
        // reads it. The new high-water mark is not durable until `sync`.
        self.ensure_capacity(self.next_id)?;
        Ok(id)
    }

    fn free(&mut self, id: PageId) {
        // Durable once `sync` rewrites the superblock.
        self.free_list.push(id.0);
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
    fn allocate_reuses_freed_blocks_before_growing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut vfs = FileVfs::open(&path).unwrap();

        let a = vfs.allocate().unwrap();
        let b = vfs.allocate().unwrap();
        let high_water = vfs.next_id;

        // Free both; the next allocations reuse them instead of growing
        // the id space.
        vfs.free(a);
        vfs.free(b);
        let reused1 = vfs.allocate().unwrap();
        let reused2 = vfs.allocate().unwrap();
        assert_eq!([reused1, reused2], [b, a]); // LIFO
        assert_eq!(vfs.next_id, high_water, "no fresh ids were issued");

        // Free list drained: the next allocation issues a fresh id.
        let fresh = vfs.allocate().unwrap();
        assert_eq!(fresh.0, high_water);
    }

    #[test]
    fn free_list_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");

        let freed = {
            let mut vfs = FileVfs::open(&path).unwrap();
            let _a = vfs.allocate().unwrap();
            let b = vfs.allocate().unwrap();
            vfs.free(b);
            vfs.sync().unwrap();
            b
        };

        // Reopen recovers the free list, so the freed id is reused first.
        let mut vfs = FileVfs::open(&path).unwrap();
        assert_eq!(vfs.allocate().unwrap(), freed);
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
