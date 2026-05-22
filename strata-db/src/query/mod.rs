//! Query processing: planning and execution.
//!
//! A query passes through this module in stages: parse → bind → logical
//! plan → optimize → physical plan → execute. Today only the physical
//! plan and the (forthcoming) execution backends live here; a parser,
//! binder, and logical planner will land alongside.
//!
//! Two trees show up at this level. [`PhysicalPlan`] / [`PlanNode`]
//! describe dataflow over rows — scans, filters, joins. [`Expr`]
//! describes per-row computation — column refs, comparisons, boolean
//! combinators — and lives inside plan nodes that need predicates or
//! projections.

pub mod context;
pub mod expression;
pub mod physical_plan;
pub mod volcano;

pub use context::QueryContext;
pub use expression::{BinaryOperator, Expr};
pub use physical_plan::{PhysicalPlan, PlanNode};

use crate::catalog::CatalogError;
use crate::storage::codec::DecodeError;

/// Either side of the codec boundary: our binary codec (tuple/value
/// encoding) or the serde-JSON path used by catalog metadata blobs.
#[derive(Debug)]
pub enum CodecError {
    Decode(DecodeError),
    Serde(serde_json::Error),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Decode(e) => write!(f, "decode: {e:?}"),
            CodecError::Serde(e) => write!(f, "serde: {e}"),
        }
    }
}

impl std::error::Error for CodecError {}

impl From<DecodeError> for CodecError {
    fn from(e: DecodeError) -> Self {
        CodecError::Decode(e)
    }
}

impl From<serde_json::Error> for CodecError {
    fn from(e: serde_json::Error) -> Self {
        CodecError::Serde(e)
    }
}

#[derive(Debug)]
pub enum QueryError {
    /// Genuine catalog-domain errors: name lookups, duplicate creation.
    Catalog(CatalogError),
    /// Anything bubbling up from the storage engine.
    Storage(strata_store::StorageError),
    /// Tuple/value codec or catalog-metadata serde failures.
    Codec(CodecError),
    /// Invariant violation that the binder or planner should have
    /// caught — out-of-bounds column refs, type mismatches in already-
    /// type-checked expressions, schema-shape mismatches, and the like.
    /// If this fires, it's a bug above us.
    Internal(String),
}

impl From<CatalogError> for QueryError {
    fn from(e: CatalogError) -> Self {
        QueryError::Catalog(e)
    }
}

impl From<strata_store::StorageError> for QueryError {
    fn from(e: strata_store::StorageError) -> Self {
        QueryError::Storage(e)
    }
}

impl From<CodecError> for QueryError {
    fn from(e: CodecError) -> Self {
        QueryError::Codec(e)
    }
}

impl From<DecodeError> for QueryError {
    fn from(e: DecodeError) -> Self {
        QueryError::Codec(CodecError::Decode(e))
    }
}

impl From<serde_json::Error> for QueryError {
    fn from(e: serde_json::Error) -> Self {
        QueryError::Codec(CodecError::Serde(e))
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Catalog(e) => write!(f, "catalog: {e:?}"),
            QueryError::Storage(e) => write!(f, "storage: {e}"),
            QueryError::Codec(e) => write!(f, "codec: {e}"),
            QueryError::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}

impl std::error::Error for QueryError {}
