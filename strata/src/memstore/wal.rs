use std::io::{self, Read, Write};

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
/// Put:    `| 0x01 (1B) | key_len (2B) | val_len (2B) | key | value | crc32 (4B) |`
/// Delete: `| 0x02 (1B) | key_len (2B) | key | crc32 (4B) |`
#[derive(Debug)]
pub enum WalOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl WalOp {
    /// Encode this operation to the writer, appending a CRC32 checksum.
    pub fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let mut hasher = Hasher::new();

        match self {
            WalOp::Put { key, value } => {
                let op = [OP_PUT];
                let key_len = (key.len() as u16).to_be_bytes();
                let val_len = (value.len() as u16).to_be_bytes();

                hasher.update(&op);
                hasher.update(&key_len);
                hasher.update(&val_len);
                hasher.update(key);
                hasher.update(value);

                w.write_all(&op)?;
                w.write_all(&key_len)?;
                w.write_all(&val_len)?;
                w.write_all(key)?;
                w.write_all(value)?;
            }
            WalOp::Delete { key } => {
                let op = [OP_DELETE];
                let key_len = (key.len() as u16).to_be_bytes();

                hasher.update(&op);
                hasher.update(&key_len);
                hasher.update(key);

                w.write_all(&op)?;
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

                WalOp::Put { key, value }
            }
            OP_DELETE => {
                // Read key.
                let mut key = vec![0u8; key_len];
                r.read_exact(&mut key)?;
                hasher.update(&key);

                WalOp::Delete { key }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // --- round-trip ---

    #[test]
    fn put_round_trip() {
        let op = WalOp::Put {
            key: b"user:alice".to_vec(),
            value: b"admin".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        let decoded = WalOp::decode(&mut Cursor::new(&buf)).unwrap();
        match decoded {
            WalOp::Put { key, value } => {
                assert_eq!(key, b"user:alice");
                assert_eq!(value, b"admin");
            }
            _ => panic!("expected Put"),
        }
    }

    #[test]
    fn delete_round_trip() {
        let op = WalOp::Delete {
            key: b"session:expired:abc123".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        let decoded = WalOp::decode(&mut Cursor::new(&buf)).unwrap();
        match decoded {
            WalOp::Delete { key } => {
                assert_eq!(key, b"session:expired:abc123");
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn multiple_entries_round_trip() {
        let ops = vec![
            WalOp::Put {
                key: b"order:1001".to_vec(),
                value: b"pending".to_vec(),
            },
            WalOp::Delete {
                key: b"order:0999".to_vec(),
            },
            WalOp::Put {
                key: b"order:1002".to_vec(),
                value: b"shipped".to_vec(),
            },
        ];

        let mut buf = Vec::new();
        for op in &ops {
            op.encode(&mut buf).unwrap();
        }

        let mut cursor = Cursor::new(&buf);
        // Entry 1: Put
        match WalOp::decode(&mut cursor).unwrap() {
            WalOp::Put { key, value } => {
                assert_eq!(key, b"order:1001");
                assert_eq!(value, b"pending");
            }
            _ => panic!("expected Put"),
        }
        // Entry 2: Delete
        match WalOp::decode(&mut cursor).unwrap() {
            WalOp::Delete { key } => assert_eq!(key, b"order:0999"),
            _ => panic!("expected Delete"),
        }
        // Entry 3: Put
        match WalOp::decode(&mut cursor).unwrap() {
            WalOp::Put { key, value } => {
                assert_eq!(key, b"order:1002");
                assert_eq!(value, b"shipped");
            }
            _ => panic!("expected Put"),
        }
    }

    // --- checksum verification ---

    #[test]
    fn corrupted_value_fails_checksum() {
        let op = WalOp::Put {
            key: b"metric:cpu_usage".to_vec(),
            value: b"72.5".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the value region.
        // Header: op(1) + key_len(2) + val_len(2) + key(15) = 20, value starts at 20.
        buf[20] ^= 0xFF;

        let result = WalOp::decode(&mut Cursor::new(&buf));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn corrupted_key_fails_checksum() {
        let op = WalOp::Delete {
            key: b"cache:page:/home".to_vec(),
        };

        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Flip a byte in the key region.
        // Header: op(1) + key_len(2) = 3, key starts at 3.
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

    #[test]
    fn unknown_op_code_fails() {
        let mut buf = vec![0xFF, 0x00, 0x01, 0x41, 0x00, 0x00, 0x00, 0x00];

        let result = WalOp::decode(&mut Cursor::new(&mut buf));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown WAL op code")
        );
    }
}
