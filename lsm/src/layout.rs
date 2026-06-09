//! On-disk layout under a configurable storage root.
//!
//! ```text
//! <root>/
//!   sstables/<id>.sst   data files
//!   manifests/          manifest versions + CURRENT  (used once the manifest lands)
//!   memtable.jrnl       the memtable journal
//! ```
//!
//! Everything the tree writes is derived from the root, so the storage
//! location is chosen entirely by the caller of [`Lsm::with_memtable`](crate::Lsm).

use std::path::PathBuf;

/// Resolves the paths under a storage root.
pub(crate) struct Layout {
    root: PathBuf,
}

impl Layout {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Directory holding SSTable data files (`<root>/sstables`).
    pub(crate) fn sstables(&self) -> PathBuf {
        self.root.join("sstables")
    }

    /// Directory holding manifest versions and the `CURRENT` pointer.
    // Reserved for the manifest manager (next stage).
    #[allow(dead_code)]
    pub(crate) fn manifests(&self) -> PathBuf {
        self.root.join("manifests")
    }

    /// Path of the memtable journal file.
    pub(crate) fn memtable_journal(&self) -> PathBuf {
        self.root.join("memtable.jrnl")
    }
}
