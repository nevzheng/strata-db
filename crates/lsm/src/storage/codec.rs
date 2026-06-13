//! Encode/decode traits for on-disk structures, with big-endian helpers.

use thiserror::Error;

/// Serialize `self` by appending to `out`.
pub trait Encode {
    fn encode(&self, out: &mut Vec<u8>);
}

/// Parse `Self` from the front of `bytes`, advancing the slice past what was
/// read.
pub trait Decode: Sized {
    fn decode(bytes: &mut &[u8]) -> Result<Self, DecodeError>;
}

/// Why decoding on-disk bytes failed.
#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("bad magic: expected {expected:#010x}, got {got:#010x}")]
    BadMagic { expected: u32, got: u32 },
    #[error("unsupported format version: {0}")]
    UnsupportedVersion(u16),
    #[error("unknown op-type byte: {0}")]
    UnknownOpType(u8),
}

/// Split `n` bytes off the front of `bytes`, advancing it.
pub(crate) fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], DecodeError> {
    if bytes.len() < n {
        return Err(DecodeError::UnexpectedEof);
    }
    let (head, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(head)
}

pub(crate) fn get_u8(bytes: &mut &[u8]) -> Result<u8, DecodeError> {
    Ok(take(bytes, 1)?[0])
}

pub(crate) fn get_u16(bytes: &mut &[u8]) -> Result<u16, DecodeError> {
    Ok(u16::from_be_bytes(take(bytes, 2)?.try_into().unwrap()))
}

pub(crate) fn get_u32(bytes: &mut &[u8]) -> Result<u32, DecodeError> {
    Ok(u32::from_be_bytes(take(bytes, 4)?.try_into().unwrap()))
}

pub(crate) fn get_u64(bytes: &mut &[u8]) -> Result<u64, DecodeError> {
    Ok(u64::from_be_bytes(take(bytes, 8)?.try_into().unwrap()))
}

/// Read a `u32`-length-prefixed byte string.
pub(crate) fn get_bytes<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], DecodeError> {
    let len = get_u32(bytes)? as usize;
    take(bytes, len)
}

/// Append a `u32`-length-prefixed byte string.
pub(crate) fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}
