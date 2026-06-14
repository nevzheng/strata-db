//! Grace hash join — equi-joins of any type, built around two cleanly
//! separated phases.
//!
//! # Phase 1 — Partition (pipeline breaker)
//!
//! Both inputs are consumed up front and hash-partitioned into
//! [`hash_partitions`](JoinConfig::hash_partitions) buckets on disk. Equal join
//! keys always route to the same bucket (same hash function, same seed, same
//! fan-out), so each bucket pair is a self-contained join over ~1/N of the data.
//! NULL keys can never match; they bypass partitioning and are held aside for
//! outer-join emission at the end.
//!
//! # Phase 2 — Build + Probe (streaming)
//!
//! For each bucket pair:
//!
//! 1. **Choose sides.** The smaller bucket (by [`Workspace::used`] bytes) becomes
//!    the build side; the larger becomes the probe side.
//!
//! 2. **Build.** Stream the build-side bucket into an in-memory
//!    `HashMap<key_bytes, Vec<row>>`. If the materialized rows exceed
//!    [`build_budget`](JoinConfig::build_budget) before the bucket is fully read,
//!    the build is abandoned and **only that bucket** is repartitioned — the rest
//!    are unaffected.
//!
//! 3. **Repartition (if needed).** Both sides of the oversized bucket are
//!    re-spilled using a **fresh hash seed** (same
//!    [`hash_partitions`](JoinConfig::hash_partitions) fan-out). Changing the seed
//!    — not the bucket count — is what causes keys to actually redistribute rather
//!    than re-collide into the same slot. Retried up to [`MAX_REPARTITION_LEVELS`]
//!    times; after that the bucket is built unconditionally in memory (irreducible
//!    single-key skew — no hash seed can split identical keys, so in-memory
//!    O(n+m) beats random disk I/O).
//!
//! 4. **Probe.** Stream the probe-side bucket one tuple at a time. For each row,
//!    hash its join key into the table and — because hash collisions are possible
//!    — **confirm equality on the raw key bytes** before emitting a match. The
//!    probe side is never held in memory; [`GraceHashStream`] is a genuine lazy
//!    iterator.
//!
//! 5. **Outer join tails.** After the probe pass, any build-side rows that were
//!    never matched are emitted NULL-padded (for the outer side they belong to).
//!    NULL-keyed rows collected in phase 1 are emitted last.
//!
//! # Memory
//!
//! [`build_budget`](JoinConfig::build_budget) is the only knob that matters for
//! fit: it controls how large a hash table we'll build before repartitioning. On
//! a machine with ample RAM this should be set generously — the default derives
//! from 25% of available system memory so a 48 GB host gets a ~3 GB build budget
//! and almost never spills. See [`JoinConfig`], carried on the [`QueryContext`].

// FIXME(grace): MARKED WRONG by the author — do not trust this; rework pending
// review. The intended shape is a plain single-build hash join, not this
// partitioning machinery:
//
//   1. Take only ONE side and build its hash map (in full).
//   2. Stream the LEFT side against that map and emit matching tuples.
//
// Rewrite to that. The current build/probe split, per-bucket side choice, and
// recursive repartitioning are not what's wanted.

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

// Partition fan-out, buffer sizes, and the build-side budget are per-query and
// machine-derived; they live in [`JoinConfig`] on the `QueryContext`. The two
// constants below are algorithmic — not resource limits — so they stay fixed.

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

/// Grace hash join. Partitions both inputs eagerly (phase 1), then returns a lazy
/// stream that runs each bucket pair through build-then-probe (phase 2). See the
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

    // Phase 1 — partition both inputs to disk now (consuming the volcano input
    // streams). NULL keys can't match, so they sit out of partitioning; outer
    // joins emit them (padded) once the buckets are drained.
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

    // Bucket i of the left lines up with bucket i of the right (same hash). The
    // smaller-side choice is per pair, made when the pair is built, not here.
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

/// Lazy stream over the partitioned join (phase 2). Pulls one bucket pair off the
/// worklist at a time, runs the build phase to get a [`HashTable`], then streams
/// the probe phase, yielding matches one at a time.
struct GraceHashStream {
    params: HashJoinParams,
    /// Pending `(left, right, level)` bucket pairs. `level` is how many times
    /// these inputs have already been (re)partitioned — it seeds the next hash
    /// and bounds the recursion.
    worklist: Vec<(FileWorkspace, FileWorkspace, usize)>,
    /// The bucket pair currently being probed, if any.
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
            if let Some(state) = self.current.as_mut() {
                // 1. Drain anything buffered for the current probe row.
                if let Some(tuple) = state.pending.next() {
                    return Some(Ok(tuple));
                }
                // 2a. Probe drained: emit the unmatched-build tail, then drop the
                //     pair. (Checked first so its borrow ends before we reborrow
                //     `state` for the probe step below.)
                if let Some(tail) = state.unmatched.as_mut() {
                    match tail.next() {
                        Some(tuple) => return Some(Ok(tuple)),
                        None => {
                            self.current = None; // bucket pair fully drained
                            continue;
                        }
                    }
                }
                // 2b. Still probing the larger side, one tuple at a time.
                match state.cursor.next() {
                    Some(bytes) => {
                        if let Err(e) = probe_one(&self.params, state, &bytes) {
                            self.failed = true;
                            return Some(Err(e));
                        }
                        continue; // loop back to drain `pending`
                    }
                    None => {
                        // Probe drained → switch to the unmatched-build tail.
                        state.unmatched = Some(unmatched_build_tail(&self.params, state));
                        continue;
                    }
                }
            }
            // 3. No current pair: build the next one (which may repartition first),
            //    else fall through to the NULL-row tail.
            match self.build_next_pair() {
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
    /// Build the next bucket pair: choose the smaller side, build its table
    /// (measuring as we go), and on overflow repartition *only this pair* and
    /// retry one level deeper. Sets `self.current` and returns `Ok(true)` when a
    /// pair is ready to probe, `Ok(false)` when the worklist is empty.
    fn build_next_pair(&mut self) -> Result<bool, QueryError> {
        while let Some((left_ws, right_ws, level)) = self.worklist.pop() {
            // Phase 2.1 — choose sides: build the smaller bucket, probe the larger.
            let build_is_left = left_ws.used() <= right_ws.used();
            let (build_ws, probe_ws) = if build_is_left {
                (left_ws, right_ws)
            } else {
                (right_ws, left_ws)
            };
            let (build_schema, build_key) = self.params.build_of(build_is_left);

            // Phase 2.2 — build, bailing the moment the rows outgrow the budget.
            // Out of repartition levels ⇒ build unconditionally (cap = None): the
            // bucket is irreducible single-key skew no reseed can split.
            let cap = (level < MAX_REPARTITION_LEVELS).then_some(self.params.config.build_budget);
            match build_table(&build_ws, build_schema, build_key, cap)? {
                Some(table) => {
                    // The table is now COMPLETE — `build_table` read the whole
                    // build bucket. Only here do we set up the probe cursor, so no
                    // probe row is ever looked up against a half-built table (which
                    // could miss a build row still in the unread tail).
                    drop(build_ws); // table materialized; the spill file is done
                    self.current = Some(Probe::new(table, build_is_left, probe_ws.into_tuples()));
                    return Ok(true);
                }
                None => {
                    // Phase 2.3 — over budget: repartition both sides of THIS pair
                    // with a fresh seed so its keys redistribute. Other buckets are
                    // untouched.
                    let (probe_schema, probe_key) = self.params.probe_of(build_is_left);
                    let next = level + 1;
                    let config = &self.params.config;
                    let build_sub = repartition(&build_ws, build_key, build_schema, next, config)?;
                    let probe_sub = repartition(&probe_ws, probe_key, probe_schema, next, config)?;
                    drop(build_ws); // free the parent buckets' temp files now
                    drop(probe_ws);
                    for (b, p) in build_sub.into_iter().zip(probe_sub) {
                        // Restore left/right roles so the worklist pair stays (l, r).
                        let pair = if build_is_left { (b, p) } else { (p, b) };
                        self.worklist.push((pair.0, pair.1, next));
                    }
                }
            }
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Build phase
// ---------------------------------------------------------------------------

/// One build-side row in the hash table, with a flag for the outer-join pass.
struct BuildEntry {
    row: Tuple,
    matched: bool,
}

/// The build phase's product: join-key bytes → the build rows carrying that key.
/// Rows live in the map (not in a side vector the map indexes into), so the table
/// is self-contained — and the map keying on the full key bytes *is* the raw-byte
/// equality the probe needs, so a hash collision never produces a false match.
type HashTable = HashMap<Vec<u8>, Vec<BuildEntry>>;

/// **Build phase.** Stream the *entire* `bucket` into a [`HashTable`] keyed by
/// join-key bytes. A returned `Some(table)` is complete — every build row is
/// indexed — which is the invariant probing depends on (a probe key must be able
/// to find any matching build row, so the table can't be probed until it's whole).
///
/// With `cap = Some(budget)` the build is abandoned (returns `None`) the moment
/// the bytes read exceed `budget`, so the caller can repartition instead of
/// holding an oversized table; `cap = None` builds unconditionally.
fn build_table(
    bucket: &FileWorkspace,
    schema: &Schema,
    key: usize,
    cap: Option<usize>,
) -> Result<Option<HashTable>, QueryError> {
    let mut table: HashTable = HashMap::new();
    let mut used = 0usize;
    for bytes in bucket.tuples() {
        // Measure the real build as we go; bail the moment it won't fit.
        used += bytes.len();
        if let Some(budget) = cap
            && used > budget
        {
            return Ok(None);
        }
        let tuple = schema.decode(&bytes)?;
        let k = key_bytes(&tuple.values[key])?.expect("null keys never partition");
        table.entry(k).or_default().push(BuildEntry {
            row: tuple,
            matched: false,
        });
    }
    Ok(Some(table))
}

// ---------------------------------------------------------------------------
// Probe phase
// ---------------------------------------------------------------------------

/// One bucket pair's probe state: the build-side table plus the streaming cursor
/// over the probe-side bucket.
struct Probe {
    table: HashTable,
    /// Whether the build side is the join's *left* input — chosen per bucket pair,
    /// so output and NULL-padding orient as `left ++ right` either way.
    build_is_left: bool,
    /// The larger side, streamed one tuple at a time (never materialized).
    cursor: FileWorkspaceTuples,
    /// Output rows buffered for the current probe row (one probe row can match
    /// several build rows).
    pending: std::vec::IntoIter<Tuple>,
    /// `None` while probing; `Some(tail)` once the probe side is drained and we
    /// emit the unmatched build rows (outer joins). Building the tail consumes
    /// `table`, which is how the probe phase ends.
    unmatched: Option<std::vec::IntoIter<Tuple>>,
}

impl Probe {
    fn new(table: HashTable, build_is_left: bool, cursor: FileWorkspaceTuples) -> Self {
        Probe {
            table,
            build_is_left,
            cursor,
            pending: Vec::new().into_iter(),
            unmatched: None,
        }
    }
}

/// **Probe phase.** Probe one streamed tuple against the build table, buffering
/// its output rows into `state.pending` (matches, or one NULL-padded row if it's
/// an unmatched outer-side row).
fn probe_one(params: &HashJoinParams, state: &mut Probe, bytes: &[u8]) -> Result<(), QueryError> {
    let build_is_left = state.build_is_left;
    let (probe_schema, probe_key) = params.probe_of(build_is_left);
    let probe = probe_schema.decode(bytes)?;
    let key = key_bytes(&probe.values[probe_key])?.expect("null keys never partition");

    let mut out: Vec<Tuple> = Vec::new();
    if let Some(entries) = state.table.get_mut(&key) {
        // The map keys on raw bytes, so reaching here is a confirmed key match,
        // not just a hash collision.
        out.reserve(entries.len());
        for entry in entries.iter_mut() {
            entry.matched = true;
            // Output is always left ++ right, whichever side we built on.
            out.push(if build_is_left {
                concat(&entry.row, &probe)
            } else {
                concat(&probe, &entry.row)
            });
        }
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

/// The outer-join tail: the build rows nothing probed, NULL-padded for the side
/// they belong to. Consumes `state.table` (the probe phase is over), so it runs
/// once when the probe side drains. Empty for inner joins, or when the build side
/// isn't an outer side.
fn unmatched_build_tail(params: &HashJoinParams, state: &mut Probe) -> std::vec::IntoIter<Tuple> {
    let build_is_left = state.build_is_left;
    let keeps_unmatched = if build_is_left {
        matches!(params.join_type, JoinType::Left | JoinType::Full)
    } else {
        matches!(params.join_type, JoinType::Right | JoinType::Full)
    };
    if !keeps_unmatched {
        return Vec::new().into_iter();
    }
    let table = std::mem::take(&mut state.table);
    table
        .into_values()
        .flatten()
        .filter(|entry| !entry.matched)
        .map(|entry| {
            if build_is_left {
                pad_right(&entry.row, params.right_width)
            } else {
                pad_left(params.left_width, &entry.row)
            }
        })
        .collect::<Vec<_>>()
        .into_iter()
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

/// Re-spill one over-budget bucket into a fresh set of sub-buckets, hashing the
/// join key at `level`'s seed. The encoded bytes are re-spilled as-is (decode
/// only to read the key) — no re-encode.
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
        // Many rows over 200 keys: a small budget forces a repartition pass, then
        // each sub-bucket builds in memory. The result must equal the single-pass
        // (huge-budget) join, for every join type — which also proves the
        // repartitioned sub-buckets stay correctly paired.
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
