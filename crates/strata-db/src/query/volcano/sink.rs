//! Write sinks — operators that consume rows and return a count rather than a
//! stream. They need `&mut QueryContext`, so (unlike read operators) they can't
//! sit inside a pull-iterator chain that holds `&ctx`: they run at the top of a
//! plan, eagerly draining their input before writing.

use crate::catalog::tables::Table;
use crate::query::physical_plan::PlanNode;
use crate::query::{QueryContext, QueryError};

/// Insert every row produced by `input` into `table`. Returns the row count.
pub(super) fn insert(
    table: Table,
    input: PlanNode,
    ctx: &mut QueryContext<'_>,
) -> Result<u64, QueryError> {
    let tuples = super::drain(input, &*ctx)?;
    let mut writer = ctx.table_mut(&table);
    let mut count = 0;
    for tuple in &tuples {
        writer.put(tuple)?;
        count += 1;
    }
    Ok(count)
}

/// Delete each row produced by `input` by its primary-key column (column 0 by
/// convention). Returns the row count.
pub(super) fn delete(
    table: Table,
    input: PlanNode,
    ctx: &mut QueryContext<'_>,
) -> Result<u64, QueryError> {
    let tuples = super::drain(input, &*ctx)?;
    let mut writer = ctx.table_mut(&table);
    let mut count = 0;
    for tuple in &tuples {
        let key = tuple.values.first().ok_or_else(|| {
            QueryError::Internal("delete source has no primary-key column".into())
        })?;
        writer.delete(key)?;
        count += 1;
    }
    Ok(count)
}
