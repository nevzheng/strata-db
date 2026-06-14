//! Binding for DML: `INSERT ... VALUES`, with value coercion against the
//! target table's schema.

use sqlparser::ast::{Expr as AstExpr, Insert, SetExpr, TableObject};

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::query::QueryError;
use crate::query::expression::Expr;
use crate::query::logical_plan::{LogicalNode, LogicalPlan};
use crate::storage::types::{Field, LogicalType, Tuple, Value};

use super::{BindNode, Binder, three_part_name};

// --- INSERT ----------------------------------------------------------------

/// Bind `INSERT INTO p.d.t [(cols)] VALUES (..), (..)` into an
/// [`LogicalNode::Insert`] over a [`LogicalNode::Values`] source. v1 limits:
/// `VALUES` source only (no `INSERT ... SELECT`), constant expressions only.
///
/// Columns not supplied for a row are filled from the column's `DEFAULT`
/// (or `NULL` if nullable) — see [`resolve_default`]. With an explicit
/// column list, "missing" is the set of table columns not named; without
/// one, values map positionally to the leading columns and the trailing
/// columns are missing.
pub(super) fn bind_insert(insert: &Insert, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
    let TableObject::TableName(name) = &insert.table else {
        return Err(QueryError::unsupported(
            "INSERT target must be a table name",
        ));
    };
    let (project, dataset, table_name) = three_part_name(name)?;
    let table = binder
        .ctx()
        .catalog()
        .resolve_table(project, dataset, table_name)?;
    let schema = table.schema().clone();

    // Map each listed column name to its position in the table schema.
    // An empty list means positional (values map to the leading columns).
    let target_cols = resolve_target_columns(&insert.columns, &schema.fields, table.name())?;

    let source = insert
        .source
        .as_ref()
        .ok_or_else(|| QueryError::unsupported("INSERT without VALUES"))?;
    let SetExpr::Values(values) = source.body.as_ref() else {
        return Err(QueryError::unsupported("INSERT source must be VALUES"));
    };

    let expected = target_cols.as_ref().map_or(schema.fields.len(), Vec::len);
    let mut rows = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        let exprs = &row.content;
        if exprs.len() > expected {
            return Err(QueryError::type_error(format!(
                "INSERT has {} value(s) but {} column(s) are targeted in `{}`",
                exprs.len(),
                expected,
                table.name(),
            )));
        }

        // Slot i holds the supplied value for field i, if any.
        let mut slots: Vec<Option<&AstExpr>> = vec![None; schema.fields.len()];
        for (pos, expr) in exprs.iter().enumerate() {
            // With a column list, the value's position picks the named
            // column; without one, position is the column index directly.
            let field_idx = target_cols.as_ref().map_or(pos, |cols| cols[pos]);
            slots[field_idx] = Some(expr);
        }

        let mut row_values = Vec::with_capacity(schema.fields.len());
        for (field, slot) in schema.fields.iter().zip(&slots) {
            let value = match slot {
                Some(expr) => coerce_value(const_eval(expr, binder)?, field)?,
                None => resolve_default(field)?,
            };
            row_values.push(value);
        }
        rows.push(Tuple { values: row_values });
    }

    Ok(LogicalPlan::new(LogicalNode::Insert {
        table,
        input: Box::new(LogicalNode::Values { rows }),
    }))
}

/// Resolve an explicit `INSERT` column list to field indices, in the order
/// the values will be supplied. Returns `None` for the positional form (no
/// column list). Errors on an unknown or repeated column name.
fn resolve_target_columns(
    columns: &[sqlparser::ast::ObjectName],
    fields: &[Field],
    table_name: &str,
) -> Result<Option<Vec<usize>>, QueryError> {
    if columns.is_empty() {
        return Ok(None);
    }
    let mut indices = Vec::with_capacity(columns.len());
    for col in columns {
        // A target column is a plain, unqualified name.
        let name = match col.0.as_slice() {
            [part] => part.as_ident().map(|i| i.value.as_str()).ok_or_else(|| {
                QueryError::unsupported(format!("non-identifier column in INSERT: {col}"))
            })?,
            _ => {
                return Err(QueryError::unsupported(format!(
                    "qualified column in INSERT: {col}"
                )));
            }
        };
        let idx = fields
            .iter()
            .position(|f| f.name.as_str() == name)
            .ok_or_else(|| {
                QueryError::type_error(format!("column `{name}` does not exist in `{table_name}`"))
            })?;
        if indices.contains(&idx) {
            return Err(QueryError::type_error(format!(
                "column `{name}` specified more than once"
            )));
        }
        indices.push(idx);
    }
    Ok(Some(indices))
}

/// Resolve the value for a column with no supplied value: evaluate its
/// `DEFAULT` (per row, so a future volatile default like `now()` runs each
/// time), else `NULL` if the column is nullable, else reject. The stored
/// default is JSON — deserialized back into an [`Expr`] here, keeping the
/// query layer the only place that understands the expression.
fn resolve_default(field: &Field) -> Result<Value, QueryError> {
    match &field.default {
        Some(json) => {
            let expr: Expr = serde_json::from_str(json).map_err(|e| {
                QueryError::Internal(format!("deserialize DEFAULT expression: {e}"))
            })?;
            // Defaults can't reference columns, so an empty tuple suffices.
            let value = expr.eval(&Tuple { values: vec![] })?;
            coerce_value(value, field)
        }
        None if field.nullable => Ok(Value::Null),
        None => Err(QueryError::type_error(format!(
            "missing value for column `{}` (no default, NOT NULL)",
            field.name.as_str()
        ))),
    }
}

/// Evaluate a `VALUES` expression to a constant. It binds with no scope,
/// so any column reference fails — `VALUES` rows are constants. Supports
/// literals and constant expressions (e.g. `1 = 1`); negative numeric
/// literals aren't supported yet (`-` binds as an unsupported unary op).
fn const_eval(expr: &AstExpr, binder: &mut Binder) -> Result<Value, QueryError> {
    let bound = expr.bind(binder)?;
    bound.eval(&Tuple { values: vec![] })
}

/// Validate and coerce `value` to `field`'s type. Same type passes;
/// integers widen freely and narrow with a range check; everything else
/// (cross-category, `NULL` into `NOT NULL`) is a type error.
fn coerce_value(value: Value, field: &Field) -> Result<Value, QueryError> {
    use LogicalType as T;
    match value {
        Value::Null if field.nullable => Ok(Value::Null),
        Value::Null => Err(QueryError::type_error(format!(
            "NULL into NOT NULL column `{}`",
            field.name.as_str()
        ))),
        Value::Bool(b) if matches!(field.ty, T::Bool) => Ok(Value::Bool(b)),
        Value::Date(d) if matches!(field.ty, T::Date) => Ok(Value::Date(d)),
        Value::Timestamp(t) if matches!(field.ty, T::Timestamp) => Ok(Value::Timestamp(t)),
        Value::Time(t) if matches!(field.ty, T::Time) => Ok(Value::Time(t)),
        Value::Uuid(u) if matches!(field.ty, T::Uuid) => Ok(Value::Uuid(u)),
        Value::Interval(i) if matches!(field.ty, T::Interval) => Ok(Value::Interval(i)),
        // Arrays: coerce each element to the column's element type. NULL
        // elements aren't supported in v1.
        Value::Array(items) if matches!(field.ty, T::Array(_)) => {
            let T::Array(elem) = &field.ty else {
                unreachable!()
            };
            let elem_field = Field {
                name: field.name.clone(),
                ty: (**elem).clone(),
                nullable: false,
                default: None,
            };
            let coerced = items
                .into_iter()
                .map(|v| {
                    if matches!(v, Value::Null) {
                        Err(QueryError::type_error(
                            "NULL array elements aren't supported",
                        ))
                    } else {
                        coerce_value(v, &elem_field)
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Array(coerced))
        }
        Value::Text(s) if matches!(field.ty, T::Text) => Ok(Value::Text(s)),
        Value::Bytes(b) if matches!(field.ty, T::Bytes) => Ok(Value::Bytes(b)),
        Value::Json(j) if matches!(field.ty, T::Json) => Ok(Value::Json(j)),
        Value::Int16(_) | Value::Int32(_) | Value::Int64(_)
            if matches!(field.ty, T::Int16 | T::Int32 | T::Int64) =>
        {
            let n = match value {
                Value::Int16(x) => x as i64,
                Value::Int32(x) => x as i64,
                Value::Int64(x) => x,
                _ => unreachable!(),
            };
            fit_int(n, field)
        }
        // Floats: exact-type passes; cross-width and int→float convert
        // (REAL narrows from DOUBLE/int, possibly to ±inf — same as SQL
        // assignment). Float→int is not implicit; it needs an explicit cast.
        Value::Float64(f) if matches!(field.ty, T::Float64) => Ok(Value::Float64(f)),
        Value::Float64(f) if matches!(field.ty, T::Float32) => Ok(Value::Float32(f as f32)),
        Value::Float32(f) if matches!(field.ty, T::Float32) => Ok(Value::Float32(f)),
        Value::Float32(f) if matches!(field.ty, T::Float64) => Ok(Value::Float64(f as f64)),
        Value::Int16(_) | Value::Int32(_) | Value::Int64(_)
            if matches!(field.ty, T::Float32 | T::Float64) =>
        {
            let n = match value {
                Value::Int16(x) => x as f64,
                Value::Int32(x) => x as f64,
                Value::Int64(x) => x as f64,
                _ => unreachable!(),
            };
            Ok(match field.ty {
                T::Float32 => Value::Float32(n as f32),
                _ => Value::Float64(n),
            })
        }
        // Numerics: exact into a numeric column; integers convert in
        // exactly; numeric → float is allowed (may lose precision).
        Value::Numeric(d) if matches!(field.ty, T::Numeric) => Ok(Value::Numeric(d)),
        Value::Int16(_) | Value::Int32(_) | Value::Int64(_) if matches!(field.ty, T::Numeric) => {
            let n = match value {
                Value::Int16(x) => x as i64,
                Value::Int32(x) => x as i64,
                Value::Int64(x) => x,
                _ => unreachable!(),
            };
            Ok(Value::Numeric(Decimal::from(n)))
        }
        Value::Numeric(d) if matches!(field.ty, T::Float64) => d
            .to_f64()
            .map(Value::Float64)
            .ok_or_else(|| QueryError::type_error("NUMERIC out of range for DOUBLE")),
        Value::Numeric(d) if matches!(field.ty, T::Float32) => d
            .to_f32()
            .map(Value::Float32)
            .ok_or_else(|| QueryError::type_error("NUMERIC out of range for REAL")),
        other => Err(QueryError::type_error(format!(
            "cannot insert {other:?} into column `{}` of type {:?}",
            field.name.as_str(),
            field.ty
        ))),
    }
}

/// Fit an integer into an integer column: widening always succeeds, a
/// narrowing conversion that overflows is a type error.
fn fit_int(n: i64, field: &Field) -> Result<Value, QueryError> {
    let out_of_range = || {
        QueryError::type_error(format!(
            "value {n} out of range for column `{}` of type {:?}",
            field.name.as_str(),
            field.ty
        ))
    };
    match field.ty {
        LogicalType::Int64 => Ok(Value::Int64(n)),
        LogicalType::Int32 => i32::try_from(n)
            .map(Value::Int32)
            .map_err(|_| out_of_range()),
        LogicalType::Int16 => i16::try_from(n)
            .map(Value::Int16)
            .map_err(|_| out_of_range()),
        _ => unreachable!("fit_int is only called for integer columns"),
    }
}
