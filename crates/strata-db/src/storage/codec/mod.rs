//! Wire encoding for logical types.
//!
//! Two codecs sit side by side, one per role:
//!
//! - [`ValueCodec`] — the in-row column encoding. Variable-length types
//!   carry a `u32` length prefix so [`crate::catalog::schema::Schema`]
//!   can decode columns positionally.
//! - [`KeyCodec`] — the storage user-key encoding, **order-preserving**
//!   so the engine's lex byte-sort matches the value's numeric / content
//!   sort. Required for prefix / range scans to behave.
//!
//! Both traits are implemented once per primitive Rust type that backs a
//! [`LogicalType`] variant. The per-type `impl`s live in focused
//! submodules — [`scalar`] (fixed-width: bool, ints, floats), [`text`]
//! (variable-length: Text, Bytes, Json), and [`decimal`] (Numeric). This
//! module owns the traits, the [`Value`] dispatch, and the errors.
//! Adding a logical type means: extend the enums, add the two `impl`s in
//! the right submodule, and extend the matches below.
//!
//! Conventions:
//!
//! - Integers: fixed width. `ValueCodec` is little-endian (positional
//!   decode, so byte order is irrelevant); `KeyCodec` flips the sign bit
//!   and is big-endian, so negatives sort before positives.
//! - Floats: little-endian in-row; an IEEE-754 total-order transform for
//!   the key.
//! - Numeric: rust_decimal's 16-byte form in-row; a bespoke
//!   order-preserving encoding for the key (see [`decimal`]).
//! - Bool: one byte, `0x00` / `0x01`. Same in both codecs.
//! - Text / Bytes / Json: length-prefixed by `ValueCodec`, raw by
//!   `KeyCodec`.
//! - `Value::Null` is **not** encoded by either codec — nulls are
//!   recorded in the schema-level null bitmap by
//!   [`crate::catalog::schema::Schema::encode`], and they cannot be keys.
//!
//! Decoding takes `&mut &[u8]` and advances it past the bytes consumed;
//! short buffers surface as [`DecodeError::UnexpectedEof`]. `KeyCodec`
//! is encode-only — single-column variable-length keys can't be
//! decoded unambiguously without an end marker; composite-key decoding
//! will land alongside composite keys.

mod decimal;
mod interval;
mod scalar;
mod text;

use rust_decimal::Decimal;
use uuid::Uuid;

use crate::storage::types::{Interval, LogicalType, Value};

/// Tried to encode a value as a key that has no meaningful key form.
/// Today the only such case is `Value::Null`.
#[derive(Debug)]
pub enum KeyEncodeError {
    /// `Value::Null` cannot be a key.
    NullKey,
}

#[derive(Debug)]
pub enum DecodeError {
    /// Not enough bytes remaining in the buffer for the expected value.
    UnexpectedEof,
    /// Bool byte was neither `0x00` nor `0x01`.
    InvalidBool(u8),
    /// Text bytes were not valid UTF-8.
    InvalidUtf8,
    /// JSON bytes did not parse.
    InvalidJson,
    /// Buffer had bytes left over after decoding every schema field.
    TrailingBytes,
}

/// In-row column codec implemented once per primitive logical type.
///
/// Each impl block holds the byte layout for one type — easier to read
/// and extend than a single matched giant function.
pub trait ValueCodec: Sized {
    fn encoded_size(&self) -> usize;
    fn encode(&self, buf: &mut Vec<u8>);
    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError>;
}

/// Storage user-key codec — order-preserving (see module docs).
/// Encode-only for now.
pub trait KeyCodec {
    fn encode_key(&self, buf: &mut Vec<u8>);
}

impl Value {
    /// Bytes `encode` will append. `Null` is 0 — nulls live in the bitmap.
    pub fn encoded_size(&self) -> usize {
        match self {
            Value::Null => 0,
            Value::Bool(b) => b.encoded_size(),
            Value::Int16(n) => n.encoded_size(),
            Value::Int32(n) => n.encoded_size(),
            Value::Int64(n) => n.encoded_size(),
            Value::Text(s) => s.encoded_size(),
            Value::Bytes(b) => b.encoded_size(),
            Value::Json(j) => j.encoded_size(),
            // Date / Timestamp ride on i32 / i64's fixed-width encodings.
            Value::Date(n) => n.encoded_size(),
            Value::Timestamp(n) => n.encoded_size(),
            Value::Float32(f) => f.encoded_size(),
            Value::Float64(f) => f.encoded_size(),
            Value::Numeric(d) => d.encoded_size(),
            Value::Time(n) => n.encoded_size(),
            Value::Uuid(u) => u.encoded_size(),
            Value::Interval(i) => i.encoded_size(),
            // u32 count + each element's encoding.
            Value::Array(items) => 4 + items.iter().map(Value::encoded_size).sum::<usize>(),
        }
    }

    /// Append the non-null encoding of `self` to `buf`. Panics on `Null` —
    /// the schema-level bitmap is responsible for nulls.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            Value::Null => {
                unreachable!("Value::encode called on Null; caller must handle via the bitmap")
            }
            Value::Bool(b) => b.encode(buf),
            Value::Int16(n) => n.encode(buf),
            Value::Int32(n) => n.encode(buf),
            Value::Int64(n) => n.encode(buf),
            Value::Text(s) => s.encode(buf),
            Value::Bytes(b) => b.encode(buf),
            Value::Json(j) => j.encode(buf),
            Value::Date(n) => n.encode(buf),
            Value::Timestamp(n) => n.encode(buf),
            Value::Float32(f) => f.encode(buf),
            Value::Float64(f) => f.encode(buf),
            Value::Numeric(d) => d.encode(buf),
            Value::Time(n) => n.encode(buf),
            Value::Uuid(u) => u.encode(buf),
            Value::Interval(i) => i.encode(buf),
            Value::Array(items) => {
                buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for item in items {
                    item.encode(buf);
                }
            }
        }
    }

    /// Append the user-key encoding of `self` to `buf`. Errors on
    /// `Null` — nulls have no key form.
    pub fn encode_key(&self, buf: &mut Vec<u8>) -> Result<(), KeyEncodeError> {
        match self {
            Value::Null => return Err(KeyEncodeError::NullKey),
            Value::Bool(b) => b.encode_key(buf),
            Value::Int16(n) => n.encode_key(buf),
            Value::Int32(n) => n.encode_key(buf),
            Value::Int64(n) => n.encode_key(buf),
            Value::Text(s) => s.encode_key(buf),
            Value::Bytes(b) => b.encode_key(buf),
            Value::Json(j) => j.encode_key(buf),
            Value::Date(n) => n.encode_key(buf),
            Value::Timestamp(n) => n.encode_key(buf),
            Value::Float32(f) => f.encode_key(buf),
            Value::Float64(f) => f.encode_key(buf),
            Value::Numeric(d) => d.encode_key(buf),
            Value::Time(n) => n.encode_key(buf),
            Value::Uuid(u) => u.encode_key(buf),
            Value::Interval(i) => i.encode_key(buf),
            // Element keys concatenated — element-wise ordering (correct
            // for fixed-width elements; a null element has no key).
            Value::Array(items) => {
                for item in items {
                    item.encode_key(buf)?;
                }
            }
        }
        Ok(())
    }

    /// Decode a single non-null value of the given type, advancing `buf`
    /// past the bytes it consumed.
    pub fn decode(ty: &LogicalType, buf: &mut &[u8]) -> Result<Value, DecodeError> {
        match ty {
            LogicalType::Bool => Ok(Value::Bool(bool::decode(buf)?)),
            LogicalType::Int16 => Ok(Value::Int16(i16::decode(buf)?)),
            LogicalType::Int32 => Ok(Value::Int32(i32::decode(buf)?)),
            LogicalType::Int64 => Ok(Value::Int64(i64::decode(buf)?)),
            LogicalType::Text => Ok(Value::Text(String::decode(buf)?)),
            LogicalType::Bytes => Ok(Value::Bytes(<Vec<u8>>::decode(buf)?)),
            LogicalType::Json => Ok(Value::Json(serde_json::Value::decode(buf)?)),
            LogicalType::Date => Ok(Value::Date(i32::decode(buf)?)),
            LogicalType::Timestamp => Ok(Value::Timestamp(i64::decode(buf)?)),
            LogicalType::Float32 => Ok(Value::Float32(f32::decode(buf)?)),
            LogicalType::Float64 => Ok(Value::Float64(f64::decode(buf)?)),
            LogicalType::Numeric => Ok(Value::Numeric(Decimal::decode(buf)?)),
            LogicalType::Time => Ok(Value::Time(i64::decode(buf)?)),
            LogicalType::Uuid => Ok(Value::Uuid(Uuid::decode(buf)?)),
            LogicalType::Interval => Ok(Value::Interval(Interval::decode(buf)?)),
            LogicalType::Array(elem) => {
                let count = u32::from_le_bytes(take(buf, 4)?.try_into().unwrap()) as usize;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(Value::decode(elem, buf)?);
                }
                Ok(Value::Array(items))
            }
        }
    }
}

/// Split the first `n` bytes off `buf`, advancing the cursor past them.
/// Errors with `UnexpectedEof` if fewer than `n` bytes remain. Shared by
/// every `ValueCodec::decode` impl in the submodules.
fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], DecodeError> {
    if buf.len() < n {
        return Err(DecodeError::UnexpectedEof);
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `v`, then decode and assert we get the same value back and
    /// consume every byte. `encoded_size` must match the written length.
    fn roundtrip<T: ValueCodec + PartialEq + std::fmt::Debug + Clone>(v: T) {
        let mut buf = Vec::new();
        v.encode(&mut buf);
        assert_eq!(
            buf.len(),
            v.encoded_size(),
            "encoded_size mismatch for {v:?}"
        );
        let mut cursor: &[u8] = &buf;
        let decoded = T::decode(&mut cursor).unwrap();
        assert_eq!(decoded, v);
        assert!(cursor.is_empty(), "trailing bytes after decode of {v:?}");
    }

    /// Key encoding must be order-preserving: for ascending values, the
    /// encoded byte vectors must be lexicographically ascending too —
    /// crucially across the negative/positive boundary.
    fn assert_key_order_preserving<T: KeyCodec + Copy>(ascending: &[T]) {
        let keys: Vec<Vec<u8>> = ascending
            .iter()
            .map(|v| {
                let mut b = Vec::new();
                v.encode_key(&mut b);
                b
            })
            .collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "keys not ascending: {:?}", pair);
        }
    }

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn bool_roundtrip() {
        roundtrip(true);
        roundtrip(false);
    }

    #[test]
    fn i16_roundtrip() {
        for n in [0i16, 1, -1, i16::MAX, i16::MIN] {
            roundtrip(n);
        }
    }

    #[test]
    fn i32_roundtrip() {
        for n in [0i32, 1, -1, i32::MAX, i32::MIN] {
            roundtrip(n);
        }
    }

    #[test]
    fn i64_roundtrip() {
        for n in [0i64, 1, -1, i64::MAX, i64::MIN] {
            roundtrip(n);
        }
    }

    #[test]
    fn string_roundtrip() {
        for s in ["", "hi", "hello world", "Привет", "🦀 rust"] {
            roundtrip(s.to_string());
        }
    }

    #[test]
    fn invalid_bool_byte_errors() {
        let buf = [0x02u8];
        let mut cursor: &[u8] = &buf;
        assert!(matches!(
            bool::decode(&mut cursor),
            Err(DecodeError::InvalidBool(0x02))
        ));
    }

    #[test]
    fn invalid_utf8_errors() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        let mut cursor: &[u8] = &buf;
        assert!(matches!(
            String::decode(&mut cursor),
            Err(DecodeError::InvalidUtf8)
        ));
    }

    #[test]
    fn short_buffer_errors() {
        let buf = [0x01u8, 0x02];
        let mut cursor: &[u8] = &buf;
        assert!(matches!(
            i32::decode(&mut cursor),
            Err(DecodeError::UnexpectedEof)
        ));
    }

    #[test]
    fn value_dispatch_matches_codec() {
        let mut buf = Vec::new();
        Value::Int32(42).encode(&mut buf);
        let mut cursor: &[u8] = &buf;
        let decoded = Value::decode(&LogicalType::Int32, &mut cursor).unwrap();
        assert_eq!(decoded, Value::Int32(42));
    }

    #[test]
    fn json_roundtrip() {
        for v in [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!("hello"),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({"name": "acme", "id": 7, "tags": ["a", "b"]}),
        ] {
            roundtrip(v);
        }
    }

    #[test]
    fn json_via_value_dispatch_roundtrip() {
        let original = Value::Json(serde_json::json!({"k": "v", "n": 1}));
        let mut buf = Vec::new();
        original.encode(&mut buf);
        assert_eq!(buf.len(), original.encoded_size());
        let mut cursor: &[u8] = &buf;
        let decoded = Value::decode(&LogicalType::Json, &mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn invalid_json_errors() {
        let bad = b"{not json}";
        let mut buf = Vec::new();
        buf.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        buf.extend_from_slice(bad);
        let mut cursor: &[u8] = &buf;
        assert!(matches!(
            serde_json::Value::decode(&mut cursor),
            Err(DecodeError::InvalidJson)
        ));
    }

    #[test]
    fn integer_keys_sort_in_numeric_order() {
        assert_key_order_preserving(&[i16::MIN, -100, -1, 0, 1, 100, i16::MAX]);
        assert_key_order_preserving(&[i32::MIN, -100, -1, 0, 1, 100, i32::MAX]);
        assert_key_order_preserving(&[i64::MIN, -100, -1, 0, 1, 100, i64::MAX]);
    }

    #[test]
    fn float_keys_sort_in_numeric_order() {
        // Order-preserving across sign, signed zero, and the infinities.
        assert_key_order_preserving(&[
            f64::NEG_INFINITY,
            -100.5,
            -1.0,
            -0.0,
            0.0,
            1.0,
            100.5,
            f64::INFINITY,
        ]);
        assert_key_order_preserving(&[
            f32::NEG_INFINITY,
            -100.5f32,
            -1.0,
            -0.0,
            0.0,
            1.0,
            100.5,
            f32::INFINITY,
        ]);
    }

    #[test]
    fn date_keys_sort_chronologically() {
        // Date rides on i32's key codec, including pre-epoch (negative) days.
        let mut prev: Option<Vec<u8>> = None;
        for days in [-1000i32, -1, 0, 1, 20617] {
            let mut buf = Vec::new();
            Value::Date(days).encode_key(&mut buf).unwrap();
            if let Some(p) = &prev {
                assert!(*p < buf, "date keys not ascending at {days}");
            }
            prev = Some(buf);
        }
    }

    #[test]
    fn decimal_value_roundtrips() {
        for s in [
            "0",
            "1.5",
            "-2.25",
            "100",
            "0.001",
            "-0.001",
            "79228162514264337593543950335", // Decimal::MAX
        ] {
            let d = dec(s);
            let mut buf = Vec::new();
            d.encode(&mut buf);
            assert_eq!(buf.len(), d.encoded_size());
            let mut cur: &[u8] = &buf;
            assert_eq!(Decimal::decode(&mut cur).unwrap(), d);
            assert!(cur.is_empty());
        }
    }

    #[test]
    fn decimal_keys_sort_in_numeric_order() {
        let ascending: Vec<Decimal> = [
            "-1000.25", "-100", "-10", "-9.5", "-1", "-0.5", "-0.05", "-0.001", "0", "0.001",
            "0.05", "0.5", "1", "9.5", "10", "100", "1000.25",
        ]
        .iter()
        .map(|s| dec(s))
        .collect();
        assert_key_order_preserving(&ascending);
    }

    #[test]
    fn array_value_roundtrips() {
        let arr = Value::Array(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
        let mut buf = Vec::new();
        arr.encode(&mut buf);
        assert_eq!(buf.len(), arr.encoded_size());
        let ty = LogicalType::Array(Box::new(LogicalType::Int32));
        let mut cur: &[u8] = &buf;
        assert_eq!(Value::decode(&ty, &mut cur).unwrap(), arr);
        assert!(cur.is_empty());
    }

    #[test]
    fn array_keys_order_element_wise() {
        let key = |items: &[i32]| {
            let mut b = Vec::new();
            Value::Array(items.iter().map(|n| Value::Int32(*n)).collect())
                .encode_key(&mut b)
                .unwrap();
            b
        };
        // A prefix sorts before a longer array; otherwise element order wins.
        assert!(key(&[1, 2]) < key(&[1, 2, 9]));
        assert!(key(&[1, 2, 9]) < key(&[1, 3]));
        assert!(key(&[1]) < key(&[2]));
    }

    #[test]
    fn decimal_equal_values_encode_identically() {
        // Different textual scales, same value → identical keys, so a
        // NUMERIC primary key treats them as one row.
        let mut a = Vec::new();
        dec("1.5").encode_key(&mut a);
        let mut b = Vec::new();
        dec("1.50").encode_key(&mut b);
        assert_eq!(a, b);
    }
}
