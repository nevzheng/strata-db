use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;

/// Max key size: 64 KB.
pub const MAX_KEY_SIZE: usize = u16::MAX as usize;

/// Max value size: 64 KB.
pub const MAX_VALUE_SIZE: usize = u16::MAX as usize;

const OP_PUT: u8 = 0x01;
const OP_DELETE: u8 = 0x02;

/// Current metadata size: just seq (8 bytes).
const META_SIZE: u16 = 8;

/// Per-entry metadata written to the WAL.
///
/// Wire format (big-endian): `| meta_len (2B) | seq (8B) | ...future fields... |`
///
/// The `meta_len` prefix allows adding fields without breaking the format.
/// Decoders skip any unrecognised trailing bytes inside the metadata region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalMeta {
    pub seq: u64,
}

impl WalMeta {
    fn encode(&self, w: &mut impl Write, hasher: &mut Hasher) -> io::Result<()> {
        let meta_len = META_SIZE.to_be_bytes();
        let seq = self.seq.to_be_bytes();

        hasher.update(&meta_len);
        hasher.update(&seq);

        w.write_all(&meta_len)?;
        w.write_all(&seq)?;
        Ok(())
    }

    fn decode(r: &mut impl Read, hasher: &mut Hasher) -> io::Result<Self> {
        let mut meta_len_buf = [0u8; 2];
        r.read_exact(&mut meta_len_buf)?;
        hasher.update(&meta_len_buf);
        let meta_len = u16::from_be_bytes(meta_len_buf) as usize;

        if meta_len < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("metadata too short: {meta_len} bytes, need at least 8"),
            ));
        }

        let mut seq_buf = [0u8; 8];
        r.read_exact(&mut seq_buf)?;
        hasher.update(&seq_buf);
        let seq = u64::from_be_bytes(seq_buf);

        // Skip any future metadata fields we don't understand.
        let remaining = meta_len - 8;
        if remaining > 0 {
            let mut skip = vec![0u8; remaining];
            r.read_exact(&mut skip)?;
            hasher.update(&skip);
        }

        Ok(Self { seq })
    }
}

/// The operation type for a WAL entry.
///
/// Wire formats (all integers big-endian):
///
/// Put:    `| 0x01 (1B) | meta_len (2B) | seq (8B) | key_len (2B) | val_len (2B) | key | value | crc32 (4B) |`
/// Delete: `| 0x02 (1B) | meta_len (2B) | seq (8B) | key_len (2B) | key | crc32 (4B) |`
///
/// The metadata section is length-prefixed so future fields can be added
/// without breaking the format. Decoders skip any unrecognised trailing
/// bytes inside the metadata region.
#[derive(Debug, PartialEq, Eq)]
pub enum WalOp {
    Put {
        meta: WalMeta,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        meta: WalMeta,
        key: Vec<u8>,
    },
}

impl WalOp {
    pub fn meta(&self) -> &WalMeta {
        match self {
            WalOp::Put { meta, .. } | WalOp::Delete { meta, .. } => meta,
        }
    }

    /// Encode this operation to the writer, appending a CRC32 checksum.
    pub fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let mut hasher = Hasher::new();

        match self {
            WalOp::Put { meta, key, value } => {
                let op = [OP_PUT];
                hasher.update(&op);
                w.write_all(&op)?;

                meta.encode(w, &mut hasher)?;

                let key_len = (key.len() as u16).to_be_bytes();
                let val_len = (value.len() as u16).to_be_bytes();

                hasher.update(&key_len);
                hasher.update(&val_len);
                hasher.update(key);
                hasher.update(value);

                w.write_all(&key_len)?;
                w.write_all(&val_len)?;
                w.write_all(key)?;
                w.write_all(value)?;
            }
            WalOp::Delete { meta, key } => {
                let op = [OP_DELETE];
                hasher.update(&op);
                w.write_all(&op)?;

                meta.encode(w, &mut hasher)?;

                let key_len = (key.len() as u16).to_be_bytes();

                hasher.update(&key_len);
                hasher.update(key);

                w.write_all(&key_len)?;
                w.write_all(key)?;
            }
        }

        let checksum = hasher.finalize();
        w.write_all(&checksum.to_be_bytes())?;
        Ok(())
    }

    /// Decode a WAL operation from the reader, verifying the CRC32 checksum.
    pub fn decode(r: &mut impl Read) -> io::Result<Self> {
        let mut hasher = Hasher::new();

        // Read op code.
        let mut op = [0u8; 1];
        r.read_exact(&mut op)?;
        hasher.update(&op);

        // Read metadata.
        let meta = WalMeta::decode(r, &mut hasher)?;

        // Read key_len.
        let mut key_len_buf = [0u8; 2];
        r.read_exact(&mut key_len_buf)?;
        hasher.update(&key_len_buf);
        let key_len = u16::from_be_bytes(key_len_buf) as usize;

        let entry = match op[0] {
            OP_PUT => {
                // Read val_len.
                let mut val_len_buf = [0u8; 2];
                r.read_exact(&mut val_len_buf)?;
                hasher.update(&val_len_buf);
                let val_len = u16::from_be_bytes(val_len_buf) as usize;

                // Read key.
                let mut key = vec![0u8; key_len];
                r.read_exact(&mut key)?;
                hasher.update(&key);

                // Read value.
                let mut value = vec![0u8; val_len];
                r.read_exact(&mut value)?;
                hasher.update(&value);

                WalOp::Put { meta, key, value }
            }
            OP_DELETE => {
                // Read key.
                let mut key = vec![0u8; key_len];
                r.read_exact(&mut key)?;
                hasher.update(&key);

                WalOp::Delete { meta, key }
            }
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown WAL op code: {:#x}", unknown),
                ));
            }
        };

        // Read and verify checksum.
        let mut checksum_buf = [0u8; 4];
        r.read_exact(&mut checksum_buf)?;
        let stored = u32::from_be_bytes(checksum_buf);
        let computed = hasher.finalize();

        if stored != computed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "checksum mismatch: stored={:#x}, computed={:#x}",
                    stored, computed
                ),
            ));
        }

        Ok(entry)
    }
}

/// A write-ahead log backed by a file on disk.
///
/// Writes are unbuffered and synced to disk after each append
/// to ensure durability across crashes.
#[derive(Debug)]
pub struct WriteAheadLog {
    dir: PathBuf,
    file: File,
}

impl WriteAheadLog {
    /// Create a new WAL in the given directory.
    ///
    /// Creates the directory if it does not exist.
    /// Returns an error if `wal_dir` is empty.
    pub fn new(wal_dir: &Path) -> io::Result<Self> {
        if wal_dir.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WAL directory path cannot be empty",
            ));
        }
        fs::create_dir_all(wal_dir)?;
        let path = wal_dir.join("wal");
        let file = File::options().create(true).append(true).open(&path)?;
        Ok(Self {
            dir: wal_dir.to_path_buf(),
            file,
        })
    }

    /// Append a WAL operation to the log and sync to disk.
    pub fn append(&mut self, op: &WalOp) -> io::Result<()> {
        op.encode(&mut self.file)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Replay all entries in the WAL, returning them in order.
    ///
    /// Stops at EOF or at the first corrupted entry.
    pub fn replay(&self) -> io::Result<Vec<WalOp>> {
        let path = self.dir.join("wal");
        let file = File::open(&path)?;
        let mut reader = io::BufReader::new(file);
        let mut ops = Vec::new();

        loop {
            match WalOp::decode(&mut reader) {
                Ok(op) => ops.push(op),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
        }

        Ok(ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn meta(seq: u64) -> WalMeta {
        WalMeta { seq }
    }

    // --- round-trip ---

    #[test]
    fn put_round_trip() {
        let op = WalOp::Put {
            meta: meta(1),
            key: b"user:alice".to_vec(),
            value: b"admin".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        let decoded = WalOp::decode(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn delete_round_trip() {
        let op = WalOp::Delete {
            meta: meta(42),
            key: b"session:expired:abc123".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        let decoded = WalOp::decode(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn multiple_entries_round_trip() {
        let ops = vec![
            WalOp::Put {
                meta: meta(1),
                key: b"order:1001".to_vec(),
                value: b"pending".to_vec(),
            },
            WalOp::Delete {
                meta: meta(2),
                key: b"order:0999".to_vec(),
            },
            WalOp::Put {
                meta: meta(3),
                key: b"order:1002".to_vec(),
                value: b"shipped".to_vec(),
            },
        ];

        let mut buf = Vec::new();
        for op in &ops {
            op.encode(&mut buf).unwrap();
        }

        let mut cursor = Cursor::new(&buf);
        for expected in &ops {
            let decoded = WalOp::decode(&mut cursor).unwrap();
            assert_eq!(&decoded, expected);
        }
    }

    #[test]
    fn seq_is_preserved_in_round_trip() {
        let op = WalOp::Put {
            meta: meta(99),
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        let decoded = WalOp::decode(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded.meta().seq, 99);
    }

    // --- checksum verification ---

    #[test]
    fn corrupted_value_fails_checksum() {
        let op = WalOp::Put {
            meta: meta(1),
            key: b"metric:cpu_usage".to_vec(),
            value: b"72.5".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the value region.
        // Header: op(1) + meta_len(2) + seq(8) + key_len(2) + val_len(2) + key(15) = 30
        buf[30] ^= 0xFF;

        let result = WalOp::decode(&mut Cursor::new(&buf));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn corrupted_key_fails_checksum() {
        let op = WalOp::Delete {
            meta: meta(1),
            key: b"cache:page:/home".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the key region.
        // Header: op(1) + meta_len(2) + seq(8) + key_len(2) = 13, key starts at 13.
        buf[13] ^= 0xFF;

        let result = WalOp::decode(&mut Cursor::new(&buf));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    #[test]
    fn corrupted_seq_fails_checksum() {
        let op = WalOp::Put {
            meta: meta(1),
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the seq region (starts at offset 3, after op + meta_len).
        buf[3] ^= 0xFF;

        let result = WalOp::decode(&mut Cursor::new(&buf));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    // --- WriteAheadLog creation ---

    #[test]
    fn wal_creates_directory_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal_data");

        let _wal = WriteAheadLog::new(&wal_dir).unwrap();
        assert!(wal_dir.exists());
        assert!(wal_dir.join("wal").exists());
    }

    #[test]
    fn wal_opens_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal_data");
        fs::create_dir_all(&wal_dir).unwrap();

        let _wal = WriteAheadLog::new(&wal_dir).unwrap();
        assert!(wal_dir.join("wal").exists());
    }

    #[test]
    fn wal_rejects_empty_path() {
        let result = WriteAheadLog::new(Path::new(""));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("cannot be empty"));
    }

    // --- append and replay ---

    #[test]
    fn append_and_replay_single_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut wal = WriteAheadLog::new(&tmp.path().join("wal_data")).unwrap();

        let op = WalOp::Put {
            meta: meta(1),
            key: b"user:alice".to_vec(),
            value: b"admin".to_vec(),
        };
        wal.append(&op).unwrap();

        let replayed = wal.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0], op);
    }

    #[test]
    fn append_and_replay_multiple_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut wal = WriteAheadLog::new(&tmp.path().join("wal_data")).unwrap();

        let ops = vec![
            WalOp::Put {
                meta: meta(1),
                key: b"order:1001".to_vec(),
                value: b"pending".to_vec(),
            },
            WalOp::Delete {
                meta: meta(2),
                key: b"order:0999".to_vec(),
            },
            WalOp::Put {
                meta: meta(3),
                key: b"order:1002".to_vec(),
                value: b"shipped".to_vec(),
            },
        ];

        for op in &ops {
            wal.append(op).unwrap();
        }

        let replayed = wal.replay().unwrap();
        assert_eq!(replayed, ops);
    }

    #[test]
    fn replay_empty_wal_returns_no_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = WriteAheadLog::new(&tmp.path().join("wal_data")).unwrap();

        let replayed = wal.replay().unwrap();
        assert!(replayed.is_empty());
    }

    #[test]
    fn replay_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_dir = tmp.path().join("wal_data");

        let op = WalOp::Put {
            meta: meta(1),
            key: b"config:theme".to_vec(),
            value: b"dark".to_vec(),
        };

        // Write and drop.
        {
            let mut wal = WriteAheadLog::new(&wal_dir).unwrap();
            wal.append(&op).unwrap();
        }

        // Reopen and replay.
        let wal = WriteAheadLog::new(&wal_dir).unwrap();
        let replayed = wal.replay().unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0], op);
    }

    #[test]
    fn unknown_op_code_fails() {
        // op=0xFF, then enough bytes for meta_len + padding
        let buf = vec![
            0xFF, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x01, 0x41, 0, 0, 0, 0,
        ];

        let result = WalOp::decode(&mut Cursor::new(&buf));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown WAL op code")
        );
    }
}
