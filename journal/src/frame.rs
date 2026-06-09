//! Record framing.
//!
//! ```text
//! frame: | payload_len (4B) | crc32 (4B) | payload |
//! ```
//!
//! The CRC is what makes a *torn tail* — a partial or garbled append left by a
//! crash — detectable: on replay it fails the length or checksum check and the
//! record is discarded, so appends are effectively atomic regardless of size.

/// `payload_len (4B) + crc32 (4B)`.
pub(crate) const FRAME_HEADER_LEN: usize = 8;

/// Append a framed `payload` to `out`.
pub(crate) fn write_frame(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32fast::hash(payload).to_be_bytes());
    out.extend_from_slice(payload);
}

/// Read the frame at `buf[pos..]`, returning its payload and the position just
/// past it. Returns `None` at a clean end *or* a torn/corrupt tail — both are
/// treated as "end of valid records", which is the crash-safe behavior.
pub(crate) fn read_frame(buf: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let rest = buf.get(pos..)?;
    if rest.len() < FRAME_HEADER_LEN {
        return None; // no full frame header — clean end or torn
    }
    let len = u32::from_be_bytes(rest[0..4].try_into().unwrap()) as usize;
    let crc = u32::from_be_bytes(rest[4..8].try_into().unwrap());
    let payload = rest.get(FRAME_HEADER_LEN..FRAME_HEADER_LEN + len)?; // torn payload
    if crc32fast::hash(payload) != crc {
        return None; // corrupt tail
    }
    Some((payload, pos + FRAME_HEADER_LEN + len))
}
