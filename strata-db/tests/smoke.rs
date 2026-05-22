//! End-to-end smoke tests for the public `strata-db` API.

mod common;

use strata_db::{
    CatalogError, Db, Field, LevelConfig, LogicalType, ResourceKind, Schema, Tuple, TypedStore,
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
    let schema = Schema {
        fields: vec![Field::new("v", LogicalType::Int64)],
    };
    let table = dataset.create_table("events", schema).unwrap();

    let row = Tuple {
        values: vec![Value::Int64(1)],
    };
    table.put(b"k", &row).unwrap();
    let got = table.get(b"k").unwrap();
    assert_eq!(got, Some(row));
}

#[test]
fn delete_removes_row() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let schema = Schema {
        fields: vec![Field::new("v", LogicalType::Int64)],
    };
    let table = dataset.create_table("events", schema).unwrap();

    table
        .put(
            b"k",
            &Tuple {
                values: vec![Value::Int64(1)],
            },
        )
        .unwrap();
    table.delete(b"k").unwrap();
    assert_eq!(table.get(b"k").unwrap(), None);
}

#[test]
fn create_project_twice_is_already_exists() {
    let (_tmp, db) = common::temp_db();
    db.create_project("acme").unwrap();
    let result = db.create_project("acme");
    assert!(matches!(
        result,
        Err(CatalogError::AlreadyExists {
            kind: ResourceKind::Project,
            ..
        })
    ));
}

#[test]
fn drop_missing_project_is_not_found() {
    let (_tmp, db) = common::temp_db();
    let result = db.drop_project("nope");
    assert!(matches!(
        result,
        Err(CatalogError::NotFound {
            kind: ResourceKind::Project,
            ..
        })
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
            fields: vec![Field::new("v", LogicalType::Int64)],
        };
        let table = dataset.create_table("events", schema).unwrap();
        table
            .put(
                b"k",
                &Tuple {
                    values: vec![Value::Int64(1)],
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
    assert_eq!(
        table.get(b"k").unwrap(),
        Some(Tuple {
            values: vec![Value::Int64(1)]
        })
    );
}

#[test]
fn table_scan_returns_decoded_tuples() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let schema = Schema {
        fields: vec![Field::new("v", LogicalType::Int64)],
    };
    let table = dataset.create_table("events", schema).unwrap();

    for (k, v) in [("a:1", 1i64), ("a:2", 2), ("b:1", 3)] {
        table
            .put(
                k.as_bytes(),
                &Tuple {
                    values: vec![Value::Int64(v)],
                },
            )
            .unwrap();
    }

    // Empty prefix → every row, with the table prefix stripped from each key.
    let mut all = table.scan(&[]).unwrap();
    all.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].0, b"a:1");
    assert_eq!(
        all[0].1,
        Tuple {
            values: vec![Value::Int64(1)]
        }
    );
    assert_eq!(all[1].0, b"a:2");
    assert_eq!(all[2].0, b"b:1");

    // Prefix filter narrows the scan.
    let a_rows = table.scan(b"a:").unwrap();
    assert_eq!(a_rows.len(), 2);
    for (k, _) in &a_rows {
        assert!(k.starts_with(b"a:"));
    }

    // Prefix matching nothing returns an empty result.
    let none = table.scan(b"z:").unwrap();
    assert!(none.is_empty());
}

#[test]
fn rows_survive_forced_level_compaction() {
    // Tiny memtable + tight L0 forces writes to flush into L0 and then
    // cascade into L1. The public API should hide that — every row we
    // wrote must still be readable through `table.get`.
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
        fields: vec![Field::new("i", LogicalType::Int64)],
    };
    let table = db
        .create_project("acme")
        .unwrap()
        .create_dataset("metrics")
        .unwrap()
        .create_table("events", schema)
        .unwrap();

    for i in 0..50i64 {
        table
            .put(
                format!("k:{i:04}").as_bytes(),
                &Tuple {
                    values: vec![Value::Int64(i)],
                },
            )
            .unwrap();
    }

    for i in 0..50i64 {
        let got = table.get(format!("k:{i:04}").as_bytes()).unwrap();
        assert_eq!(
            got,
            Some(Tuple {
                values: vec![Value::Int64(i)]
            }),
            "missing row {i}"
        );
    }
}
