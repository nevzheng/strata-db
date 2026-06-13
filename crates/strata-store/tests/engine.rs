//! Integration tests over the `lsm`-index + filesystem-heap engine: put / get /
//! delete / scan, manual flush, and recovery across reopen.
//!
//! Note: compaction is still manual (`flush` is explicit); there's no automatic
//! compaction yet.

use strata_store::StorageEngine;
use strata_store::memstore::BTreeMapStore;

fn engine(tmp: &tempfile::TempDir) -> StorageEngine<BTreeMapStore> {
    StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap()
}

/// Materialize a point lookup's view into owned bytes.
fn get_bytes(engine: &StorageEngine<BTreeMapStore>, key: &[u8]) -> Option<Vec<u8>> {
    engine
        .get(key)
        .unwrap()
        .map(|view| view.bytes().expect("live tuple").to_vec())
}

/// Drain a full scan into owned (key, value) pairs.
fn scan_all(engine: &StorageEngine<BTreeMapStore>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut scan = engine.scan(..);
    let mut out = Vec::new();
    while let Some(row) = scan.next() {
        let row = row.unwrap();
        out.push((row.key.clone(), row.tuple.bytes().unwrap().to_vec()));
    }
    out
}

#[test]
fn put_get_delete_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine(&tmp);

    engine.put(b"user:alice", b"admin").unwrap();
    assert_eq!(get_bytes(&engine, b"user:alice"), Some(b"admin".to_vec()));

    engine.delete(b"user:alice").unwrap();
    assert_eq!(get_bytes(&engine, b"user:alice"), None);
}

#[test]
fn scan_returns_sorted_merged_view() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine(&tmp);

    engine.put(b"k:c", b"3").unwrap();
    engine.put(b"k:a", b"1").unwrap();
    engine.flush().unwrap(); // a, c land in L0
    engine.put(b"k:b", b"2").unwrap();
    engine.put(b"k:a", b"1b").unwrap(); // memtable overwrite of flushed a

    assert_eq!(
        scan_all(&engine),
        vec![
            (b"k:a".to_vec(), b"1b".to_vec()), // memtable wins over L0
            (b"k:b".to_vec(), b"2".to_vec()),
            (b"k:c".to_vec(), b"3".to_vec()),
        ]
    );
}

/// Write many keys with periodic flushes (several L0 runs), then read every one
/// back — exercising the memtable + multi-run merge and heap eviction.
#[test]
fn many_writes_with_periodic_flush_all_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine(&tmp);

    let n = 500;
    for i in 0..n {
        engine
            .put(format!("k:{i:06}").as_bytes(), format!("v:{i}").as_bytes())
            .unwrap();
        if i % 50 == 49 {
            engine.flush().unwrap();
        }
    }

    for i in 0..n {
        let val = get_bytes(&engine, format!("k:{i:06}").as_bytes());
        assert_eq!(val, Some(format!("v:{i}").into_bytes()), "missing k:{i:06}");
    }
}

/// Unflushed writes do not yet survive a crash: the index journals every put,
/// but the heap is durable only at `flush()`, so a reopen can find the index
/// pointing at heap pages that never reached disk. Cross-journal ordering (one
/// log / LSN protocol) is deferred — see the backlog.
#[test]
#[ignore = "cross-journal durability of unflushed writes is deferred (index durable per-put, heap durable per-flush)"]
fn data_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        engine.put(b"config:theme", b"dark").unwrap();
        engine.put(b"config:lang", b"en").unwrap();
        engine.flush().unwrap();
        engine.put(b"config:lang", b"fr").unwrap(); // unflushed override
        engine.delete(b"config:theme").unwrap(); // unflushed tombstone
    }
    let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
    assert_eq!(get_bytes(&engine, b"config:theme"), None);
    assert_eq!(get_bytes(&engine, b"config:lang"), Some(b"fr".to_vec()));
}

/// What *does* survive reopen today: everything committed by `flush()`.
#[test]
fn flushed_data_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        engine.put(b"config:theme", b"dark").unwrap();
        engine.put(b"config:lang", b"en").unwrap();
        engine.flush().unwrap();
    }
    let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
    assert_eq!(get_bytes(&engine, b"config:theme"), Some(b"dark".to_vec()));
    assert_eq!(get_bytes(&engine, b"config:lang"), Some(b"en".to_vec()));
}

#[test]
fn delete_after_flush_is_hidden() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine(&tmp);

    engine.put(b"k", b"v").unwrap();
    engine.flush().unwrap();
    engine.delete(b"k").unwrap();
    assert_eq!(get_bytes(&engine, b"k"), None);

    // The tombstone is still authoritative after it too is flushed.
    engine.flush().unwrap();
    assert_eq!(get_bytes(&engine, b"k"), None);
}
