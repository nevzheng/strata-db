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
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl ManifestOp {
    fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            ManifestOp::AddTable {
                level,
                run_id,
                sst_id,
                data_size,
                min_key,
                max_key,
            } => {
                w.write_all(&[OP_ADD_TABLE])?;
                w.write_all(&level.to_be_bytes())?;
                w.write_all(&run_id.to_be_bytes())?;
                w.write_all(&sst_id.to_be_bytes())?;
                w.write_all(&data_size.to_be_bytes())?;
                w.write_all(&(min_key.len() as u16).to_be_bytes())?;
                w.write_all(min_key)?;
                w.write_all(&(max_key.len() as u16).to_be_bytes())?;
                w.write_all(max_key)?;
            }
            ManifestOp::RemoveTable { level, sst_id } => {
                w.write_all(&[OP_REMOVE_TABLE])?;
                w.write_all(&level.to_be_bytes())?;
                w.write_all(&sst_id.to_be_bytes())?;
            }
        }
        Ok(())
    }

    fn decode(r: &mut impl Read) -> io::Result<Self> {
        let mut op = [0u8; 1];
        r.read_exact(&mut op)?;

        let mut level_buf = [0u8; 2];
        r.read_exact(&mut level_buf)?;
        let level = u16::from_be_bytes(level_buf);

        match op[0] {
            OP_ADD_TABLE => {
                let mut buf8 = [0u8; 8];
                r.read_exact(&mut buf8)?;
                let run_id = u64::from_be_bytes(buf8);

                r.read_exact(&mut buf8)?;
                let sst_id = u64::from_be_bytes(buf8);

                let mut buf4 = [0u8; 4];
                r.read_exact(&mut buf4)?;
                let data_size = u32::from_be_bytes(buf4);

                let mut buf2 = [0u8; 2];
                r.read_exact(&mut buf2)?;
                let min_key_len = u16::from_be_bytes(buf2) as usize;
                let mut min_key = vec![0u8; min_key_len];
                r.read_exact(&mut min_key)?;

                r.read_exact(&mut buf2)?;
                let max_key_len = u16::from_be_bytes(buf2) as usize;
                let mut max_key = vec![0u8; max_key_len];
                r.read_exact(&mut max_key)?;

                Ok(ManifestOp::AddTable {
                    level,
                    run_id,
                    sst_id,
                    data_size,
                    min_key,
                    max_key,
                })
            }
            OP_REMOVE_TABLE => {
                let mut buf8 = [0u8; 8];
                r.read_exact(&mut buf8)?;
                let sst_id = u64::from_be_bytes(buf8);

                Ok(ManifestOp::RemoveTable { level, sst_id })
            }
            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown manifest opcode: 0x{unknown:02x}"),
            )),
        }
    }
}

/// A batch of manifest operations written atomically.
///
/// Wire format (all integers big-endian):
/// ```text
/// | op_count (4B) | max_seq (8B) | op₁ | op₂ | ... | opₙ | crc32 (4B) |
/// ```
///
/// The CRC32 covers everything from `op_count` through the last op byte.
/// On replay, a truncated or corrupted batch is discarded entirely.
#[derive(Debug, PartialEq, Eq)]
pub struct ManifestBatch {
    pub ops: Vec<ManifestOp>,
    pub max_seq: u64,
}

impl ManifestBatch {
    pub fn encode(&self, w: &mut impl Write) -> io::Result<()> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.ops.len() as u32).to_be_bytes());
        buf.extend_from_slice(&self.max_seq.to_be_bytes());
        for op in &self.ops {
            op.encode(&mut buf)?;
        }
        let mut hasher = Hasher::new();
        hasher.update(&buf);
        let crc = hasher.finalize();

        w.write_all(&buf)?;
        w.write_all(&crc.to_be_bytes())?;
        Ok(())
    }

    pub fn decode(r: &mut impl Read) -> io::Result<Self> {
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let op_count = u32::from_be_bytes(buf4) as usize;

        let mut buf8 = [0u8; 8];
        r.read_exact(&mut buf8)?;
        let max_seq = u64::from_be_bytes(buf8);

        let mut hasher = Hasher::new();
        hasher.update(&buf4);
        hasher.update(&buf8);

        let mut ops = Vec::with_capacity(op_count);
        for _ in 0..op_count {
            let mut op_bytes = Vec::new();
            let op = ManifestOp::decode(&mut TeeReader::new(r, &mut op_bytes))?;
            hasher.update(&op_bytes);
            ops.push(op);
        }

        let expected = hasher.finalize();
        let mut crc_buf = [0u8; 4];
        r.read_exact(&mut crc_buf)?;
        let actual = u32::from_be_bytes(crc_buf);

        if expected != actual {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("batch checksum mismatch: expected {expected:#010x}, got {actual:#010x}"),
            ));
        }

        Ok(Self { ops, max_seq })
    }
}

/// A reader that captures all bytes read into a side buffer.
struct TeeReader<'a, R> {
    inner: &'a mut R,
    capture: &'a mut Vec<u8>,
}

impl<'a, R> TeeReader<'a, R> {
    fn new(inner: &'a mut R, capture: &'a mut Vec<u8>) -> Self {
        Self { inner, capture }
    }
}

impl<R: Read> Read for TeeReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.capture.extend_from_slice(&buf[..n]);
        Ok(n)
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
    max_seq: u64,
}

impl Manifest {
    /// Open or create a manifest file, replaying existing entries.
    pub fn new(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let (tables, max_seq) = if path.exists() {
            Self::replay(path)?
        } else {
            (HashMap::new(), 0)
        };

        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self {
            file,
            tables,
            max_seq,
        })
    }

    /// Append a batch of operations atomically to the manifest log.
    ///
    /// The batch is framed with a header (op count + max_seq) and a
    /// trailing CRC32 covering the entire batch. Written in a single
    /// `write_all` + `sync_data` call.
    pub fn append(&mut self, ops: &[ManifestOp], max_seq: u64, dir: &Path) -> io::Result<()> {
        let batch = ManifestBatch {
            ops: ops.to_vec(),
            max_seq,
        };
        let mut buf = Vec::new();
        batch.encode(&mut buf)?;
        self.file.write_all(&buf)?;
        self.file.sync_data()?;
        for op in ops {
            self.apply(op, dir);
        }
        if max_seq > self.max_seq {
            self.max_seq = max_seq;
        }
        Ok(())
    }

    /// Current set of active tables.
    pub fn tables(&self) -> &HashMap<u64, ManifestEntry> {
        &self.tables
    }

    /// Highest sequence number persisted across all batches.
    pub fn max_seq(&self) -> u64 {
        self.max_seq
    }

    fn replay(path: &Path) -> io::Result<(HashMap<u64, ManifestEntry>, u64)> {
        let mut file = File::open(path)?;
        let mut tables = HashMap::new();
        let mut max_seq = 0u64;
        let dir = path.parent().unwrap_or(Path::new("."));

        loop {
            match ManifestBatch::decode(&mut file) {
                Ok(batch) => {
                    if batch.max_seq > max_seq {
                        max_seq = batch.max_seq;
                    }
                    for op in &batch.ops {
                        Self::apply_to(op, dir, &mut tables);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => break,
                Err(e) => return Err(e),
            }
        }

        Ok((tables, max_seq))
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
    fn batch_round_trip() {
        let batch = ManifestBatch {
            ops: vec![
                ManifestOp::AddTable {
                    level: 2,
                    run_id: 42,
                    sst_id: 100,
                    data_size: 4096,
                    min_key: b"aaa".to_vec(),
                    max_key: b"zzz".to_vec(),
                },
                ManifestOp::RemoveTable {
                    level: 1,
                    sst_id: 55,
                },
            ],
            max_seq: 99,
        };
        let mut buf = Vec::new();
        batch.encode(&mut buf).unwrap();
        let decoded = ManifestBatch::decode(&mut buf.as_slice()).unwrap();
        assert_eq!(batch, decoded);
    }

    #[test]
    fn corrupted_batch_checksum_detected() {
        let batch = ManifestBatch {
            ops: vec![ManifestOp::AddTable {
                level: 0,
                run_id: 1,
                sst_id: 1,
                data_size: 100,
                min_key: b"a".to_vec(),
                max_key: b"z".to_vec(),
            }],
            max_seq: 10,
        };
        let mut buf = Vec::new();
        batch.encode(&mut buf).unwrap();

        // Corrupt a data byte.
        buf[5] ^= 0xff;

        let err = ManifestBatch::decode(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn unknown_opcode_rejected() {
        // A batch with 1 op, max_seq=0, then an invalid opcode.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_be_bytes()); // op_count
        buf.extend_from_slice(&0u64.to_be_bytes()); // max_seq
        buf.extend_from_slice(&[0xff]); // bad opcode
        // Pad enough bytes so decode doesn't hit EOF before the opcode error.
        buf.extend_from_slice(&[0u8; 20]);
        let err = ManifestBatch::decode(&mut buf.as_slice()).unwrap_err();
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
                    42,
                    dir,
                )
                .unwrap();
            manifest
                .append(
                    &[ManifestOp::RemoveTable {
                        level: 0,
                        sst_id: 10,
                    }],
                    42,
                    dir,
                )
                .unwrap();

            assert_eq!(manifest.tables().len(), 1);
            assert!(manifest.tables().contains_key(&11));
            assert_eq!(manifest.max_seq(), 42);
        }

        // Reopen and verify replay produces the same state.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert_eq!(manifest.tables().len(), 1);
        assert_eq!(manifest.max_seq(), 42);
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
        assert_eq!(manifest.max_seq(), 0);

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
                        sst_id,
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
                201,
                dir,
            )
            .unwrap();

        assert_eq!(manifest.tables().len(), 4);
        assert!(!manifest.tables().contains_key(&100));
        assert!(!manifest.tables().contains_key(&101));
    }

    #[test]
    fn truncated_batch_discarded_on_replay() {
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
                    5,
                    dir,
                )
                .unwrap();
        }

        // Simulate a crash by appending partial bytes.
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&manifest_path)
                .unwrap();
            file.write_all(&[0x00, 0x00, 0x00, 0x01, 0x00]).unwrap();
        }

        // Replay should recover the first batch and discard the truncated tail.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert_eq!(manifest.tables().len(), 1);
        assert!(manifest.tables().contains_key(&10));
        assert_eq!(manifest.max_seq(), 5);
    }

    #[test]
    fn corrupted_batch_discarded_on_replay() {
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
                    5,
                    dir,
                )
                .unwrap();
        }

        // Append a complete but corrupted second batch.
        {
            let batch = ManifestBatch {
                ops: vec![ManifestOp::AddTable {
                    level: 0,
                    run_id: 2,
                    sst_id: 20,
                    data_size: 100,
                    min_key: b"m".to_vec(),
                    max_key: b"n".to_vec(),
                }],
                max_seq: 10,
            };
            let mut buf = Vec::new();
            batch.encode(&mut buf).unwrap();
            buf[5] ^= 0xff;

            let mut file = OpenOptions::new()
                .append(true)
                .open(&manifest_path)
                .unwrap();
            file.write_all(&buf).unwrap();
        }

        // Replay should keep first batch, discard corrupted second.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert_eq!(manifest.tables().len(), 1);
        assert!(manifest.tables().contains_key(&10));
        assert_eq!(manifest.max_seq(), 5);
    }

    #[test]
    fn max_seq_tracks_highest_across_batches() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("MANIFEST");
        let dir = tmp.path();

        let mut manifest = Manifest::new(&manifest_path).unwrap();
        manifest
            .append(
                &[ManifestOp::AddTable {
                    level: 0,
                    run_id: 0,
                    sst_id: 1,
                    data_size: 100,
                    min_key: b"a".to_vec(),
                    max_key: b"z".to_vec(),
                }],
                50,
                dir,
            )
            .unwrap();
        manifest
            .append(
                &[ManifestOp::AddTable {
                    level: 0,
                    run_id: 1,
                    sst_id: 2,
                    data_size: 100,
                    min_key: b"a".to_vec(),
                    max_key: b"z".to_vec(),
                }],
                100,
                dir,
            )
            .unwrap();
        assert_eq!(manifest.max_seq(), 100);

        // Reopen — max_seq should survive.
        let manifest = Manifest::new(&manifest_path).unwrap();
        assert_eq!(manifest.max_seq(), 100);
    }
}
