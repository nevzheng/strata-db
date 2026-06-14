//! Pipelined scalar operators — one row in, at most one row out, no buffering:
//! Filter, Project, Limit, Offset. Each wraps its input stream and is itself a
//! `RowStream`; the constructors below are what [`build`](super::build) calls.

use crate::query::executor::{RowResult, RowStream};
use crate::query::expression::Expr;
use crate::storage::types::{Tuple, Value};

/// Drop rows where `predicate` isn't `true` (a `NULL` predicate drops the row).
pub(super) fn filter<'a>(input: RowStream<'a>, predicate: Expr) -> RowStream<'a> {
    RowStream::new(Filter { input, predicate })
}

/// Compute a new tuple per input row from a list of expressions.
pub(super) fn project<'a>(input: RowStream<'a>, expressions: Vec<Expr>) -> RowStream<'a> {
    RowStream::new(Project { input, expressions })
}

/// Yield at most `count` rows, then stop.
pub(super) fn limit<'a>(input: RowStream<'a>, count: usize) -> RowStream<'a> {
    RowStream::new(Limit {
        input,
        remaining: count,
    })
}

/// Skip the first `count` rows, then pass the rest through.
pub(super) fn offset<'a>(input: RowStream<'a>, count: usize) -> RowStream<'a> {
    RowStream::new(Offset {
        input,
        remaining: count,
    })
}

struct Filter<'ctx> {
    input: RowStream<'ctx>,
    predicate: Expr,
}

impl Iterator for Filter<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let tuple = match self.input.next()? {
                Ok(t) => t,
                err @ Err(_) => return Some(err),
            };
            match self.predicate.eval(&tuple) {
                Ok(Value::Bool(true)) => return Some(Ok(tuple)),
                Ok(_) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

struct Project<'ctx> {
    input: RowStream<'ctx>,
    expressions: Vec<Expr>,
}

impl Iterator for Project<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        let tuple = match self.input.next()? {
            Ok(t) => t,
            err @ Err(_) => return Some(err),
        };
        let mut values = Vec::with_capacity(self.expressions.len());
        for expr in &self.expressions {
            match expr.eval(&tuple) {
                Ok(v) => values.push(v),
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Ok(Tuple { values }))
    }
}

struct Limit<'ctx> {
    input: RowStream<'ctx>,
    remaining: usize,
}

impl Iterator for Limit<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let row = self.input.next()?;
        self.remaining -= 1;
        Some(row)
    }
}

struct Offset<'ctx> {
    input: RowStream<'ctx>,
    /// Rows still to skip before passing input through.
    remaining: usize,
}

impl Iterator for Offset<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        // Drop the first `remaining` rows, but surface an error hit while
        // skipping instead of swallowing it.
        while self.remaining > 0 {
            match self.input.next()? {
                Ok(_) => self.remaining -= 1,
                err @ Err(_) => return Some(err),
            }
        }
        self.input.next()
    }
}
