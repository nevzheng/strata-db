use thiserror::Error;

/// Errors from journal operations.
#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal io: {0}")]
    Io(#[from] std::io::Error),

    /// The file isn't a journal we recognize (wrong magic or version).
    #[error("journal: unrecognized file header")]
    BadHeader,

    /// A [`Codec`](crate::Codec) failed to decode a record's payload. The frame
    /// itself was intact (CRC checked), so this is a codec/format mismatch.
    #[error("journal decode: {0}")]
    Decode(String),
}
