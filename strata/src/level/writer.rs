use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use super::Run;
use super::manifest::{Manifest, ManifestOp};
use super::sstable::SsTableRef;
use crate::StorageError;
use crate::memstore::InternalKey;

/// Centralizes SSTable writing, ID generation, and manifest tracking.
///
/// During a compaction cascade the writer accumulates `ManifestOp`s in a
/// pending buffer. The caller commits them atomically via [`commit`] once
/// the entire cascade completes successfully.
pub struct SsTableWriter {
    next_sst_id: u64,
    manifest: Manifest,
    pending_ops: Vec<ManifestOp>,
    dir: PathBuf,
    max_sst_size: usize,
}

impl SsTableWriter {
    /// Create a new writer, recovering `next_sst_id` from the manifest.
    pub fn new(manifest: Manifest, dir: PathBuf, max_sst_size: usize) -> Self {
        let next_sst_id = manifest
            .tables()
            .keys()
            .copied()
            .max()
            .map(|id| id + 1)
            .unwrap_or(0);
        Self {
            next_sst_id,
            manifest,
            pending_ops: Vec::new(),
            dir,
            max_sst_size,
        }
    }

    /// Write sorted entries as a new run of SSTable files.
    ///
    /// Assigns globally unique IDs, writes files to `{dir}/sst/`, and
    /// records `AddTable` ops in the pending buffer.
    /// Returns the resulting `Run`.
    pub fn write_run(
        &mut self,
        level: u16,
        entries: impl IntoIterator<Item = (InternalKey, Vec<u8>)>,
    ) -> Result<Run, StorageError> {
        let sst_dir = self.dir.join("sst");
        let run_id = self.next_sst_id;
        let start_id = self.next_sst_id;
        let refs = write_sstables(&sst_dir, start_id, self.max_sst_size, entries)?;
        self.next_sst_id = start_id + refs.len() as u64;

        for r in &refs {
            self.pending_ops.push(ManifestOp::AddTable {
                level,
                run_id,
                sst_id: r.id,
                data_size: r.data_size as u32,
                min_key: r.min_key.clone(),
                max_key: r.max_key.clone(),
            });
        }

        Ok(Run::from_refs(refs))
    }

    /// Record `RemoveTable` ops for the given SSTable IDs.
    pub fn remove(&mut self, level: u16, sst_id: u64) {
        self.pending_ops
            .push(ManifestOp::RemoveTable { level, sst_id });
    }

    /// Flush all accumulated ops to the manifest as a single atomic batch.
    pub fn commit(&mut self, max_seq: u64) -> Result<(), StorageError> {
        if !self.pending_ops.is_empty() {
            let ops: Vec<_> = self.pending_ops.drain(..).collect();
            self.manifest.append(&ops, max_seq, &self.dir)?;
        }
        Ok(())
    }

    /// Discard pending ops without writing them.
    pub fn rollback(&mut self) {
        self.pending_ops.clear();
    }

    /// Current next SSTable ID.
    pub fn next_sst_id(&self) -> u64 {
        self.next_sst_id
    }

    /// Active SSTable entries from the manifest.
    pub fn tables(&self) -> &std::collections::HashMap<u64, super::ManifestEntry> {
        self.manifest.tables()
    }

    /// Highest sequence number persisted in the manifest.
    pub fn max_seq(&self) -> u64 {
        self.manifest.max_seq()
    }
}

// --- SSTable file writing ---

/// Encoded byte size of one entry.
///
/// Wire format: `| key_len (2B) | key | seq (8B) | op (1B) | val_len (2B) | value |`
pub(super) fn encoded_entry_size(key: &InternalKey, value: &[u8]) -> usize {
    2 + key.key.len() + 8 + 1 + 2 + value.len()
}

fn write_entry(w: &mut impl Write, key: &InternalKey, value: &[u8]) -> std::io::Result<()> {
    key.encode(w)?;
    w.write_all(&(value.len() as u16).to_be_bytes())?;
    w.write_all(value)?;
    Ok(())
}

/// Footer wire format:
/// `| data_size (4B) | max_key_len (2B) | max_key | min_key_len (2B) | min_key | footer_size (4B) |`
fn write_footer(
    w: &mut impl Write,
    data_size: u32,
    min_key: &[u8],
    max_key: &[u8],
) -> std::io::Result<()> {
    let footer_size: u32 = 4 + 2 + max_key.len() as u32 + 2 + min_key.len() as u32 + 4;
    w.write_all(&data_size.to_be_bytes())?;
    w.write_all(&(max_key.len() as u16).to_be_bytes())?;
    w.write_all(max_key)?;
    w.write_all(&(min_key.len() as u16).to_be_bytes())?;
    w.write_all(min_key)?;
    w.write_all(&footer_size.to_be_bytes())?;
    Ok(())
}

/// Pull entries from `iter` and write them into a single SSTable file at `path`,
/// stopping when the next entry would exceed `max_file_size`.
fn write_sstable_file(
    path: &Path,
    id: u64,
    max_file_size: usize,
    iter: &mut std::iter::Peekable<impl Iterator<Item = (InternalKey, Vec<u8>)>>,
) -> std::io::Result<SsTableRef> {
    let file = File::create(path)?;
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

    Ok(SsTableRef {
        id,
        path: path.to_path_buf(),
        min_key,
        max_key,
        data_size: bytes_written,
    })
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
) -> std::io::Result<Vec<SsTableRef>> {
    fs::create_dir_all(dir)?;

    let mut tables = Vec::new();
    let mut iter = entries.into_iter().peekable();
    let mut id = start_id;

    while iter.peek().is_some() {
        let path = dir.join(format!("{id}.sst"));
        tables.push(write_sstable_file(&path, id, max_file_size, &mut iter)?);
        id += 1;
    }

    Ok(tables)
}
