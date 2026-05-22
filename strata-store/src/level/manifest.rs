use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use crc32fast::Hasher;

use super::SsTableRef;

const OP_ADD_TABLE: u8 = 0x01;
const OP_REMOVE_TABLE: u8 = 0x02;

/// A manifest log entry.
///
/// The manifest file is an append-only sequence of individually checksummed
/// records. Multiple records are written as a single atomic batch (one
/// `write_all` + `sync_data`), so either all records in a batch land on
/// disk or none do. On replay, a truncated or corrupted trailing record
/// is treated as an incomplete batch and discarded.
///
/// Wire formats (all integers big-endian):
///
/// AddTable:
/// ```text
/// | 0x01 (1B) | level (2B) | run_id (8B) | sst_id (8B)
/// | data_size (4B) | min_key_len (2B) | min_key | max_key_len (2B) | max_key
/// | crc32 (4B) |
/// ```
///
/// RemoveTable:
/// ```text
/// | 0x02 (1B) | level (2B) | sst_id (8B) | crc32 (4B) |
/// ```
#[derive(Debug, PartialEq, Eq)]
pub enum ManifestOp {
    AddTable {
        level: u16,
        run_id: u64,
        sst_id: u64,
        data_size: u32,
        min_key: Vec<u8>,
        max_key: Vec<u8>,
    },
    RemoveTable {
        level: u16,
        sst_id: u64,
    },
}

fn write_and_hash(w: &mut impl Write, hasher: &mut Hasher, bytes: &[u8]) -> io::Result<()> {
    hasher.update(bytes);
    w.write_all(bytes)
}

fn read_and_hash(r: &mut impl Read, hasher: &mut Hasher, buf: &mut [u8]) -> io::Result<()> {
    r.read_exact(buf)?;
    hasher.update(buf);
    Ok(())
}

impl ManifestOp {
    pub fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let mut hasher = Hasher::new();

        match self {
            ManifestOp::AddTable {
                level,
                run_id,
                sst_id,
                data_size,
                min_key,
                max_key,
            } => {
                write_and_hash(w, &mut hasher, &[OP_ADD_TABLE])?;
                write_and_hash(w, &mut hasher, &level.to_be_bytes())?;
                write_and_hash(w, &mut hasher, &run_id.to_be_bytes())?;
                write_and_hash(w, &mut hasher, &sst_id.to_be_bytes())?;
                write_and_hash(w, &mut hasher, &data_size.to_be_bytes())?;
                write_and_hash(w, &mut hasher, &(min_key.len() as u16).to_be_bytes())?;
                write_and_hash(w, &mut hasher, min_key)?;
                write_and_hash(w, &mut hasher, &(max_key.len() as u16).to_be_bytes())?;
                write_and_hash(w, &mut hasher, max_key)?;
            }
            ManifestOp::RemoveTable { level, sst_id } => {
                write_and_hash(w, &mut hasher, &[OP_REMOVE_TABLE])?;
                write_and_hash(w, &mut hasher, &level.to_be_bytes())?;
                write_and_hash(w, &mut hasher, &sst_id.to_be_bytes())?;
            }
        }

        let checksum = hasher.finalize();
        w.write_all(&checksum.to_be_bytes())?;
        Ok(())
    }

    pub fn decode(r: &mut impl Read) -> io::Result<Self> {
        let mut hasher = Hasher::new();

        let mut op = [0u8; 1];
        read_and_hash(r, &mut hasher, &mut op)?;

        let mut level_buf = [0u8; 2];
        read_and_hash(r, &mut hasher, &mut level_buf)?;
        let level = u16::from_be_bytes(level_buf);

        let entry = match op[0] {
            OP_ADD_TABLE => {
                let mut run_id_buf = [0u8; 8];
                read_and_hash(r, &mut hasher, &mut run_id_buf)?;
                let run_id = u64::from_be_bytes(run_id_buf);

                let mut sst_id_buf = [0u8; 8];
                read_and_hash(r, &mut hasher, &mut sst_id_buf)?;
                let sst_id = u64::from_be_bytes(sst_id_buf);

                let mut data_size_buf = [0u8; 4];
                read_and_hash(r, &mut hasher, &mut data_size_buf)?;
                let data_size = u32::from_be_bytes(data_size_buf);

                let mut min_key_len_buf = [0u8; 2];
                read_and_hash(r, &mut hasher, &mut min_key_len_buf)?;
                let min_key_len = u16::from_be_bytes(min_key_len_buf) as usize;

                let mut min_key = vec![0u8; min_key_len];
                read_and_hash(r, &mut hasher, &mut min_key)?;

                let mut max_key_len_buf = [0u8; 2];
                read_and_hash(r, &mut hasher, &mut max_key_len_buf)?;
                let max_key_len = u16::from_be_bytes(max_key_len_buf) as usize;

                let mut max_key = vec![0u8; max_key_len];
                read_and_hash(r, &mut hasher, &mut max_key)?;

                ManifestOp::AddTable {
                    level,
                    run_id,
                    sst_id,
                    data_size,
                    min_key,
                    max_key,
                }
            }
            OP_REMOVE_TABLE => {
                let mut sst_id_buf = [0u8; 8];
                read_and_hash(r, &mut hasher, &mut sst_id_buf)?;
                let sst_id = u64::from_be_bytes(sst_id_buf);

                ManifestOp::RemoveTable { level, sst_id }
            }
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown manifest opcode: 0x{unknown:02x}"),
                ));
            }
        };

        let expected = hasher.finalize();
        let mut checksum_buf = [0u8; 4];
        r.read_exact(&mut checksum_buf)?;
        let actual = u32::from_be_bytes(checksum_buf);

        if expected != actual {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "manifest checksum mismatch: expected {expected:#010x}, got {actual:#010x}"
                ),
            ));
        }

        Ok(entry)
    }
}

/// An active SSTable entry tracked by the manifest.
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub level: u16,
    pub run_id: u64,
    pub sst_ref: SsTableRef,
}

/// Persistent log tracking which SSTables are active across all levels.
pub struct Manifest {
    file: File,
    tables: HashMap<u64, ManifestEntry>,
}

impl Manifest {
    /// Open or create a manifest file, replaying existing entries.
    pub fn new(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tables = if path.exists() {
            Self::replay(path)?
        } else {
            HashMap::new()
        };

        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self { file, tables })
    }

    /// Append a batch of operations atomically to the manifest log.
    ///
    /// All ops are encoded to an in-memory buffer and written in a single
    /// `write_all` + `sync_data` call, so either the entire batch lands on
    /// disk or none of it does. On replay, a partially written trailing
    /// record is detected by CRC mismatch and discarded.
    pub fn append(&mut self, ops: &[ManifestOp], dir: &Path) -> io::Result<()> {
        let mut buf = Vec::new();
        for op in ops {
            op.encode(&mut buf)?;
        }
        self.file.write_all(&buf)?;
        self.file.sync_data()?;
        for op in ops {
            self.apply(op, dir);
        }
        Ok(())
    }

    /// Current set of active tables.
    pub fn tables(&self) -> &HashMap<u64, ManifestEntry> {
        &self.tables
    }

    fn replay(path: &Path) -> io::Result<HashMap<u64, ManifestEntry>> {
        let mut file = File::open(path)?;
        let mut tables = HashMap::new();
        let dir = path.parent().unwrap_or(Path::new("."));

        loop {
            match ManifestOp::decode(&mut file) {
                Ok(op) => Self::apply_to(&op, dir, &mut tables),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                // Treat checksum mismatch or truncated data at the tail as
                // a partial write from a crash — discard and stop replaying.
                Err(e) if e.kind() == io::ErrorKind::InvalidData => break,
                Err(e) => return Err(e),
            }
        }

        Ok(tables)
    }

    fn apply(&mut self, op: &ManifestOp, dir: &Path) {
        Self::apply_to(op, dir, &mut self.tables);
    }

    fn apply_to(op: &ManifestOp, dir: &Path, tables: &mut HashMap<u64, ManifestEntry>) {
        match op {
            ManifestOp::AddTable {
                level,
                run_id,
                sst_id,
                data_size,
                min_key,
                max_key,
            } => {
                tables.insert(
                    *sst_id,
                    ManifestEntry {
                        level: *level,
                        run_id: *run_id,
                        sst_ref: SsTableRef {
                            id: *sst_id,
                            path: dir.join(format!("sst/{sst_id}.sst")),
                            min_key: min_key.clone(),
                            max_key: max_key.clone(),
                            data_size: *data_size as usize,
                        },
                    },
                );
            }
            ManifestOp::RemoveTable { sst_id, .. } => {
                tables.remove(sst_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_table_round_trip() {
        let op = ManifestOp::AddTable {
            level: 2,
            run_id: 42,
            sst_id: 100,
            data_size: 4096,
            min_key: b"aaa".to_vec(),
            max_key: b"zzz".to_vec(),
        };
        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();
        let decoded = ManifestOp::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(op, decoded);
    }

    #[test]
    fn remove_table_round_trip() {
        let op = ManifestOp::RemoveTable {
            level: 1,
            sst_id: 55,
        };
        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();
        let decoded = ManifestOp::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(op, decoded);
    }

    #[test]
    fn corrupted_checksum_detected() {
        let op = ManifestOp::AddTable {
            level: 0,
            run_id: 1,
            sst_id: 1,
            data_size: 100,
            min_key: b"a".to_vec(),
            max_key: b"z".to_vec(),
        };
        let mut buf = Vec::new();
        op.encode(&mut buf).unwrap();

        // Corrupt the last byte (part of checksum).
        let last = buf.len() - 1;
        buf[last] ^= 0xff;

        let err = ManifestOp::decode(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn unknown_opcode_rejected() {
        let buf = vec![0xff, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        let err = ManifestOp::decode(&mut buf.as_slice()).unwrap_err();
        assert!(err.to_string().contains("unknown manifest opcode"));
    }

    #[test]
    fn manifest_replay_add_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("MANIFEST");
        let dir = tmp.path();

        {
            let mut manifest = Manifest::new(&manifest_path).unwrap();
            manifest
                .append(
                    &[
                        ManifestOp::AddTable {
                            level: 0,
                            run_id: 1,
                            sst_id: 10,
                            data_size: 500,
                            min_key: b"a".to_vec(),
                            max_key: b"m".to_vec(),
                        },
                        ManifestOp::AddTable {
                            level: 0,
                            run_id: 1,
                            sst_id: 11,
                            data_size: 600,
                            min_key: b"n".to_vec(),
                            max_key: b"z".to_vec(),
                        },
                    ],
                    dir,
                )
                .unwrap();
            manifest
                .append(
                    &[ManifestOp::RemoveTable {
                        level: 0,
                        sst_id: 10,
                    }],
                    dir,
                )
                .unwrap();

            assert_eq!(manifest.tables().len(), 1);
            assert!(manifest.tables().contains_key(&11));
        }

        // Reopen and verify replay produces the same state.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert_eq!(manifest.tables().len(), 1);
        let entry = manifest.tables().get(&11).unwrap();
        assert_eq!(entry.level, 0);
        assert_eq!(entry.run_id, 1);
        assert_eq!(entry.sst_ref.min_key, b"n");
        assert_eq!(entry.sst_ref.max_key, b"z");
        assert_eq!(entry.sst_ref.data_size, 600);
    }

    #[test]
    fn manifest_empty_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("MANIFEST");

        let manifest = Manifest::new(&manifest_path).unwrap();
        assert!(manifest.tables().is_empty());

        // Reopen empty manifest.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert!(manifest.tables().is_empty());
    }

    #[test]
    fn manifest_multiple_levels_and_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("MANIFEST");
        let dir = tmp.path();
        let mut manifest = Manifest::new(&manifest_path).unwrap();

        // Add tables across multiple levels and runs.
        for level in 0..3u16 {
            for run in 0..2u64 {
                let sst_id = (level as u64) * 100 + run;
                manifest
                    .append(
                        &[ManifestOp::AddTable {
                            level,
                            run_id: run,
                            sst_id,
                            data_size: 1024,
                            min_key: vec![b'a' + level as u8],
                            max_key: vec![b'z'],
                        }],
                        dir,
                    )
                    .unwrap();
            }
        }

        assert_eq!(manifest.tables().len(), 6);

        // Remove all tables from level 1 in a single batch.
        manifest
            .append(
                &[
                    ManifestOp::RemoveTable {
                        level: 1,
                        sst_id: 100,
                    },
                    ManifestOp::RemoveTable {
                        level: 1,
                        sst_id: 101,
                    },
                ],
                dir,
            )
            .unwrap();

        assert_eq!(manifest.tables().len(), 4);
        assert!(!manifest.tables().contains_key(&100));
        assert!(!manifest.tables().contains_key(&101));
    }

    #[test]
    fn truncated_trailing_record_discarded_on_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("MANIFEST");
        let dir = tmp.path();

        {
            let mut manifest = Manifest::new(&manifest_path).unwrap();
            // Write a complete batch.
            manifest
                .append(
                    &[ManifestOp::AddTable {
                        level: 0,
                        run_id: 1,
                        sst_id: 10,
                        data_size: 500,
                        min_key: b"a".to_vec(),
                        max_key: b"z".to_vec(),
                    }],
                    dir,
                )
                .unwrap();
        }

        // Simulate a crash by appending a partial record directly to the file.
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&manifest_path)
                .unwrap();
            // Write a few bytes of a second record — not enough for a valid op.
            file.write_all(&[OP_ADD_TABLE, 0x00, 0x01, 0x00]).unwrap();
        }

        // Replay should recover the first record and discard the truncated tail.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert_eq!(manifest.tables().len(), 1);
        assert!(manifest.tables().contains_key(&10));
    }

    #[test]
    fn corrupted_trailing_record_discarded_on_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("MANIFEST");
        let dir = tmp.path();

        {
            let mut manifest = Manifest::new(&manifest_path).unwrap();
            manifest
                .append(
                    &[ManifestOp::AddTable {
                        level: 0,
                        run_id: 1,
                        sst_id: 10,
                        data_size: 500,
                        min_key: b"a".to_vec(),
                        max_key: b"z".to_vec(),
                    }],
                    dir,
                )
                .unwrap();
        }

        // Append a complete but corrupted second record.
        {
            let op = ManifestOp::AddTable {
                level: 0,
                run_id: 2,
                sst_id: 20,
                data_size: 100,
                min_key: b"m".to_vec(),
                max_key: b"n".to_vec(),
            };
            let mut buf = Vec::new();
            op.encode(&mut buf).unwrap();
            // Corrupt a data byte (not just checksum).
            buf[5] ^= 0xff;

            let mut file = OpenOptions::new()
                .append(true)
                .open(&manifest_path)
                .unwrap();
            file.write_all(&buf).unwrap();
        }

        // Replay should keep first record, discard corrupted second.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert_eq!(manifest.tables().len(), 1);
        assert!(manifest.tables().contains_key(&10));
    }
}
