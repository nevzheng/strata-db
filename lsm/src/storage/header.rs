//! The SSTable header — the on-page form of an [`SsTableRef`].
//!
//! It holds the cheap, always-loaded metadata (id, range, bloom, size) from
//! the front of a file, so a lookup can bound or skip the table without
//! reading its data. `Header` and `SsTableRef` are the same concept in two
//! representations and convert into each other; `Header` owns the on-disk
//! encoding, `SsTableRef` is the in-memory handle the tree holds.

use super::codec::{self, Decode, DecodeError, Encode};
use crate::{BloomFilter, KeyRange, SsTableId, SsTableRef};

const MAGIC: u32 = 0x5353_5431; // "SST1"
const VERSION: u16 = 1;

/// An SSTable file's header.
///
/// ```text
/// ┌────────────┬──────────────┬─────────────┐
/// │ magic (4B) │ version (2B) │ sst_id (8B) │
/// ├────────────────┬──────────┼─────────────┴──┬──────────┐
/// │ min_key_len(4B)│ min_key  │ max_key_len(4B)│ max_key  │
/// ├────────────────┼──────────┴────┬───────────┴──────────┤
/// │ size_bytes(8B) │ num_hashes(4B)│ block_count (4B)     │
/// ├────────────────┴───────────────┴──────────────────────┤
/// │ bloom blocks (8B × block_count)                        │
/// └────────────────────────────────────────────────────────┘
/// ```
/// (Prefixed by `magic`/`version` so a corrupt or foreign file is rejected.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub sst_id: SsTableId,
    pub range: KeyRange,
    pub bloom: BloomFilter,
    pub size_bytes: u64,
}

impl From<&SsTableRef> for Header {
    fn from(r: &SsTableRef) -> Self {
        Header {
            sst_id: r.id,
            range: r.range.clone(),
            bloom: r.bloom.clone(),
            size_bytes: r.size_bytes,
        }
    }
}

impl From<Header> for SsTableRef {
    fn from(h: Header) -> Self {
        SsTableRef {
            id: h.sst_id,
            range: h.range,
            bloom: h.bloom,
            size_bytes: h.size_bytes,
        }
    }
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
        Ok(Header {
            sst_id,
            range: KeyRange { min, max },
            bloom,
            size_bytes,
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
    fn converts_to_and_from_sstable_ref() {
        let header = sample_header();
        let r: SsTableRef = header.clone().into();
        let back: Header = (&r).into();
        assert_eq!(back, header);
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
