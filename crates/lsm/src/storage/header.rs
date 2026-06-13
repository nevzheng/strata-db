//! The SSTable header — the cheap metadata at the *end* of a file: id,
//! whole-table key range, bloom, data size, and the **block index** a lookup
//! uses to bound/skip the table and locate the right block without scanning.
//!
//! One header *page type*, two *roles*:
//!
//! - [`Header::Root`] is the file-tail header loaded on `open`. For a small
//!   table it carries the whole block index ([`Index::Inline`]); for a large
//!   one it is a directory of child headers ([`Index::Sharded`]) so the index
//!   never has to be one unbounded page.
//! - [`Header::Child`] is an index *shard* — a slice of the block index,
//!   faulted on demand through the header cache, that points at data blocks.
//!
//! Both [`ChunkRef`] (root → child) and [`BlockMeta`] (→ data block) are the
//! same shape — a key range plus where the bytes sit (`offset`/`len`) — so a
//! lookup binary-searches the same way at each level. Two levels only: a child
//! never points at another child.
//!
//! ```text
//! root:  | magic(4) | version(2) | role=ROOT(1) | sst_id(8) | min | max
//!        | size(8) | bloom | index_tag(1) | count(4) | metas… |
//! child: | magic(4) | version(2) | role=CHILD(1) | sst_id(8) | count(4) | metas… |
//! meta:  | min | max | offset(8) | len(4) |
//! ```

use super::bloom::BloomFilter;
use super::codec::{self, Decode, DecodeError, Encode};
use crate::{KeyRange, SsTableId};

const MAGIC: u32 = 0x5353_5431; // "SST1"
const VERSION: u16 = 2; // v2: role-tagged, partitionable index

const ROLE_ROOT: u8 = 0;
const ROLE_CHILD: u8 = 1;
const INDEX_INLINE: u8 = 0;
const INDEX_SHARDED: u8 = 1;

/// A key range plus where the bytes it covers sit in the file (`offset` from
/// the file start, plus `len`). Used for a data block; [`ChunkRef`] is the same
/// shape pointing at a child header instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockMeta {
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
    pub offset: u64,
    pub len: u32,
}

/// A root's pointer to one child header: the child's key span and where its
/// bytes sit in the file. Identical layout to [`BlockMeta`] — distinct name so
/// call sites read clearly (`ChunkRef` → child, `BlockMeta` → data block).
pub type ChunkRef = BlockMeta;

/// A root's block index: small tables keep it inline, large tables shard it
/// across child headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Index {
    /// The whole block index, in the root itself.
    Inline(Vec<BlockMeta>),
    /// Pointers to child headers, each indexing a contiguous slice of blocks.
    Sharded(Vec<ChunkRef>),
}

/// An SSTable file's header — either the file-tail [`Root`](Header::Root) or an
/// on-demand index [`Child`](Header::Child).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Header {
    Root {
        sst_id: SsTableId,
        range: KeyRange,
        bloom: BloomFilter,
        size_bytes: u64,
        index: Index,
    },
    Child {
        sst_id: SsTableId,
        blocks: Vec<BlockMeta>,
    },
}

impl Encode for Header {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.extend_from_slice(&VERSION.to_be_bytes());
        match self {
            Header::Root {
                sst_id,
                range,
                bloom,
                size_bytes,
                index,
            } => {
                out.push(ROLE_ROOT);
                out.extend_from_slice(&sst_id.0.to_be_bytes());
                codec::put_bytes(out, &range.min);
                codec::put_bytes(out, &range.max);
                out.extend_from_slice(&size_bytes.to_be_bytes());
                encode_bloom(out, bloom);
                match index {
                    Index::Inline(blocks) => {
                        out.push(INDEX_INLINE);
                        put_metas(out, blocks);
                    }
                    Index::Sharded(chunks) => {
                        out.push(INDEX_SHARDED);
                        put_metas(out, chunks);
                    }
                }
            }
            Header::Child { sst_id, blocks } => {
                out.push(ROLE_CHILD);
                out.extend_from_slice(&sst_id.0.to_be_bytes());
                put_metas(out, blocks);
            }
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
        let role = codec::get_u8(bytes)?;
        let sst_id = SsTableId(codec::get_u64(bytes)?);
        match role {
            ROLE_ROOT => {
                let min = codec::get_bytes(bytes)?.to_vec();
                let max = codec::get_bytes(bytes)?.to_vec();
                let size_bytes = codec::get_u64(bytes)?;
                let bloom = decode_bloom(bytes)?;
                let index = match codec::get_u8(bytes)? {
                    INDEX_INLINE => Index::Inline(get_metas(bytes)?),
                    INDEX_SHARDED => Index::Sharded(get_metas(bytes)?),
                    other => return Err(DecodeError::UnknownOpType(other)),
                };
                Ok(Header::Root {
                    sst_id,
                    range: KeyRange { min, max },
                    bloom,
                    size_bytes,
                    index,
                })
            }
            ROLE_CHILD => Ok(Header::Child {
                sst_id,
                blocks: get_metas(bytes)?,
            }),
            other => Err(DecodeError::UnknownOpType(other)),
        }
    }
}

/// `| count (4B) | per meta: min | max | offset(8) | len(4) |`
fn put_metas(out: &mut Vec<u8>, metas: &[BlockMeta]) {
    out.extend_from_slice(&(metas.len() as u32).to_be_bytes());
    for m in metas {
        codec::put_bytes(out, &m.min_key);
        codec::put_bytes(out, &m.max_key);
        out.extend_from_slice(&m.offset.to_be_bytes());
        out.extend_from_slice(&m.len.to_be_bytes());
    }
}

fn get_metas(bytes: &mut &[u8]) -> Result<Vec<BlockMeta>, DecodeError> {
    let count = codec::get_u32(bytes)? as usize;
    let mut metas = Vec::with_capacity(count);
    for _ in 0..count {
        let min_key = codec::get_bytes(bytes)?.to_vec();
        let max_key = codec::get_bytes(bytes)?.to_vec();
        let offset = codec::get_u64(bytes)?;
        let len = codec::get_u32(bytes)?;
        metas.push(BlockMeta {
            min_key,
            max_key,
            offset,
            len,
        });
    }
    Ok(metas)
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

    fn meta(min: &[u8], max: &[u8], offset: u64, len: u32) -> BlockMeta {
        BlockMeta {
            min_key: min.to_vec(),
            max_key: max.to_vec(),
            offset,
            len,
        }
    }

    fn sample_root(index: Index) -> Header {
        let keys: Vec<&[u8]> = vec![b"a", b"m", b"z"];
        Header::Root {
            sst_id: SsTableId(42),
            range: KeyRange {
                min: b"a".to_vec(),
                max: b"z".to_vec(),
            },
            bloom: BloomFilter::build(BloomConfig { bits_per_key: 10 }, keys.len(), keys),
            size_bytes: 4096,
            index,
        }
    }

    #[test]
    fn inline_root_round_trips_through_a_page() {
        let header = sample_root(Index::Inline(vec![
            meta(b"a", b"m", 0, 2048),
            meta(b"n", b"z", 2048, 2048),
        ]));
        let page = Page::encode(&header);
        assert_eq!(page.decode::<Header>().unwrap(), header);
    }

    #[test]
    fn sharded_root_round_trips_through_a_page() {
        let header = sample_root(Index::Sharded(vec![
            meta(b"a", b"m", 4096, 64),
            meta(b"n", b"z", 4160, 64),
        ]));
        let page = Page::encode(&header);
        assert_eq!(page.decode::<Header>().unwrap(), header);
    }

    #[test]
    fn child_round_trips_through_a_page() {
        let header = Header::Child {
            sst_id: SsTableId(42),
            blocks: vec![meta(b"a", b"f", 0, 1024), meta(b"g", b"m", 1024, 1024)],
        };
        let page = Page::encode(&header);
        assert_eq!(page.decode::<Header>().unwrap(), header);
    }

    #[test]
    fn empty_inline_index_round_trips() {
        let header = sample_root(Index::Inline(vec![]));
        assert_eq!(Page::encode(&header).decode::<Header>().unwrap(), header);
    }

    #[test]
    fn empty_sharded_index_round_trips() {
        let header = sample_root(Index::Sharded(vec![]));
        assert_eq!(Page::encode(&header).decode::<Header>().unwrap(), header);
    }

    #[test]
    fn metas_with_empty_keys_round_trip() {
        // Empty min/max are valid (the empty-table sentinel range uses them).
        let header = Header::Child {
            sst_id: SsTableId(7),
            blocks: vec![meta(b"", b"", 0, 0)],
        };
        assert_eq!(Page::encode(&header).decode::<Header>().unwrap(), header);
    }

    fn child_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        Header::Child {
            sst_id: SsTableId(1),
            blocks: vec![meta(b"a", b"b", 0, 8)],
        }
        .encode(&mut out);
        out
    }

    #[test]
    fn bad_magic_is_rejected() {
        let page = Page::new(vec![0u8; 16]);
        assert!(matches!(
            page.decode::<Header>(),
            Err(DecodeError::BadMagic { .. })
        ));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut bytes = child_bytes();
        bytes[4] = 0xFF; // clobber the version word (bytes 4..6)
        bytes[5] = 0xFF;
        assert!(matches!(
            Header::decode(&mut bytes.as_slice()),
            Err(DecodeError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn unknown_role_is_rejected() {
        let mut bytes = child_bytes();
        bytes[6] = 99; // the role byte, right after magic(4) + version(2)
        assert!(matches!(
            Header::decode(&mut bytes.as_slice()),
            Err(DecodeError::UnknownOpType(99))
        ));
    }

    #[test]
    fn truncated_input_is_eof() {
        let bytes = child_bytes();
        let mut short = &bytes[..5]; // mid-header: magic read, version short
        assert!(matches!(
            Header::decode(&mut short),
            Err(DecodeError::UnexpectedEof)
        ));
    }
}
