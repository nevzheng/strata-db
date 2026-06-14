//! Sort operator tests — driving `PlanNode::Sort` directly, including the
//! multi-run external-merge path (more rows than one in-memory run).

mod common;

use strata_db::query::executor::{ExecuteResult, Executor, RowStream};
use strata_db::query::expression::Expr;
use strata_db::query::logical_plan::SortKey;
use strata_db::query::physical_plan::{PhysicalPlan, PlanNode};
use strata_db::query::volcano::Volcano;
use strata_db::{Db, Field, LogicalType, Schema, Table, Tuple, Value};

/// A table `t(id INT, k INT)` filled from `rows` of `(id, k)`.
fn build_table(db: &Db, rows: impl IntoIterator<Item = (i32, i32)>) -> Table {
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("s").unwrap();
    let table = dataset
        .create_table(
            "t",
            Schema {
                fields: vec![
                    Field::new("id", LogicalType::Int32),
                    Field::new("k", LogicalType::Int32),
                ],
            },
        )
        .unwrap();
    let mut ctx = db.query_context();
    let mut writer = ctx.table_mut(&table);
    for (id, k) in rows {
        writer
            .put(&Tuple {
                values: vec![Value::Int32(id), Value::Int32(k)],
            })
            .unwrap();
    }
    table
}

fn sort_plan(table: Table, keys: Vec<SortKey>) -> PhysicalPlan {
    let input_schema = table.schema().clone();
    PhysicalPlan::new(PlanNode::Sort {
        input: Box::new(PlanNode::SeqScan { table }),
        keys,
        input_schema,
    })
}

fn key(column: usize, ascending: bool) -> SortKey {
    SortKey {
        expr: Expr::column(column),
        ascending,
        nulls_first: !ascending,
    }
}

fn run(plan: PhysicalPlan, db: &Db) -> Vec<Tuple> {
    let mut ctx = db.query_context();
    match Volcano.execute(plan, &mut ctx).unwrap() {
        ExecuteResult::Rows(stream) => collect(stream),
        ExecuteResult::Affected(_) => panic!("expected Rows"),
    }
}

fn collect(stream: RowStream<'_>) -> Vec<Tuple> {
    stream.collect::<Result<Vec<_>, _>>().unwrap()
}

fn ks(rows: &[Tuple]) -> Vec<i32> {
    rows.iter()
        .map(|t| match t.values[1] {
            Value::Int32(k) => k,
            ref other => panic!("expected Int32 k, got {other:?}"),
        })
        .collect()
}

#[test]
fn sorts_ascending_and_descending() {
    let (_tmp, db) = common::temp_db();
    let table = build_table(&db, [(1, 30), (2, 10), (3, 20), (4, 10)]);

    let asc = run(sort_plan(table.clone(), vec![key(1, true)]), &db);
    assert_eq!(ks(&asc), vec![10, 10, 20, 30]);

    let desc = run(sort_plan(table, vec![key(1, false)]), &db);
    assert_eq!(ks(&desc), vec![30, 20, 10, 10]);
}

#[test]
fn external_merge_spans_multiple_runs() {
    // More rows than one in-memory run (SORT_RUN_TUPLES = 4096), so run
    // generation + k-way merge both run for real. Keys are scrambled so a
    // single-run sort can't accidentally pass.
    let (_tmp, db) = common::temp_db();
    let n = 5_000i32;
    let table = build_table(
        &db,
        (0..n).map(|id| (id, ((id as i64).wrapping_mul(2_654_435_761) % 9973) as i32)),
    );

    let sorted = run(sort_plan(table, vec![key(1, true)]), &db);
    assert_eq!(sorted.len(), n as usize);

    let keys = ks(&sorted);
    let mut expected = keys.clone();
    expected.sort_unstable();
    assert_eq!(keys, expected, "external merge sort must be fully ordered");
}
