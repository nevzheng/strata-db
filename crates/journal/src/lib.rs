//! `journal` — a general-purpose, append-only, crash-safe log.
//!
//! A journal durably records a stream of changes and replays them in order on
//! open. It's the primitive under a write-ahead log, a page-system journal, and
//! similar: the framing, checksums, and crash-safe replay live here once, and
//! callers parameterize the record type with a [`Codec`] (default [`BytesCodec`]).
//!
//! # Durability contract
//!
//! [`append`](Journal::append) returns `Ok` **only after the record is
//! `fsync`'d to stable storage**. So "append returned" is the signal that the
//! record is durable and it's safe to proceed — callers never have to guess.
//! (Group-commit batching, which would move the durability point to an explicit
//! sync, is a future option.) True durability still depends on the OS and disk
//! honoring `fsync` — that's the layer below this one.
//!
//! # Atomicity & record size
//!
//! There is no durability-based size limit. A crash mid-append can leave a torn
//! tail, but each record is framed with a length and CRC, so replay discards a
//! partial or garbled trailing frame — and since `append` hadn't returned `Ok`,
//! that record correctly counts as never committed. The real limits are
//! practical: a `u32` length (≈4 GiB per record) and the memory to buffer and
//! replay one. Journal small change-records; store large blobs elsewhere and
//! journal a reference.
//!
//! ```text
//! file: | magic (4B) | version (2B) | frame | frame | ... |
//! frame:                            | len (4B) | crc (4B) | payload |
//! ```

mod codec;
mod error;
mod frame;

pub use codec::{BytesCodec, Codec};
pub use error::JournalError;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: u32 = 0x4A_52_4E_4C; // "JRNL"
const VERSION: u16 = 1;
const HEADER_LEN: u64 = 6; // magic(4) + version(2)

/// An append-only, crash-safe log of `C::Record`s.
pub struct Journal<C: Codec = BytesCodec> {
    file: File,
    path: PathBuf,
    codec: C,
}

impl<C: Codec + Default> Journal<C> {
    /// Open (creating if needed) a journal at `path` with the default codec.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        Self::with_codec(path, C::default())
    }
}

impl<C: Codec> Journal<C> {
    /// Open (creating if needed) a journal at `path` using `codec`.
    pub fn with_codec(path: impl AsRef<Path>, codec: C) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            write_header(&mut file)?;
            file.sync_all()?;
        } else {
            verify_header(&mut file)?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self { file, path, codec })
    }

    /// Durably append a record. Returns only once the record is `fsync`'d.
    pub fn append(&mut self, record: &C::Record) -> Result<(), JournalError> {
        let mut payload = Vec::new();
        self.codec.encode(record, &mut payload);

        let mut frame = Vec::with_capacity(frame::FRAME_HEADER_LEN + payload.len());
        frame::write_frame(&mut frame, &payload);

        self.file.write_all(&frame)?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Replay every durably-recorded record in append order. Stops at the first
    /// torn/corrupt frame (a crash's partial tail), yielding the good prefix.
    pub fn replay(&self) -> Result<Replay<'_, C>, JournalError> {
        let mut file = File::open(&self.path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(Replay {
            codec: &self.codec,
            buf,
            pos: HEADER_LEN as usize,
        })
    }

    /// Discard all records (a checkpoint), keeping the file valid and empty.
    /// Call after the records have been applied somewhere durable (e.g. an LSM
    /// flush) so they're no longer needed for recovery.
    pub fn truncate(&mut self) -> Result<(), JournalError> {
        self.file.set_len(HEADER_LEN)?;
        self.file.seek(SeekFrom::Start(HEADER_LEN))?;
        self.file.sync_all()?;
        Ok(())
    }
}

/// Iterator over a journal's records, produced by [`Journal::replay`].
pub struct Replay<'a, C: Codec> {
    codec: &'a C,
    buf: Vec<u8>,
    pos: usize,
}

impl<C: Codec> Iterator for Replay<'_, C> {
    type Item = Result<C::Record, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        let (payload, next) = frame::read_frame(&self.buf, self.pos)?;
        self.pos = next;
        Some(self.codec.decode(payload))
    }
}

fn write_header(file: &mut File) -> Result<(), JournalError> {
    file.write_all(&MAGIC.to_be_bytes())?;
    file.write_all(&VERSION.to_be_bytes())?;
    Ok(())
}

fn verify_header(file: &mut File) -> Result<(), JournalError> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; HEADER_LEN as usize];
    file.read_exact(&mut buf)?;
    let magic = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let version = u16::from_be_bytes(buf[4..6].try_into().unwrap());
    if magic != MAGIC || version != VERSION {
        return Err(JournalError::BadHeader);
    }
    Ok(())
}
