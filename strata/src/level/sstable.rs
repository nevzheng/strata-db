use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};

use crate::ReadStore;
use crate::memstore::{InternalKey, OpType, ReadError};

/// A fully materialized SSTable: metadata plus sorted entries in memory.
pub struct SsTable {
    pub table_ref: SsTableRef,
    pub entries: Vec<(InternalKey, Vec<u8>)>,
}

impl SsTable {
    /// Open an SSTable from disk, fully materializing its entries in memory.
    pub fn open(table_ref: SsTableRef) -> io::Result<Self> {
        let mut file_data = Vec::new();
        File::open(&table_ref.path)?.read_to_end(&mut file_data)?;

        let (data_size, _, _) = read_footer(&file_data)?;
        let mut cursor = std::io::Cursor::new(&file_data[..data_size as usize]);
        let mut entries = Vec::new();

        while (cursor.position() as usize) < data_size as usize {
            entries.push(read_entry(&mut cursor)?);
        }

        Ok(Self { table_ref, entries })
    }
}

impl ReadStore for SsTable {
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError> {
        let probe = InternalKey {
            key: key.to_vec(),
            seq: max_seq,
            op: OpType::Put,
        };
        let idx = self.entries.partition_point(|(ik, _)| ik < &probe);
        if idx < self.entries.len() && self.entries[idx].0.key == key {
            return match self.entries[idx].0.op {
                OpType::Put => Ok(Some(self.entries[idx].1.clone())),
                OpType::Delete => Ok(None),
            };
        }
        Ok(None)
    }

    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_ {
        let start_idx = match range.start_bound() {
            Bound::Included(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: u64::MAX,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik < &probe)
            }
            Bound::Excluded(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: 0,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik <= &probe)
            }
            Bound::Unbounded => 0,
        };
        let end_idx = match range.end_bound() {
            Bound::Included(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: 0,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik <= &probe)
            }
            Bound::Excluded(k) => {
                let probe = InternalKey {
                    key: k.clone(),
                    seq: u64::MAX,
                    op: OpType::Put,
                };
                self.entries.partition_point(|(ik, _)| ik < &probe)
            }
            Bound::Unbounded => self.entries.len(),
        };
        self.entries[start_idx..end_idx]
            .iter()
            .filter(move |(ik, _)| ik.seq <= max_seq)
            .map(|(ik, v)| Ok((ik.clone(), v.clone())))
    }
}

/// Lightweight metadata for an SSTable on disk.
#[derive(Clone)]
pub struct SsTableRef {
    pub id: u64,
    pub path: PathBuf,
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
    pub data_size: usize,
}

/// Encoded byte size of one entry.
///
/// Wire format: `| key_len (2B) | key | seq (8B) | op (1B) | val_len (2B) | value |`
fn encoded_entry_size(key: &InternalKey, value: &[u8]) -> usize {
    2 + key.key.len() + 8 + 1 + 2 + value.len()
}

fn write_entry(w: &mut impl Write, key: &InternalKey, value: &[u8]) -> io::Result<()> {
    key.encode(w)?;
    w.write_all(&(value.len() as u16).to_be_bytes())?;
    w.write_all(value)?;
    Ok(())
}

fn read_entry(r: &mut impl Read) -> io::Result<(InternalKey, Vec<u8>)> {
    let ik = InternalKey::decode(r)?;
    let mut val_len_buf = [0u8; 2];
    r.read_exact(&mut val_len_buf)?;
    let val_len = u16::from_be_bytes(val_len_buf) as usize;
    let mut value = vec![0u8; val_len];
    r.read_exact(&mut value)?;
    Ok((ik, value))
}

/// Footer wire format:
/// `| data_size (4B) | max_key_len (2B) | max_key | min_key_len (2B) | min_key | footer_size (4B) |`
fn write_footer(
    w: &mut impl Write,
    data_size: u32,
    min_key: &[u8],
    max_key: &[u8],
) -> io::Result<()> {
    // footer_size = 4 (data_size) + 2 + max_key.len() + 2 + min_key.len() + 4 (footer_size itself)
    let footer_size: u32 = 4 + 2 + max_key.len() as u32 + 2 + min_key.len() as u32 + 4;
    w.write_all(&data_size.to_be_bytes())?;
    w.write_all(&(max_key.len() as u16).to_be_bytes())?;
    w.write_all(max_key)?;
    w.write_all(&(min_key.len() as u16).to_be_bytes())?;
    w.write_all(min_key)?;
    w.write_all(&footer_size.to_be_bytes())?;
    Ok(())
}

/// Read the footer from the end of a file's bytes, returning `(data_size, min_key, max_key)`.
fn read_footer(data: &[u8]) -> io::Result<(u32, Vec<u8>, Vec<u8>)> {
    if data.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file too small for footer",
        ));
    }
    let footer_size = u32::from_be_bytes(data[data.len() - 4..].try_into().unwrap()) as usize;
    if footer_size > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "footer_size exceeds file size",
        ));
    }
    let footer = &data[data.len() - footer_size..];
    let mut cursor = std::io::Cursor::new(footer);

    let mut buf4 = [0u8; 4];
    cursor.read_exact(&mut buf4)?;
    let data_size = u32::from_be_bytes(buf4);

    let mut buf2 = [0u8; 2];
    cursor.read_exact(&mut buf2)?;
    let max_key_len = u16::from_be_bytes(buf2) as usize;
    let mut max_key = vec![0u8; max_key_len];
    cursor.read_exact(&mut max_key)?;

    cursor.read_exact(&mut buf2)?;
    let min_key_len = u16::from_be_bytes(buf2) as usize;
    let mut min_key = vec![0u8; min_key_len];
    cursor.read_exact(&mut min_key)?;

    Ok((data_size, min_key, max_key))
}

/// Write sorted entries to SSTable files on disk, splitting when a file
/// exceeds `max_file_size`. Returns an `SsTableRef` per file written.
///
/// Each file is named `{id}.sst` in `dir`. IDs start at `start_id` and
/// increment. A footer with min/max keys and data size is appended to each file.
pub fn write_sstables(
    dir: &Path,
    start_id: u64,
    max_file_size: usize,
    entries: impl IntoIterator<Item = (InternalKey, Vec<u8>)>,
) -> io::Result<Vec<SsTableRef>> {
    fs::create_dir_all(dir)?;

    let mut tables = Vec::new();
    let mut iter = entries.into_iter().peekable();
    let mut id = start_id;

    while iter.peek().is_some() {
        let path = dir.join(format!("{id}.sst"));
        let file = File::create(&path)?;
        let mut writer = BufWriter::new(file);

        let mut bytes_written = 0usize;
        let mut min_key: Option<Vec<u8>> = None;
        let mut max_key: Vec<u8> = Vec::new();

        while let Some((key, value)) = iter.peek() {
            let size = encoded_entry_size(key, value);
            if bytes_written > 0 && bytes_written + size > max_file_size {
                break;
            }
            let (key, value) = iter.next().unwrap();
            write_entry(&mut writer, &key, &value)?;
            bytes_written += size;
            if min_key.is_none() {
                min_key = Some(key.key.clone());
            }
            max_key = key.key;
        }

        let min_key = min_key.unwrap_or_default();
        write_footer(&mut writer, bytes_written as u32, &min_key, &max_key)?;
        writer.flush()?;
        writer.get_ref().sync_data()?;

        tables.push(SsTableRef {
            id,
            path,
            min_key,
            max_key,
            data_size: bytes_written,
        });
        id += 1;
    }

    Ok(tables)
}

/// Read an SSTable's metadata from a file on disk, returning an `SsTableRef`.
pub fn read_sstable_ref(path: &Path, id: u64) -> io::Result<SsTableRef> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;

    let (data_size, min_key, max_key) = read_footer(&data)?;
    Ok(SsTableRef {
        id,
        path: path.to_path_buf(),
        min_key,
        max_key,
        data_size: data_size as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_entry(key: &[u8], value: &[u8], seq: u64) -> (InternalKey, Vec<u8>) {
        (
            InternalKey {
                key: key.to_vec(),
                seq,
                op: OpType::Put,
            },
            value.to_vec(),
        )
    }

    fn delete_entry(key: &[u8], seq: u64) -> (InternalKey, Vec<u8>) {
        (
            InternalKey {
                key: key.to_vec(),
                seq,
                op: OpType::Delete,
            },
            Vec::new(),
        )
    }

    /// Write entries to a single SSTable file and open it.
    fn write_and_open(mut entries: Vec<(InternalKey, Vec<u8>)>) -> SsTable {
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sst");
        let refs = write_sstables(&dir, 0, usize::MAX, entries).unwrap();
        assert_eq!(refs.len(), 1);
        SsTable::open(refs.into_iter().next().unwrap()).unwrap()
    }

    // --- SsTable + ReadStore tests ---

    #[test]
    fn sstable_get_at_returns_latest_put() {
        let table = write_and_open(vec![put_entry(b"a", b"v1", 1), put_entry(b"a", b"v2", 2)]);
        assert_eq!(table.get_at(b"a", u64::MAX).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn sstable_get_at_returns_none_for_tombstone() {
        let table = write_and_open(vec![put_entry(b"a", b"v1", 1), delete_entry(b"a", 2)]);
        assert_eq!(table.get_at(b"a", u64::MAX).unwrap(), None);
    }

    #[test]
    fn sstable_get_at_returns_none_for_missing_key() {
        let table = write_and_open(vec![put_entry(b"a", b"v1", 1)]);
        assert_eq!(table.get_at(b"missing", u64::MAX).unwrap(), None);
    }

    #[test]
    fn sstable_scan_at_returns_range() {
        let table = write_and_open(vec![
            put_entry(b"a", b"1", 1),
            put_entry(b"b", b"2", 2),
            put_entry(b"c", b"3", 3),
            put_entry(b"d", b"4", 4),
        ]);
        let results: Vec<_> = table
            .scan_at(b"b".to_vec()..=b"c".to_vec(), u64::MAX)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0.key, b"b");
        assert_eq!(results[1].0.key, b"c");
    }

    #[test]
    fn sstable_scan_at_unbounded_returns_all() {
        let table = write_and_open(vec![put_entry(b"a", b"1", 1), put_entry(b"b", b"2", 2)]);
        let results: Vec<_> = table
            .scan_at(.., u64::MAX)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    // --- write / read round-trip tests ---

    #[test]
    fn write_round_trip_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sst");

        let entries = vec![
            put_entry(b"a", b"v1", 1),
            put_entry(b"b", b"v2", 2),
            delete_entry(b"c", 3),
        ];
        let refs = write_sstables(&dir, 0, 4096, entries).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, 0);
        assert_eq!(refs[0].min_key, b"a");
        assert_eq!(refs[0].max_key, b"c");
        assert!(refs[0].data_size > 0);

        let table = SsTable::open(refs.into_iter().next().unwrap()).unwrap();
        assert_eq!(table.table_ref.min_key, b"a");
        assert_eq!(table.table_ref.max_key, b"c");
        assert_eq!(table.entries.len(), 3);
        assert_eq!(table.entries[0].0.key, b"a");
        assert_eq!(table.entries[0].1, b"v1");
        assert_eq!(table.entries[1].0.key, b"b");
        assert_eq!(table.entries[1].1, b"v2");
        assert_eq!(table.entries[2].0.key, b"c");
        assert_eq!(table.entries[2].0.op, OpType::Delete);
    }

    #[test]
    fn write_splits_files_on_overflow() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sst");

        let entry_size = encoded_entry_size(
            &InternalKey {
                key: b"x".to_vec(),
                seq: 1,
                op: OpType::Put,
            },
            b"v",
        );
        let max_file_size = entry_size * 2;

        let entries: Vec<_> = (0..5u8)
            .map(|i| put_entry(&[b'a' + i], b"v", i as u64 + 1))
            .collect();
        let refs = write_sstables(&dir, 10, max_file_size, entries).unwrap();

        // 5 entries at 2 per file = 3 files (2, 2, 1)
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].id, 10);
        assert_eq!(refs[0].min_key, b"a");
        assert_eq!(refs[0].max_key, b"b");
        assert_eq!(refs[1].id, 11);
        assert_eq!(refs[1].min_key, b"c");
        assert_eq!(refs[1].max_key, b"d");
        assert_eq!(refs[2].id, 12);
        assert_eq!(refs[2].min_key, b"e");
        assert_eq!(refs[2].max_key, b"e");

        let expected_counts = [2, 2, 1];
        for (r, &expected) in refs.into_iter().zip(&expected_counts) {
            let table = SsTable::open(r).unwrap();
            assert_eq!(table.entries.len(), expected);
        }
    }

    #[test]
    fn write_empty_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sst");

        let tables = write_sstables(&dir, 0, 4096, std::iter::empty()).unwrap();
        assert!(tables.is_empty());
    }

    #[test]
    fn encoded_entry_size_is_correct() {
        let key = InternalKey {
            key: b"hello".to_vec(),
            seq: 1,
            op: OpType::Put,
        };
        // key_len(2) + key(5) + seq(8) + op(1) + val_len(2) + value(5) = 23
        assert_eq!(encoded_entry_size(&key, b"world"), 23);
    }
}
