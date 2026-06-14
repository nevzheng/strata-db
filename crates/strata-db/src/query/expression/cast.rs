//! `CAST` / `::` type conversions for the expression evaluator.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use uuid::Uuid;

use crate::query::QueryError;
use crate::storage::types::{LogicalType, Value};

use super::function::concat_str;

/// `CAST`/`::` conversion. `NULL` casts to `NULL` for any target. Each
/// target accepts the sources that convert sensibly; anything else is a
/// type error. Float/numeric → integer rounds; out-of-range narrowing is
/// an error.
pub(super) fn eval_cast(v: Value, target: &LogicalType) -> Result<Value, QueryError> {
    use LogicalType as T;
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    match target {
        T::Bool => cast_to_bool(v),
        T::Int16 => fit_int_cast(cast_to_i64(&v)?, i16::try_from, Value::Int16),
        T::Int32 => fit_int_cast(cast_to_i64(&v)?, i32::try_from, Value::Int32),
        T::Int64 => Ok(Value::Int64(cast_to_i64(&v)?)),
        T::Float32 => Ok(Value::Float32(cast_to_f64(&v)? as f32)),
        T::Float64 => Ok(Value::Float64(cast_to_f64(&v)?)),
        T::Numeric => cast_to_numeric(&v),
        T::Text => Ok(Value::Text(cast_to_text(&v)?)),
        T::Bytes => cast_to_bytes(v),
        T::Date => cast_to_date(&v),
        T::Timestamp => cast_to_timestamp(&v),
        T::Time => cast_to_time(&v),
        T::Uuid => cast_to_uuid(&v),
        T::Interval => cast_to_interval(&v),
        T::Json => cast_to_json(&v),
        // No text->array parsing yet; only an array passes through.
        T::Array(_) => match v {
            arr @ Value::Array(_) => Ok(arr),
            other => Err(cast_err(&other, "array")),
        },
    }
}

fn cast_to_interval(v: &Value) -> Result<Value, QueryError> {
    match v {
        Value::Interval(i) => Ok(Value::Interval(*i)),
        Value::Text(s) => crate::storage::temporal::parse_interval(s.trim())
            .map(Value::Interval)
            .map_err(QueryError::type_error),
        other => Err(cast_err(other, "interval")),
    }
}

fn cast_to_time(v: &Value) -> Result<Value, QueryError> {
    match v {
        Value::Time(t) => Ok(Value::Time(*t)),
        Value::Text(s) => crate::storage::temporal::parse_time(s.trim())
            .map(Value::Time)
            .map_err(QueryError::type_error),
        other => Err(cast_err(other, "time")),
    }
}

fn cast_to_uuid(v: &Value) -> Result<Value, QueryError> {
    match v {
        Value::Uuid(u) => Ok(Value::Uuid(*u)),
        Value::Text(s) => Uuid::parse_str(s.trim())
            .map(Value::Uuid)
            .map_err(|_| cast_err(v, "uuid")),
        other => Err(cast_err(other, "uuid")),
    }
}

fn cast_err(v: &Value, target: &str) -> QueryError {
    QueryError::type_error(format!("cannot cast {v:?} to {target}"))
}

/// Narrow a cast `i64` into a smaller integer, erroring on overflow.
fn fit_int_cast<I, E>(
    n: i64,
    try_from: fn(i64) -> Result<I, E>,
    wrap: fn(I) -> Value,
) -> Result<Value, QueryError> {
    try_from(n)
        .map(wrap)
        .map_err(|_| QueryError::type_error(format!("value {n} out of range for target integer")))
}

fn cast_to_i64(v: &Value) -> Result<i64, QueryError> {
    match v {
        Value::Int16(n) => Ok(*n as i64),
        Value::Int32(n) => Ok(*n as i64),
        Value::Int64(n) => Ok(*n),
        Value::Bool(b) => Ok(*b as i64),
        Value::Float32(f) => Ok(f.round() as i64),
        Value::Float64(f) => Ok(f.round() as i64),
        Value::Numeric(d) => d.round().to_i64().ok_or_else(|| cast_err(v, "integer")),
        Value::Text(s) => s.trim().parse::<i64>().map_err(|_| cast_err(v, "integer")),
        other => Err(cast_err(other, "integer")),
    }
}

fn cast_to_f64(v: &Value) -> Result<f64, QueryError> {
    match v {
        Value::Int16(n) => Ok(*n as f64),
        Value::Int32(n) => Ok(*n as f64),
        Value::Int64(n) => Ok(*n as f64),
        Value::Float32(f) => Ok(*f as f64),
        Value::Float64(f) => Ok(*f),
        Value::Numeric(d) => d.to_f64().ok_or_else(|| cast_err(v, "float")),
        Value::Text(s) => s.trim().parse::<f64>().map_err(|_| cast_err(v, "float")),
        other => Err(cast_err(other, "float")),
    }
}

fn cast_to_numeric(v: &Value) -> Result<Value, QueryError> {
    let d = match v {
        Value::Int16(n) => Decimal::from(*n),
        Value::Int32(n) => Decimal::from(*n),
        Value::Int64(n) => Decimal::from(*n),
        Value::Numeric(d) => *d,
        Value::Float32(f) => Decimal::from_f32_retain(*f).ok_or_else(|| cast_err(v, "numeric"))?,
        Value::Float64(f) => Decimal::from_f64_retain(*f).ok_or_else(|| cast_err(v, "numeric"))?,
        Value::Text(s) => s.trim().parse().map_err(|_| cast_err(v, "numeric"))?,
        other => return Err(cast_err(other, "numeric")),
    };
    Ok(Value::Numeric(d))
}

fn cast_to_bool(v: Value) -> Result<Value, QueryError> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(b)),
        Value::Int16(n) => Ok(Value::Bool(n != 0)),
        Value::Int32(n) => Ok(Value::Bool(n != 0)),
        Value::Int64(n) => Ok(Value::Bool(n != 0)),
        Value::Text(ref s) => match s.trim().to_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Ok(Value::Bool(true)),
            "false" | "f" | "no" | "n" | "off" | "0" => Ok(Value::Bool(false)),
            _ => Err(cast_err(&v, "boolean")),
        },
        other => Err(cast_err(&other, "boolean")),
    }
}

fn cast_to_text(v: &Value) -> Result<String, QueryError> {
    match v {
        Value::Bytes(b) => Ok(format!(
            "\\x{}",
            b.iter().fold(String::new(), |mut s, byte| {
                s.push_str(&format!("{byte:02x}"));
                s
            })
        )),
        Value::Json(j) => Ok(j.to_string()),
        // concat_str covers bool / numerics / text / date / timestamp.
        other => concat_str(other),
    }
}

fn cast_to_bytes(v: Value) -> Result<Value, QueryError> {
    match v {
        Value::Bytes(b) => Ok(Value::Bytes(b)),
        Value::Text(s) => Ok(Value::Bytes(s.into_bytes())),
        other => Err(cast_err(&other, "bytes")),
    }
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

fn cast_to_date(v: &Value) -> Result<Value, QueryError> {
    match v {
        Value::Date(d) => Ok(Value::Date(*d)),
        Value::Text(s) => crate::storage::temporal::parse_date(s.trim())
            .map(Value::Date)
            .map_err(QueryError::type_error),
        // Drop the time of day (floor toward earlier days for negatives).
        Value::Timestamp(t) => Ok(Value::Date(t.div_euclid(MICROS_PER_DAY) as i32)),
        other => Err(cast_err(other, "date")),
    }
}

fn cast_to_timestamp(v: &Value) -> Result<Value, QueryError> {
    match v {
        Value::Timestamp(t) => Ok(Value::Timestamp(*t)),
        Value::Text(s) => crate::storage::temporal::parse_timestamptz(s.trim())
            .map(Value::Timestamp)
            .map_err(QueryError::type_error),
        // Midnight UTC of the date.
        Value::Date(d) => Ok(Value::Timestamp(*d as i64 * MICROS_PER_DAY)),
        other => Err(cast_err(other, "timestamp")),
    }
}

fn cast_to_json(v: &Value) -> Result<Value, QueryError> {
    match v {
        Value::Json(j) => Ok(Value::Json(j.clone())),
        Value::Text(s) => serde_json::from_str(s)
            .map(Value::Json)
            .map_err(|_| cast_err(v, "json")),
        other => Err(cast_err(other, "json")),
    }
}
