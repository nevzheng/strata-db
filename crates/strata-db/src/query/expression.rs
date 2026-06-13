//! Scalar expressions.
//!
//! A tree that computes a [`Value`] from an input [`Tuple`]. Lives
//! inside plan nodes that need per-row logic — `Filter` holds a
//! predicate, `Project` holds an expression per output column.
//!
//! Expressions are *data*, not closures. That's the whole reason for
//! this representation: the optimizer can push them down past joins, a
//! pretty-printer can render them, a future JIT can walk the tree to
//! emit code. Closures would be opaque to all of that.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::storage::types::{LogicalType, Tuple, Value};

use super::QueryError;

/// Binary operators. These are the *constants* of the expression
/// language — the verbs that combine two sub-expressions into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BinaryOperator {
    // Comparison
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    // Logical
    And,
    Or,
    // String
    Concat,
}

/// A built-in scalar function — a row-wise operation on its arguments
/// (no aggregation across rows). `Greatest`/`Least` back both the
/// `GREATEST`/`LEAST` names and the multi-argument `MAX`/`MIN` forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ScalarFunc {
    /// `ABS(x)` — absolute value of a number.
    Abs,
    /// `GREATEST(a, b, …)` / `MAX(a, b, …)` — largest non-null argument.
    Greatest,
    /// `LEAST(a, b, …)` / `MIN(a, b, …)` — smallest non-null argument.
    Least,
    /// `CEIL(x)` — round a number up to the nearest integer value.
    Ceil,
    /// `FLOOR(x)` — round a number down to the nearest integer value.
    Floor,
    /// `ROUND(x)` — round a number to the nearest integer value.
    Round,
    /// `COALESCE(a, b, …)` — the first non-null argument (or null).
    Coalesce,
    /// `LENGTH(s)` — number of characters in a text value.
    Length,
    /// `REPEAT(s, n)` — `s` concatenated `n` times.
    Repeat,
    /// `UPPER(s)` — text upper-cased.
    Upper,
    /// `LOWER(s)` — text lower-cased.
    Lower,
    /// `NULLIF(a, b)` — `NULL` if `a = b`, else `a`.
    Nullif,
    /// `CONCAT(a, b, …)` — args stringified and joined; nulls skipped.
    Concat,
    /// `SUBSTRING(s, start, len)` — 1-indexed; the binder supplies
    /// defaults (`start = 1`, `len = i64::MAX` for "to the end").
    Substr,
    /// `TRIM(BOTH chars FROM s)` — trim any of `chars` from both ends.
    TrimBoth,
    /// `TRIM(LEADING chars FROM s)`.
    TrimLeading,
    /// `TRIM(TRAILING chars FROM s)`.
    TrimTrailing,
}

/// A scalar expression. Each variant is a "basic operation" carrying
/// its own data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Expr {
    /// Reference column `index` in the input tuple.
    Column { index: usize },
    /// A constant value.
    Literal { value: Value },
    /// A binary operation on two sub-expressions.
    Binary {
        op: BinaryOperator,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Logical negation.
    Not { input: Box<Expr> },
    /// A built-in scalar function applied to its arguments.
    Call { func: ScalarFunc, args: Vec<Expr> },
    /// `x IS NULL` / `x IS NOT NULL` (`negated`). Unlike comparisons,
    /// this is total — it always returns `true`/`false`, never `NULL`.
    IsNull { expr: Box<Expr>, negated: bool },
    /// `s LIKE pattern` / `NOT LIKE` (`negated`). `%` matches any run of
    /// characters, `_` matches one; `NULL` operands propagate.
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    /// Arithmetic negation (`-x`), evaluated per row. (Distinct from
    /// [`Expr::Not`], which is logical negation.)
    Neg { input: Box<Expr> },
    /// `CASE` — the first branch whose condition is `true` wins; if none,
    /// `else_result` (or `NULL`). The binder lowers simple `CASE x WHEN v`
    /// into searched form (`x = v`), so each branch holds a boolean
    /// condition and a result.
    Case {
        branches: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    /// `CAST(x AS ty)` / `x::ty` — convert to `target`. `NULL` propagates.
    Cast {
        input: Box<Expr>,
        target: LogicalType,
    },
}

impl Expr {
    pub fn column(index: usize) -> Self {
        Expr::Column { index }
    }

    pub fn lit(value: impl Into<Value>) -> Self {
        Expr::Literal {
            value: value.into(),
        }
    }

    pub fn binary(op: BinaryOperator, lhs: Expr, rhs: Expr) -> Self {
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    pub fn negate(input: Expr) -> Self {
        Expr::Not {
            input: Box::new(input),
        }
    }

    /// Evaluate the expression against `tuple`.
    ///
    /// Null semantics follow SQL three-valued logic where it's cheap:
    /// `NULL` propagates through comparisons (`NULL = x` → `NULL`), and
    /// `AND` / `OR` short-circuit on `false` / `true` even with the
    /// other side `NULL`; otherwise they return `NULL`.
    ///
    /// Comparisons coerce across integer widths (so a column compares
    /// against an `Int64` literal); a non-numeric type mismatch is a
    /// [`QueryError::Type`] surfaced to the user.
    pub fn eval(&self, tuple: &Tuple) -> Result<Value, QueryError> {
        match self {
            Expr::Column { index } => tuple.values.get(*index).cloned().ok_or_else(|| {
                QueryError::Internal(format!(
                    "column index {index} out of bounds (arity {})",
                    tuple.values.len()
                ))
            }),
            Expr::Literal { value } => Ok(value.clone()),
            Expr::Not { input } => match input.eval(tuple)? {
                Value::Null => Ok(Value::Null),
                Value::Bool(b) => Ok(Value::Bool(!b)),
                other => Err(QueryError::Internal(format!(
                    "NOT expects Bool, got {other:?}"
                ))),
            },
            Expr::Binary { op, lhs, rhs } => {
                let l = lhs.eval(tuple)?;
                let r = rhs.eval(tuple)?;
                eval_binary(*op, l, r)
            }
            Expr::Call { func, args } => eval_call(*func, args, tuple),
            Expr::IsNull { expr, negated } => {
                let is_null = matches!(expr.eval(tuple)?, Value::Null);
                Ok(Value::Bool(is_null ^ negated))
            }
            Expr::Like {
                expr,
                pattern,
                negated,
            } => eval_like(expr.eval(tuple)?, pattern.eval(tuple)?, *negated),
            Expr::Neg { input } => eval_neg(input.eval(tuple)?),
            Expr::Case {
                branches,
                else_result,
            } => {
                for (cond, result) in branches {
                    // Only a true condition matches; NULL/false skip.
                    if matches!(cond.eval(tuple)?, Value::Bool(true)) {
                        return result.eval(tuple);
                    }
                }
                match else_result {
                    Some(e) => e.eval(tuple),
                    None => Ok(Value::Null),
                }
            }
            Expr::Cast { input, target } => eval_cast(input.eval(tuple)?, *target),
        }
    }
}

/// `CAST`/`::` conversion. `NULL` casts to `NULL` for any target. Each
/// target accepts the sources that convert sensibly; anything else is a
/// type error. Float/numeric → integer rounds; out-of-range narrowing is
/// an error.
fn eval_cast(v: Value, target: LogicalType) -> Result<Value, QueryError> {
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
        T::Json => cast_to_json(&v),
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

/// Arithmetic negation. `NULL` propagates; `i*::MIN` overflows to a type
/// error; non-numeric values are a type error.
fn eval_neg(v: Value) -> Result<Value, QueryError> {
    let overflow = || QueryError::type_error("negation out of range");
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int16(n) => n.checked_neg().map(Value::Int16).ok_or_else(overflow),
        Value::Int32(n) => n.checked_neg().map(Value::Int32).ok_or_else(overflow),
        Value::Int64(n) => n.checked_neg().map(Value::Int64).ok_or_else(overflow),
        Value::Float32(f) => Ok(Value::Float32(-f)),
        Value::Float64(f) => Ok(Value::Float64(-f)),
        Value::Numeric(d) => Ok(Value::Numeric(-d)),
        other => Err(QueryError::type_error(format!("cannot negate {other:?}"))),
    }
}

/// `LIKE` evaluation: text vs text, `NULL` operands propagate, a
/// non-text operand is a type error.
fn eval_like(value: Value, pattern: Value, negated: bool) -> Result<Value, QueryError> {
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

fn eval_call(func: ScalarFunc, args: &[Expr], tuple: &Tuple) -> Result<Value, QueryError> {
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
        ScalarFunc::Nullif => {
            let a = args[0].eval(tuple)?;
            let b = args[1].eval(tuple)?;
            Ok(if values_eq(&a, &b) { Value::Null } else { a })
        }
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
fn concat_str(v: &Value) -> Result<String, QueryError> {
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

fn eval_binary(op: BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, QueryError> {
    use BinaryOperator::*;

    // SQL 3VL short-circuit for AND/OR before null propagation.
    match (op, &lhs, &rhs) {
        (And, Value::Bool(false), _) | (And, _, Value::Bool(false)) => {
            return Ok(Value::Bool(false));
        }
        (Or, Value::Bool(true), _) | (Or, _, Value::Bool(true)) => {
            return Ok(Value::Bool(true));
        }
        _ => {}
    }

    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
        return Ok(Value::Null);
    }

    match op {
        Eq => Ok(Value::Bool(values_eq(&lhs, &rhs))),
        NotEq => Ok(Value::Bool(!values_eq(&lhs, &rhs))),
        Lt => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_lt())),
        LtEq => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_le())),
        Gt => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_gt())),
        GtEq => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_ge())),
        And => match (lhs, rhs) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
            (l, r) => Err(QueryError::Internal(format!(
                "AND expects Bool, got {l:?} / {r:?}"
            ))),
        },
        Or => match (lhs, rhs) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
            (l, r) => Err(QueryError::Internal(format!(
                "OR expects Bool, got {l:?} / {r:?}"
            ))),
        },
        // `||`: stringify both sides and join (NULL already handled above).
        Concat => {
            let mut s = concat_str(&lhs)?;
            s.push_str(&concat_str(&rhs)?);
            Ok(Value::Text(s))
        }
    }
}

/// The integer value of `v`, widened to `i64`, if it is any integer
/// type. Lets comparisons mix integer widths — notably a column against
/// a SQL literal, which always binds as `Int64`.
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int16(n) => Some(*n as i64),
        Value::Int32(n) => Some(*n as i64),
        Value::Int64(n) => Some(*n),
        _ => None,
    }
}

/// The exact `Decimal` value of `v`, if it is an integer or a numeric.
/// Lets numerics compare exactly with each other and with integers
/// (floats are excluded — a float comparison goes through [`as_f64`]).
fn as_decimal(v: &Value) -> Option<Decimal> {
    match v {
        Value::Int16(n) => Some(Decimal::from(*n)),
        Value::Int32(n) => Some(Decimal::from(*n)),
        Value::Int64(n) => Some(Decimal::from(*n)),
        Value::Numeric(d) => Some(*d),
        _ => None,
    }
}

/// The value of `v` as `f64`, if it is any numeric type (integer, float,
/// or numeric). Used when a float is involved, so the comparison happens
/// in `f64` (possibly lossy for a large numeric — matching Postgres,
/// which promotes to float8 for a numeric-vs-float8 comparison).
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int16(n) => Some(*n as f64),
        Value::Int32(n) => Some(*n as f64),
        Value::Int64(n) => Some(*n as f64),
        Value::Float32(f) => Some(*f as f64),
        Value::Float64(f) => Some(*f),
        Value::Numeric(d) => d.to_f64(),
        _ => None,
    }
}

/// Equality with numeric coercion: two integers compare by value
/// regardless of width; a float against any number compares as `f64`
/// (`total_cmp`, so `NaN == NaN`); otherwise fall back to structural
/// equality (`Text == Text`, mismatched types → not equal).
fn values_eq(lhs: &Value, rhs: &Value) -> bool {
    if let (Some(a), Some(b)) = (as_i64(lhs), as_i64(rhs)) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (as_decimal(lhs), as_decimal(rhs)) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (as_f64(lhs), as_f64(rhs)) {
        return a.total_cmp(&b).is_eq();
    }
    lhs == rhs
}

/// Total order for ordered comparisons. Integers compare across widths
/// (coerced to `i64`); a float against any number compares as `f64` via
/// `total_cmp` (consistent with the float key encoding — `NaN` sorts
/// highest, and `-0.0` < `0.0`); same-typed bools / text / dates /
/// timestamps compare directly. Anything else — a non-numeric vs numeric
/// mix — is a type error the caller surfaces to the user.
fn cmp_values(lhs: &Value, rhs: &Value) -> Result<std::cmp::Ordering, QueryError> {
    if let (Some(a), Some(b)) = (as_i64(lhs), as_i64(rhs)) {
        return Ok(a.cmp(&b));
    }
    if let (Some(a), Some(b)) = (as_decimal(lhs), as_decimal(rhs)) {
        return Ok(a.cmp(&b));
    }
    if let (Some(a), Some(b)) = (as_f64(lhs), as_f64(rhs)) {
        return Ok(a.total_cmp(&b));
    }
    match (lhs, rhs) {
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        (Value::Text(a), Value::Text(b)) => Ok(a.cmp(b)),
        // Dates / timestamps order by their integer count; they don't
        // coerce with plain integers (`date < 5` is a type error).
        (Value::Date(a), Value::Date(b)) => Ok(a.cmp(b)),
        (Value::Timestamp(a), Value::Timestamp(b)) => Ok(a.cmp(b)),
        (l, r) => Err(QueryError::type_error(format!(
            "cannot compare {l:?} and {r:?}"
        ))),
    }
}

// Conveniences for building literals.
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<i16> for Value {
    fn from(v: i16) -> Self {
        Value::Int16(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int32(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int64(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(values: Vec<Value>) -> Tuple {
        Tuple { values }
    }

    #[test]
    fn eval_column_and_literal() {
        let tuple = t(vec![Value::Int32(10), Value::Text("hi".into())]);
        assert_eq!(Expr::column(0).eval(&tuple).unwrap(), Value::Int32(10));
        assert_eq!(Expr::lit(7i32).eval(&tuple).unwrap(), Value::Int32(7));
    }

    #[test]
    fn eval_comparison() {
        let tuple = t(vec![Value::Int32(10)]);
        let pred = Expr::binary(BinaryOperator::Lt, Expr::column(0), Expr::lit(20i32));
        assert_eq!(pred.eval(&tuple).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_compares_across_integer_widths() {
        // An Int32 column against an Int64 literal (how SQL literals bind).
        let tuple = t(vec![Value::Int32(2)]);
        let eq = Expr::binary(BinaryOperator::Eq, Expr::column(0), Expr::lit(2i64));
        assert_eq!(eq.eval(&tuple).unwrap(), Value::Bool(true));
        let gt = Expr::binary(BinaryOperator::Gt, Expr::column(0), Expr::lit(1i64));
        assert_eq!(gt.eval(&tuple).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_non_numeric_comparison_mismatch_is_type_error() {
        let tuple = t(vec![Value::Text("x".into())]);
        let pred = Expr::binary(BinaryOperator::Lt, Expr::column(0), Expr::lit(5i64));
        assert!(matches!(pred.eval(&tuple), Err(QueryError::Type(_))));
    }

    #[test]
    fn eval_and_short_circuits_on_null() {
        // false AND NULL -> false (3VL short-circuit, even though one side is null)
        let expr = Expr::binary(
            BinaryOperator::And,
            Expr::lit(false),
            Expr::lit(Value::Null),
        );
        assert_eq!(expr.eval(&t(vec![])).unwrap(), Value::Bool(false));
    }

    #[test]
    fn eval_null_propagates_through_comparison() {
        let expr = Expr::binary(BinaryOperator::Eq, Expr::lit(Value::Null), Expr::lit(1i32));
        assert_eq!(expr.eval(&t(vec![])).unwrap(), Value::Null);
    }

    #[test]
    fn eval_type_mismatch_is_error() {
        // bool vs int: not coercible, so a type error.
        let expr = Expr::binary(BinaryOperator::Lt, Expr::lit(true), Expr::lit(1i32));
        assert!(matches!(expr.eval(&t(vec![])), Err(QueryError::Type(_))));
    }

    #[test]
    fn eval_date_comparison() {
        // Two dates compare by their day count; no integer coercion.
        let lhs = Expr::lit(Value::Date(20617)); // 2026-06-13
        let rhs = Expr::lit(Value::Date(10957)); // 2000-01-01
        let gt = Expr::binary(BinaryOperator::Gt, lhs.clone(), rhs.clone());
        assert_eq!(gt.eval(&t(vec![])).unwrap(), Value::Bool(true));
        let eq = Expr::binary(BinaryOperator::Eq, lhs.clone(), lhs);
        assert_eq!(eq.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_date_vs_integer_is_type_error() {
        let cmp = Expr::binary(
            BinaryOperator::Lt,
            Expr::lit(Value::Date(1)),
            Expr::lit(5i64),
        );
        assert!(matches!(cmp.eval(&t(vec![])), Err(QueryError::Type(_))));
    }

    #[test]
    fn eval_timestamp_comparison() {
        let later = Expr::lit(Value::Timestamp(1_000_000));
        let earlier = Expr::lit(Value::Timestamp(0));
        let gt = Expr::binary(BinaryOperator::Gt, later, earlier);
        assert_eq!(gt.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_float_comparison() {
        let lt = Expr::binary(
            BinaryOperator::Lt,
            Expr::lit(Value::Float64(1.5)),
            Expr::lit(Value::Float64(2.5)),
        );
        assert_eq!(lt.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_float_coerces_with_integer() {
        // A float column against an integer literal compares numerically.
        let gt = Expr::binary(
            BinaryOperator::Gt,
            Expr::lit(Value::Float64(2.5)),
            Expr::lit(2i64),
        );
        assert_eq!(gt.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    fn num(s: &str) -> Value {
        Value::Numeric(s.parse().unwrap())
    }

    #[test]
    fn eval_numeric_exact_equality() {
        // Trailing-zero scales are equal; numerics compare by value.
        let eq = Expr::binary(
            BinaryOperator::Eq,
            Expr::lit(num("1.10")),
            Expr::lit(num("1.1")),
        );
        assert_eq!(eq.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_numeric_vs_integer_is_exact() {
        let gt = Expr::binary(BinaryOperator::Gt, Expr::lit(num("2.5")), Expr::lit(2i64));
        assert_eq!(gt.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_numeric_ordered_comparison() {
        let lt = Expr::binary(
            BinaryOperator::Lt,
            Expr::lit(num("-0.001")),
            Expr::lit(num("0")),
        );
        assert_eq!(lt.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    fn call(func: ScalarFunc, args: Vec<Expr>) -> Expr {
        Expr::Call { func, args }
    }

    #[test]
    fn eval_abs_of_int_and_float() {
        assert_eq!(
            call(ScalarFunc::Abs, vec![Expr::lit(-7i64)])
                .eval(&t(vec![]))
                .unwrap(),
            Value::Int64(7)
        );
        assert_eq!(
            call(ScalarFunc::Abs, vec![Expr::lit(Value::Float64(-2.5))])
                .eval(&t(vec![]))
                .unwrap(),
            Value::Float64(2.5)
        );
    }

    #[test]
    fn eval_abs_of_null_is_null() {
        let e = call(ScalarFunc::Abs, vec![Expr::lit(Value::Null)]);
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Null);
    }

    #[test]
    fn eval_abs_int_min_overflows() {
        let e = call(ScalarFunc::Abs, vec![Expr::lit(i64::MIN)]);
        assert!(matches!(e.eval(&t(vec![])), Err(QueryError::Type(_))));
    }

    #[test]
    fn eval_greatest_and_least() {
        let args = || vec![Expr::lit(3i64), Expr::lit(1i64), Expr::lit(2i64)];
        assert_eq!(
            call(ScalarFunc::Greatest, args()).eval(&t(vec![])).unwrap(),
            Value::Int64(3)
        );
        assert_eq!(
            call(ScalarFunc::Least, args()).eval(&t(vec![])).unwrap(),
            Value::Int64(1)
        );
    }

    #[test]
    fn eval_greatest_skips_nulls_and_coerces() {
        // NULL is ignored; a float argument wins over smaller integers.
        let e = call(
            ScalarFunc::Greatest,
            vec![
                Expr::lit(1i64),
                Expr::lit(Value::Null),
                Expr::lit(Value::Float64(2.5)),
            ],
        );
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Float64(2.5));
    }

    #[test]
    fn eval_least_all_null_is_null() {
        let e = call(
            ScalarFunc::Least,
            vec![Expr::lit(Value::Null), Expr::lit(Value::Null)],
        );
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Null);
    }

    #[test]
    fn eval_round_family() {
        let f = |func, v| call(func, vec![Expr::lit(v)]).eval(&t(vec![])).unwrap();
        assert_eq!(
            f(ScalarFunc::Ceil, Value::Float64(2.1)),
            Value::Float64(3.0)
        );
        assert_eq!(
            f(ScalarFunc::Floor, Value::Float64(2.9)),
            Value::Float64(2.0)
        );
        assert_eq!(
            f(ScalarFunc::Round, Value::Float64(2.4)),
            Value::Float64(2.0)
        );
        assert_eq!(f(ScalarFunc::Ceil, num("2.1")), num("3"));
        // Integers are already whole — passed through unchanged.
        assert_eq!(f(ScalarFunc::Floor, Value::Int64(7)), Value::Int64(7));
    }

    #[test]
    fn eval_coalesce_first_non_null() {
        let e = call(
            ScalarFunc::Coalesce,
            vec![
                Expr::lit(Value::Null),
                Expr::lit(Value::Null),
                Expr::lit(3i64),
                Expr::lit(4i64),
            ],
        );
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Int64(3));
    }

    #[test]
    fn eval_length_counts_characters() {
        // "Привет 🦀" is 8 Unicode scalar values, not bytes.
        let e = call(ScalarFunc::Length, vec![Expr::lit("Привет 🦀")]);
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Int64(8));
    }

    #[test]
    fn eval_repeat_concatenates() {
        let e = call(ScalarFunc::Repeat, vec![Expr::lit("ab"), Expr::lit(3i64)]);
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Text("ababab".into()));
    }

    #[test]
    fn eval_upper_lower() {
        let up = call(ScalarFunc::Upper, vec![Expr::lit("Hello")]);
        assert_eq!(up.eval(&t(vec![])).unwrap(), Value::Text("HELLO".into()));
        let lo = call(ScalarFunc::Lower, vec![Expr::lit("Hello")]);
        assert_eq!(lo.eval(&t(vec![])).unwrap(), Value::Text("hello".into()));
    }

    fn is_null(expr: Expr, negated: bool) -> Expr {
        Expr::IsNull {
            expr: Box::new(expr),
            negated,
        }
    }

    #[test]
    fn eval_is_null_is_total() {
        // IS NULL never returns NULL — `NULL IS NULL` is true.
        assert_eq!(
            is_null(Expr::lit(Value::Null), false)
                .eval(&t(vec![]))
                .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            is_null(Expr::lit(1i64), false).eval(&t(vec![])).unwrap(),
            Value::Bool(false)
        );
        // IS NOT NULL
        assert_eq!(
            is_null(Expr::lit(1i64), true).eval(&t(vec![])).unwrap(),
            Value::Bool(true)
        );
    }

    fn like(s: &str, p: &str, negated: bool) -> Expr {
        Expr::Like {
            expr: Box::new(Expr::lit(s)),
            pattern: Box::new(Expr::lit(p)),
            negated,
        }
    }

    #[test]
    fn eval_like_wildcards() {
        let m = |s, p| like(s, p, false).eval(&t(vec![])).unwrap();
        assert_eq!(m("hello", "h%o"), Value::Bool(true));
        assert_eq!(m("hello", "h_llo"), Value::Bool(true));
        assert_eq!(m("hello", "h_o"), Value::Bool(false));
        assert_eq!(m("hello", "%"), Value::Bool(true));
        assert_eq!(m("hello", "hello"), Value::Bool(true));
        assert_eq!(m("hello", "Hello"), Value::Bool(false)); // case-sensitive
        // NOT LIKE
        assert_eq!(
            like("hello", "x%", true).eval(&t(vec![])).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn eval_like_null_propagates() {
        let e = Expr::Like {
            expr: Box::new(Expr::lit(Value::Null)),
            pattern: Box::new(Expr::lit("%")),
            negated: false,
        };
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Null);
    }

    #[test]
    fn eval_neg_on_column() {
        // -x where x is a column (the case constant-folding couldn't do).
        let e = Expr::Neg {
            input: Box::new(Expr::column(0)),
        };
        assert_eq!(e.eval(&t(vec![Value::Int32(5)])).unwrap(), Value::Int32(-5));
        assert_eq!(e.eval(&t(vec![Value::Null])).unwrap(), Value::Null);
    }

    #[test]
    fn eval_nullif() {
        let nf = |a, b| {
            call(ScalarFunc::Nullif, vec![Expr::lit(a), Expr::lit(b)])
                .eval(&t(vec![]))
                .unwrap()
        };
        assert_eq!(nf(Value::Int64(5), Value::Int64(5)), Value::Null);
        assert_eq!(nf(Value::Int64(5), Value::Int64(6)), Value::Int64(5));
    }

    #[test]
    fn eval_cast_matrix() {
        let cast = |v: Value, ty| {
            Expr::Cast {
                input: Box::new(Expr::lit(v)),
                target: ty,
            }
            .eval(&t(vec![]))
            .unwrap()
        };
        assert_eq!(
            cast(Value::Text("42".into()), LogicalType::Int32),
            Value::Int32(42)
        );
        assert_eq!(
            cast(Value::Float64(1.7), LogicalType::Int64),
            Value::Int64(2)
        );
        assert_eq!(
            cast(Value::Int64(42), LogicalType::Text),
            Value::Text("42".into())
        );
        assert_eq!(
            cast(Value::Text("yes".into()), LogicalType::Bool),
            Value::Bool(true)
        );
        // NULL casts to NULL for any target.
        assert_eq!(cast(Value::Null, LogicalType::Int32), Value::Null);
    }

    #[test]
    fn eval_cast_invalid_is_error() {
        let e = Expr::Cast {
            input: Box::new(Expr::lit("abc")),
            target: LogicalType::Int32,
        };
        assert!(matches!(e.eval(&t(vec![])), Err(QueryError::Type(_))));
    }

    #[test]
    fn eval_case_branches() {
        // First true branch wins; no match + no else -> NULL.
        let case = |branches, else_result| {
            Expr::Case {
                branches,
                else_result,
            }
            .eval(&t(vec![]))
            .unwrap()
        };
        let t_branch = (Expr::lit(true), Expr::lit("yes"));
        let f_branch = (Expr::lit(false), Expr::lit("no"));
        assert_eq!(
            case(vec![f_branch.clone(), t_branch.clone()], None),
            Value::Text("yes".into())
        );
        assert_eq!(case(vec![f_branch.clone()], None), Value::Null);
        assert_eq!(
            case(vec![f_branch], Some(Box::new(Expr::lit("else")))),
            Value::Text("else".into())
        );
    }

    #[test]
    fn eval_string_concat_operator() {
        let e = Expr::binary(BinaryOperator::Concat, Expr::lit("a"), Expr::lit("b"));
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Text("ab".into()));
        // NULL propagates.
        let n = Expr::binary(
            BinaryOperator::Concat,
            Expr::lit("a"),
            Expr::lit(Value::Null),
        );
        assert_eq!(n.eval(&t(vec![])).unwrap(), Value::Null);
    }

    #[test]
    fn eval_concat_stringifies_and_skips_null() {
        let e = call(
            ScalarFunc::Concat,
            vec![Expr::lit("a"), Expr::lit(Value::Null), Expr::lit(1i64)],
        );
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Text("a1".into()));
    }

    #[test]
    fn eval_substring_one_indexed() {
        // SUBSTRING('hello' FROM 2 FOR 3) -> 'ell'
        let e = call(
            ScalarFunc::Substr,
            vec![Expr::lit("hello"), Expr::lit(2i64), Expr::lit(3i64)],
        );
        assert_eq!(e.eval(&t(vec![])).unwrap(), Value::Text("ell".into()));
        // No length (i64::MAX sentinel) -> to the end.
        let e2 = call(
            ScalarFunc::Substr,
            vec![Expr::lit("hello"), Expr::lit(3i64), Expr::lit(i64::MAX)],
        );
        assert_eq!(e2.eval(&t(vec![])).unwrap(), Value::Text("llo".into()));
    }

    #[test]
    fn eval_trim_sides() {
        let trim = |f, s, c| {
            call(f, vec![Expr::lit(s), Expr::lit(c)])
                .eval(&t(vec![]))
                .unwrap()
        };
        assert_eq!(
            trim(ScalarFunc::TrimBoth, "xxhixx", "x"),
            Value::Text("hi".into())
        );
        assert_eq!(
            trim(ScalarFunc::TrimLeading, "xxhixx", "x"),
            Value::Text("hixx".into())
        );
        assert_eq!(
            trim(ScalarFunc::TrimTrailing, "  hi  ", " "),
            Value::Text("  hi".into())
        );
    }

    #[test]
    fn eval_float_widths_compare_equal() {
        // REAL 1.5 equals DOUBLE 1.5 (f32 widens exactly to f64).
        let eq = Expr::binary(
            BinaryOperator::Eq,
            Expr::lit(Value::Float32(1.5)),
            Expr::lit(Value::Float64(1.5)),
        );
        assert_eq!(eq.eval(&t(vec![])).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_column_index_out_of_bounds_is_error() {
        let tuple = t(vec![Value::Int32(0)]);
        let res = Expr::column(5).eval(&tuple);
        assert!(matches!(res, Err(QueryError::Internal(_))));
    }
}
