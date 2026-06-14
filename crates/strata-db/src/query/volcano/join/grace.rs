//! Grace hash join — equi-joins of any type, built to keep the hash table in
//! memory at any scale and to *stream* its output.
//!
//! Partitioning is a pipeline breaker: both inputs are consumed up front and
//! hash-partitioned to disk (sequential writes), so equal keys co-locate and
//! each partition pair is an independent join over ~1/N of the data. Everything
//! after that streams — [`GraceHashStream`] is a real lazy iterator, not a
//! materialized `Vec` wrapped in a stream.
//!
//! For each partition pair we **build the hash table from the smaller side**
//! (the build side must fit in memory; the smaller it is, the more often it
//! does) and **probe by streaming the larger side one tuple at a time** — the
//! probe side is never held in memory.
//!
//! We don't predict the fit — we **build the table and measure it**, bailing
//! the moment the materialized rows outgrow the budget. An oversized partition
//! is then **repartitioned** with a fresh hash seed (so keys actually
//! redistribute instead of re-colliding) and retried, up to a small cap. A
//! partition that even repartitioning can't split is a single key value larger
//! than the budget; on the last attempt we build it in memory anyway rather than
//! spill the table to disk — one big in-memory map is O(n+m) with no random I/O,
//! where a disk-resident hash table would be random reads, the worst case.
//!
//! NULL keys can never match, so they bypass partitioning entirely; outer joins
//! still emit them, NULL-padded, as unmatched rows.

use std::collections::HashMap;

use strata_store::{FileWorkspace, FileWorkspaceTuples, Workspace};
use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::catalog::schema::Schema;
use crate::query::executor::RowStream;
use crate::query::logical_plan::JoinType;
use crate::query::physical_plan::PlanNode;
use crate::query::{QueryContext, QueryError};
use crate::storage::types::{Tuple, Value};

// Renamed on import so it can't be confused with the hash-join *build* phase:
// this materializes a volcano input stream (for partitioning), it does not build
// a hash table.
use super::super::build as build_input;
use super::{JoinPlan, concat, equi_keys, output_arity, pad_left, pad_right};

/// Fan-out per partitioning pass. Equal keys hash to the same partition, so the
/// join decomposes into this many independent, smaller joins.
const HASH_PARTITIONS: usize = 16;
/// In-RAM working set per partition file; full pages spill to disk and the RAM
/// is reused, so this is the per-partition write buffer, not its capacity.
const PARTITION_MEMORY: usize = 64 * 1024;
/// Per-partition on-disk ceiling — the spill backstop.
const PARTITION_DISK: usize = 1 << 30;
/// Build-side memory ceiling: a hash table whose materialized rows exceed this
/// many bytes is abandoned and its partition repartitioned. Counts encoded
/// bytes; the live table costs a few× more, so this is conservative.
const BUILD_MEMORY_BUDGET: usize = 64 * 1024 * 1024;
/// Cap on repartitioning retries. Past this, a still-oversized partition is
/// irreducible skew (one key value) — built in memory anyway. Small on purpose:
/// each pass rewrites the data, and >2 rarely helps once a single key dominates.
const MAX_REPARTITION_LEVELS: usize = 2;
/// Base seed for partition routing (per the slab-bucket convention); each
/// recursion level mixes in its depth so re-hashing actually redistributes.
const BUCKET_SEED: u64 = 0xdead_beef_cafe_babe;

/// The join-invariant state shared across all partitions.
struct HashJoinParams {
    join_type: JoinType,
    /// Join-key column within the left / right tuple.
    left_key: usize,
    right_key: usize,
    /// Output widths, for NULL-padding the absent outer side.
    left_width: usize,
    right_width: usize,
    /// The schema-driven codec for each side's spilled tuples.
    left_schema: Schema,
    right_schema: Schema,
    /// Build-side fit threshold (a field so tests can shrink it).
    build_budget: usize,
}

/// Grace hash join. Partitions eagerly (consuming the inputs), then returns a
/// lazy stream over the partitions. See the module docs.
pub(in crate::query::volcano) fn grace_hash_join<'ctx>(
    left: PlanNode,
    right: PlanNode,
    plan: JoinPlan,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    let (left_key, right_key) = equi_keys(&plan.on, &left)?;
    let params = HashJoinParams {
        join_type: plan.join_type,
        left_key,
        right_key,
        left_width: output_arity(&left),
        right_width: output_arity(&right),
        left_schema: plan.left_schema,
        right_schema: plan.right_schema,
        build_budget: BUILD_MEMORY_BUDGET,
    };

    // Pipeline breaker: partition both inputs to disk now (consuming the volcano
    // input streams). NULL keys can't match, so they sit out of partitioning;
    // outer joins emit them (padded) once the partitions are drained.
    let mut left_parts = new_partitions()?;
    let mut right_parts = new_partitions()?;
    let mut left_nulls: Vec<Tuple> = Vec::new();
    let mut right_nulls: Vec<Tuple> = Vec::new();

    for row in build_input(left, ctx)? {
        let tuple = row?;
        match key_bytes(&tuple.values[params.left_key])? {
            None => left_nulls.push(tuple),
            Some(k) => spill(
                &mut left_parts[bucket(&k, 0)],
                &params.left_schema.encode(&tuple),
            )?,
        }
    }
    for row in build_input(right, ctx)? {
        let tuple = row?;
        match key_bytes(&tuple.values[params.right_key])? {
            None => right_nulls.push(tuple),
            Some(k) => spill(
                &mut right_parts[bucket(&k, 0)],
                &params.right_schema.encode(&tuple),
            )?,
        }
    }

    let worklist = left_parts
        .into_iter()
        .zip(right_parts)
        .map(|(l, r)| (l, r, 0usize))
        .collect();

    Ok(RowStream::new(GraceHashStream {
        params,
        worklist,
        current: None,
        left_nulls: left_nulls.into_iter(),
        right_nulls: right_nulls.into_iter(),
        failed: false,
    }))
}

/// Lazy stream over the partitioned join. Pulls one partition pair off the
/// worklist at a time (repartitioning oversized ones), builds the smaller side's
/// table, then yields probe matches one at a time.
struct GraceHashStream {
    params: HashJoinParams,
    /// Pending `(left, right, level)` partition pairs. `level` is how many times
    /// these inputs have already been (re)partitioned — it seeds the next hash
    /// and bounds the recursion.
    worklist: Vec<(FileWorkspace, FileWorkspace, usize)>,
    /// The partition currently being probed, if any.
    current: Option<Probe>,
    /// NULL-keyed input rows, emitted (padded) by the outer sides at the end.
    left_nulls: std::vec::IntoIter<Tuple>,
    right_nulls: std::vec::IntoIter<Tuple>,
    failed: bool,
}

/// One partition's build-side hash table plus the streaming probe cursor.
struct Probe {
    /// key bytes → indices into `build_rows`. Full key bytes are compared, so a
    /// hash collision is never mistaken for a match.
    table: HashMap<Vec<u8>, Vec<usize>>,
    build_rows: Vec<Tuple>,
    build_matched: Vec<bool>,
    /// Whether the build side is the join's *left* input (so output and padding
    /// go the right direction regardless of which side we chose to build on).
    build_is_left: bool,
    /// The larger side, streamed one tuple at a time.
    probe: FileWorkspaceTuples,
    /// Output rows buffered for the current probe row (one probe row can match
    /// several build rows).
    pending: std::vec::IntoIter<Tuple>,
    /// `true` while consuming the probe side; `false` during the post-probe pass
    /// over unmatched build rows (outer joins).
    probing: bool,
    /// Cursor into `build_matched` for that unmatched-build pass.
    drain: usize,
}

impl Iterator for GraceHashStream {
    type Item = Result<Tuple, QueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            // 1. Anything buffered for the current probe row.
            if let Some(state) = self.current.as_mut()
                && let Some(tuple) = state.pending.next()
            {
                return Some(Ok(tuple));
            }
            // 2. Advance the current partition.
            if let Some(state) = self.current.as_mut() {
                if state.probing {
                    match state.probe.next() {
                        Some(bytes) => {
                            if let Err(e) = probe_one(&self.params, state, &bytes) {
                                self.failed = true;
                                return Some(Err(e));
                            }
                            continue; // loop back to drain `pending`
                        }
                        None => {
                            state.probing = false; // probe done → unmatched-build pass
                            continue;
                        }
                    }
                } else if let Some(tuple) = next_unmatched_build(&self.params, state) {
                    return Some(Ok(tuple));
                } else {
                    self.current = None; // partition fully drained
                    continue;
                }
            }
            // 3. No current partition: load the next (may repartition), else
            //    fall through to the NULL-row tail.
            match self.load_next_partition() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
            // 4. Partitions exhausted — emit NULL-keyed rows for the outer sides.
            if matches!(self.params.join_type, JoinType::Left | JoinType::Full)
                && let Some(t) = self.left_nulls.next()
            {
                return Some(Ok(pad_right(&t, self.params.right_width)));
            }
            if matches!(self.params.join_type, JoinType::Right | JoinType::Full)
                && let Some(t) = self.right_nulls.next()
            {
                return Some(Ok(pad_left(self.params.left_width, &t)));
            }
            return None;
        }
    }
}

impl GraceHashStream {
    /// Pop the next partition pair, repartitioning while the smaller side is over
    /// budget and we still have recursion levels left. Sets `self.current` and
    /// returns `Ok(true)` when a partition is ready, `Ok(false)` when the
    /// worklist is empty.
    fn load_next_partition(&mut self) -> Result<bool, QueryError> {
        while let Some((left_ws, right_ws, level)) = self.worklist.pop() {
            // Build the table from the smaller side (cheap `used()` heuristic);
            // stream-probe the larger.
            let build_is_left = left_ws.used() <= right_ws.used();
            let (build_ws, probe_ws, build_schema, build_key) = if build_is_left {
                (
                    left_ws,
                    right_ws,
                    &self.params.left_schema,
                    self.params.left_key,
                )
            } else {
                (
                    right_ws,
                    left_ws,
                    &self.params.right_schema,
                    self.params.right_key,
                )
            };

            // Try to build it in memory, bailing if it outgrows the budget. Out
            // of retries, build unconditionally (irreducible single-key skew).
            let cap = (level < MAX_REPARTITION_LEVELS).then_some(self.params.build_budget);
            let built = build_table(&build_ws, build_schema, build_key, cap)?;

            let Some((table, build_rows, build_matched)) = built else {
                // Didn't fit — repartition both sides with a fresh seed (so keys
                // redistribute) and retry one level deeper.
                let next = level + 1;
                let (probe_schema, probe_key) = if build_is_left {
                    (&self.params.right_schema, self.params.right_key)
                } else {
                    (&self.params.left_schema, self.params.left_key)
                };
                let build_sub = repartition(&build_ws, build_key, build_schema, next)?;
                let probe_sub = repartition(&probe_ws, probe_key, probe_schema, next)?;
                drop(build_ws); // free the parent partitions' temp files now
                drop(probe_ws);
                for (b, p) in build_sub.into_iter().zip(probe_sub) {
                    // Restore left/right roles so output stays left ++ right.
                    let pair = if build_is_left { (b, p) } else { (p, b) };
                    self.worklist.push((pair.0, pair.1, next));
                }
                continue;
            };
            drop(build_ws); // table is materialized; the file is no longer needed

            self.current = Some(Probe {
                table,
                build_rows,
                build_matched,
                build_is_left,
                probe: probe_ws.into_tuples(),
                pending: Vec::new().into_iter(),
                probing: true,
                drain: 0,
            });
            return Ok(true);
        }
        Ok(false)
    }
}

/// Build a hash table from one partition side — the obvious build phase: decode
/// each tuple, index it by its join-key bytes. Returns the table, the rows it
/// points into, and a per-row matched bitmap (for outer joins).
///
/// With `cap = Some(budget)`, gives up (returns `None`) once the materialized
/// rows exceed `budget` bytes, so the caller can repartition instead of holding
/// an oversized table; `cap = None` builds unconditionally.
type BuiltTable = (HashMap<Vec<u8>, Vec<usize>>, Vec<Tuple>, Vec<bool>);
fn build_table(
    ws: &FileWorkspace,
    schema: &Schema,
    key: usize,
    cap: Option<usize>,
) -> Result<Option<BuiltTable>, QueryError> {
    let mut table: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    let mut rows: Vec<Tuple> = Vec::new();
    let mut matched: Vec<bool> = Vec::new();
    let mut used = 0usize;
    for bytes in ws.tuples() {
        // Measure the real build as we go; bail the moment it won't fit.
        used += bytes.len();
        if let Some(budget) = cap
            && used > budget
        {
            return Ok(None);
        }
        let tuple = schema.decode(&bytes)?;
        let k = key_bytes(&tuple.values[key])?.expect("null keys never partition");
        table.entry(k).or_default().push(rows.len());
        matched.push(false);
        rows.push(tuple);
    }
    Ok(Some((table, rows, matched)))
}

/// Probe one streamed tuple against the build table, buffering its output rows
/// into `state.pending` (matches, or one NULL-padded row if it's an unmatched
/// outer-side row).
fn probe_one(params: &HashJoinParams, state: &mut Probe, bytes: &[u8]) -> Result<(), QueryError> {
    // The probe side is whichever input we didn't build on.
    let (probe_schema, probe_key) = if state.build_is_left {
        (&params.right_schema, params.right_key)
    } else {
        (&params.left_schema, params.left_key)
    };
    let probe = probe_schema.decode(bytes)?;
    let key = key_bytes(&probe.values[probe_key])?.expect("null keys never partition");

    let hits = state.table.get(&key).cloned().unwrap_or_default();
    let mut out: Vec<Tuple> = Vec::with_capacity(hits.len());
    for i in hits {
        state.build_matched[i] = true;
        // Output is always left ++ right, whichever side we built on.
        out.push(if state.build_is_left {
            concat(&state.build_rows[i], &probe)
        } else {
            concat(&probe, &state.build_rows[i])
        });
    }
    if out.is_empty() {
        // Unmatched probe row, kept only by the outer side it belongs to.
        if state.build_is_left {
            // probe side is the RIGHT input
            if matches!(params.join_type, JoinType::Right | JoinType::Full) {
                out.push(pad_left(params.left_width, &probe));
            }
        } else if matches!(params.join_type, JoinType::Left | JoinType::Full) {
            // probe side is the LEFT input
            out.push(pad_right(&probe, params.right_width));
        }
    }
    state.pending = out.into_iter();
    Ok(())
}

/// The next build row that nothing probed, padded for the outer side it belongs
/// to — the post-probe pass. Advances `state.drain`; `None` when exhausted.
fn next_unmatched_build(params: &HashJoinParams, state: &mut Probe) -> Option<Tuple> {
    let keeps_unmatched = if state.build_is_left {
        matches!(params.join_type, JoinType::Left | JoinType::Full)
    } else {
        matches!(params.join_type, JoinType::Right | JoinType::Full)
    };
    if !keeps_unmatched {
        return None;
    }
    while state.drain < state.build_matched.len() {
        let i = state.drain;
        state.drain += 1;
        if !state.build_matched[i] {
            return Some(if state.build_is_left {
                pad_right(&state.build_rows[i], params.right_width)
            } else {
                pad_left(params.left_width, &state.build_rows[i])
            });
        }
    }
    None
}

/// Re-spill a partition into a fresh set of sub-partitions, hashing the join key
/// at `level`'s seed. The encoded bytes are re-spilled as-is (decode only to
/// read the key) — no re-encode.
fn repartition(
    src: &FileWorkspace,
    key: usize,
    schema: &Schema,
    level: usize,
) -> Result<Vec<FileWorkspace>, QueryError> {
    let mut parts = new_partitions()?;
    for bytes in src.tuples() {
        let tuple = schema.decode(&bytes)?;
        let k = key_bytes(&tuple.values[key])?.expect("null keys never partition");
        spill(&mut parts[bucket(&k, level)], &bytes)?;
    }
    Ok(parts)
}

/// `HASH_PARTITIONS` spilling file workspaces.
fn new_partitions() -> Result<Vec<FileWorkspace>, QueryError> {
    (0..HASH_PARTITIONS)
        .map(|_| {
            FileWorkspace::new(PARTITION_MEMORY, PARTITION_DISK)
                .map_err(|e| QueryError::Internal(format!("hash join partition: {e}")))
        })
        .collect()
}

/// Append an encoded tuple to a partition, mapping a full workspace to a query
/// error.
fn spill(part: &mut FileWorkspace, bytes: &[u8]) -> Result<(), QueryError> {
    part.append(bytes)
        .map(|_| ())
        .map_err(|e| QueryError::Internal(format!("hash join spill: {e}")))
}

/// The partition `key` routes to at this recursion `level`. xxh3's per-call seed
/// gives each level an independent hash, so a repartition genuinely
/// redistributes instead of re-colliding into the same bucket.
fn bucket(key: &[u8], level: usize) -> usize {
    let seed = BUCKET_SEED ^ (level as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (xxh3_64_with_seed(key, seed) as usize) % HASH_PARTITIONS
}

/// A join key's canonical bytes (order-preserving encoding), or `None` for NULL.
/// Equal values encode equal, so byte equality *is* join equality.
fn key_bytes(value: &Value) -> Result<Option<Vec<u8>>, QueryError> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let mut buf = Vec::new();
    value
        .encode_key(&mut buf)
        .map_err(|e| QueryError::type_error(format!("cannot hash this join key: {e:?}")))?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::types::{Field, LogicalType};

    fn schema() -> Schema {
        Schema {
            fields: vec![
                Field::new("id", LogicalType::Int32),
                Field::new("k", LogicalType::Int32),
            ],
        }
    }

    /// A file workspace filled with `(id, k)` rows encoded by [`schema`].
    fn ws(rows: &[(i32, i32)]) -> FileWorkspace {
        let s = schema();
        let mut w = FileWorkspace::new(PARTITION_MEMORY, PARTITION_DISK).unwrap();
        for &(id, k) in rows {
            let t = Tuple {
                values: vec![Value::Int32(id), Value::Int32(k)],
            };
            w.append(&s.encode(&t)).unwrap();
        }
        w
    }

    /// Drive the lazy stream over a single partition pair with the given build
    /// budget; sorted for order-independent comparison.
    fn run_stream(budget: usize, jt: JoinType, l: &[(i32, i32)], r: &[(i32, i32)]) -> Vec<Tuple> {
        let params = HashJoinParams {
            join_type: jt,
            left_key: 1,
            right_key: 1,
            left_width: 2,
            right_width: 2,
            left_schema: schema(),
            right_schema: schema(),
            build_budget: budget,
        };
        let stream = GraceHashStream {
            params,
            worklist: vec![(ws(l), ws(r), 0)],
            current: None,
            left_nulls: Vec::new().into_iter(),
            right_nulls: Vec::new().into_iter(),
            failed: false,
        };
        let mut out: Vec<Tuple> = stream.map(|r| r.unwrap()).collect();
        out.sort_by_key(|t| format!("{:?}", t.values));
        out
    }

    #[test]
    fn repartitioning_preserves_results() {
        // Many rows over 200 keys: a small budget forces a repartition pass,
        // then each sub-partition builds in memory. Result must equal the
        // single-pass (huge-budget) join, for every join type.
        let l: Vec<(i32, i32)> = (0..2000).map(|i| (i, i % 200)).collect();
        let r: Vec<(i32, i32)> = (0..2000).map(|i| (i, i % 200)).collect();
        for jt in [
            JoinType::Inner,
            JoinType::Left,
            JoinType::Right,
            JoinType::Full,
        ] {
            let repartitioned = run_stream(8 * 1024, jt, &l, &r);
            let single_pass = run_stream(1 << 30, jt, &l, &r);
            assert_eq!(repartitioned, single_pass, "{jt:?} repartition mismatch");
        }
    }

    #[test]
    fn builds_on_the_smaller_side() {
        // Tiny left, huge right: the stream must build on the left and probe the
        // right, and still produce the correct inner join (every left key 5
        // matches every right key-5 row).
        let l = [(1, 5), (2, 5)];
        let r: Vec<(i32, i32)> = (0..500).map(|i| (i, if i < 100 { 5 } else { 9 })).collect();
        let rows = run_stream(1 << 30, JoinType::Inner, &l, &r);
        assert_eq!(rows.len(), 2 * 100); // 2 left × 100 matching right
    }

    #[test]
    fn irreducible_single_key_still_joins() {
        // One key value, more rows than any repartition can split — no hash seed
        // separates identical keys, so this exhausts the recursion and builds in
        // memory anyway, producing the full cartesian product.
        let l: Vec<(i32, i32)> = (0..40).map(|i| (i, 7)).collect();
        let r: Vec<(i32, i32)> = (0..40).map(|i| (i, 7)).collect();
        let skewed = run_stream(64, JoinType::Inner, &l, &r); // < 1 page → max recursion
        assert_eq!(skewed.len(), 40 * 40);
        assert_eq!(skewed, run_stream(1 << 30, JoinType::Inner, &l, &r));
    }

    #[test]
    fn skew_preserves_outer_joins() {
        // Skewed keys (7 vs 9) that never match → every left and right row
        // survives, padded, even through the recursion.
        let l: Vec<(i32, i32)> = (0..30).map(|i| (i, 7)).collect();
        let r: Vec<(i32, i32)> = (0..30).map(|i| (i, 9)).collect();
        let full = run_stream(64, JoinType::Full, &l, &r);
        assert_eq!(full.len(), 60);
        assert_eq!(full, run_stream(1 << 30, JoinType::Full, &l, &r));
    }
}
