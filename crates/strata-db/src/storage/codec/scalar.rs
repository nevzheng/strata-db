//! Codecs for the fixed-width scalar types: `bool`, the signed integers,
//! and the IEEE floats.
//!
//! `ValueCodec` is little-endian (in-row decode is positional, so byte
//! order is irrelevant). `KeyCodec` is **order-preserving**: signed
//! integers flip the sign bit and emit big-endian; floats apply the
//! IEEE-754 total-order transform. Lex byte-sort then matches numeric
//! order across the negative/positive boundary.

use uuid::Uuid;

use super::{DecodeError, KeyCodec, ValueCodec, take};

impl ValueCodec for bool {
    fn encoded_size(&self) -> usize {
        1
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(if *self { 1 } else { 0 });
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 1)?;
        match bytes[0] {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DecodeError::InvalidBool(other)),
        }
    }
}

impl ValueCodec for i16 {
    fn encoded_size(&self) -> usize {
        2
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.to_le_bytes());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 2)?;
        Ok(i16::from_le_bytes(bytes.try_into().unwrap()))
    }
}

impl ValueCodec for i32 {
    fn encoded_size(&self) -> usize {
        4
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.to_le_bytes());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 4)?;
        Ok(i32::from_le_bytes(bytes.try_into().unwrap()))
    }
}

impl ValueCodec for i64 {
    fn encoded_size(&self) -> usize {
        8
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.to_le_bytes());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 8)?;
        Ok(i64::from_le_bytes(bytes.try_into().unwrap()))
    }
}

impl ValueCodec for f32 {
    fn encoded_size(&self) -> usize {
        4
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.to_le_bytes());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 4)?;
        Ok(f32::from_le_bytes(bytes.try_into().unwrap()))
    }
}

impl ValueCodec for f64 {
    fn encoded_size(&self) -> usize {
        8
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.to_le_bytes());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 8)?;
        Ok(f64::from_le_bytes(bytes.try_into().unwrap()))
    }
}

impl ValueCodec for Uuid {
    fn encoded_size(&self) -> usize {
        16
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.as_bytes());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 16)?;
        Ok(Uuid::from_bytes(bytes.try_into().unwrap()))
    }
}

// --- KeyCodec: order-preserving ---

impl KeyCodec for bool {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        buf.push(if *self { 1 } else { 0 });
    }
}

impl KeyCodec for i16 {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        // Flip the sign bit (negatives before positives), big-endian.
        buf.extend_from_slice(&((*self as u16) ^ (1 << 15)).to_be_bytes());
    }
}

impl KeyCodec for i32 {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&((*self as u32) ^ (1 << 31)).to_be_bytes());
    }
}

impl KeyCodec for i64 {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&((*self as u64) ^ (1 << 63)).to_be_bytes());
    }
}

impl KeyCodec for f32 {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        // IEEE-754 total-order transform (see the f64 impl), 32-bit width.
        let bits = self.to_bits();
        let mask = (bits >> 31).wrapping_neg() | (1 << 31);
        buf.extend_from_slice(&(bits ^ mask).to_be_bytes());
    }
}

impl KeyCodec for f64 {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        // IEEE-754 total-order transform: for negatives flip every bit
        // (incl. the sign), for non-negatives flip only the sign bit;
        // then big-endian. Result sorts -inf < … < +inf (NaN at the top),
        // matching numeric order under lex byte compare.
        let bits = self.to_bits();
        let mask = (bits >> 63).wrapping_neg() | (1 << 63);
        buf.extend_from_slice(&(bits ^ mask).to_be_bytes());
    }
}

impl KeyCodec for Uuid {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        // Raw 16 bytes already sort like Postgres orders uuids.
        buf.extend_from_slice(self.as_bytes());
    }
}
