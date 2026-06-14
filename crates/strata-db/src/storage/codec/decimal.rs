//! Codecs for `Numeric` (exact decimal, backed by `rust_decimal`).
//!
//! `ValueCodec` uses `rust_decimal`'s compact 16-byte form. That form
//! isn't byte-sortable, so `KeyCodec` implements a bespoke
//! order-preserving encoding (see [`Decimal::encode_key`]).

use rust_decimal::Decimal;

use super::{DecodeError, KeyCodec, ValueCodec, take};

// Sign-bucket tags for the decimal key encoding. Ordering negative <
// zero < positive falls out of `0x03 < 0x04 < 0x05`.
const DEC_NEG: u8 = 0x03;
const DEC_ZERO: u8 = 0x04;
const DEC_POS: u8 = 0x05;

impl ValueCodec for Decimal {
    fn encoded_size(&self) -> usize {
        16
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        // rust_decimal's own 16-byte form. Compact and exact, but NOT
        // order-preserving — that's `KeyCodec`'s job.
        buf.extend_from_slice(&self.serialize());
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = take(buf, 16)?;
        Ok(Decimal::deserialize(bytes.try_into().unwrap()))
    }
}

impl KeyCodec for Decimal {
    /// Order-preserving decimal encoding (CockroachDB-style): sign bucket,
    /// then exponent, then big-endian base-100 "centimal" digits. The
    /// exponent leads the digits so magnitude dominates (`100 > 9`); for
    /// negatives the whole payload is one's-complemented so larger
    /// magnitudes sort lower. `rust_decimal`'s own bytes aren't sortable,
    /// hence this bespoke form.
    fn encode_key(&self, buf: &mut Vec<u8>) {
        let d = self.normalize();
        if d.is_zero() {
            buf.push(DEC_ZERO);
            return;
        }
        let negative = d.is_sign_negative();
        let a = d.abs();

        // a = coeff × 10^(-scale); `digits` is the coefficient's decimal
        // text. In the F×10^E form with F ∈ [0.1, 1), the significand is
        // `digits` and E = (#digits) − scale.
        let digits = a.mantissa().to_string();
        let exponent = digits.len() as i32 - a.scale() as i32;

        let mut payload = Vec::with_capacity(2 + digits.len() / 2);
        // Exponent as one order-preserving signed byte (range is small:
        // rust_decimal caps digits at 29 and scale at 28, so E ∈ [-27, 29]).
        payload.push((exponent as i8 as u8) ^ 0x80);

        // Pair the digits into base-100; pad an odd tail with a trailing
        // zero (safe — `normalize` already stripped real trailing zeros).
        let mut ds = digits.into_bytes();
        if ds.len() % 2 == 1 {
            ds.push(b'0');
        }
        for pair in ds.chunks(2) {
            let cent = (pair[0] - b'0') * 10 + (pair[1] - b'0'); // 0..=99
            payload.push(cent + 1); // 1..=100, leaving 0x00 as a terminator
        }
        payload.push(0x00);

        if negative {
            buf.push(DEC_NEG);
            buf.extend(payload.iter().map(|b| !b)); // complement reverses order
        } else {
            buf.push(DEC_POS);
            buf.extend_from_slice(&payload);
        }
    }
}
