//! Grace hash join — equi-joins of any type, built to keep the hash table in
//! memory no matter how big the inputs are.
//!
//! The naive hash join builds a table from one whole input; if it doesn't fit,
//! it spills the *table* to disk and every probe becomes a random disk read —
//! the worst possible I/O pattern. Grace avoids that: it hash-partitions both
//! inputs to disk first (sequential writes), so equal keys co-locate and each
//! partition pair is an independent join over ~1/N of the data. Then it builds
//! the table from one partition at a time.
//!
//! If a partition's build side *still* doesn't fit (skew, or just a lot of
//! data), we **recursively repartition** it with a fresh hash seed so the keys
//! actually redistribute — re-hashing with the same seed would just re-collide.
//! The fit decision reads [`Workspace::used`] (the partition's spilled
//! footprint); we never grow an unbounded in-memory table. When even
//! repartitioning can't split a partition (one key value larger than the
//! budget), we fall back to a streaming nested loop — sequential reads, no
//! random I/O.
//!
//! NULL keys can never match, so they bypass partitioning entirely; outer joins
//! still emit them, NULL-padded, as unmatched rows.

use std::collections::HashMap;

use strata_store::{FileWorkspace, Workspace};
use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::catalog::schema::Schema;
use crate::query::executor::RowStream;
use crate::query::expression::Expr;
use crate::query::logical_plan::JoinType;
use crate::query::physical_plan::PlanNode;
use crate::query::{QueryContext, QueryError};
use crate::storage::types::{Tuple, Value};

use super::super::build;
use super::{concat, equi_keys, output_arity, pad_left, pad_right};

/// Fan-out per partitioning pass. Equal keys hash to the same partition, so the
/// join decomposes into this many independent, smaller joins.
const HASH_PARTITIONS: usize = 16;
/// In-RAM working set per partition file; full pages spill to disk and the RAM
/// is reused, so this is the per-partition write buffer, not its capacity.
const PARTITION_MEMORY: usize = 64 * 1024;
/// Per-partition on-disk ceiling — the spill backstop.
const PARTITION_DISK: usize = 1 << 30;
/// A partition whose spilled footprint is at or under this builds its hash table
/// in memory. Measured against [`Workspace::used`] (encoded bytes); the decoded
/// table costs a few× more, so this is a deliberately conservative proxy.
const BUILD_MEMORY_BUDGET: usize = 64 * 1024 * 1024;
/// Cap on recursive repartitioning passes. Past this, a still-oversized
/// partition is irreducible skew (one key value) → nested-loop fallback.
const MAX_REPARTITION_LEVELS: usize = 4;
/// Base seed for partition routing (per the slab-bucket convention); each
/// recursion level mixes in its depth so re-hashing actually redistributes.
const BUCKET_SEED: u64 = 0xdead_beef_cafe_babe;

/// Everything the recursive join needs that doesn't change between partitions.
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

/// Grace hash join. See the module docs for the partition / repartition / fall
/// back strategy.
#[allow(clippy::too_many_arguments)]
pub(in crate::query::volcano) fn grace_hash_join<'ctx>(
    left: PlanNode,
    right: PlanNode,
    on: Option<Expr>,
    join_type: JoinType,
    left_schema: Schema,
    right_schema: Schema,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    let (left_key, right_key) = equi_keys(&on, &left)?;
    let params = HashJoinParams {
        join_type,
        left_key,
        right_key,
        left_width: output_arity(&left),
        right_width: output_arity(&right),
        left_schema,
        right_schema,
        build_budget: BUILD_MEMORY_BUDGET,
    };

    // Pass 0 — partition both live inputs by the join key. NULL keys can't
    // match, so they sit out; outer joins emit them (padded) at the very end.
    let mut left_parts = new_partitions()?;
    let mut right_parts = new_partitions()?;
    let mut left_nulls: Vec<Tuple> = Vec::new();
    let mut right_nulls: Vec<Tuple> = Vec::new();

    for row in build(left, ctx)? {
        let tuple = row?;
        match key_bytes(&tuple.values[params.left_key])? {
            None => left_nulls.push(tuple),
            Some(k) => spill(
                &mut left_parts[bucket(&k, 0)],
                &params.left_schema.encode(&tuple),
            )?,
        }
    }
    for row in build(right, ctx)? {
        let tuple = row?;
        match key_bytes(&tuple.values[params.right_key])? {
            None => right_nulls.push(tuple),
            Some(k) => spill(
                &mut right_parts[bucket(&k, 0)],
                &params.right_schema.encode(&tuple),
            )?,
        }
    }

    // Join each partition pair — recursing when the build side won't fit.
    let mut out: Vec<Tuple> = Vec::new();
    for p in 0..HASH_PARTITIONS {
        join_partition(&left_parts[p], &right_parts[p], 0, &params, &mut out)?;
    }

    // NULL-keyed rows: unmatched by definition, kept only by the outer sides.
    if matches!(params.join_type, JoinType::Left | JoinType::Full) {
        for l in &left_nulls {
            out.push(pad_right(l, params.right_width));
        }
    }
    if matches!(params.join_type, JoinType::Right | JoinType::Full) {
        for r in &right_nulls {
            out.push(pad_left(params.left_width, r));
        }
    }

    Ok(RowStream::new(out.into_iter().map(Ok)))
}

/// Join one partition pair, appending matches to `out`. `level` is how many
/// times these inputs have already been (re)partitioned — it seeds the next
/// hash and bounds the recursion.
fn join_partition(
    left: &FileWorkspace,
    right: &FileWorkspace,
    level: usize,
    params: &HashJoinParams,
    out: &mut Vec<Tuple>,
) -> Result<(), QueryError> {
    // Fits → build in memory and probe. (An empty build side uses 0 bytes, so
    // this also covers the empty-partition case.)
    if right.used() <= params.build_budget {
        return build_probe(left, right, params, out);
    }
    // Doesn't fit, but we can still split: repartition both sides with a fresh
    // seed so the keys land in different buckets this time, then recurse.
    if level < MAX_REPARTITION_LEVELS {
        let next = level + 1;
        let left_sub = repartition(left, params.left_key, &params.left_schema, next)?;
        let right_sub = repartition(right, params.right_key, &params.right_schema, next)?;
        for p in 0..HASH_PARTITIONS {
            join_partition(&left_sub[p], &right_sub[p], next, params, out)?;
        }
        return Ok(());
    }
    // Irreducible skew (one key value beyond the budget) — repartitioning can't
    // help. Stream a nested loop: sequential reads, never a random hash spill.
    nested_loop_fallback(left, right, params, out)
}

/// Build a hash table from the right partition, probe it with the left.
fn build_probe(
    left: &FileWorkspace,
    right: &FileWorkspace,
    params: &HashJoinParams,
    out: &mut Vec<Tuple>,
) -> Result<(), QueryError> {
    // key bytes → indices into `build_rows`. The HashMap compares full key
    // bytes, so a hash collision is never mistaken for a match.
    let mut build_rows: Vec<Tuple> = Vec::new();
    let mut build_matched: Vec<bool> = Vec::new();
    let mut table: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for bytes in right.tuples() {
        let tuple = params.right_schema.decode(&bytes)?;
        let key = key_bytes(&tuple.values[params.right_key])?.expect("null keys never partition");
        table.entry(key).or_default().push(build_rows.len());
        build_matched.push(false);
        build_rows.push(tuple);
    }

    for bytes in left.tuples() {
        let l = params.left_schema.decode(&bytes)?;
        let key = key_bytes(&l.values[params.left_key])?.expect("null keys never partition");
        let mut matched = false;
        if let Some(idxs) = table.get(&key) {
            for &idx in idxs {
                matched = true;
                build_matched[idx] = true;
                out.push(concat(&l, &build_rows[idx]));
            }
        }
        // LEFT/FULL: an unmatched probe row survives, padded with right NULLs.
        if !matched && matches!(params.join_type, JoinType::Left | JoinType::Full) {
            out.push(pad_right(&l, params.right_width));
        }
    }

    // RIGHT/FULL: build rows that nothing probed, padded with left NULLs.
    if matches!(params.join_type, JoinType::Right | JoinType::Full) {
        for (idx, matched) in build_matched.iter().enumerate() {
            if !matched {
                out.push(pad_left(params.left_width, &build_rows[idx]));
            }
        }
    }
    Ok(())
}

/// Streaming nested loop over two spilled partitions — the skew fallback. Reads
/// are sequential (rescans the right file per left row); only a per-right-row
/// matched bitmap is held in memory, never the tuples.
fn nested_loop_fallback(
    left: &FileWorkspace,
    right: &FileWorkspace,
    params: &HashJoinParams,
    out: &mut Vec<Tuple>,
) -> Result<(), QueryError> {
    let right_count = right.tuples().count();
    let mut right_matched = vec![false; right_count];

    for lbytes in left.tuples() {
        let l = params.left_schema.decode(&lbytes)?;
        let lkey = key_bytes(&l.values[params.left_key])?.expect("null keys never partition");
        let mut matched = false;
        for (j, rbytes) in right.tuples().enumerate() {
            let r = params.right_schema.decode(&rbytes)?;
            let rkey = key_bytes(&r.values[params.right_key])?.expect("null keys never partition");
            if lkey == rkey {
                matched = true;
                right_matched[j] = true;
                out.push(concat(&l, &r));
            }
        }
        if !matched && matches!(params.join_type, JoinType::Left | JoinType::Full) {
            out.push(pad_right(&l, params.right_width));
        }
    }

    if matches!(params.join_type, JoinType::Right | JoinType::Full) {
        for (j, rbytes) in right.tuples().enumerate() {
            if !right_matched[j] {
                out.push(pad_left(
                    params.left_width,
                    &params.right_schema.decode(&rbytes)?,
                ));
            }
        }
    }
    Ok(())
}

/// Re-spill a partition into a fresh set of sub-partitions, hashing the join key
/// at `level`'s seed. The encoded tuple bytes are re-spilled as-is (decode only
/// to read the key) — no re-encode.
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

    fn params(join_type: JoinType, build_budget: usize) -> HashJoinParams {
        HashJoinParams {
            join_type,
            left_key: 1,
            right_key: 1,
            left_width: 2,
            right_width: 2,
            left_schema: schema(),
            right_schema: schema(),
            build_budget,
        }
    }

    /// Run a single partition-pair join with the given build budget, sorted for
    /// order-independent comparison.
    fn join_with(budget: usize, jt: JoinType, l: &[(i32, i32)], r: &[(i32, i32)]) -> Vec<Tuple> {
        let (lw, rw) = (ws(l), ws(r));
        let p = params(jt, budget);
        let mut out = Vec::new();
        join_partition(&lw, &rw, 0, &p, &mut out).unwrap();
        out.sort_by_key(|t| format!("{:?}", t.values));
        out
    }

    #[test]
    fn repartitioning_preserves_results() {
        // Many rows over 200 keys: a small budget forces a repartition pass,
        // then each sub-partition builds in memory. Result must equal the
        // single-pass (huge-budget) join.
        let l: Vec<(i32, i32)> = (0..2000).map(|i| (i, i % 200)).collect();
        let r: Vec<(i32, i32)> = (0..2000).map(|i| (i, i % 200)).collect();
        for jt in [
            JoinType::Inner,
            JoinType::Left,
            JoinType::Right,
            JoinType::Full,
        ] {
            let repartitioned = join_with(8 * 1024, jt, &l, &r);
            let single_pass = join_with(1 << 30, jt, &l, &r);
            assert_eq!(repartitioned, single_pass, "{jt:?} repartition mismatch");
        }
    }

    #[test]
    fn irreducible_skew_falls_back_to_nested_loop() {
        // One key value, more rows than any repartition can split — no hash
        // seed separates identical keys, so this must reach the nested-loop
        // fallback and still produce the full cartesian product.
        let l: Vec<(i32, i32)> = (0..40).map(|i| (i, 7)).collect();
        let r: Vec<(i32, i32)> = (0..40).map(|i| (i, 7)).collect();
        let skewed = join_with(64, JoinType::Inner, &l, &r); // < 1 page → max recursion
        assert_eq!(skewed.len(), 40 * 40, "single-key cartesian product");
        assert_eq!(skewed, join_with(1 << 30, JoinType::Inner, &l, &r));
    }

    #[test]
    fn fallback_handles_outer_joins() {
        // Skewed keys (7 vs 9) that never match → fallback must still keep the
        // unmatched rows for the outer sides.
        let l: Vec<(i32, i32)> = (0..30).map(|i| (i, 7)).collect();
        let r: Vec<(i32, i32)> = (0..30).map(|i| (i, 9)).collect();
        let full = join_with(64, JoinType::Full, &l, &r);
        // No matches: every left and every right row survives, padded.
        assert_eq!(full.len(), 60);
        assert_eq!(full, join_with(1 << 30, JoinType::Full, &l, &r));
    }
}
