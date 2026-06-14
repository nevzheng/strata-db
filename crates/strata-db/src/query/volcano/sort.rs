//! Sort operator — external merge sort, a pipeline breaker. Phase 1 generates
//! sorted runs from memory-sized batches of the input, each spilled to a
//! workspace. Phase 2 k-way merges the runs through a min-heap (the merge tree).
//! Sort-merge join requires its inputs ordered by such a node.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use strata_store::{FileWorkspace, Workspace};

use crate::catalog::schema::Schema;
use crate::query::executor::RowStream;
use crate::query::logical_plan::SortKey;
use crate::query::physical_plan::PlanNode;
use crate::query::{QueryContext, QueryError};
use crate::storage::types::{Tuple, Value};

use super::build;

/// Tuples sorted in RAM per run before it's flushed (the run-generation batch).
const SORT_RUN_TUPLES: usize = 4096;
/// In-RAM working set per run. A run that fits stays entirely in memory; only
/// the overflow touches disk, so this is the common-case footprint, not a hard
/// cap. (The page cache allocates these frames up front, so it's a real
/// upfront cost — generous, but not unbounded.)
const SORT_RUN_MEMORY: usize = 8 * 1024 * 1024;
/// Per-run on-disk ceiling — the disk backstop when a run exceeds memory.
const SORT_RUN_DISK: usize = 1 << 30;

/// External merge sort. The whole input is consumed before the first row is
/// produced. Each run is a [`FileWorkspace`] — sorted in RAM, then spilled to
/// disk — so a sort larger than memory completes by streaming runs back through
/// the merge.
pub(super) fn sort<'ctx>(
    input: PlanNode,
    keys: Vec<SortKey>,
    input_schema: Schema,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    // Phase 1 — run generation: sort memory-sized batches, spill each as a run.
    let mut runs: Vec<FileWorkspace> = Vec::new();
    let mut batch: Vec<(Vec<Vec<u8>>, Tuple)> = Vec::with_capacity(SORT_RUN_TUPLES);
    for row in build(input, ctx)? {
        let tuple = row?;
        let key = sort_key_bytes(&tuple, &keys)?;
        batch.push((key, tuple));
        if batch.len() >= SORT_RUN_TUPLES {
            runs.push(flush_run(&mut batch, &input_schema)?);
        }
    }
    if !batch.is_empty() {
        runs.push(flush_run(&mut batch, &input_schema)?);
    }

    // Phase 2 — k-way merge of the sorted runs via a min-heap.
    let mut cursors: Vec<_> = runs.iter().map(|run| run.tuples()).collect();
    let mut heap: BinaryHeap<Reverse<MergeItem>> = BinaryHeap::new();
    for (run_idx, cursor) in cursors.iter_mut().enumerate() {
        if let Some(item) = next_item(cursor, run_idx, &keys, &input_schema)? {
            heap.push(Reverse(item));
        }
    }
    let mut out: Vec<Tuple> = Vec::new();
    while let Some(Reverse(item)) = heap.pop() {
        let run_idx = item.run_idx;
        out.push(item.tuple);
        if let Some(next) = next_item(&mut cursors[run_idx], run_idx, &keys, &input_schema)? {
            heap.push(Reverse(next));
        }
    }

    Ok(RowStream::new(out.into_iter().map(Ok)))
}

/// Sort a batch by key and spill it to a fresh on-disk run, clearing it.
fn flush_run(
    batch: &mut Vec<(Vec<Vec<u8>>, Tuple)>,
    schema: &Schema,
) -> Result<FileWorkspace, QueryError> {
    batch.sort_by(|a, b| a.0.cmp(&b.0));
    let mut run = FileWorkspace::new(SORT_RUN_MEMORY, SORT_RUN_DISK)
        .map_err(|e| QueryError::Internal(format!("sort spill: {e}")))?;
    for (_, tuple) in batch.drain(..) {
        run.append(&schema.encode(&tuple))
            .map_err(|e| QueryError::Internal(format!("sort run: {e}")))?;
    }
    Ok(run)
}

/// Pull the next tuple from a run cursor as a heap item (decoded + keyed).
fn next_item(
    cursor: &mut impl Iterator<Item = Vec<u8>>,
    run_idx: usize,
    keys: &[SortKey],
    schema: &Schema,
) -> Result<Option<MergeItem>, QueryError> {
    match cursor.next() {
        None => Ok(None),
        Some(bytes) => {
            let tuple = schema.decode(&bytes)?;
            let key = sort_key_bytes(&tuple, keys)?;
            Ok(Some(MergeItem {
                key,
                run_idx,
                tuple,
            }))
        }
    }
}

/// A run's current head in the merge heap, ordered by its normalized key bytes.
struct MergeItem {
    key: Vec<Vec<u8>>,
    run_idx: usize,
    tuple: Tuple,
}

impl PartialEq for MergeItem {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.run_idx == other.run_idx
    }
}
impl Eq for MergeItem {}
impl Ord for MergeItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then(self.run_idx.cmp(&other.run_idx))
    }
}
impl PartialOrd for MergeItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Normalize a tuple's sort keys to per-key, self-contained comparable bytes: a
/// NULL-placement prefix, then the value's order-preserving encoding (bit-
/// inverted for DESC). Plain `Ord` over the resulting `Vec<Vec<u8>>` then yields
/// the full ASC/DESC + NULLS FIRST/LAST ordering, key by key (separate elements,
/// so variable-length keys never run together).
fn sort_key_bytes(tuple: &Tuple, keys: &[SortKey]) -> Result<Vec<Vec<u8>>, QueryError> {
    let mut parts = Vec::with_capacity(keys.len());
    for key in keys {
        let value = key.expr.eval(tuple)?;
        let mut part = Vec::new();
        if matches!(value, Value::Null) {
            // NULL placement is independent of ASC/DESC.
            part.push(if key.nulls_first { 0 } else { 1 });
        } else {
            part.push(if key.nulls_first { 1 } else { 0 });
            let mut bytes = Vec::new();
            value.encode_key(&mut bytes).map_err(|e| {
                QueryError::type_error(format!("cannot ORDER BY this value: {e:?}"))
            })?;
            if !key.ascending {
                for b in &mut bytes {
                    *b = !*b;
                }
            }
            part.extend_from_slice(&bytes);
        }
        parts.push(part);
    }
    Ok(parts)
}
