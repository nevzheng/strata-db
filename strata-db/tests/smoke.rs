//! End-to-end smoke tests for the public `strata-db` API.

mod common;

use strata_db::{
    CatalogError, Db, Field, LevelConfig, LogicalType, QueryError, ResourceKind, Schema, Tuple,
    Value,
};

#[test]
fn db_opens_from_empty_dir() {
    let (_tmp, _db) = common::temp_db();
}

#[test]
fn create_and_open_project() {
    let (_tmp, db) = common::temp_db();
    let created = db.create_project("acme").unwrap();
    let opened = db.project("acme").unwrap();
    assert_eq!(created.id(), opened.id());
    assert_eq!(opened.name(), "acme");
}

#[test]
fn create_dataset_and_table() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let schema = Schema {
        fields: vec![Field::new("count", LogicalType::Int64)],
    };
    let table = dataset.create_table("events", schema).unwrap();
    assert_eq!(table.name(), "events");
    assert_eq!(table.schema().fields.len(), 1);
}

#[test]
fn put_and_get_row() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    // Column 0 is the PK by convention.
    let schema = Schema {
        fields: vec![
            Field::new("id", LogicalType::Text),
            Field::new("v", LogicalType::Int64),
        ],
    };
    let table = dataset.create_table("events", schema).unwrap();

    let row = Tuple {
        values: vec![Value::Text("k".into()), Value::Int64(1)],
    };
    let mut ctx = db.query_context();
    ctx.put(&table, &row).unwrap();
    let got = ctx.get(&table, &Value::Text("k".into())).unwrap();
    assert_eq!(got, Some(row));
}

#[test]
fn delete_removes_row() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let schema = Schema {
        fields: vec![
            Field::new("id", LogicalType::Text),
            Field::new("v", LogicalType::Int64),
        ],
    };
    let table = dataset.create_table("events", schema).unwrap();

    let mut ctx = db.query_context();
    ctx.put(
        &table,
        &Tuple {
            values: vec![Value::Text("k".into()), Value::Int64(1)],
        },
    )
    .unwrap();
    ctx.delete(&table, &Value::Text("k".into())).unwrap();
    assert_eq!(ctx.get(&table, &Value::Text("k".into())).unwrap(), None);
}

#[test]
fn create_project_twice_is_already_exists() {
    let (_tmp, db) = common::temp_db();
    db.create_project("acme").unwrap();
    let result = db.create_project("acme");
    assert!(matches!(
        result,
        Err(QueryError::Catalog(CatalogError::AlreadyExists {
            kind: ResourceKind::Project,
            ..
        }))
    ));
}

#[test]
fn drop_missing_project_is_not_found() {
    let (_tmp, db) = common::temp_db();
    let result = db.drop_project("nope");
    assert!(matches!(
        result,
        Err(QueryError::Catalog(CatalogError::NotFound {
            kind: ResourceKind::Project,
            ..
        }))
    ));
}

#[test]
fn list_projects_returns_created_names() {
    let (_tmp, db) = common::temp_db();
    db.create_project("a").unwrap();
    db.create_project("b").unwrap();
    let mut names = db.list_projects().unwrap();
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn data_survives_reopen() {
    let tmp = tempfile::TempDir::new().unwrap();

    {
        let db = strata_db::Db::open(tmp.path()).unwrap();
        let project = db.create_project("acme").unwrap();
        let dataset = project.create_dataset("metrics").unwrap();
        let schema = Schema {
            fields: vec![
                Field::new("id", LogicalType::Text),
                Field::new("v", LogicalType::Int64),
            ],
        };
        let table = dataset.create_table("events", schema).unwrap();

        let mut ctx = db.query_context();
        ctx.put(
            &table,
            &Tuple {
                values: vec![Value::Text("k".into()), Value::Int64(1)],
            },
        )
        .unwrap();
    }

    let db = strata_db::Db::open(tmp.path()).unwrap();
    let table = db
        .project("acme")
        .unwrap()
        .dataset("metrics")
        .unwrap()
        .table("events")
        .unwrap();
    let ctx = db.query_context();
    assert_eq!(
        ctx.get(&table, &Value::Text("k".into())).unwrap(),
        Some(Tuple {
            values: vec![Value::Text("k".into()), Value::Int64(1)]
        })
    );
}

#[test]
fn scan_returns_inserted_rows() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let schema = Schema {
        fields: vec![
            Field::new("id", LogicalType::Text),
            Field::new("v", LogicalType::Int64),
        ],
    };
    let table = dataset.create_table("events", schema).unwrap();

    let mut ctx = db.query_context();
    for (k, v) in [("a:1", 1i64), ("a:2", 2), ("b:1", 3)] {
        ctx.put(
            &table,
            &Tuple {
                values: vec![Value::Text(k.into()), Value::Int64(v)],
            },
        )
        .unwrap();
    }
    drop(ctx);

    let ctx = db.query_context();
    let mut tuples: Vec<Tuple> = ctx.scan(&table).collect::<Result<_, _>>().unwrap();
    tuples.sort_by(|a, b| match (&a.values[0], &b.values[0]) {
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    assert_eq!(tuples.len(), 3);
    assert_eq!(tuples[0].values[0], Value::Text("a:1".into()));
    assert_eq!(tuples[0].values[1], Value::Int64(1));
    assert_eq!(tuples[1].values[0], Value::Text("a:2".into()));
    assert_eq!(tuples[2].values[0], Value::Text("b:1".into()));
}

#[test]
fn rows_survive_forced_level_compaction() {
    // Tiny memtable + tight L0 forces writes to flush into L0 and then
    // cascade into L1. The public API should hide that — every row we
    // wrote must still be readable through `ctx.get`.
    let tmp = tempfile::TempDir::new().unwrap();
    let db = Db::builder()
        .mem_capacity(128)
        .levels(vec![
            LevelConfig {
                max_runs: 2,
                max_run_size_bytes: 64 * 1024 * 1024,
            },
            LevelConfig {
                max_runs: 64,
                max_run_size_bytes: 256 * 1024 * 1024,
            },
        ])
        .open(tmp.path())
        .unwrap();

    let schema = Schema {
        fields: vec![
            Field::new("id", LogicalType::Text),
            Field::new("i", LogicalType::Int64),
        ],
    };
    let table = db
        .create_project("acme")
        .unwrap()
        .create_dataset("metrics")
        .unwrap()
        .create_table("events", schema)
        .unwrap();

    let mut ctx = db.query_context();
    for i in 0..50i64 {
        ctx.put(
            &table,
            &Tuple {
                values: vec![Value::Text(format!("k:{i:04}")), Value::Int64(i)],
            },
        )
        .unwrap();
    }
    drop(ctx);

    let ctx = db.query_context();
    for i in 0..50i64 {
        let got = ctx.get(&table, &Value::Text(format!("k:{i:04}"))).unwrap();
        assert_eq!(
            got,
            Some(Tuple {
                values: vec![Value::Text(format!("k:{i:04}")), Value::Int64(i)]
            }),
            "missing row {i}"
        );
    }
}
