//! The SSTable header — the cheap, always-loaded metadata at the *end* of a
//! file: id, whole-table key range, bloom, data size, and the **block index**.
//! A lookup uses it to bound/skip the table and locate the right block
//! without scanning data.
//!
//! This is the physical home of the bloom and key ranges; the logical tree
//! holds only the [`SsTableId`], which the cache resolves to this header.
//!
//! ```text
//! header:
//! | magic(4) | version(2) | sst_id(8) | min_key | max_key | size(8)
//! | bloom: num_hashes(4) | block_count(4) | blocks(8 each)
//! | index: count(4) | per block: min_key | max_key | offset(8) | len(4) |
//! ```

use super::bloom::BloomFilter;
use super::codec::{self, Decode, DecodeError, Encode};
use crate::{KeyRange, SsTableId};

const MAGIC: u32 = 0x5353_5431; // "SST1"
const VERSION: u16 = 1;

/// Index entry for one data block: its key range and where its bytes sit in
/// the file's data section (offset from the section start, plus length).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockMeta {
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
    pub offset: u64,
    pub len: u32,
}

/// An SSTable file's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub sst_id: SsTableId,
    pub range: KeyRange,
    pub bloom: BloomFilter,
    pub size_bytes: u64,
    pub blocks: Vec<BlockMeta>,
}

impl Encode for Header {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.extend_from_slice(&VERSION.to_be_bytes());
        out.extend_from_slice(&self.sst_id.0.to_be_bytes());
        codec::put_bytes(out, &self.range.min);
        codec::put_bytes(out, &self.range.max);
        out.extend_from_slice(&self.size_bytes.to_be_bytes());
        encode_bloom(out, &self.bloom);
        out.extend_from_slice(&(self.blocks.len() as u32).to_be_bytes());
        for b in &self.blocks {
            codec::put_bytes(out, &b.min_key);
            codec::put_bytes(out, &b.max_key);
            out.extend_from_slice(&b.offset.to_be_bytes());
            out.extend_from_slice(&b.len.to_be_bytes());
        }
    }
}

impl Decode for Header {
    fn decode(bytes: &mut &[u8]) -> Result<Self, DecodeError> {
        let magic = codec::get_u32(bytes)?;
        if magic != MAGIC {
            return Err(DecodeError::BadMagic {
                expected: MAGIC,
                got: magic,
            });
        }
        let version = codec::get_u16(bytes)?;
        if version != VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let sst_id = SsTableId(codec::get_u64(bytes)?);
        let min = codec::get_bytes(bytes)?.to_vec();
        let max = codec::get_bytes(bytes)?.to_vec();
        let size_bytes = codec::get_u64(bytes)?;
        let bloom = decode_bloom(bytes)?;

        let block_count = codec::get_u32(bytes)? as usize;
        let mut blocks = Vec::with_capacity(block_count);
        for _ in 0..block_count {
            let min_key = codec::get_bytes(bytes)?.to_vec();
            let max_key = codec::get_bytes(bytes)?.to_vec();
            let offset = codec::get_u64(bytes)?;
            let len = codec::get_u32(bytes)?;
            blocks.push(BlockMeta {
                min_key,
                max_key,
                offset,
                len,
            });
        }

        Ok(Header {
            sst_id,
            range: KeyRange { min, max },
            bloom,
            size_bytes,
            blocks,
        })
    }
}

/// `| num_hashes (4B) | block_count (4B) | blocks (8B each) |`
fn encode_bloom(out: &mut Vec<u8>, bloom: &BloomFilter) {
    out.extend_from_slice(&bloom.num_hashes().to_be_bytes());
    let blocks = bloom.blocks();
    out.extend_from_slice(&(blocks.len() as u32).to_be_bytes());
    for block in blocks {
        out.extend_from_slice(&block.to_be_bytes());
    }
}

fn decode_bloom(bytes: &mut &[u8]) -> Result<BloomFilter, DecodeError> {
    let num_hashes = codec::get_u32(bytes)?;
    let count = codec::get_u32(bytes)? as usize;
    let mut blocks = Vec::with_capacity(count);
    for _ in 0..count {
        blocks.push(codec::get_u64(bytes)?);
    }
    Ok(BloomFilter::from_blocks(blocks, num_hashes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BloomConfig;
    use crate::storage::Page;

    fn sample_header() -> Header {
        let keys: Vec<&[u8]> = vec![b"a", b"m", b"z"];
        Header {
            sst_id: SsTableId(42),
            range: KeyRange {
                min: b"a".to_vec(),
                max: b"z".to_vec(),
            },
            bloom: BloomFilter::build(BloomConfig { bits_per_key: 10 }, keys.len(), keys),
            size_bytes: 4096,
            blocks: vec![
                BlockMeta {
                    min_key: b"a".to_vec(),
                    max_key: b"m".to_vec(),
                    offset: 0,
                    len: 2048,
                },
                BlockMeta {
                    min_key: b"n".to_vec(),
                    max_key: b"z".to_vec(),
                    offset: 2048,
                    len: 2048,
                },
            ],
        }
    }

    #[test]
    fn header_round_trips_through_a_page() {
        let header = sample_header();
        let page = Page::encode(&header);
        let decoded: Header = page.decode().unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let page = Page::new(vec![0u8; 16]);
        assert!(matches!(
            page.decode::<Header>(),
            Err(DecodeError::BadMagic { .. })
        ));
    }
}
