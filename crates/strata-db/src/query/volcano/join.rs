//! Join operators — the nested-loop family. All produce `left ++ right` output
//! rows and handle every join type (INNER/LEFT/RIGHT/FULL; cross = `on: None`),
//! NULL-padding the absent side of an outer join. Hash and sort-merge are
//! separate strategies.

use std::cmp::Ordering;

use strata_store::{MemoryWorkspace, Workspace};

use crate::catalog::schema::Schema;
use crate::query::executor::RowStream;
use crate::query::expression::{BinaryOperator, Expr, cmp_values};
use crate::query::logical_plan::JoinType;
use crate::query::physical_plan::PlanNode;
use crate::query::{QueryContext, QueryError};
use crate::storage::types::{Tuple, Value};

use super::build;

/// Outer rows buffered per block (one inner rescan per block).
const OUTER_BLOCK_TUPLES: usize = 1024;
/// Ceiling for the in-RAM inner side. Spilling to a file workspace is future
/// work; until then a larger inner fails with a clear resource error.
const INNER_WORKSPACE_BUDGET: usize = 256 * 1024 * 1024;

/// Tuple-at-a-time nested-loop join — the general algorithm, correct for any
/// predicate (and cross joins with `on: None`).
///
/// Materializes both inputs up front (the inner must be rescanned; outer/full
/// joins need a second pass over unmatched right rows). The slow, always-right
/// baseline; the block variant below subsumes it for the optimizer.
pub(super) fn nested_loop_join<'ctx>(
    left: PlanNode,
    right: PlanNode,
    on: Option<Expr>,
    join_type: JoinType,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    // Arities (for NULL-padding) read structurally, so an empty side still
    // pads to the right width.
    let left_width = output_arity(&left);
    let right_width = output_arity(&right);
    let left_rows: Vec<Tuple> = build(left, ctx)?.collect::<Result<_, _>>()?;
    let right_rows: Vec<Tuple> = build(right, ctx)?.collect::<Result<_, _>>()?;

    let mut out: Vec<Tuple> = Vec::new();
    // Tracks which right rows matched, for RIGHT/FULL's unmatched pass.
    let mut right_matched = vec![false; right_rows.len()];

    for l in &left_rows {
        let mut l_matched = false;
        for (j, r) in right_rows.iter().enumerate() {
            let combined = concat(l, r);
            let matched = match &on {
                None => true,
                Some(pred) => matches!(pred.eval(&combined)?, Value::Bool(true)),
            };
            if matched {
                l_matched = true;
                right_matched[j] = true;
                out.push(combined);
            }
        }
        // LEFT/FULL: an unmatched left row survives, padded with right NULLs.
        if !l_matched && matches!(join_type, JoinType::Left | JoinType::Full) {
            out.push(pad_right(l, right_width));
        }
    }

    // RIGHT/FULL: unmatched right rows survive, padded with left NULLs.
    if matches!(join_type, JoinType::Right | JoinType::Full) {
        for (j, r) in right_rows.iter().enumerate() {
            if !right_matched[j] {
                out.push(pad_left(left_width, r));
            }
        }
    }

    Ok(RowStream::new(out.into_iter().map(Ok)))
}

/// Block nested-loop join. Buffers the outer in fixed-size blocks and scans the
/// inner *once per block* (instead of once per outer tuple), amortizing the
/// rescans. The inner is materialized into a [`MemoryWorkspace`] — the seam
/// where a spilling backing (file workspace / grace) plugs in later.
///
/// Output order differs from tuple-nested-loop (inner-major within a block),
/// which is fine: SQL join output is unordered.
pub(super) fn block_nested_loop_join<'ctx>(
    left: PlanNode,
    right: PlanNode,
    on: Option<Expr>,
    join_type: JoinType,
    right_schema: Schema,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    let left_width = output_arity(&left);
    let right_width = output_arity(&right);

    // Materialize the inner (right) into a rescannable workspace, encoded with
    // its schema (the schema-driven codec — same format as stored rows).
    let mut inner = MemoryWorkspace::new(INNER_WORKSPACE_BUDGET);
    let mut inner_count = 0usize;
    for row in build(right, ctx)? {
        inner
            .append(&right_schema.encode(&row?))
            .map_err(|e| QueryError::Internal(format!("join workspace: {e}")))?;
        inner_count += 1;
    }
    // Tracks inner rows that matched anything, for RIGHT/FULL's unmatched pass.
    let mut inner_matched = vec![false; inner_count];

    let mut out: Vec<Tuple> = Vec::new();
    let mut outer = build(left, ctx)?;
    let mut block: Vec<Tuple> = Vec::with_capacity(OUTER_BLOCK_TUPLES);

    loop {
        // Fill one block of outer rows.
        block.clear();
        for row in outer.by_ref() {
            block.push(row?);
            if block.len() == OUTER_BLOCK_TUPLES {
                break;
            }
        }
        if block.is_empty() {
            break;
        }

        let mut block_matched = vec![false; block.len()];
        // One inner scan serves the whole block.
        for (k, bytes) in inner.tuples().enumerate() {
            let inner_tuple = right_schema.decode(&bytes)?;
            for (i, l) in block.iter().enumerate() {
                let combined = concat(l, &inner_tuple);
                let matched = match &on {
                    None => true,
                    Some(pred) => matches!(pred.eval(&combined)?, Value::Bool(true)),
                };
                if matched {
                    block_matched[i] = true;
                    inner_matched[k] = true;
                    out.push(combined);
                }
            }
        }
        // LEFT/FULL: unmatched outer rows in this block, padded with right NULLs.
        if matches!(join_type, JoinType::Left | JoinType::Full) {
            for (i, l) in block.iter().enumerate() {
                if !block_matched[i] {
                    out.push(pad_right(l, right_width));
                }
            }
        }
    }

    // RIGHT/FULL: inner rows that never matched, padded with left NULLs.
    if matches!(join_type, JoinType::Right | JoinType::Full) {
        for (k, bytes) in inner.tuples().enumerate() {
            if !inner_matched[k] {
                out.push(pad_left(left_width, &right_schema.decode(&bytes)?));
            }
        }
    }

    Ok(RowStream::new(out.into_iter().map(Ok)))
}

/// Sort-merge join (inner equi-joins only). Both inputs arrive sorted on their
/// join key — the optimizer inserts `Sort` enforcers — so one merge pass emits
/// matches, expanding equal-key groups to their cartesian product. NULL keys
/// never match (SQL semantics) and are skipped. Non-equi / outer joins use
/// other strategies, so this handles just the inner equi case.
pub(super) fn sort_merge_join<'ctx>(
    left: PlanNode,
    right: PlanNode,
    on: Option<Expr>,
    join_type: JoinType,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    debug_assert!(
        matches!(join_type, JoinType::Inner),
        "sort-merge join is inner-only"
    );
    let (left_key, right_key) = equi_keys(&on, &left)?;

    // The inputs are sorted (by the Sort enforcers), so a linear merge suffices.
    let left_rows: Vec<Tuple> = build(left, ctx)?.collect::<Result<_, _>>()?;
    let right_rows: Vec<Tuple> = build(right, ctx)?.collect::<Result<_, _>>()?;
    debug_assert!(is_sorted_by(&left_rows, left_key));
    debug_assert!(is_sorted_by(&right_rows, right_key));

    let mut out: Vec<Tuple> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < left_rows.len() && j < right_rows.len() {
        let lk = &left_rows[i].values[left_key];
        let rk = &right_rows[j].values[right_key];
        // NULL keys never match. Inputs sort nulls last, so once a side turns
        // null the merge just drains the other side and ends.
        if matches!(lk, Value::Null) {
            i += 1;
            continue;
        }
        if matches!(rk, Value::Null) {
            j += 1;
            continue;
        }
        match cmp_values(lk, rk)? {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                // Equal-key groups on both sides → cartesian product.
                let l_start = i;
                while i < left_rows.len()
                    && cmp_values(&left_rows[i].values[left_key], lk)? == Ordering::Equal
                {
                    i += 1;
                }
                let r_start = j;
                while j < right_rows.len()
                    && cmp_values(&right_rows[j].values[right_key], rk)? == Ordering::Equal
                {
                    j += 1;
                }
                for l in &left_rows[l_start..i] {
                    for r in &right_rows[r_start..j] {
                        out.push(concat(l, r));
                    }
                }
            }
        }
    }

    Ok(RowStream::new(out.into_iter().map(Ok)))
}

/// The (left, right) join-key positions within their own tuples, parsed from an
/// equi-join `col = col` predicate. `left` is the (Sort-wrapped) left input, so
/// its arity de-offsets the right index from the combined `left ++ right` row.
fn equi_keys(on: &Option<Expr>, left: &PlanNode) -> Result<(usize, usize), QueryError> {
    let left_arity = output_arity(left);
    let Some(Expr::Binary {
        op: BinaryOperator::Eq,
        lhs,
        rhs,
    }) = on
    else {
        return Err(QueryError::Internal(
            "sort-merge join requires an equi-join predicate".into(),
        ));
    };
    let (Expr::Column { index: a }, Expr::Column { index: b }) = (lhs.as_ref(), rhs.as_ref())
    else {
        return Err(QueryError::Internal(
            "sort-merge join key must be `column = column`".into(),
        ));
    };
    let (a, b) = (*a, *b);
    if a < left_arity && b >= left_arity {
        Ok((a, b - left_arity))
    } else if b < left_arity && a >= left_arity {
        Ok((b, a - left_arity))
    } else {
        Err(QueryError::Internal(
            "sort-merge join key must reference both sides".into(),
        ))
    }
}

/// Debug check that `rows` are non-decreasing on `key` (NULLs last) — the
/// invariant the merge relies on the Sort enforcers to provide.
fn is_sorted_by(rows: &[Tuple], key: usize) -> bool {
    rows.windows(2).all(|w| {
        match (&w[0].values[key], &w[1].values[key]) {
            (Value::Null, _) => matches!(w[1].values[key], Value::Null), // nulls last
            (_, Value::Null) => true,
            (a, b) => cmp_values(a, b)
                .map(|o| o != Ordering::Greater)
                .unwrap_or(true),
        }
    })
}

/// `left ++ right`.
fn concat(left: &Tuple, right: &Tuple) -> Tuple {
    let mut values = Vec::with_capacity(left.values.len() + right.values.len());
    values.extend(left.values.iter().cloned());
    values.extend(right.values.iter().cloned());
    Tuple { values }
}

/// `left ++ NULLs` — an unmatched left row in a LEFT/FULL join.
fn pad_right(left: &Tuple, right_width: usize) -> Tuple {
    let mut values = left.values.clone();
    values.resize(values.len() + right_width, Value::Null);
    Tuple { values }
}

/// `NULLs ++ right` — an unmatched right row in a RIGHT/FULL join.
fn pad_left(left_width: usize, right: &Tuple) -> Tuple {
    let mut values = vec![Value::Null; left_width];
    values.extend(right.values.iter().cloned());
    Tuple { values }
}

/// A plan node's output column count. Needed to NULL-pad the absent side of an
/// outer join even when that side yields zero rows. Computed structurally —
/// plans don't carry an output schema yet.
fn output_arity(node: &PlanNode) -> usize {
    match node {
        PlanNode::SeqScan { table } => table.schema().fields.len(),
        PlanNode::SystemScan { relation } => relation.schema().fields.len(),
        PlanNode::Filter { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. }
        | PlanNode::Sort { input, .. } => output_arity(input),
        PlanNode::Project { expressions, .. } => expressions.len(),
        PlanNode::Values { rows } => rows.first().map_or(0, |t| t.values.len()),
        PlanNode::Join { left, right, .. } => output_arity(left) + output_arity(right),
        // Sinks produce a row count, not rows.
        PlanNode::Insert { .. }
        | PlanNode::Delete { .. }
        | PlanNode::CreateTable { .. }
        | PlanNode::CreateDataset { .. } => 0,
    }
}
