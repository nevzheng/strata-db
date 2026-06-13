//! [`TupleLoc`] — the physical address of a tuple in the page heap.
//!
//! A tuple's logical identity is `(BlockId, slot_id)`: which page holds it and
//! which slot within that page (see [`TuplePage`](crate::TuplePage)). The slot
//! id is stable for the tuple's lifetime, so a `TupleLoc` is a durable pointer —
//! it's what an index (the LSM) stores as the value for a row key, and what the
//! heap resolves back to tuple bytes.

use crate::BlockId;

/// Where a tuple lives: a page and a slot within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TupleLoc {
    /// The page holding the tuple.
    pub page_id: BlockId,
    /// The slot index within that page.
    pub slot_id: u16,
}

impl TupleLoc {
    /// Encoded byte length: an 8-byte page id plus a 2-byte slot id.
    pub const ENCODED_LEN: usize = 10;

    /// A location for `slot_id` on `page_id`.
    pub fn new(page_id: BlockId, slot_id: u16) -> Self {
        Self { page_id, slot_id }
    }

    /// Encode to a fixed 10-byte, big-endian form — the value an index stores.
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..8].copy_from_slice(&self.page_id.0.to_be_bytes());
        out[8..10].copy_from_slice(&self.slot_id.to_be_bytes());
        out
    }

    /// Decode from [`encode`](Self::encode)'s form. `None` if `bytes` is not
    /// exactly [`ENCODED_LEN`](Self::ENCODED_LEN) long (a corrupt index value).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::ENCODED_LEN {
            return None;
        }
        Some(Self {
            page_id: BlockId(u64::from_be_bytes(bytes[0..8].try_into().unwrap())),
            slot_id: u16::from_be_bytes(bytes[8..10].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let loc = TupleLoc::new(BlockId(0x0102_0304_0506_0708), 0xBEEF);
        let bytes = loc.encode();
        assert_eq!(bytes.len(), TupleLoc::ENCODED_LEN);
        assert_eq!(TupleLoc::decode(&bytes), Some(loc));
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert_eq!(TupleLoc::decode(&[]), None);
        assert_eq!(TupleLoc::decode(&[0u8; 9]), None);
        assert_eq!(TupleLoc::decode(&[0u8; 11]), None);
    }
}
