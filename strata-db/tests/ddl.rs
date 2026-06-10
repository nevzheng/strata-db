//! End-to-end tests for DDL.
//!
//! Drives `CREATE TABLE` SQL through the full `Planner` + `Volcano`
//! pipeline against a real `Db`, then asserts the table landed in the
//! catalog with the right schema and that rows round-trip through it.

mod common;

use strata_db::query::Planner;
use strata_db::query::executor::{ExecuteResult, Executor};
use strata_db::query::volcano::Volcano;
use strata_db::{Db, LogicalType, QueryError, Tuple, Value};

/// What one statement produced: streamed rows or an affected-row count.
#[derive(Debug)]
enum StmtResult {
    Rows(Vec<Tuple>),
    Affected(u64),
}

/// Plan and execute `sql` end-to-end, returning one result per statement.
fn exec(db: &Db, sql: &str) -> Result<Vec<StmtResult>, QueryError> {
    let mut ctx = db.query_context();
    let planner = Planner::builder().build().unwrap();
    let pq = planner.plan(sql, &ctx)?;
    let mut out = Vec::with_capacity(pq.physical.len());
    for plan in pq.physical {
        match Volcano.execute(plan, &mut ctx)? {
            ExecuteResult::Rows(stream) => {
                out.push(StmtResult::Rows(stream.collect::<Result<_, _>>()?));
            }
            ExecuteResult::Affected(n) => out.push(StmtResult::Affected(n)),
        }
    }
    Ok(out)
}

fn setup_dataset(db: &Db) {
    let project = db.create_project("acme").unwrap();
    project.create_dataset("metrics").unwrap();
}

#[test]
fn create_table_persists_schema_and_round_trips() {
    let (_tmp, db) = common::temp_db();
    setup_dataset(&db);

    let results = exec(
        &db,
        "CREATE TABLE acme.metrics.events (id INT, name TEXT NOT NULL)",
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0], StmtResult::Affected(0)));

    // The table is now resolvable through the catalog, with the schema
    // we declared.
    let table = db
        .project("acme")
        .unwrap()
        .dataset("metrics")
        .unwrap()
        .table("events")
        .unwrap();
    let fields = &table.schema().fields;
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name.as_str(), "id");
    assert!(matches!(fields[0].ty, LogicalType::Int32));
    assert!(fields[0].nullable, "id has no NOT NULL");
    assert_eq!(fields[1].name.as_str(), "name");
    assert!(matches!(fields[1].ty, LogicalType::Text));
    assert!(!fields[1].nullable, "name is NOT NULL");

    // And the schema actually drives encode/decode: write a row, read
    // it back through a fresh scan.
    {
        let mut ctx = db.query_context();
        ctx.table_mut(&table)
            .put(&Tuple {
                values: vec![Value::Int32(7), Value::Text("alpha".into())],
            })
            .unwrap();
    }
    let rows = match &exec(&db, "SELECT * FROM acme.metrics.events").unwrap()[0] {
        StmtResult::Rows(r) => r.clone(),
        StmtResult::Affected(_) => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int32(7));
    assert_eq!(rows[0].values[1], Value::Text("alpha".into()));
}

#[test]
fn create_or_replace_swaps_schema_and_drops_old_data() {
    let (_tmp, db) = common::temp_db();
    setup_dataset(&db);

    // Create v0 and write a row into it.
    exec(&db, "CREATE TABLE acme.metrics.events (id INT, name TEXT)").unwrap();
    {
        let table = db
            .project("acme")
            .unwrap()
            .dataset("metrics")
            .unwrap()
            .table("events")
            .unwrap();
        let mut ctx = db.query_context();
        ctx.table_mut(&table)
            .put(&Tuple {
                values: vec![Value::Int32(1), Value::Text("old".into())],
            })
            .unwrap();
    }

    // Replace with a different schema.
    exec(
        &db,
        "CREATE OR REPLACE TABLE acme.metrics.events (id BIGINT, label TEXT, flag BOOLEAN)",
    )
    .unwrap();

    let table = db
        .project("acme")
        .unwrap()
        .dataset("metrics")
        .unwrap()
        .table("events")
        .unwrap();
    // New incarnation: new schema...
    let fields = &table.schema().fields;
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[1].name.as_str(), "label");
    // ...and the old row is gone — the new incarnation starts empty.
    let rows = match &exec(&db, "SELECT * FROM acme.metrics.events").unwrap()[0] {
        StmtResult::Rows(r) => r.clone(),
        StmtResult::Affected(_) => panic!("expected rows"),
    };
    assert!(
        rows.is_empty(),
        "replaced table should read empty, got {rows:?}"
    );
}

#[test]
fn create_or_replace_on_missing_table_acts_like_create() {
    let (_tmp, db) = common::temp_db();
    setup_dataset(&db);

    // No prior table — OR REPLACE should just create it.
    let results = exec(&db, "CREATE OR REPLACE TABLE acme.metrics.events (id INT)").unwrap();
    assert!(matches!(results[0], StmtResult::Affected(0)));
    assert!(
        db.project("acme")
            .unwrap()
            .dataset("metrics")
            .unwrap()
            .table("events")
            .is_ok()
    );
}

#[test]
fn create_table_in_missing_dataset_errors_catalog() {
    let (_tmp, db) = common::temp_db();
    db.create_project("acme").unwrap();

    let err = exec(&db, "CREATE TABLE acme.nope.events (id INT)").unwrap_err();
    assert!(matches!(err, QueryError::Catalog(_)), "got {err:?}");
}

#[test]
fn duplicate_create_table_errors_already_exists() {
    let (_tmp, db) = common::temp_db();
    setup_dataset(&db);

    exec(&db, "CREATE TABLE acme.metrics.events (id INT)").unwrap();
    let err = exec(&db, "CREATE TABLE acme.metrics.events (id INT)").unwrap_err();
    assert!(
        matches!(
            err,
            QueryError::Catalog(strata_db::CatalogError::AlreadyExists { .. })
        ),
        "got {err:?}"
    );
}
