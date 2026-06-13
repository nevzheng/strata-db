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

use crate::storage::types::{Tuple, Value};

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
        }
    }
}

fn eval_call(func: ScalarFunc, args: &[Expr], tuple: &Tuple) -> Result<Value, QueryError> {
    match func {
        // The binder guarantees ABS has exactly one argument.
        ScalarFunc::Abs => eval_abs(args[0].eval(tuple)?),
        ScalarFunc::Greatest => eval_extreme(args, tuple, true),
        ScalarFunc::Least => eval_extreme(args, tuple, false),
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

/// The value of `v` as `f64`, if it is any numeric type (integer or
/// float). Lets a float coerce across the numeric types — a float
/// against an integer literal, or `REAL` against `DOUBLE`.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int16(n) => Some(*n as f64),
        Value::Int32(n) => Some(*n as f64),
        Value::Int64(n) => Some(*n as f64),
        Value::Float32(f) => Some(*f as f64),
        Value::Float64(f) => Some(*f),
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
