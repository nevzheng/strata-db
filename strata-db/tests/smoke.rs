//! End-to-end smoke tests for the public `strata-db` API.
//!
//! Most tests here are `#[ignore]`'d until the catalog bodies are filled in.
//! They double as targets for the implementation work — unignore each one
//! when its code path stops panicking with `todo!()`.

mod common;

use serde_json::json;
use strata_db::{CatalogError, Field, FieldType, ResourceKind, Schema};

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
    let schema = Schema::new(vec![Field::new("count", FieldType::Integer)]);
    let table = dataset.create_table("events", schema).unwrap();
    assert_eq!(table.name(), "events");
    assert_eq!(table.schema().fields.len(), 1);
}

#[test]
fn put_and_get_row() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let table = dataset.create_table("events", Schema::empty()).unwrap();

    table.put(b"k", json!({"v": 1})).unwrap();
    let got = table.get(b"k").unwrap();
    assert_eq!(got, Some(json!({"v": 1})));
}

#[test]
fn delete_removes_row() {
    let (_tmp, db) = common::temp_db();
    let project = db.create_project("acme").unwrap();
    let dataset = project.create_dataset("metrics").unwrap();
    let table = dataset.create_table("events", Schema::empty()).unwrap();

    table.put(b"k", json!({"v": 1})).unwrap();
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
        let table = dataset.create_table("events", Schema::empty()).unwrap();
        table.put(b"k", json!({"v": 1})).unwrap();
    }

    let db = strata_db::Db::open(tmp.path()).unwrap();
    let table = db
        .project("acme")
        .unwrap()
        .dataset("metrics")
        .unwrap()
        .table("events")
        .unwrap();
    assert_eq!(table.get(b"k").unwrap(), Some(json!({"v": 1})));
}
