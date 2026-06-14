//! Codec for `Interval` (months / days / microseconds).
//!
//! `ValueCodec` stores the three components verbatim (16 bytes), so reads
//! stay faithful — `'1 mon'` decodes back distinct from `'30 days'`.
//! `KeyCodec` instead encodes the normalized `i128` micros (see
//! [`Interval::to_micros`]) order-preserving, so the key sorts by the
//! interpreted value and `'1 mon'` / `'30 days'` share a key — matching
//! SQL `=`.

use crate::storage::types::Interval;

use super::{DecodeError, KeyCodec, ValueCodec, take};

impl ValueCodec for Interval {
    fn encoded_size(&self) -> usize {
        16
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.months.to_le_bytes());
        buf.extend_from_slice(&self.days.to_le_bytes());
        buf.extend_from_slice(&self.micros.to_le_bytes());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let months = i32::from_le_bytes(take(buf, 4)?.try_into().unwrap());
        let days = i32::from_le_bytes(take(buf, 4)?.try_into().unwrap());
        let micros = i64::from_le_bytes(take(buf, 8)?.try_into().unwrap());
        Ok(Interval {
            months,
            days,
            micros,
        })
    }
}

impl KeyCodec for Interval {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        // Normalized i128 micros, order-preserving: flip the sign bit and
        // emit big-endian — the same transform as the signed integers.
        let ordered = (self.to_micros() as u128) ^ (1 << 127);
        buf.extend_from_slice(&ordered.to_be_bytes());
    }
}
