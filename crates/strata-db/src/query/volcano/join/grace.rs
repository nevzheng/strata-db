//! Grace hash join — equi-joins of any type, in two cleanly separated phases.
//!
//! The algorithm splits in half, and so does this file:
//!
//! 1. **Build phase** ([`build`] + [`choose_build_side`]). We hash-partition both
//!    inputs to disk up front — a pipeline breaker — so equal keys co-locate in
//!    the same bucket and each bucket pair is an independent join over ~1/N of the
//!    data. For each pair we load the *smaller* side into an in-memory hash index
//!    keyed by the join-key bytes (the build side; the smaller it is, the more
//!    often it fits). A bucket whose spilled size — read straight off the file
//!    store stat, [`Workspace::used`] — exceeds the in-memory budget is
//!    redistributed into sub-buckets with a fresh hash seed (so keys actually
//!    move instead of re-colliding) and retried, up to a small cap.
//!
//! 2. **Probe phase** ([`Probe`] + [`probe_one`]). We stream the *other*,
//!    larger bucket one tuple at a time, look each up in the hash index, and emit
//!    the matches. Nothing is materialized — [`GraceHashStream`] is a real lazy
//!    iterator, and the probe side is never held in memory.
//!
//! The two phases never share a type: [`build`] returns a [`HashTable`], and the
//! probe phase consumes it inside a [`Probe`]. The orchestrating iterator runs
//! one bucket pair through build-then-probe before moving to the next.
//!
//! A bucket that even repartitioning can't split is a single key value larger
//! than the budget; at the recursion cap we build it in memory anyway rather than
//! spill the hash table to disk — one big in-memory map is O(n+m) with no random
//! I/O, where a disk-resident table would be all random reads, the worst case.
//!
//! NULL keys can never match, so they bypass partitioning entirely; outer joins
//! still emit them, NULL-padded, as unmatched rows.

// FIXME(grace): this implementation is NOT up to par — do not trust it; it needs
// a rewrite before it's wired into the planner. Review notes (2026-06-14):
//
//  1. The build phase doesn't stream. We eagerly partition BOTH inputs to disk
//     up front, materializing the left *and* the right side. We should instead
//     pick ONE stream as the build side and build its hash map while streaming —
//     not pull both sides into spill files first.
//  2. The hash-table structure is wrong. `HashMap<Vec<u8>, Vec<usize>>` plus the
//     parallel `rows` / `matched` vectors needs to be redesigned.
//  3. Repartitioning is bolted on. `repartition` is a whole second function that
//     re-splits a bucket; it should split a bucket into sub-buckets in place, and
//     ideally share/merge code with the initial partition pass instead of
//     duplicating it.
//
// Bottom line: rework the build/repartition path — see notes 1–3 above.

use std::collections::HashMap;

use strata_store::{FileWorkspace, FileWorkspaceTuples, Workspace};
use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::catalog::schema::Schema;
use crate::query::executor::RowStream;
use crate::query::logical_plan::JoinType;
use crate::query::physical_plan::PlanNode;
use crate::query::{JoinConfig, QueryContext, QueryError};
use crate::storage::types::{Tuple, Value};

// Renamed on import so it can't be confused with the hash-join *build phase*:
// this materializes a volcano input stream (for partitioning), it does not build
// a hash table.
use super::super::build as build_input;
use super::{JoinPlan, concat, equi_keys, output_arity, pad_left, pad_right};

// Bucket sizing — fan-out, partition buffers, and the build-side memory ceiling
// — is per-query and machine-derived; it lives in [`JoinConfig`], carried on the
// `QueryContext`. The two constants below are algorithmic, not resource limits,
// so they stay fixed.

/// Cap on repartitioning retries. Past this, a still-oversized bucket is
/// irreducible skew (one key value) — built in memory anyway. Small on purpose:
/// each pass rewrites the data, and >2 rarely helps once a single key dominates.
const MAX_REPARTITION_LEVELS: usize = 2;
/// Base seed for bucket routing; each recursion level mixes in its depth so
/// re-hashing actually redistributes instead of re-colliding.
const BUCKET_SEED: u64 = 0xdead_beef_cafe_babe;

/// The join-invariant state shared across all buckets.
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
    /// Machine-derived scratch sizing (partition buffers, build-side budget,
    /// fan-out). Carried here so the stream's helpers read one source of truth;
    /// tests shrink it to force spilling/repartitioning.
    config: JoinConfig,
}

impl HashJoinParams {
    /// Codec + key column of the side we build the hash table on.
    fn build_of(&self, build_is_left: bool) -> (&Schema, usize) {
        if build_is_left {
            (&self.left_schema, self.left_key)
        } else {
            (&self.right_schema, self.right_key)
        }
    }
    /// Codec + key column of the side we stream-probe.
    fn probe_of(&self, build_is_left: bool) -> (&Schema, usize) {
        if build_is_left {
            (&self.right_schema, self.right_key)
        } else {
            (&self.left_schema, self.left_key)
        }
    }
}

/// Grace hash join. Partitions eagerly (consuming the inputs), then returns a
/// lazy stream that runs each bucket pair through build-then-probe. See the
/// module docs.
pub(in crate::query::volcano) fn grace_hash_join<'ctx>(
    left: PlanNode,
    right: PlanNode,
    plan: JoinPlan,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    let (left_key, right_key) = equi_keys(&plan.on, &left)?;
    let config = ctx.join_config();
    let params = HashJoinParams {
        join_type: plan.join_type,
        left_key,
        right_key,
        left_width: output_arity(&left),
        right_width: output_arity(&right),
        left_schema: plan.left_schema,
        right_schema: plan.right_schema,
        config,
    };

    // Pipeline breaker: partition both inputs to disk now (consuming the volcano
    // input streams). NULL keys can't match, so they sit out of partitioning;
    // outer joins emit them (padded) once the buckets are drained.
    let mut left_parts = new_buckets(&config)?;
    let mut right_parts = new_buckets(&config)?;
    let mut left_nulls: Vec<Tuple> = Vec::new();
    let mut right_nulls: Vec<Tuple> = Vec::new();
    partition_input(
        build_input(left, ctx)?,
        params.left_key,
        &params.left_schema,
        config.hash_partitions,
        &mut left_parts,
        &mut left_nulls,
    )?;
    partition_input(
        build_input(right, ctx)?,
        params.right_key,
        &params.right_schema,
        config.hash_partitions,
        &mut right_parts,
        &mut right_nulls,
    )?;

    // Pick which input becomes the hash-table (build) side, then pair buckets as
    // (build, probe) so the rest of the stream never re-decides.
    let build_is_left = choose_build_side(&left_parts, &right_parts);
    let worklist: Vec<_> = if build_is_left {
        left_parts.into_iter().zip(right_parts)
    } else {
        right_parts.into_iter().zip(left_parts)
    }
    .map(|(build, probe)| (build, probe, 0usize))
    .collect();

    Ok(RowStream::new(GraceHashStream {
        params,
        build_is_left,
        worklist,
        current: None,
        left_nulls: left_nulls.into_iter(),
        right_nulls: right_nulls.into_iter(),
        failed: false,
    }))
}

/// Lazy stream over the partitioned join. Pulls one `(build, probe)` bucket pair
/// off the worklist at a time, runs the **build phase** to get a [`HashTable`],
/// then enters the **probe phase**, yielding matches one at a time.
struct GraceHashStream {
    params: HashJoinParams,
    /// Whether the build side is the join's *left* input. Fixed once, globally,
    /// so output and NULL-padding always orient as `left ++ right`.
    build_is_left: bool,
    /// Pending `(build, probe, level)` bucket pairs. `level` is how many times
    /// these inputs have already been (re)partitioned — it seeds the next hash
    /// and bounds the recursion.
    worklist: Vec<(FileWorkspace, FileWorkspace, usize)>,
    /// The bucket currently in its probe phase, if any.
    current: Option<Probe>,
    /// NULL-keyed input rows, emitted (padded) by the outer sides at the end.
    left_nulls: std::vec::IntoIter<Tuple>,
    right_nulls: std::vec::IntoIter<Tuple>,
    failed: bool,
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
            // 2. Advance the current bucket's probe phase.
            if let Some(state) = self.current.as_mut() {
                if state.probing {
                    match state.cursor.next() {
                        Some(bytes) => {
                            if let Err(e) =
                                probe_one(&self.params, self.build_is_left, state, &bytes)
                            {
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
                } else if let Some(tuple) =
                    next_unmatched_build(&self.params, self.build_is_left, state)
                {
                    return Some(Ok(tuple));
                } else {
                    self.current = None; // bucket fully drained
                    continue;
                }
            }
            // 3. No current bucket: run the build phase on the next one (which may
            //    repartition), else fall through to the NULL-row tail.
            match self.build_next_bucket() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
            // 4. Buckets exhausted — emit NULL-keyed rows for the outer sides.
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
    /// Run the **build phase** on the next bucket pair: redistribute it while the
    /// build side is over budget and recursion levels remain, otherwise build the
    /// hash table and hand off to the probe phase (sets `self.current`). Returns
    /// `Ok(true)` when a bucket is ready to probe, `Ok(false)` when the worklist
    /// is empty.
    fn build_next_bucket(&mut self) -> Result<bool, QueryError> {
        let build_is_left = self.build_is_left;
        let (build_schema, build_key) = self.params.build_of(build_is_left);
        while let Some((build_ws, probe_ws, level)) = self.worklist.pop() {
            // Fit decision: consult the file store stat (`used()` — page-granular
            // bytes spilled). Over budget and levels left ⇒ redistribute both
            // sides into sub-buckets with a fresh seed and retry one level deeper.
            if build_ws.used() > self.params.config.build_budget && level < MAX_REPARTITION_LEVELS {
                let (probe_schema, probe_key) = self.params.probe_of(build_is_left);
                let next = level + 1;
                let config = &self.params.config;
                let build_sub = repartition(&build_ws, build_key, build_schema, next, config)?;
                let probe_sub = repartition(&probe_ws, probe_key, probe_schema, next, config)?;
                drop(build_ws); // free the parent buckets' temp files now
                drop(probe_ws);
                for (b, p) in build_sub.into_iter().zip(probe_sub) {
                    self.worklist.push((b, p, next));
                }
                continue;
            }

            // BUILD PHASE: materialize the (smaller) bucket into a hash index. At
            // the recursion cap an over-budget bucket is built anyway — it's
            // irreducible single-key skew that no reseed can split.
            let table = build(&build_ws, build_schema, build_key)?;
            drop(build_ws); // table is materialized; the file is no longer needed

            // Hand off to the PROBE PHASE.
            self.current = Some(Probe::new(table, probe_ws.into_tuples()));
            return Ok(true);
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Build phase
// ---------------------------------------------------------------------------

/// Pick which input becomes the build (hash-table) side. We prefer to build the
/// *smaller* side — a smaller table fits in memory more often and is cheaper to
/// hold while probing — estimated here by total bytes spilled per side. A
/// deliberately simple heuristic; this is the seam where a real cardinality
/// estimate plugs in. `true` ⇒ build the LEFT side.
fn choose_build_side(left: &[FileWorkspace], right: &[FileWorkspace]) -> bool {
    let total = |parts: &[FileWorkspace]| parts.iter().map(Workspace::used).sum::<usize>();
    total(left) <= total(right)
}

/// The build phase's product: an in-memory hash index over one bucket's rows.
struct HashTable {
    /// key bytes → indices into `rows`. Full key bytes are compared on probe, so
    /// a hash collision is never mistaken for a match.
    index: HashMap<Vec<u8>, Vec<usize>>,
    rows: Vec<Tuple>,
    /// Per-row matched bitmap, for the outer-join unmatched-build pass.
    matched: Vec<bool>,
}

/// **Build phase.** Load every tuple of `bucket` into a [`HashTable`] keyed by
/// its join-key bytes. Whether the bucket fits was decided by the caller off the
/// file store stat, so this just builds — at the recursion cap it builds an
/// over-budget bucket anyway (irreducible single-key skew).
fn build(bucket: &FileWorkspace, schema: &Schema, key: usize) -> Result<HashTable, QueryError> {
    let mut index: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    let mut rows: Vec<Tuple> = Vec::new();
    let mut matched: Vec<bool> = Vec::new();
    for bytes in bucket.tuples() {
        let tuple = schema.decode(&bytes)?;
        let k = key_bytes(&tuple.values[key])?.expect("null keys never partition");
        index.entry(k).or_default().push(rows.len());
        matched.push(false);
        rows.push(tuple);
    }
    Ok(HashTable {
        index,
        rows,
        matched,
    })
}

// ---------------------------------------------------------------------------
// Probe phase
// ---------------------------------------------------------------------------

/// One bucket's probe phase: the built hash table plus the streaming probe
/// cursor over the opposite bucket.
struct Probe {
    table: HashTable,
    /// The larger side, streamed one tuple at a time (never materialized).
    cursor: FileWorkspaceTuples,
    /// Output rows buffered for the current probe row (one probe row can match
    /// several build rows).
    pending: std::vec::IntoIter<Tuple>,
    /// `true` while consuming the probe side; `false` during the post-probe pass
    /// over unmatched build rows (outer joins).
    probing: bool,
    /// Cursor into `table.matched` for that unmatched-build pass.
    drain: usize,
}

impl Probe {
    fn new(table: HashTable, cursor: FileWorkspaceTuples) -> Self {
        Probe {
            table,
            cursor,
            pending: Vec::new().into_iter(),
            probing: true,
            drain: 0,
        }
    }
}

/// **Probe phase.** Probe one streamed tuple against the build table, buffering
/// its output rows into `state.pending` (matches, or one NULL-padded row if it's
/// an unmatched outer-side row).
fn probe_one(
    params: &HashJoinParams,
    build_is_left: bool,
    state: &mut Probe,
    bytes: &[u8],
) -> Result<(), QueryError> {
    let (probe_schema, probe_key) = params.probe_of(build_is_left);
    let probe = probe_schema.decode(bytes)?;
    let key = key_bytes(&probe.values[probe_key])?.expect("null keys never partition");

    let hits = state.table.index.get(&key).cloned().unwrap_or_default();
    let mut out: Vec<Tuple> = Vec::with_capacity(hits.len());
    for i in hits {
        state.table.matched[i] = true;
        // Output is always left ++ right, whichever side we built on.
        out.push(if build_is_left {
            concat(&state.table.rows[i], &probe)
        } else {
            concat(&probe, &state.table.rows[i])
        });
    }
    if out.is_empty() {
        // Unmatched probe row, kept only by the outer side it belongs to.
        if build_is_left {
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
fn next_unmatched_build(
    params: &HashJoinParams,
    build_is_left: bool,
    state: &mut Probe,
) -> Option<Tuple> {
    let keeps_unmatched = if build_is_left {
        matches!(params.join_type, JoinType::Left | JoinType::Full)
    } else {
        matches!(params.join_type, JoinType::Right | JoinType::Full)
    };
    if !keeps_unmatched {
        return None;
    }
    while state.drain < state.table.matched.len() {
        let i = state.drain;
        state.drain += 1;
        if !state.table.matched[i] {
            return Some(if build_is_left {
                pad_right(&state.table.rows[i], params.right_width)
            } else {
                pad_left(params.left_width, &state.table.rows[i])
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Partitioning
// ---------------------------------------------------------------------------

/// Hash-partition a freshly-decoded input stream into `parts`, setting NULL-keyed
/// rows aside (they can never match). The first partitioning pass, at level 0.
fn partition_input(
    rows: impl Iterator<Item = Result<Tuple, QueryError>>,
    key: usize,
    schema: &Schema,
    hash_partitions: usize,
    parts: &mut [FileWorkspace],
    nulls: &mut Vec<Tuple>,
) -> Result<(), QueryError> {
    for row in rows {
        let tuple = row?;
        match key_bytes(&tuple.values[key])? {
            None => nulls.push(tuple),
            Some(k) => spill(
                &mut parts[bucket(&k, 0, hash_partitions)],
                &schema.encode(&tuple),
            )?,
        }
    }
    Ok(())
}

/// Re-spill a bucket into a fresh set of sub-buckets, hashing the join key at
/// `level`'s seed. The encoded bytes are re-spilled as-is (decode only to read
/// the key) — no re-encode.
fn repartition(
    src: &FileWorkspace,
    key: usize,
    schema: &Schema,
    level: usize,
    config: &JoinConfig,
) -> Result<Vec<FileWorkspace>, QueryError> {
    let mut parts = new_buckets(config)?;
    for bytes in src.tuples() {
        let tuple = schema.decode(&bytes)?;
        let k = key_bytes(&tuple.values[key])?.expect("null keys never partition");
        spill(
            &mut parts[bucket(&k, level, config.hash_partitions)],
            &bytes,
        )?;
    }
    Ok(parts)
}

/// `config.hash_partitions` spilling file workspaces, sized by `config`.
fn new_buckets(config: &JoinConfig) -> Result<Vec<FileWorkspace>, QueryError> {
    (0..config.hash_partitions)
        .map(|_| {
            FileWorkspace::new(config.partition_memory, config.partition_disk)
                .map_err(|e| QueryError::Internal(format!("hash join bucket: {e}")))
        })
        .collect()
}

/// Append an encoded tuple to a bucket, mapping a full workspace to a query
/// error.
fn spill(part: &mut FileWorkspace, bytes: &[u8]) -> Result<(), QueryError> {
    part.append(bytes)
        .map(|_| ())
        .map_err(|e| QueryError::Internal(format!("hash join spill: {e}")))
}

/// The bucket `key` routes to at this recursion `level`. xxh3's per-call seed
/// gives each level an independent hash, so a repartition genuinely redistributes
/// instead of re-colliding into the same bucket.
fn bucket(key: &[u8], level: usize, hash_partitions: usize) -> usize {
    let seed = BUCKET_SEED ^ (level as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (xxh3_64_with_seed(key, seed) as usize) % hash_partitions
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

    /// Small, fixed scratch sizing with a caller-set build budget — the seam the
    /// tests use (in place of machine-derived limits) to force spill/repartition.
    fn test_config(build_budget: usize) -> JoinConfig {
        JoinConfig {
            partition_memory: 64 * 1024,
            partition_disk: 1 << 30,
            build_budget,
            hash_partitions: 16,
        }
    }

    /// A file workspace filled with `(id, k)` rows encoded by [`schema`].
    fn ws(rows: &[(i32, i32)]) -> FileWorkspace {
        let s = schema();
        let mut w = FileWorkspace::new(64 * 1024, 1 << 30).unwrap();
        for &(id, k) in rows {
            let t = Tuple {
                values: vec![Value::Int32(id), Value::Int32(k)],
            };
            w.append(&s.encode(&t)).unwrap();
        }
        w
    }

    /// Drive the lazy stream over a single bucket pair with the given build
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
            config: test_config(budget),
        };
        let left_ws = ws(l);
        let right_ws = ws(r);
        // Mirror `choose_build_side` so the pair is ordered (build, probe).
        let build_is_left = choose_build_side(&[left_ws], &[right_ws]);
        // `ws()` returns by value; rebuild since `choose_build_side` borrowed.
        let left_ws = ws(l);
        let right_ws = ws(r);
        let pair = if build_is_left {
            (left_ws, right_ws, 0)
        } else {
            (right_ws, left_ws, 0)
        };
        let stream = GraceHashStream {
            params,
            build_is_left,
            worklist: vec![pair],
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
    fn recursion_preserves_results() {
        // A sub-page budget forces every non-empty bucket over budget, so the
        // build phase repartitions (twice) before building anyway. The result
        // must equal the single-pass (huge-budget) join, for every join type —
        // which also proves repartitioned sub-buckets stay correctly paired.
        let l: Vec<(i32, i32)> = (0..2000).map(|i| (i, i % 200)).collect();
        let r: Vec<(i32, i32)> = (0..2000).map(|i| (i, i % 200)).collect();
        for jt in [
            JoinType::Inner,
            JoinType::Left,
            JoinType::Right,
            JoinType::Full,
        ] {
            let recursed = run_stream(1, jt, &l, &r);
            let single_pass = run_stream(1 << 30, jt, &l, &r);
            assert_eq!(recursed, single_pass, "{jt:?} recursion mismatch");
        }
    }

    #[test]
    fn fit_after_repartition() {
        // A build bucket that spans several pages but whose keys redistribute:
        // with a one-page budget the first pass is over budget and repartitions,
        // and each sub-bucket then fits and builds. ~3200 keys keeps the output
        // bounded while still overflowing a single bucket's first page.
        let l: Vec<(i32, i32)> = (0..16000).map(|i| (i, i % 3200)).collect();
        let r: Vec<(i32, i32)> = (0..16000).map(|i| (i, i % 3200)).collect();
        let one_page = 8 * 1024;
        let repartitioned = run_stream(one_page, JoinType::Inner, &l, &r);
        let single_pass = run_stream(1 << 30, JoinType::Inner, &l, &r);
        assert_eq!(repartitioned, single_pass);
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
        let skewed = run_stream(1, JoinType::Inner, &l, &r); // sub-page → max recursion
        assert_eq!(skewed.len(), 40 * 40);
        assert_eq!(skewed, run_stream(1 << 30, JoinType::Inner, &l, &r));
    }

    #[test]
    fn skew_preserves_outer_joins() {
        // Skewed keys (7 vs 9) that never match → every left and right row
        // survives, padded, even through the recursion.
        let l: Vec<(i32, i32)> = (0..30).map(|i| (i, 7)).collect();
        let r: Vec<(i32, i32)> = (0..30).map(|i| (i, 9)).collect();
        let full = run_stream(1, JoinType::Full, &l, &r);
        assert_eq!(full.len(), 60);
        assert_eq!(full, run_stream(1 << 30, JoinType::Full, &l, &r));
    }
}
