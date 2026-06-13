//! Binding for DML: `INSERT ... VALUES`, with value coercion against the
//! target table's schema.

use sqlparser::ast::{Expr as AstExpr, Insert, SetExpr, TableObject};

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::query::QueryError;
use crate::query::logical_plan::{LogicalNode, LogicalPlan};
use crate::storage::types::{Field, LogicalType, Tuple, Value};

use super::{BindNode, Binder, three_part_name};

// --- INSERT ----------------------------------------------------------------

/// Bind `INSERT INTO p.d.t VALUES (..), (..)` into an [`LogicalNode::Insert`]
/// over a [`LogicalNode::Values`] source. v1 limits: positional values only
/// (no column list), `VALUES` source only (no `INSERT ... SELECT`), constant
/// expressions only. Each value is validated and coerced against the table's
/// stored schema.
pub(super) fn bind_insert(insert: &Insert, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
    if !insert.columns.is_empty() {
        return Err(QueryError::unsupported(
            "INSERT with an explicit column list",
        ));
    }
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

    let source = insert
        .source
        .as_ref()
        .ok_or_else(|| QueryError::unsupported("INSERT without VALUES"))?;
    let SetExpr::Values(values) = source.body.as_ref() else {
        return Err(QueryError::unsupported("INSERT source must be VALUES"));
    };

    let mut rows = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        let exprs = &row.content;
        if exprs.len() != schema.fields.len() {
            return Err(QueryError::type_error(format!(
                "INSERT has {} value(s) but table `{}` has {} column(s)",
                exprs.len(),
                table.name(),
                schema.fields.len()
            )));
        }
        let mut row_values = Vec::with_capacity(exprs.len());
        for (expr, field) in exprs.iter().zip(&schema.fields) {
            let value = const_eval(expr, binder)?;
            row_values.push(coerce_value(value, field)?);
        }
        rows.push(Tuple { values: row_values });
    }

    Ok(LogicalPlan::new(LogicalNode::Insert {
        table,
        input: Box::new(LogicalNode::Values { rows }),
    }))
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
