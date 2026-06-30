//! An in-memory [`BlockStore`] — blocks are `Vec` entries. Non-durable (`sync` is a
//! no-op); for tests and ephemeral stores.

use super::{Block, BlockStore};
use crate::error::Error;
use crate::{BlockId, Result};

/// Blocks held in memory, indexed by `BlockId`. Block 0 is reserved (so the
/// first issued id is 1), mirroring [`FileBlockStore`](super::FileBlockStore)'s superblock and
/// keeping ids consistent if a store is later moved between backends.
#[derive(Debug)]
pub struct MemBlockStore {
    blocks: Vec<Block>,
    next_id: u64,
    /// Freed block ids available for reuse, mirroring [`FileBlockStore`](super::FileBlockStore).
    free_list: Vec<u64>,
}

impl Default for MemBlockStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemBlockStore {
    /// A fresh, empty in-memory block store.
    pub fn new() -> Self {
        Self {
            blocks: vec![zeroed()], // block 0 reserved
            next_id: 1,
            free_list: Vec::new(),
        }
    }
}

impl BlockStore for MemBlockStore {
    fn allocate(&mut self) -> Result<BlockId> {
        if let Some(id) = self.free_list.pop() {
            return Ok(BlockId(id)); // block already exists; reuse it in place
        }
        let id = self.next_id;
        self.next_id += 1;
        self.blocks.push(zeroed());
        Ok(BlockId(id))
    }

    fn free(&mut self, id: BlockId) {
        self.free_list.push(id.0);
    }

    fn ensure_allocated(&mut self, id: BlockId) -> Result<()> {
        while self.blocks.len() as u64 <= id.0 {
            self.blocks.push(zeroed());
        }
        self.next_id = self.next_id.max(id.0 + 1);
        Ok(())
    }

    fn read(&self, id: BlockId, block: &mut Block) -> Result<()> {
        let stored = self.blocks.get(id.0 as usize).ok_or(Error::Checksum(id))?;
        block.copy_from_slice(&stored[..]);
        Ok(())
    }

    fn write(&mut self, id: BlockId, block: &Block) -> Result<()> {
        let stored = self
            .blocks
            .get_mut(id.0 as usize)
            .ok_or(Error::Checksum(id))?;
        stored.copy_from_slice(&block[..]);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    fn block_count(&self) -> u64 {
        self.next_id
    }
}

fn zeroed() -> Block {
    Block::zeroed()
}
