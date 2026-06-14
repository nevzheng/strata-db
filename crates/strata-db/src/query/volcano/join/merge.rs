//! Sort-merge join — inner equi-joins over pre-sorted inputs. The optimizer
//! inserts `Sort` enforcers on each side's join key, so a single linear merge
//! pass emits matches, expanding equal-key groups to their cartesian product.

use std::cmp::Ordering;

use crate::query::executor::RowStream;
use crate::query::expression::{Expr, cmp_values};
use crate::query::logical_plan::JoinType;
use crate::query::physical_plan::PlanNode;
use crate::query::{QueryContext, QueryError};
use crate::storage::types::{Tuple, Value};

use super::super::build;
use super::{concat, equi_keys};

/// Sort-merge join (inner equi-joins only). Both inputs arrive sorted on their
/// join key — the optimizer inserts `Sort` enforcers — so one merge pass emits
/// matches, expanding equal-key groups to their cartesian product. NULL keys
/// never match (SQL semantics) and are skipped. Non-equi / outer joins use
/// other strategies, so this handles just the inner equi case.
pub(in crate::query::volcano) fn sort_merge_join<'ctx>(
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
