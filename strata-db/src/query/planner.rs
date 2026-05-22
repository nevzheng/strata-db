//! The planner.
//!
//! [`Planner::plan`] takes a [`Query`] and returns it populated with
//! the physical execution plan.

use super::QueryContext;
use super::QueryError;
use super::data::{Query, QueryStage};

pub struct Planner;

impl Planner {
    pub fn plan(&self, mut query: Query, ctx: &mut QueryContext<'_>) -> Result<Query, QueryError> {
        query.parse()?.bind(ctx)?.optimize()?.lower()?;
        Ok(query)
    }
}

// Per-phase methods are private to this module — `Planner::plan` is the
// only entry point. Tests inside this file still see them via `use super::*`.
impl Query {
    fn parse(&mut self) -> Result<&mut Self, QueryError> {
        if self.ast.is_some() {
            return Ok(self);
        }
        self.expect_stage(QueryStage::Created)?;
        self.ast = Some(crate::sql::parse(&self.sql)?);
        self.stage = QueryStage::Parsed;
        Ok(self)
    }

    fn bind(&mut self, _ctx: &mut QueryContext<'_>) -> Result<&mut Self, QueryError> {
        if self.logical_plan.is_some() {
            return Ok(self);
        }
        self.expect_stage(QueryStage::Parsed)?;
        self.stage = QueryStage::Bound;
        Ok(self)
    }

    fn optimize(&mut self) -> Result<&mut Self, QueryError> {
        if self.stage == QueryStage::Optimized || self.stage == QueryStage::Lowered {
            return Ok(self);
        }
        self.expect_stage(QueryStage::Bound)?;
        self.stage = QueryStage::Optimized;
        Ok(self)
    }

    fn lower(&mut self) -> Result<&mut Self, QueryError> {
        if self.physical_plan.is_some() {
            return Ok(self);
        }
        self.expect_stage(QueryStage::Optimized)?;
        self.stage = QueryStage::Lowered;
        Ok(self)
    }

    fn expect_stage(&self, expected: QueryStage) -> Result<(), QueryError> {
        if self.stage == expected {
            Ok(())
        } else {
            Err(QueryError::Internal(format!(
                "planner phase called in wrong order: expected stage {:?}, got {:?}",
                expected, self.stage
            )))
        }
    }
}
