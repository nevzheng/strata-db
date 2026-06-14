//! Codecs for the variable-length types: `Text`, `Bytes`, and `Json`.
//!
//! `ValueCodec` prefixes each with a `u32` length so the schema can
//! decode columns positionally. `KeyCodec` drops the prefix and emits raw
//! bytes, so lex byte-sort matches content sort.

use super::{DecodeError, KeyCodec, ValueCodec, take};

impl ValueCodec for String {
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

impl ValueCodec for Vec<u8> {
    fn encoded_size(&self) -> usize {
        4 + self.len()
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.len() as u32).to_le_bytes());
        buf.extend_from_slice(self);
    }

    fn decode(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let len_bytes = take(buf, 4)?;
        let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        let bytes = take(buf, len)?;
        Ok(bytes.to_vec())
    }
}

impl ValueCodec for serde_json::Value {
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

impl KeyCodec for String {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.as_bytes());
    }
}

impl KeyCodec for Vec<u8> {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self);
    }
}

impl KeyCodec for serde_json::Value {
    fn encode_key(&self, buf: &mut Vec<u8>) {
        let bytes = serde_json::to_vec(self).expect("serialize json value");
        buf.extend_from_slice(&bytes);
    }
}
