//! The planner — drives SQL through a typed pass pipeline.
//!
//! [`Planner::plan`] takes SQL text and returns a [`PhysicalQuery`] via
//! `parse → analyze → build_logical → optimize_logical → lower → optimize_physical`.
//! The `analyze` slot is an umbrella over the analysis substages —
//! today just [`binder::Bind`], folded together under
//! [`pass::Analyze`]. Construct via [`Planner::builder`].

pub mod builder;
pub mod pass;
pub mod rule;

mod binder;
mod lower;

use super::QueryContext;
use super::QueryError;
use super::stages::{AnalyzedQuery, LogicalQuery, ParsedQuery, PhysicalQuery, RawQuery};

pub use builder::{BuildError, PlannerBuilder};

use pass::{OptimizeLogical, OptimizePhysical, Pass};

pub struct Planner {
    parse: Box<dyn Pass<Input = RawQuery, Output = ParsedQuery>>,
    analyze: Box<dyn Pass<Input = ParsedQuery, Output = AnalyzedQuery>>,
    build_logical: Box<dyn Pass<Input = AnalyzedQuery, Output = LogicalQuery>>,
    optimize_logical: OptimizeLogical,
    lower: Box<dyn Pass<Input = LogicalQuery, Output = PhysicalQuery>>,
    optimize_physical: OptimizePhysical,
}

impl Planner {
    pub fn builder() -> PlannerBuilder {
        PlannerBuilder::default()
    }

    pub fn plan(&self, sql: &str, ctx: &QueryContext<'_>) -> Result<PhysicalQuery, QueryError> {
        let q = RawQuery {
            sql: sql.to_string(),
        };
        let q = self.parse.run(q, ctx)?;
        let q = self.analyze.run(q, ctx)?;
        let q = self.build_logical.run(q, ctx)?;
        let q = self.optimize_logical.run(q, ctx)?;
        let q = self.lower.run(q, ctx)?;
        self.optimize_physical.run(q, ctx)
    }
}
