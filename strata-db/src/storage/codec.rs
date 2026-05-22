//! Wire encoding for primitive logical types.
//!
//! The [`Codec`] trait is implemented once per primitive Rust type that
//! backs a [`LogicalType`] variant (`bool`, `i16`, `i32`, `i64`,
//! `String`). [`Value::encode`] / [`Value::decode`] dispatch to the
//! matching impl via a single `match`, so adding a new logical type
//! means: extend the enums, add one `impl Codec`, extend the match.
//!
//! Conventions:
//!
//! - Integers: little-endian, fixed width.
//! - Bool: one byte, `0x00` / `0x01`. Other bytes are a decode error.
//! - Text: `u32` little-endian length prefix, then UTF-8 bytes.
//! - `Value::Null` is **not** encoded here — nulls are recorded in the
//!   schema-level null bitmap by [`crate::schema::Schema::encode`].
//!
//! Decoding takes `&mut &[u8]` and advances it past the bytes consumed;
//! short buffers surface as [`DecodeError::UnexpectedEof`].

use crate::storage::types::{LogicalType, Value};

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

/// Encode/decode interface implemented once per primitive logical type.
///
/// Each impl block holds the byte layout for one type — easier to read
/// and extend than a single matched giant function.
pub trait Codec: Sized {
    fn encoded_size(&self) -> usize;
    fn encode(&self, buf: &mut Vec<u8>);
    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError>;
}

impl Codec for bool {
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

impl Codec for i16 {
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

impl Codec for i32 {
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

impl Codec for i64 {
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

impl Codec for String {
    fn encoded_size(&self) -> usize {
        4 + self.len()
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        let bytes = self.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let len_bytes = take(buf, 4)?;
        let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        let bytes = take(buf, len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }
}

impl Codec for serde_json::Value {
    fn encoded_size(&self) -> usize {
        // `to_vec` cannot fail for a well-formed `serde_json::Value`
        // unless it contains non-finite floats; we accept that as a panic
        // case (caller built a malformed Value).
        4 + serde_json::to_vec(self)
            .expect("serialize json value")
            .len()
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        let bytes = serde_json::to_vec(self).expect("serialize json value");
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&bytes);
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let len_bytes = take(buf, 4)?;
        let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        let bytes = take(buf, len)?;
        serde_json::from_slice(bytes).map_err(|_| DecodeError::InvalidJson)
    }
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
            Value::Json(j) => j.encoded_size(),
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
            Value::Json(j) => j.encode(buf),
        }
    }

    /// Decode a single non-null value of the given type, advancing `buf`
    /// past the bytes it consumed.
    pub fn decode(ty: LogicalType, buf: &mut &[u8]) -> Result<Value, DecodeError> {
        match ty {
            LogicalType::Bool => Ok(Value::Bool(bool::decode(buf)?)),
            LogicalType::Int16 => Ok(Value::Int16(i16::decode(buf)?)),
            LogicalType::Int32 => Ok(Value::Int32(i32::decode(buf)?)),
            LogicalType::Int64 => Ok(Value::Int64(i64::decode(buf)?)),
            LogicalType::Text => Ok(Value::Text(String::decode(buf)?)),
            LogicalType::Json => Ok(Value::Json(serde_json::Value::decode(buf)?)),
        }
    }
}

/// Split the first `n` bytes off `buf`, advancing the cursor past them.
/// Errors with `UnexpectedEof` if fewer than `n` bytes remain.
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
    fn roundtrip<T: Codec + PartialEq + std::fmt::Debug + Clone>(v: T) {
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
        let decoded = Value::decode(LogicalType::Int32, &mut cursor).unwrap();
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
        let decoded = Value::decode(LogicalType::Json, &mut cursor).unwrap();
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
}
