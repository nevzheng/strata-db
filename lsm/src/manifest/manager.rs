//! The manifest manager: owns the on-disk manifest and the folded [`Version`].
//!
//! It captures the tree's structure and is the gatekeeper for changing it:
//! reads *query* [`version`](ManifestManager::version); flush and compaction
//! *mutate* it via [`commit`](ManifestManager::commit); it is compacted by
//! [`checkpoint`](ManifestManager::checkpoint) and used to
//! [`garbage_collect`](ManifestManager::garbage_collect) dead SSTables. Writes
//! (puts) never touch it.
//!
//! Layout under the manifests dir: `MANIFEST-NNNNNN` files (each a [`journal`]
//! of [`ManifestEdit`]s, the active one starting with a snapshot) plus a
//! `CURRENT` text file naming the active number. `CURRENT` is flipped by an
//! atomic write-temp + rename — that rename is the commit point of a checkpoint.
//!
//! Staged: built and tested here; wired into the tree's open/flush path next.
#![allow(dead_code)]

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use journal::Journal;

use super::codec::ManifestEditCodec;
use super::{ManifestEdit, Version};
use crate::{LsmError, SsTableId};

type ManifestJournal = Journal<ManifestEditCodec>;

pub(crate) struct ManifestManager {
    dir: PathBuf,
    current: u64,
    journal: ManifestJournal,
    version: Version,
}

impl ManifestManager {
    /// Open the manifest under `dir`, replaying the active `MANIFEST-*` into a
    /// [`Version`]. Creates a fresh, empty manifest if none exists.
    pub fn open(dir: &Path) -> Result<Self, LsmError> {
        fs::create_dir_all(dir)?;
        let current_pointer = dir.join("CURRENT");

        if current_pointer.exists() {
            let current = read_current(&current_pointer)?;
            let journal = Journal::with_codec(manifest_path(dir, current), ManifestEditCodec)?;
            let mut version = Version::default();
            for edit in journal.replay()? {
                version.apply(&edit?);
            }
            Ok(Self {
                dir: dir.to_path_buf(),
                current,
                journal,
                version,
            })
        } else {
            let current = 0;
            let journal = Journal::with_codec(manifest_path(dir, current), ManifestEditCodec)?;
            write_current(dir, current)?;
            Ok(Self {
                dir: dir.to_path_buf(),
                current,
                journal,
                version: Version::default(),
            })
        }
    }

    /// The current tree structure — what reads consult.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Durably record a structural change, then apply it in memory. The journal
    /// append (one atomic framed record) is the commit point.
    pub fn commit(&mut self, edit: ManifestEdit) -> Result<(), LsmError> {
        self.journal.append(&edit)?;
        self.version.apply(&edit);
        Ok(())
    }

    /// Compact the manifest: write a fresh `MANIFEST` that begins with a
    /// snapshot of the current version, atomically flip `CURRENT` to it, then
    /// drop the old file. Bounds manifest size and replay time on open.
    pub fn checkpoint(&mut self) -> Result<(), LsmError> {
        let next = self.current + 1;
        let mut fresh = Journal::with_codec(manifest_path(&self.dir, next), ManifestEditCodec)?;
        // Snapshot first; fsync'd by append.
        fresh.append(&self.version.to_snapshot())?;
        // The rename inside write_current is the atomic commit of the switch.
        write_current(&self.dir, next)?;

        let old = self.current;
        self.current = next;
        self.journal = fresh;
        // Best-effort: a leftover old manifest is just an orphan.
        let _ = fs::remove_file(manifest_path(&self.dir, old));
        Ok(())
    }

    /// Delete SSTables in `sstables_dir` that the current version doesn't
    /// reference. Only files below the `next_sst_id` watermark are eligible, so
    /// output an in-flight operation hasn't committed yet is never removed.
    /// Returns the ids deleted.
    pub fn garbage_collect(&self, sstables_dir: &Path) -> Result<Vec<SsTableId>, LsmError> {
        let live: HashSet<SsTableId> = self.version.live_files().collect();
        let watermark = self.version.next_sst_id();
        let mut removed = Vec::new();
        if !sstables_dir.exists() {
            return Ok(removed);
        }
        for entry in fs::read_dir(sstables_dir)? {
            let path = entry?.path();
            if let Some(id) = parse_sst_id(&path)
                && id.0 < watermark
                && !live.contains(&id)
            {
                fs::remove_file(&path)?;
                removed.push(id);
            }
        }
        Ok(removed)
    }
}

fn manifest_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("MANIFEST-{n:06}"))
}

fn read_current(path: &Path) -> Result<u64, LsmError> {
    fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(|_| LsmError::Internal("malformed CURRENT pointer".into()))
}

/// Atomically point `CURRENT` at manifest `n` (write temp, fsync, rename).
fn write_current(dir: &Path, n: u64) -> Result<(), LsmError> {
    let tmp = dir.join("CURRENT.tmp");
    let mut file = File::create(&tmp)?;
    write!(file, "{n}")?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join("CURRENT"))?;
    Ok(())
}

fn parse_sst_id(path: &Path) -> Option<SsTableId> {
    if path.extension()? != "sst" {
        return None;
    }
    path.file_stem()?.to_str()?.parse().ok().map(SsTableId)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunDescriptor;
    use crate::manifest::RunId;

    fn run(level: u32, id: u64, files: &[u64]) -> RunDescriptor {
        RunDescriptor {
            level,
            run: RunId(id),
            files: files.iter().map(|&f| SsTableId(f)).collect(),
        }
    }

    #[test]
    fn commits_survive_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut m = ManifestManager::open(tmp.path()).unwrap();
            m.commit(
                ManifestEdit::new()
                    .add_run(run(0, 1, &[10]))
                    .set_next_sst_id(11),
            )
            .unwrap();
            m.commit(
                ManifestEdit::new()
                    .add_run(run(0, 2, &[11]))
                    .set_next_sst_id(12),
            )
            .unwrap();
        }
        let m = ManifestManager::open(tmp.path()).unwrap();
        assert_eq!(m.version().runs_in(0).count(), 2);
        assert_eq!(m.version().next_sst_id(), 12);
    }

    #[test]
    fn checkpoint_compacts_and_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = ManifestManager::open(tmp.path()).unwrap();
        m.commit(
            ManifestEdit::new()
                .add_run(run(0, 1, &[10]))
                .set_next_sst_id(11),
        )
        .unwrap();
        m.commit(
            ManifestEdit::new()
                .add_run(run(1, 2, &[11]))
                .set_next_sst_id(12),
        )
        .unwrap();
        m.checkpoint().unwrap();
        // The pre-checkpoint manifest is gone; CURRENT names the new one.
        assert!(!manifest_path(tmp.path(), 0).exists());

        let reopened = ManifestManager::open(tmp.path()).unwrap();
        assert_eq!(reopened.version(), m.version());
    }

    #[test]
    fn gc_drops_dead_keeps_live_and_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let sstables = tmp.path().join("sstables");
        fs::create_dir_all(&sstables).unwrap();
        for id in [3u64, 5, 9, 12] {
            fs::write(sstables.join(format!("{id}.sst")), b"x").unwrap();
        }

        let mut m = ManifestManager::open(&tmp.path().join("manifests")).unwrap();
        // Live = file 5; watermark (next id) = 10.
        m.commit(
            ManifestEdit::new()
                .add_run(run(0, 1, &[5]))
                .set_next_sst_id(10),
        )
        .unwrap();

        let mut removed = m.garbage_collect(&sstables).unwrap();
        removed.sort();
        assert_eq!(removed, vec![SsTableId(3), SsTableId(9)]); // dead, below watermark
        assert!(sstables.join("5.sst").exists()); // live
        assert!(sstables.join("12.sst").exists()); // in-flight (>= watermark)
    }
}
