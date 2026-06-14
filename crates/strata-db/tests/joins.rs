//! Per-strategy join tests. Each physical join algorithm is exercised
//! directly by building a `PlanNode::Join` with an explicit `strategy`
//! (the optimizer only ever picks one), checking concrete results and that
//! the strategies agree across join types.

mod common;

use strata_db::query::executor::{ExecuteResult, Executor, RowStream};
use strata_db::query::expression::{BinaryOperator, Expr};
use strata_db::query::logical_plan::{JoinType, SortKey};
use strata_db::query::physical_plan::{JoinStrategy, PhysicalPlan, PlanNode};
use strata_db::query::volcano::Volcano;
use strata_db::{Db, Field, LogicalType, Schema, Table, Tuple, Value};

/// `l(id, k)` and `r(id, label)`, joined on `l.k = r.id`. `l` row (3,99) and
/// `r` row (40,'C') have no match — so outer joins have unmatched rows on
/// both sides.
fn build_tables(db: &Db) -> (Table, Table) {
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("j").unwrap();
    let l = dataset
        .create_table(
            "l",
            Schema {
                fields: vec![
                    Field::new("id", LogicalType::Int32),
                    Field::new("k", LogicalType::Int32),
                ],
            },
        )
        .unwrap();
    let r = dataset
        .create_table(
            "r",
            Schema {
                fields: vec![
                    Field::new("id", LogicalType::Int32),
                    Field::new("label", LogicalType::Text),
                ],
            },
        )
        .unwrap();

    let mut ctx = db.query_context();
    {
        let mut w = ctx.table_mut(&l);
        for (id, k) in [(1, 10), (2, 20), (3, 99)] {
            w.put(&row2(id, Value::Int32(k))).unwrap();
        }
    }
    {
        let mut w = ctx.table_mut(&r);
        for (id, label) in [(10, "A"), (20, "B"), (40, "C")] {
            w.put(&row2(id, Value::Text(label.to_string()))).unwrap();
        }
    }
    (l, r)
}

fn row2(id: i32, second: Value) -> Tuple {
    Tuple {
        values: vec![Value::Int32(id), second],
    }
}

/// `l.k = r.id` over the concatenated `l ++ r` row (cols: l.id=0, l.k=1, r.id=2).
fn on_lk_eq_rid() -> Expr {
    Expr::Binary {
        op: BinaryOperator::Eq,
        lhs: Box::new(Expr::column(1)),
        rhs: Box::new(Expr::column(2)),
    }
}

fn join_plan(
    l: Table,
    r: Table,
    on: Option<Expr>,
    join_type: JoinType,
    strategy: JoinStrategy,
) -> PhysicalPlan {
    let left_schema = l.schema().clone();
    let right_schema = r.schema().clone();
    PhysicalPlan::new(PlanNode::Join {
        left: Box::new(PlanNode::SeqScan { table: l }),
        right: Box::new(PlanNode::SeqScan { table: r }),
        on,
        join_type,
        left_schema,
        right_schema,
        strategy,
    })
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

/// Order-independent comparison — join output is unordered.
fn sorted(mut rows: Vec<Tuple>) -> Vec<Tuple> {
    rows.sort_by_key(|t| format!("{:?}", t.values));
    rows
}

const STRATEGIES: [JoinStrategy; 2] = [JoinStrategy::NestedLoop, JoinStrategy::BlockNestedLoop];

#[test]
fn each_strategy_inner_join_matches_expected() {
    let (_tmp, db) = common::temp_db();
    let (l, r) = build_tables(&db);

    let expected = sorted(vec![
        Tuple {
            values: vec![
                Value::Int32(1),
                Value::Int32(10),
                Value::Int32(10),
                Value::Text("A".into()),
            ],
        },
        Tuple {
            values: vec![
                Value::Int32(2),
                Value::Int32(20),
                Value::Int32(20),
                Value::Text("B".into()),
            ],
        },
    ]);

    for strategy in STRATEGIES {
        let rows = run(
            join_plan(
                l.clone(),
                r.clone(),
                Some(on_lk_eq_rid()),
                JoinType::Inner,
                strategy,
            ),
            &db,
        );
        assert_eq!(sorted(rows), expected, "inner join, {strategy:?}");
    }
}

#[test]
fn each_strategy_outer_joins_pad_unmatched() {
    let (_tmp, db) = common::temp_db();
    let (l, r) = build_tables(&db);

    let l3 = || Value::Int32(3); // l (3,99) is the unmatched left row
    let unmatched_left = Tuple {
        values: vec![l3(), Value::Int32(99), Value::Null, Value::Null],
    };
    let unmatched_right = Tuple {
        values: vec![
            Value::Null,
            Value::Null,
            Value::Int32(40),
            Value::Text("C".into()),
        ],
    };

    for strategy in STRATEGIES {
        let left = run(
            join_plan(
                l.clone(),
                r.clone(),
                Some(on_lk_eq_rid()),
                JoinType::Left,
                strategy,
            ),
            &db,
        );
        assert!(
            sorted(left.clone()).contains(&unmatched_left),
            "LEFT keeps unmatched left, {strategy:?}"
        );
        assert_eq!(left.len(), 3, "LEFT row count, {strategy:?}");

        let right = run(
            join_plan(
                l.clone(),
                r.clone(),
                Some(on_lk_eq_rid()),
                JoinType::Right,
                strategy,
            ),
            &db,
        );
        assert!(
            sorted(right.clone()).contains(&unmatched_right),
            "RIGHT keeps unmatched right, {strategy:?}"
        );
        assert_eq!(right.len(), 3, "RIGHT row count, {strategy:?}");

        let full = run(
            join_plan(
                l.clone(),
                r.clone(),
                Some(on_lk_eq_rid()),
                JoinType::Full,
                strategy,
            ),
            &db,
        );
        let full = sorted(full);
        assert!(full.contains(&unmatched_left) && full.contains(&unmatched_right));
        assert_eq!(full.len(), 4, "FULL row count, {strategy:?}"); // 2 matched + 2 unmatched
    }
}

#[test]
fn strategies_agree_including_cross_join() {
    let (_tmp, db) = common::temp_db();
    let (l, r) = build_tables(&db);

    for join_type in [
        JoinType::Inner,
        JoinType::Left,
        JoinType::Right,
        JoinType::Full,
    ] {
        let nlj = sorted(run(
            join_plan(
                l.clone(),
                r.clone(),
                Some(on_lk_eq_rid()),
                join_type,
                JoinStrategy::NestedLoop,
            ),
            &db,
        ));
        let bnlj = sorted(run(
            join_plan(
                l.clone(),
                r.clone(),
                Some(on_lk_eq_rid()),
                join_type,
                JoinStrategy::BlockNestedLoop,
            ),
            &db,
        ));
        assert_eq!(nlj, bnlj, "strategies disagree for {join_type:?}");
    }

    // Cross join (on: None) — 3 x 3 = 9 pairs, both strategies.
    for strategy in STRATEGIES {
        let rows = run(
            join_plan(l.clone(), r.clone(), None, JoinType::Inner, strategy),
            &db,
        );
        assert_eq!(rows.len(), 9, "cross join, {strategy:?}");
    }
}

#[test]
fn block_nested_loop_spans_multiple_blocks() {
    // More outer rows than one block (OUTER_BLOCK_TUPLES = 1024) so the
    // blocking path runs for real; results must still match nested-loop.
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("big").unwrap();
    let big = dataset
        .create_table(
            "outer",
            Schema {
                fields: vec![
                    Field::new("id", LogicalType::Int32),
                    Field::new("k", LogicalType::Int32),
                ],
            },
        )
        .unwrap();
    let small = dataset
        .create_table(
            "inner",
            Schema {
                fields: vec![
                    Field::new("id", LogicalType::Int32),
                    Field::new("label", LogicalType::Text),
                ],
            },
        )
        .unwrap();
    {
        let mut ctx = db.query_context();
        let mut w = ctx.table_mut(&big);
        for id in 0..2500i32 {
            // k cycles 0..5; only k in {1,2,3} match the inner.
            w.put(&row2(id, Value::Int32(id % 5))).unwrap();
        }
    }
    {
        let mut ctx = db.query_context();
        let mut w = ctx.table_mut(&small);
        for id in 1..=3i32 {
            w.put(&row2(id, Value::Text(format!("L{id}")))).unwrap();
        }
    }

    let nlj = sorted(run(
        join_plan(
            big.clone(),
            small.clone(),
            Some(on_lk_eq_rid()),
            JoinType::Inner,
            JoinStrategy::NestedLoop,
        ),
        &db,
    ));
    let bnlj = sorted(run(
        join_plan(
            big,
            small,
            Some(on_lk_eq_rid()),
            JoinType::Inner,
            JoinStrategy::BlockNestedLoop,
        ),
        &db,
    ));
    assert_eq!(nlj, bnlj);
    // 2500 outer rows, 3/5 of them match exactly one inner row.
    assert_eq!(bnlj.len(), 1500);
}

// --- sort-merge join (inner equi-joins; needs pre-sorted inputs) -----------

fn skey(column: usize) -> SortKey {
    SortKey {
        expr: Expr::column(column),
        ascending: true,
        nulls_first: false,
    }
}

/// A sort-merge join plan with `Sort` enforcers on the join keys — what the
/// optimizer builds for an inner equi-join. `left_key`/`right_key` are the
/// per-side key column positions.
fn sort_merge_plan(
    l: Table,
    r: Table,
    on: Expr,
    left_key: usize,
    right_key: usize,
) -> PhysicalPlan {
    let left_schema = l.schema().clone();
    let right_schema = r.schema().clone();
    let left = PlanNode::Sort {
        input: Box::new(PlanNode::SeqScan { table: l }),
        keys: vec![skey(left_key)],
        input_schema: left_schema.clone(),
    };
    let right = PlanNode::Sort {
        input: Box::new(PlanNode::SeqScan { table: r }),
        keys: vec![skey(right_key)],
        input_schema: right_schema.clone(),
    };
    PhysicalPlan::new(PlanNode::Join {
        left: Box::new(left),
        right: Box::new(right),
        on: Some(on),
        join_type: JoinType::Inner,
        left_schema,
        right_schema,
        strategy: JoinStrategy::SortMerge,
    })
}

#[test]
fn sort_merge_matches_nested_loop_inner() {
    let (_tmp, db) = common::temp_db();
    let (l, r) = build_tables(&db);
    // l.k (col 1) = r.id (col 0).
    let smj = sorted(run(
        sort_merge_plan(l.clone(), r.clone(), on_lk_eq_rid(), 1, 0),
        &db,
    ));
    let nlj = sorted(run(
        join_plan(
            l,
            r,
            Some(on_lk_eq_rid()),
            JoinType::Inner,
            JoinStrategy::NestedLoop,
        ),
        &db,
    ));
    assert_eq!(smj, nlj, "sort-merge must match the nested-loop reference");
    assert_eq!(smj.len(), 2);
}

#[test]
fn sort_merge_expands_duplicate_key_groups() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("dup").unwrap();
    let two_int = || Schema {
        fields: vec![
            Field::new("id", LogicalType::Int32),
            Field::new("k", LogicalType::Int32),
        ],
    };
    let l = dataset.create_table("l", two_int()).unwrap();
    let r = dataset.create_table("r", two_int()).unwrap();
    {
        let mut ctx = db.query_context();
        let mut w = ctx.table_mut(&l);
        for (id, k) in [(1, 5), (2, 5), (3, 9)] {
            w.put(&row2(id, Value::Int32(k))).unwrap();
        }
    }
    {
        let mut ctx = db.query_context();
        let mut w = ctx.table_mut(&r);
        for (id, k) in [(10, 5), (11, 5), (12, 7)] {
            w.put(&row2(id, Value::Int32(k))).unwrap();
        }
    }
    // l.k (col 1) = r.k (combined col 3 → right-local col 1).
    let on = Expr::Binary {
        op: BinaryOperator::Eq,
        lhs: Box::new(Expr::column(1)),
        rhs: Box::new(Expr::column(3)),
    };
    let rows = run(sort_merge_plan(l, r, on, 1, 1), &db);
    // The k=5 group is 2 left × 2 right; k=9 / k=7 don't match.
    assert_eq!(rows.len(), 4);
}
