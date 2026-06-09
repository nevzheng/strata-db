//! The manifest — the LSM's durable record of *tree structure* (which runs
//! live in which level), as opposed to the memtable journal, which records the
//! *data*. Both ride the reusable [`journal`] crate.
//!
//! # Model
//!
//! Structure changes are expressed as [`ManifestOp`] primitives — add a run,
//! remove a run, advance the id allocator. An operation (flush, compaction,
//! drop) bundles its primitives into one [`ManifestEdit`], which is logged as a
//! single framed journal record and so applies **all-or-nothing**. Folding the
//! edit stream over a [`Version`] gives the current structure.
//!
//! ```text
//! flush       → ManifestEdit[ AddRun(L0, …), SetNextSstId(n) ]
//! compaction  → ManifestEdit[ AddRun(L1, …), RemoveRun(a), RemoveRun(b) ]
//! drop a run  → ManifestEdit[ RemoveRun(r) ]
//! ```
//!
//! # Durability & recovery (design)
//!
//! - The manifest is a [`journal`] of [`ManifestEdit`]s. To bound its size it
//!   is **checkpointed**: a fresh manifest file begins with a *snapshot* edit
//!   ([`Version::to_snapshot`] — the whole version as one big `AddRun` batch),
//!   then incrementals; a `CURRENT` pointer names the active file and is flipped
//!   atomically (write-temp + rename).
//! - **Recovery**: read `CURRENT` → replay the manifest into a [`Version`];
//!   replay the memtable journal into the memtable; then **GC** — delete any
//!   `*.sst` not in [`Version::live_files`].
//! - **No in-flight tracking.** An operation's effect is binary: its edit
//!   reached the manifest or it didn't. Incomplete operations leave only
//!   orphaned files, which the GC sweep removes. Ordering makes this safe:
//!   write new files → log the edit (commit) → delete superseded files.
//!
//! (The on-disk codec for [`ManifestEdit`] and the `CURRENT`/checkpoint manager
//! land in later stages; this module is the in-memory model the journal will
//! carry.)

use crate::SsTableId;

/// Identity of a run within the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(pub u64);

/// A run as the manifest records it: the level it lives in, its id, and the
/// SSTable files it is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDescriptor {
    pub level: u32,
    pub run: RunId,
    pub files: Vec<SsTableId>,
}

/// A primitive change to tree structure. [`ManifestEdit`]s are built from these,
/// and a [`Version`] is reconstructed by applying them in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestOp {
    /// A run entered the tree.
    AddRun(RunDescriptor),
    /// A run left the tree.
    RemoveRun(RunId),
    /// Advance the SSTable-id allocator, so ids never collide after a restart.
    SetNextSstId(u64),
}

/// An atomic batch of [`ManifestOp`]s — logged as one framed journal record and
/// applied all-or-nothing. One operation (flush, compaction, drop) = one edit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestEdit {
    pub ops: Vec<ManifestOp>,
}

impl ManifestEdit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_run(mut self, run: RunDescriptor) -> Self {
        self.ops.push(ManifestOp::AddRun(run));
        self
    }

    pub fn remove_run(mut self, run: RunId) -> Self {
        self.ops.push(ManifestOp::RemoveRun(run));
        self
    }

    pub fn set_next_sst_id(mut self, next: u64) -> Self {
        self.ops.push(ManifestOp::SetNextSstId(next));
        self
    }
}

/// The folded state of all applied edits: which runs are live, and the id
/// allocator. Reconstructed on open by replaying the manifest journal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Version {
    runs: Vec<RunDescriptor>, // live runs, in the order they were added
    next_sst_id: u64,
}

impl Version {
    /// Apply one edit's ops in order.
    pub fn apply(&mut self, edit: &ManifestEdit) {
        for op in &edit.ops {
            match op {
                ManifestOp::AddRun(run) => self.runs.push(run.clone()),
                ManifestOp::RemoveRun(id) => self.runs.retain(|r| r.run != *id),
                ManifestOp::SetNextSstId(next) => self.next_sst_id = *next,
            }
        }
    }

    /// A single edit that reconstructs this version from empty — the first
    /// record of a freshly-checkpointed manifest.
    pub fn to_snapshot(&self) -> ManifestEdit {
        let mut edit = ManifestEdit::new();
        for run in &self.runs {
            edit.ops.push(ManifestOp::AddRun(run.clone()));
        }
        edit.ops.push(ManifestOp::SetNextSstId(self.next_sst_id));
        edit
    }

    /// The next SSTable id to allocate.
    pub fn next_sst_id(&self) -> u64 {
        self.next_sst_id
    }

    /// Live runs in `level`, in the order they were added (callers that need
    /// newest-first — e.g. L0 reads — iterate in reverse).
    pub fn runs_in(&self, level: u32) -> impl Iterator<Item = &RunDescriptor> {
        self.runs.iter().filter(move |r| r.level == level)
    }

    /// Every file referenced by a live run — the GC keep-set.
    pub fn live_files(&self) -> impl Iterator<Item = SsTableId> + '_ {
        self.runs.iter().flat_map(|r| r.files.iter().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(level: u32, id: u64, files: &[u64]) -> RunDescriptor {
        RunDescriptor {
            level,
            run: RunId(id),
            files: files.iter().map(|&f| SsTableId(f)).collect(),
        }
    }

    #[test]
    fn flush_edit_adds_an_l0_run() {
        let mut v = Version::default();
        v.apply(
            &ManifestEdit::new()
                .add_run(run(0, 1, &[10]))
                .set_next_sst_id(11),
        );
        assert_eq!(v.runs_in(0).count(), 1);
        assert_eq!(v.next_sst_id(), 11);
        assert_eq!(v.live_files().collect::<Vec<_>>(), vec![SsTableId(10)]);
    }

    #[test]
    fn compaction_edit_swaps_runs_atomically() {
        let mut v = Version::default();
        v.apply(
            &ManifestEdit::new()
                .add_run(run(0, 1, &[10]))
                .add_run(run(0, 2, &[11])),
        );
        // Compact the two L0 runs into one L1 run, in a single edit.
        v.apply(
            &ManifestEdit::new()
                .add_run(run(1, 3, &[12]))
                .remove_run(RunId(1))
                .remove_run(RunId(2)),
        );
        assert_eq!(v.runs_in(0).count(), 0);
        assert_eq!(v.runs_in(1).count(), 1);
        assert_eq!(v.live_files().collect::<Vec<_>>(), vec![SsTableId(12)]);
    }

    #[test]
    fn snapshot_round_trips() {
        let mut v = Version::default();
        v.apply(
            &ManifestEdit::new()
                .add_run(run(0, 1, &[10]))
                .add_run(run(1, 2, &[11])),
        );
        v.apply(&ManifestEdit::new().set_next_sst_id(99));

        // Replaying the snapshot onto an empty version reproduces it.
        let mut restored = Version::default();
        restored.apply(&v.to_snapshot());
        assert_eq!(restored, v);
    }
}
