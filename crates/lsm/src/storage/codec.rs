//! On-disk serialization for the LSM, re-exported from the shared codec the
//! filesystem (the VFS layer) owns. The LSM's own structures — [`KeyValue`],
//! [`Header`], etc. — implement these traits; the encode/decode primitives
//! themselves live once, in `filesystem::codec`.
//!
//! [`KeyValue`]: super::data::DataBlock
//! [`Header`]: super::header::Header

pub use filesystem::{
    Decode, DecodeError, Encode, get_bytes, get_u8, get_u16, get_u32, get_u64, put_bytes,
};
