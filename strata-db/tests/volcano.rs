//! End-to-end tests for the Volcano backend.
//!
//! Builds a real `Db` with a populated table, then runs `PhysicalPlan`s
//! through the `Executor` trait and asserts the emitted rows.

mod common;

use strata_db::query::data::{Query, QueryStage};
use strata_db::query::executor::{ExecuteResult, Executor, RowStream};
use strata_db::query::expression::{BinaryOperator, Expr};
use strata_db::query::physical_plan::{PhysicalPlan, PlanNode};
use strata_db::query::volcano::Volcano;
use strata_db::{Field, LogicalType, Schema, Table, Tuple, Value};

fn build_events_table(db: &strata_db::Db) -> Table {
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    // Column 0 (`id`) is the primary key by convention.
    let schema = Schema {
        fields: vec![
            Field::new("id", LogicalType::Int32),
            Field::new("name", LogicalType::Text),
        ],
    };
    let table = dataset.create_table("events", schema).unwrap();

    let mut ctx = db.query_context();
    for (id, name) in [(1i32, "alpha"), (2, "bravo"), (3, "charlie"), (4, "delta")] {
        ctx.put(
            &table,
            &Tuple {
                values: vec![Value::Int32(id), Value::Text(name.to_string())],
            },
        )
        .unwrap();
    }
    table
}

/// Wrap a `PhysicalPlan` in a Query already at the `Lowered` stage, so
/// it can be handed straight to the executor without going through the
/// planner.
fn lowered(plan: PhysicalPlan) -> Query {
    let mut q = Query::new("");
    q.physical_plan = Some(plan);
    q.stage = QueryStage::Lowered;
    q
}

fn collect(stream: RowStream<'_>) -> Vec<Tuple> {
    stream.collect::<Result<Vec<_>, _>>().unwrap()
}

fn run_rows(query: Query, db: &strata_db::Db) -> Vec<Tuple> {
    let mut ctx = db.query_context();
    match Volcano.execute(query, &mut ctx).unwrap() {
        ExecuteResult::Rows(stream) => collect(stream),
        ExecuteResult::Affected(_) => panic!("expected Rows"),
    }
}

#[test]
fn seq_scan_yields_every_row() {
    let (_tmp, db) = common::temp_db();
    let table = build_events_table(&db);

    let plan = PhysicalPlan::new(PlanNode::SeqScan { table });
    let rows = run_rows(lowered(plan), &db);

    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].values[0], Value::Int32(1));
    assert_eq!(rows[3].values[0], Value::Int32(4));
}

#[test]
fn filter_drops_non_matching_rows() {
    let (_tmp, db) = common::temp_db();
    let table = build_events_table(&db);

    // id > 2
    let plan = PhysicalPlan::new(PlanNode::Filter {
        input: Box::new(PlanNode::SeqScan { table }),
        predicate: Expr::binary(BinaryOperator::Gt, Expr::column(0), Expr::lit(2i32)),
    });
    let rows = run_rows(lowered(plan), &db);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int32(3));
    assert_eq!(rows[1].values[0], Value::Int32(4));
}

#[test]
fn project_picks_columns() {
    let (_tmp, db) = common::temp_db();
    let table = build_events_table(&db);

    // SELECT name FROM events
    let plan = PhysicalPlan::new(PlanNode::Project {
        input: Box::new(PlanNode::SeqScan { table }),
        expressions: vec![Expr::column(1)],
    });
    let rows = run_rows(lowered(plan), &db);

    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].values.len(), 1);
    assert_eq!(rows[0].values[0], Value::Text("alpha".into()));
}

#[test]
fn limit_caps_output() {
    let (_tmp, db) = common::temp_db();
    let table = build_events_table(&db);

    let plan = PhysicalPlan::new(PlanNode::Limit {
        input: Box::new(PlanNode::SeqScan { table }),
        count: 2,
    });
    let rows = run_rows(lowered(plan), &db);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Int32(1));
    assert_eq!(rows[1].values[0], Value::Int32(2));
}

#[test]
fn filter_then_project_then_limit_composes() {
    let (_tmp, db) = common::temp_db();
    let table = build_events_table(&db);

    // SELECT name FROM events WHERE id >= 2 LIMIT 2
    let plan = PhysicalPlan::new(PlanNode::Limit {
        input: Box::new(PlanNode::Project {
            input: Box::new(PlanNode::Filter {
                input: Box::new(PlanNode::SeqScan { table }),
                predicate: Expr::binary(BinaryOperator::GtEq, Expr::column(0), Expr::lit(2i32)),
            }),
            expressions: vec![Expr::column(1)],
        }),
        count: 2,
    });
    let rows = run_rows(lowered(plan), &db);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Text("bravo".into()));
    assert_eq!(rows[1].values[0], Value::Text("charlie".into()));
}
