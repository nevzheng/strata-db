//! Join operators. Every join produces `left ++ right` output rows and handles
//! all four join types (INNER/LEFT/RIGHT/FULL; cross = `on: None`), NULL-padding
//! the absent side of an outer join. The algorithms split by family:
//!
//! - [`nested_loop`] — tuple- and block-nested-loop. The general fallback: any
//!   predicate, including cross joins and non-equi conditions.
//! - [`merge`] — sort-merge join. Inner equi-joins over pre-sorted inputs.
//! - [`grace`] — grace hash join. Equi-joins; hash-partitions to disk and
//!   recursively repartitions so each build side fits in memory.
//!
//! Shared row plumbing (`concat`, NULL padding, structural arity, equi-key
//! parsing) lives here so every family draws from one place.

mod grace;
mod merge;
mod nested_loop;

pub(super) use grace::grace_hash_join;
pub(super) use merge::sort_merge_join;
pub(super) use nested_loop::{block_nested_loop_join, nested_loop_join};

use crate::query::QueryError;
use crate::query::expression::{BinaryOperator, Expr};
use crate::query::physical_plan::PlanNode;
use crate::storage::types::{Tuple, Value};

/// The (left, right) join-key positions within their own tuples, parsed from an
/// equi-join `col = col` predicate. `left` is the left input node, so its arity
/// de-offsets the right index from the combined `left ++ right` row.
fn equi_keys(on: &Option<Expr>, left: &PlanNode) -> Result<(usize, usize), QueryError> {
    let left_arity = output_arity(left);
    let Some(Expr::Binary {
        op: BinaryOperator::Eq,
        lhs,
        rhs,
    }) = on
    else {
        return Err(QueryError::Internal(
            "equi-join requires an equi-join predicate".into(),
        ));
    };
    let (Expr::Column { index: a }, Expr::Column { index: b }) = (lhs.as_ref(), rhs.as_ref())
    else {
        return Err(QueryError::Internal(
            "equi-join key must be `column = column`".into(),
        ));
    };
    let (a, b) = (*a, *b);
    if a < left_arity && b >= left_arity {
        Ok((a, b - left_arity))
    } else if b < left_arity && a >= left_arity {
        Ok((b, a - left_arity))
    } else {
        Err(QueryError::Internal(
            "equi-join key must reference both sides".into(),
        ))
    }
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
