//! Built-in scalar functions and `LIKE` for the expression evaluator.

use rust_decimal::Decimal;

use crate::query::QueryError;
use crate::storage::types::{Tuple, Value};

use super::{Expr, ScalarFunc, as_i64, cmp_values, values_eq};

/// `LIKE` evaluation: text vs text, `NULL` operands propagate, a
/// non-text operand is a type error.
pub(super) fn eval_like(value: Value, pattern: Value, negated: bool) -> Result<Value, QueryError> {
    match (value, pattern) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(s), Value::Text(p)) => Ok(Value::Bool(like_match(&s, &p) ^ negated)),
        (l, r) => Err(QueryError::type_error(format!(
            "LIKE expects text operands, got {l:?} / {r:?}"
        ))),
    }
}

/// SQL `LIKE` wildcard match: `%` matches any run of characters
/// (including none), `_` matches exactly one. Everything else is literal.
/// Two-pointer scan with backtracking on the last `%`.
fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0, 0);
    // Position to resume from after the most recent `%`, if any.
    let (mut star_pi, mut star_ti): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            // Mismatch: let the last `%` absorb one more character.
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    // Trailing `%`s in the pattern can match the empty string.
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

pub(super) fn eval_call(
    func: ScalarFunc,
    args: &[Expr],
    tuple: &Tuple,
) -> Result<Value, QueryError> {
    // The binder validates arity, so the fixed-arity arms index safely.
    match func {
        ScalarFunc::Abs => eval_abs(args[0].eval(tuple)?),
        ScalarFunc::Greatest => eval_extreme(args, tuple, true),
        ScalarFunc::Least => eval_extreme(args, tuple, false),
        ScalarFunc::Ceil => eval_round(args[0].eval(tuple)?, Rounding::Ceil),
        ScalarFunc::Floor => eval_round(args[0].eval(tuple)?, Rounding::Floor),
        ScalarFunc::Round => eval_round(args[0].eval(tuple)?, Rounding::Nearest),
        ScalarFunc::Coalesce => eval_coalesce(args, tuple),
        ScalarFunc::Length => eval_length(args[0].eval(tuple)?),
        ScalarFunc::Repeat => eval_repeat(args[0].eval(tuple)?, args[1].eval(tuple)?),
        ScalarFunc::Upper => eval_case(args[0].eval(tuple)?, true),
        ScalarFunc::Lower => eval_case(args[0].eval(tuple)?, false),
        ScalarFunc::Nullif => eval_nullif(args[0].eval(tuple)?, args[1].eval(tuple)?),
        ScalarFunc::Concat => eval_concat(args, tuple),
        ScalarFunc::Substr => eval_substr(
            args[0].eval(tuple)?,
            args[1].eval(tuple)?,
            args[2].eval(tuple)?,
        ),
        ScalarFunc::TrimBoth => eval_trim(args[0].eval(tuple)?, args[1].eval(tuple)?, true, true),
        ScalarFunc::TrimLeading => {
            eval_trim(args[0].eval(tuple)?, args[1].eval(tuple)?, true, false)
        }
        ScalarFunc::TrimTrailing => {
            eval_trim(args[0].eval(tuple)?, args[1].eval(tuple)?, false, true)
        }
    }
}

/// `CONCAT(a, b, …)` — stringify each non-null argument and join.
fn eval_concat(args: &[Expr], tuple: &Tuple) -> Result<Value, QueryError> {
    let mut out = String::new();
    for arg in args {
        match arg.eval(tuple)? {
            Value::Null => {} // Postgres CONCAT skips nulls.
            v => out.push_str(&concat_str(&v)?),
        }
    }
    Ok(Value::Text(out))
}

/// Render a value as text for `CONCAT` / `||`. Bytes and JSON aren't
/// supported here yet.
pub(super) fn concat_str(v: &Value) -> Result<String, QueryError> {
    Ok(match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        Value::Int16(n) => n.to_string(),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::Float32(f) => f.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Numeric(d) => d.to_string(),
        Value::Text(s) => s.clone(),
        Value::Date(d) => crate::storage::temporal::format_date(*d),
        Value::Timestamp(t) => crate::storage::temporal::format_timestamptz(*t),
        Value::Time(t) => crate::storage::temporal::format_time(*t),
        Value::Uuid(u) => u.to_string(),
        Value::Interval(i) => crate::storage::temporal::format_interval(*i),
        Value::Array(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                parts.push(concat_str(item)?);
            }
            format!("{{{}}}", parts.join(","))
        }
        other => {
            return Err(QueryError::type_error(format!(
                "cannot concatenate {other:?}"
            )));
        }
    })
}

/// `SUBSTRING(s FROM start FOR len)` — 1-indexed; out-of-range bounds
/// clamp (no error). The binder defaults `start`/`len`, so all three
/// arrive present. `NULL` in any argument propagates.
fn eval_substr(s: Value, from: Value, len: Value) -> Result<Value, QueryError> {
    if matches!(s, Value::Null) || matches!(from, Value::Null) || matches!(len, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(text) = s else {
        return Err(QueryError::type_error("SUBSTRING expects a text value"));
    };
    let start = as_i64(&from)
        .ok_or_else(|| QueryError::type_error("SUBSTRING start must be an integer"))?;
    let length = as_i64(&len)
        .ok_or_else(|| QueryError::type_error("SUBSTRING length must be an integer"))?;
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len() as i64;
    // The substring covers the 1-indexed half-open range [start, start+len).
    let from_idx = start.max(1);
    let to_idx = start.saturating_add(length).min(n + 1);
    if to_idx <= from_idx {
        return Ok(Value::Text(String::new()));
    }
    Ok(Value::Text(
        chars[(from_idx - 1) as usize..(to_idx - 1) as usize]
            .iter()
            .collect(),
    ))
}

/// `TRIM` — strip any character in `chars` from the requested side(s).
/// `NULL` in either argument propagates.
fn eval_trim(s: Value, chars: Value, leading: bool, trailing: bool) -> Result<Value, QueryError> {
    if matches!(s, Value::Null) || matches!(chars, Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::Text(text), Value::Text(set)) = (s, chars) else {
        return Err(QueryError::type_error("TRIM expects text values"));
    };
    let set: Vec<char> = set.chars().collect();
    let cs: Vec<char> = text.chars().collect();
    let mut start = 0;
    let mut end = cs.len();
    if leading {
        while start < end && set.contains(&cs[start]) {
            start += 1;
        }
    }
    if trailing {
        while end > start && set.contains(&cs[end - 1]) {
            end -= 1;
        }
    }
    Ok(Value::Text(cs[start..end].iter().collect()))
}

#[derive(Clone, Copy)]
enum Rounding {
    Ceil,
    Floor,
    Nearest,
}

/// Round to a whole number. Integers pass through (already whole); floats
/// and numerics round in their own type; `NULL` propagates.
fn eval_round(v: Value, mode: Rounding) -> Result<Value, QueryError> {
    let f64_round = |f: f64| match mode {
        Rounding::Ceil => f.ceil(),
        Rounding::Floor => f.floor(),
        Rounding::Nearest => f.round(),
    };
    let dec_round = |d: Decimal| match mode {
        Rounding::Ceil => d.ceil(),
        Rounding::Floor => d.floor(),
        Rounding::Nearest => d.round(),
    };
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int16(_) | Value::Int32(_) | Value::Int64(_) => Ok(v),
        Value::Float32(f) => Ok(Value::Float32(f64_round(f as f64) as f32)),
        Value::Float64(f) => Ok(Value::Float64(f64_round(f))),
        Value::Numeric(d) => Ok(Value::Numeric(dec_round(d))),
        other => Err(QueryError::type_error(format!(
            "rounding a non-numeric value {other:?}"
        ))),
    }
}

/// `NULLIF(a, b)` — `NULL` if `a = b` (numeric coercion via `values_eq`),
/// else `a`.
fn eval_nullif(a: Value, b: Value) -> Result<Value, QueryError> {
    Ok(if values_eq(&a, &b) { Value::Null } else { a })
}

/// First non-null argument, or `NULL` if all are null.
fn eval_coalesce(args: &[Expr], tuple: &Tuple) -> Result<Value, QueryError> {
    for arg in args {
        let v = arg.eval(tuple)?;
        if !matches!(v, Value::Null) {
            return Ok(v);
        }
    }
    Ok(Value::Null)
}

/// Character count of a text value (`NULL` propagates).
fn eval_length(v: Value) -> Result<Value, QueryError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Text(s) => Ok(Value::Int64(s.chars().count() as i64)),
        other => Err(QueryError::type_error(format!(
            "LENGTH of non-text value {other:?}"
        ))),
    }
}

/// `REPEAT(s, n)` — `s` repeated `n` times; a non-positive `n` yields the
/// empty string. Either argument `NULL` propagates.
fn eval_repeat(s: Value, n: Value) -> Result<Value, QueryError> {
    if matches!(s, Value::Null) || matches!(n, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Text(text) = s else {
        return Err(QueryError::type_error(
            "REPEAT expects a text first argument",
        ));
    };
    let count =
        as_i64(&n).ok_or_else(|| QueryError::type_error("REPEAT count must be an integer"))?;
    Ok(Value::Text(text.repeat(count.max(0) as usize)))
}

/// Upper/lower-case a text value (`NULL` propagates).
fn eval_case(v: Value, upper: bool) -> Result<Value, QueryError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Text(s) => Ok(Value::Text(if upper {
            s.to_uppercase()
        } else {
            s.to_lowercase()
        })),
        other => Err(QueryError::type_error(format!(
            "case folding a non-text value {other:?}"
        ))),
    }
}

/// Absolute value. `NULL` propagates; `i*::MIN` has no positive magnitude
/// and overflows, surfaced as a type error (matching Postgres).
fn eval_abs(v: Value) -> Result<Value, QueryError> {
    let overflow = || QueryError::type_error("ABS argument out of range");
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int16(n) => n.checked_abs().map(Value::Int16).ok_or_else(overflow),
        Value::Int32(n) => n.checked_abs().map(Value::Int32).ok_or_else(overflow),
        Value::Int64(n) => n.checked_abs().map(Value::Int64).ok_or_else(overflow),
        Value::Float32(f) => Ok(Value::Float32(f.abs())),
        Value::Float64(f) => Ok(Value::Float64(f.abs())),
        Value::Numeric(d) => Ok(Value::Numeric(d.abs())),
        other => Err(QueryError::type_error(format!(
            "ABS of non-numeric value {other:?}"
        ))),
    }
}

/// `GREATEST`/`LEAST` over the arguments. Nulls are skipped (Postgres
/// semantics); all-null yields `NULL`. The winning argument is returned
/// as-is. Incomparable types surface the `cmp_values` type error.
fn eval_extreme(args: &[Expr], tuple: &Tuple, greatest: bool) -> Result<Value, QueryError> {
    let mut best: Option<Value> = None;
    for arg in args {
        let v = arg.eval(tuple)?;
        if matches!(v, Value::Null) {
            continue;
        }
        best = Some(match best {
            None => v,
            Some(cur) => {
                let ord = cmp_values(&cur, &v)?;
                let take_new = if greatest { ord.is_lt() } else { ord.is_gt() };
                if take_new { v } else { cur }
            }
        });
    }
    Ok(best.unwrap_or(Value::Null))
}
