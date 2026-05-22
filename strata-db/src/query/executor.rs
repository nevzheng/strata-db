//! Executor backends. Implementations: [`super::volcano::Volcano`]
//! (pull-iterator). Vectorized batch and JIT backends land alongside.

use crate::storage::types::Tuple;

use super::Query;
use super::QueryContext;
use super::QueryError;

/// One row produced by an executor, or the error that prevented it.
pub type RowResult = Result<Tuple, QueryError>;

/// Backend-agnostic stream of result rows.
pub struct RowStream<'ctx> {
    inner: Box<dyn Iterator<Item = RowResult> + 'ctx>,
}

impl<'ctx> RowStream<'ctx> {
    pub fn new(iter: impl Iterator<Item = RowResult> + 'ctx) -> Self {
        Self {
            inner: Box::new(iter),
        }
    }
}

impl Iterator for RowStream<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

pub enum ExecuteResult<'ctx> {
    /// Streaming row source. Caller iterates until exhausted.
    Rows(RowStream<'ctx>),
    /// Row count for write plans (Insert/Delete).
    Affected(u64),
}

pub trait Executor {
    fn execute<'ctx>(
        &self,
        query: Query,
        ctx: &'ctx mut QueryContext<'_>,
    ) -> Result<ExecuteResult<'ctx>, QueryError>;
}
