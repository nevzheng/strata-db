//! Compaction — described as data, not run here.
//!
//! Compaction is the work of merging runs to keep the tree shallow and to drop
//! shadowed and tombstoned keys. This module is the *vocabulary* for that work:
//! [`CompactionJob`] descriptions and the primitive that commits a finished job
//! to the manifest ([`CompactionJob::to_edit`]). It deliberately does **not**
//! read, merge, write, or schedule — choosing which jobs to run, and running
//! them, is the caller's concern, layered on top.
//!
//! A job's lifecycle (driven by the caller):
//! 1. read the input runs (resolved from the current [`Version`](crate::Version)),
//! 2. merge them into new SSTable file(s) — the `output` run,
//! 3. commit `job.to_edit(output)` to the manifest (one atomic add + removes),
//! 4. GC drops the now-unreferenced input files.
//!
//! Because the commit is a single [`ManifestEdit`], an interrupted job leaves
//! only orphaned output files for GC — there's nothing to roll back.

use crate::manifest::{ManifestEdit, RunDescriptor, RunId};

/// What a [`CompactionJob`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionKind {
    /// Seal the memtable into a new L0 run (no input runs).
    Flush,
    /// Merge input runs from one level into the next.
    Merge,
}

/// A unit of compaction work: which runs feed it and where the output lands.
///
/// A description only — see the module docs for how a job is run and committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionJob {
    pub kind: CompactionKind,
    pub source_level: u32,
    pub target_level: u32,
    /// Runs consumed by the job (empty for a [`CompactionKind::Flush`]).
    pub inputs: Vec<RunId>,
}

impl CompactionJob {
    /// Flush the memtable into a new L0 run.
    pub fn flush() -> Self {
        Self {
            kind: CompactionKind::Flush,
            source_level: 0,
            target_level: 0,
            inputs: Vec::new(),
        }
    }

    /// Merge `inputs` from `source_level` into `target_level`.
    pub fn merge(source_level: u32, target_level: u32, inputs: Vec<RunId>) -> Self {
        Self {
            kind: CompactionKind::Merge,
            source_level,
            target_level,
            inputs,
        }
    }

    /// The manifest edit that commits this job once its `output` run has been
    /// written: add the output, remove the inputs. Atomic when logged.
    pub fn to_edit(&self, output: RunDescriptor) -> ManifestEdit {
        let mut edit = ManifestEdit::new().add_run(output);
        for &id in &self.inputs {
            edit = edit.remove_run(id);
        }
        edit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Version;
    use crate::{RunId, SsTableId};

    fn run(level: u32, id: u64, files: &[u64]) -> RunDescriptor {
        RunDescriptor {
            level,
            run: RunId(id),
            files: files.iter().map(|&f| SsTableId(f)).collect(),
        }
    }

    #[test]
    fn flush_job_only_adds_a_run() {
        let job = CompactionJob::flush();
        assert!(job.inputs.is_empty());

        let mut v = Version::default();
        v.apply(&job.to_edit(run(0, 1, &[10])));
        assert_eq!(v.runs_in(0).count(), 1);
    }

    #[test]
    fn merge_job_swaps_inputs_for_output() {
        let mut v = Version::default();
        v.apply(&CompactionJob::flush().to_edit(run(0, 1, &[10])));
        v.apply(&CompactionJob::flush().to_edit(run(0, 2, &[11])));

        // Merge the two L0 runs into one L1 run.
        let job = CompactionJob::merge(0, 1, vec![RunId(1), RunId(2)]);
        v.apply(&job.to_edit(run(1, 3, &[12])));

        assert_eq!(v.runs_in(0).count(), 0);
        assert_eq!(v.live_files().collect::<Vec<_>>(), vec![SsTableId(12)]);
    }
}
