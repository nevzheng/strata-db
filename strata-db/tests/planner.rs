//! End-to-end tests for the planner.
//!
//! Drives SQL through `Planner::plan` against a real `Db` and asserts on
//! the produced `PhysicalPlan` shape. Execution is covered by
//! `tests/volcano.rs`; here we only verify the pipeline lowers to the
//! expected operator tree.

mod common;

use strata_db::query::Planner;
use strata_db::query::stages::PhysicalQuery;
use strata_db::{Db, Field, LogicalType, PlanNode, QueryError, Schema};

fn setup_events_table(db: &Db) {
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let schema = Schema {
        fields: vec![
            Field::new("id", LogicalType::Int32),
            Field::new("name", LogicalType::Text),
        ],
    };
    dataset.create_table("events", schema).unwrap();
}

fn plan(db: &Db, sql: &str) -> Result<PhysicalQuery, QueryError> {
    let ctx = db.query_context();
    let planner = Planner::builder().build().unwrap();
    planner.plan(sql, &ctx)
}

#[test]
fn select_star_lowers_scan_to_seqscan() {
    let (_tmp, db) = common::temp_db();
    setup_events_table(&db);

    let pq = plan(&db, "SELECT * FROM acme.metrics.events").unwrap();
    assert_eq!(pq.physical.len(), 1);
    let root = &pq.physical[0].root;
    let PlanNode::Project { input, expressions } = root else {
        panic!("expected Project root, got {root:?}");
    };
    assert_eq!(
        expressions.len(),
        2,
        "wildcard should expand to two columns"
    );
    assert!(matches!(input.as_ref(), PlanNode::SeqScan { .. }));
}

#[test]
fn where_clause_lands_under_project_as_filter() {
    let (_tmp, db) = common::temp_db();
    setup_events_table(&db);

    let pq = plan(&db, "SELECT * FROM acme.metrics.events WHERE id > 2").unwrap();
    let root = &pq.physical[0].root;
    let PlanNode::Project { input, .. } = root else {
        panic!("expected Project root, got {root:?}");
    };
    let PlanNode::Filter { input: scan, .. } = input.as_ref() else {
        panic!("expected Filter under Project, got {input:?}");
    };
    assert!(matches!(scan.as_ref(), PlanNode::SeqScan { .. }));
}

#[test]
fn select_without_from_uses_values_source() {
    let (_tmp, db) = common::temp_db();

    let pq = plan(&db, "SELECT 1").unwrap();
    let root = &pq.physical[0].root;
    let PlanNode::Project { input, expressions } = root else {
        panic!("expected Project root, got {root:?}");
    };
    assert_eq!(expressions.len(), 1);
    assert!(matches!(input.as_ref(), PlanNode::Values { .. }));
}

#[test]
fn parse_error_surfaces_from_parse_pass() {
    let (_tmp, db) = common::temp_db();
    let err = plan(&db, "NOT SQL ###").unwrap_err();
    assert!(matches!(err, QueryError::Parse(_)), "got {err:?}");
}

#[test]
fn unknown_table_errors_through_bind() {
    let (_tmp, db) = common::temp_db();
    let err = plan(&db, "SELECT * FROM acme.metrics.missing").unwrap_err();
    assert!(matches!(err, QueryError::Catalog(_)), "got {err:?}");
}

#[test]
fn distinct_is_unsupported() {
    let (_tmp, db) = common::temp_db();
    setup_events_table(&db);
    let err = plan(&db, "SELECT DISTINCT * FROM acme.metrics.events").unwrap_err();
    assert!(matches!(err, QueryError::Unsupported(_)), "got {err:?}");
}
