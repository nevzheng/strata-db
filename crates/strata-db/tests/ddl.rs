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

// --- CREATE SCHEMA + default namespace -------------------------------------
//
// CREATE SCHEMA / TABLE behavior (creating + using schemas, IF NOT EXISTS,
// the rejected forms, OR REPLACE, CREATE TABLE error paths) is covered at
// the SQL boundary by `ddl/schema.slt` and `ddl/create_replace.slt`. Only
// the catalog-seeding check, which has no SQL to drive it, lives here.

#[test]
fn default_namespace_is_seeded_on_open() {
    let (_tmp, db) = common::temp_db();
    // `strata.public` exists out of the box, no DDL needed.
    assert!(db.project("strata").unwrap().dataset("public").is_ok());
}

// --- INSERT ----------------------------------------------------------------

/// Create a table in the seeded `strata.public` namespace via SQL.
fn create_public_table(db: &Db, ddl: &str) {
    exec(db, ddl).unwrap();
}

fn rows_of(results: &[StmtResult]) -> Vec<Tuple> {
    match &results[0] {
        StmtResult::Rows(r) => r.clone(),
        StmtResult::Affected(_) => panic!("expected rows"),
    }
}

// INSERT round-trips, predicate filtering, NULL handling, and range
// checks are covered at the SQL boundary by `insert.slt` and
// `ddl/create_replace.slt`. The one case kept here asserts the *stored
// integer width* (Int16 vs Int64) after coercion — invisible in the text
// result a spec test sees, so it stays as a Rust round-trip.
#[test]
fn insert_coerces_widening_and_narrowing_within_range() {
    let (_tmp, db) = common::temp_db();
    create_public_table(&db, "CREATE TABLE strata.public.t (a SMALLINT, b BIGINT)");

    // 30000 fits SMALLINT (Int16); 30000 widens trivially into BIGINT.
    exec(&db, "INSERT INTO strata.public.t VALUES (30000, 30000)").unwrap();
    let rows = rows_of(&exec(&db, "SELECT * FROM strata.public.t").unwrap());
    assert_eq!(rows[0].values[0], Value::Int16(30000));
    assert_eq!(rows[0].values[1], Value::Int64(30000));
}
