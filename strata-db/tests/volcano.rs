//! End-to-end tests for the Volcano backend.
//!
//! Builds a real `Db` with a populated table, then runs `PhysicalPlan`s
//! through `query::volcano::Executor` and asserts the emitted rows.

mod common;

use strata_db::query::expression::{BinaryOperator, Expr};
use strata_db::query::physical_plan::{PhysicalPlan, PlanNode};
use strata_db::query::volcano::{Executor, NextRow, Operator};
use strata_db::{Field, LogicalType, Schema, Table, Tuple, TypedStore, Value};

fn build_events_table(db: &strata_db::Db) -> Table {
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let schema = Schema {
        fields: vec![
            Field::new("id", LogicalType::Int32),
            Field::new("name", LogicalType::Text),
        ],
    };
    let table = dataset.create_table("events", schema).unwrap();

    let rows = [
        (b"a" as &[u8], 1i32, "alpha"),
        (b"b", 2, "bravo"),
        (b"c", 3, "charlie"),
        (b"d", 4, "delta"),
    ];
    for (key, id, name) in rows {
        table
            .put(
                key,
                &Tuple {
                    values: vec![Value::Int32(id), Value::Text(name.to_string())],
                },
            )
            .unwrap();
    }
    table
}

/// Pull every row from an executor until it signals Done.
fn drain(mut exec: Executor) -> Vec<Tuple> {
    let mut out = vec![];
    loop {
        match exec.next().unwrap() {
            NextRow::Row(t) => out.push(t),
            NextRow::Done => return out,
        }
    }
}

#[test]
fn seq_scan_yields_every_row() {
    let (_tmp, db) = common::temp_db();
    let table = build_events_table(&db);

    let plan = PhysicalPlan::new(PlanNode::SeqScan { table });
    let rows = drain(Executor::new(plan).unwrap());

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
    let rows = drain(Executor::new(plan).unwrap());

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
    let rows = drain(Executor::new(plan).unwrap());

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
    let rows = drain(Executor::new(plan).unwrap());

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
    let rows = drain(Executor::new(plan).unwrap());

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Text("bravo".into()));
    assert_eq!(rows[1].values[0], Value::Text("charlie".into()));
}
