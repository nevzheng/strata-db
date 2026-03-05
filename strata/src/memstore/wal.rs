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

/// The operation type for a WAL entry.
///
/// Wire formats (all integers big-endian):
///
/// Put:    `| 0x01 (1B) | seq (8B) | key_len (2B) | val_len (2B) | key | value | crc32 (4B) |`
/// Delete: `| 0x02 (1B) | seq (8B) | key_len (2B) | key | crc32 (4B) |`
#[derive(Debug, PartialEq, Eq)]
pub enum WalOp {
    Put {
        seq: u64,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        seq: u64,
        key: Vec<u8>,
    },
}

/// Helper to write bytes to both a writer and a hasher.
fn write_and_hash(w: &mut impl Write, hasher: &mut Hasher, bytes: &[u8]) -> io::Result<()> {
    hasher.update(bytes);
    w.write_all(bytes)
}

/// Helper to read exact bytes into a buffer, updating the hasher.
fn read_and_hash(r: &mut impl Read, hasher: &mut Hasher, buf: &mut [u8]) -> io::Result<()> {
    r.read_exact(buf)?;
    hasher.update(buf);
    Ok(())
}

impl WalOp {
    pub fn seq(&self) -> u64 {
        match self {
            WalOp::Put { seq, .. } | WalOp::Delete { seq, .. } => *seq,
        }
    }

    /// Encode this operation to the writer, appending a CRC32 checksum.
    pub fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let mut hasher = Hasher::new();

        match self {
            WalOp::Put { seq, key, value } => {
                write_and_hash(w, &mut hasher, &[OP_PUT])?;
                write_and_hash(w, &mut hasher, &seq.to_be_bytes())?;
                write_and_hash(w, &mut hasher, &(key.len() as u16).to_be_bytes())?;
                write_and_hash(w, &mut hasher, &(value.len() as u16).to_be_bytes())?;
                write_and_hash(w, &mut hasher, key)?;
                write_and_hash(w, &mut hasher, value)?;
            }
            WalOp::Delete { seq, key } => {
                write_and_hash(w, &mut hasher, &[OP_DELETE])?;
                write_and_hash(w, &mut hasher, &seq.to_be_bytes())?;
                write_and_hash(w, &mut hasher, &(key.len() as u16).to_be_bytes())?;
                write_and_hash(w, &mut hasher, key)?;
            }
        }

        let checksum = hasher.finalize();
        w.write_all(&checksum.to_be_bytes())?;
        Ok(())
    }

    /// Decode a WAL operation from the reader, verifying the CRC32 checksum.
    pub fn decode(r: &mut impl Read) -> io::Result<Self> {
        let mut hasher = Hasher::new();

        // Op code.
        let mut op = [0u8; 1];
        read_and_hash(r, &mut hasher, &mut op)?;

        // Sequence number.
        let mut seq_buf = [0u8; 8];
        read_and_hash(r, &mut hasher, &mut seq_buf)?;
        let seq = u64::from_be_bytes(seq_buf);

        // Key length.
        let mut key_len_buf = [0u8; 2];
        read_and_hash(r, &mut hasher, &mut key_len_buf)?;
        let key_len = u16::from_be_bytes(key_len_buf) as usize;

        let entry = match op[0] {
            OP_PUT => {
                let mut val_len_buf = [0u8; 2];
                read_and_hash(r, &mut hasher, &mut val_len_buf)?;
                let val_len = u16::from_be_bytes(val_len_buf) as usize;

                let mut key = vec![0u8; key_len];
                read_and_hash(r, &mut hasher, &mut key)?;

                let mut value = vec![0u8; val_len];
                read_and_hash(r, &mut hasher, &mut value)?;

                WalOp::Put { seq, key, value }
            }
            OP_DELETE => {
                let mut key = vec![0u8; key_len];
                read_and_hash(r, &mut hasher, &mut key)?;

                WalOp::Delete { seq, key }
            }
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown WAL op code: {:#x}", unknown),
                ));
            }
        };

        // Verify checksum.
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

    /// Truncate the WAL, discarding all entries.
    pub fn truncate(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
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

    // --- round-trip ---

    #[test]
    fn put_round_trip() {
        let op = WalOp::Put {
            seq: 1,
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
            seq: 42,
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
                seq: 1,
                key: b"order:1001".to_vec(),
                value: b"pending".to_vec(),
            },
            WalOp::Delete {
                seq: 2,
                key: b"order:0999".to_vec(),
            },
            WalOp::Put {
                seq: 3,
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
            seq: 99,
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        let decoded = WalOp::decode(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded.seq(), 99);
    }

    // --- checksum verification ---

    #[test]
    fn corrupted_value_fails_checksum() {
        let op = WalOp::Put {
            seq: 1,
            key: b"metric:cpu_usage".to_vec(),
            value: b"72.5".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the value region.
        // op(1) + seq(8) + key_len(2) + val_len(2) + key(15) = 28, value starts at 28.
        buf[28] ^= 0xFF;

        let result = WalOp::decode(&mut Cursor::new(&buf));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn corrupted_key_fails_checksum() {
        let op = WalOp::Delete {
            seq: 1,
            key: b"cache:page:/home".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the key region.
        // op(1) + seq(8) + key_len(2) = 11, key starts at 11.
        buf[11] ^= 0xFF;

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
            seq: 1,
            key: b"k".to_vec(),
            value: b"v".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the seq region (starts at offset 1, after op).
        buf[1] ^= 0xFF;

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
            seq: 1,
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
                seq: 1,
                key: b"order:1001".to_vec(),
                value: b"pending".to_vec(),
            },
            WalOp::Delete {
                seq: 2,
                key: b"order:0999".to_vec(),
            },
            WalOp::Put {
                seq: 3,
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
            seq: 1,
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
        // op=0xFF, then 8 bytes for seq, then key_len + key + fake crc
        let buf = vec![0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x01, 0x41, 0, 0, 0, 0];

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
