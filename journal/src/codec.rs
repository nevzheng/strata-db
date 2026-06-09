use crate::JournalError;

/// Converts a record type to and from the bytes stored in a frame.
///
/// The journal is generic over this so it can log any record type while the
/// framing, checksums, and crash-safe replay stay in one place. Implement it
/// for your record type (e.g. an LSM `WalOp`); use [`BytesCodec`] for raw bytes.
pub trait Codec {
    type Record;

    /// Encode `record` into `buf` (the frame payload).
    fn encode(&self, record: &Self::Record, buf: &mut Vec<u8>);

    /// Decode one record from a frame payload.
    fn decode(&self, bytes: &[u8]) -> Result<Self::Record, JournalError>;
}

/// The default codec: records are raw byte vectors, stored verbatim.
#[derive(Debug, Default, Clone, Copy)]
pub struct BytesCodec;

impl Codec for BytesCodec {
    type Record = Vec<u8>;

    fn encode(&self, record: &Vec<u8>, buf: &mut Vec<u8>) {
        buf.extend_from_slice(record);
    }

    fn decode(&self, bytes: &[u8]) -> Result<Vec<u8>, JournalError> {
        Ok(bytes.to_vec())
    }
}
