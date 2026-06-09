//! The SSTable data-block format — deliberately the dumbest thing that works.
//!
//! An entry is a length-prefixed user key, its version, then a
//! length-prefixed value:
//!
//! ```text
//! entry:
//! ┌────────────────┬──────────┬─────────┬───────┬──────────────┬───────┐
//! │ ukey_len (4B)  │ user_key │ seq(8B) │ op(1B)│ val_len (4B) │ value │
//! └────────────────┴──────────┴─────────┴───────┴──────────────┴───────┘
//! ```
//!
//! A data block is entries packed back-to-back; decoding reads them until the
//! block is exhausted:
//!
//! ```text
//! data block:
//! ┌───────┬───────┬───────┬─────┐
//! │ entry │ entry │ entry │ ... │
//! └───────┴───────┴───────┴─────┘
//! ```

use super::codec::{self, Decode, DecodeError, Encode};
use crate::key::{InternalKey, KeyValue, OpType};

const OP_PUT: u8 = 1;
const OP_DELETE: u8 = 2;

impl Encode for KeyValue {
    fn encode(&self, out: &mut Vec<u8>) {
        codec::put_bytes(out, &self.key.user_key);
        out.extend_from_slice(&self.key.seq.to_be_bytes());
        out.push(match self.key.op {
            OpType::Put => OP_PUT,
            OpType::Delete => OP_DELETE,
        });
        codec::put_bytes(out, &self.value);
    }
}

impl Decode for KeyValue {
    fn decode(bytes: &mut &[u8]) -> Result<Self, DecodeError> {
        let user_key = codec::get_bytes(bytes)?.to_vec();
        let seq = codec::get_u64(bytes)?;
        let op = match codec::get_u8(bytes)? {
            OP_PUT => OpType::Put,
            OP_DELETE => OpType::Delete,
            other => return Err(DecodeError::UnknownOpType(other)),
        };
        let value = codec::get_bytes(bytes)?.to_vec();
        Ok(KeyValue {
            key: InternalKey { user_key, seq, op },
            value,
        })
    }
}

/// The data section of an SSTable file: entries packed back-to-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBlock(pub Vec<KeyValue>);

impl Encode for DataBlock {
    fn encode(&self, out: &mut Vec<u8>) {
        for entry in &self.0 {
            entry.encode(out);
        }
    }
}

impl Decode for DataBlock {
    fn decode(bytes: &mut &[u8]) -> Result<Self, DecodeError> {
        let mut entries = Vec::new();
        while !bytes.is_empty() {
            entries.push(KeyValue::decode(bytes)?);
        }
        Ok(DataBlock(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Page;

    fn kv(key: &[u8], seq: u64, op: OpType, value: &[u8]) -> KeyValue {
        KeyValue {
            key: InternalKey {
                user_key: key.to_vec(),
                seq,
                op,
            },
            value: value.to_vec(),
        }
    }

    #[test]
    fn data_block_round_trips_through_a_page() {
        let block = DataBlock(vec![
            kv(b"alice", 2, OpType::Put, b"admin"),
            kv(b"bob", 5, OpType::Delete, b""),
            kv(b"carol", 1, OpType::Put, b"viewer"),
        ]);
        let page = Page::encode(&block);
        let decoded: DataBlock = page.decode().unwrap();
        assert_eq!(decoded, block);
    }

    #[test]
    fn unknown_op_byte_is_rejected() {
        let mut bytes = Vec::new();
        codec::put_bytes(&mut bytes, b"k");
        bytes.extend_from_slice(&1u64.to_be_bytes());
        bytes.push(9);
        let page = Page::new(bytes);
        assert!(matches!(
            page.decode::<DataBlock>(),
            Err(DecodeError::UnknownOpType(9))
        ));
    }
}
