//! Rewrite rules consumed by [`OptimizeLogical`](super::pass::OptimizeLogical)
//! and [`OptimizePhysical`](super::pass::OptimizePhysical).
//!
//! No concrete rules exist yet — the traits live here so the optimize
//! passes can hold a `Vec<Box<dyn Rule>>` and grow over time.

use crate::query::QueryContext;
use crate::query::QueryError;
use crate::query::logical_plan::LogicalPlan;
use crate::query::physical_plan::PhysicalPlan;

pub trait LogicalRule {
    fn name(&self) -> &'static str;
    fn apply(&self, plan: LogicalPlan, ctx: &QueryContext<'_>) -> Result<LogicalPlan, QueryError>;
}

pub trait PhysicalRule {
    fn name(&self) -> &'static str;
    fn apply(&self, plan: PhysicalPlan, ctx: &QueryContext<'_>)
    -> Result<PhysicalPlan, QueryError>;
}
