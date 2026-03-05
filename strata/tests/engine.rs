use strata::StorageEngine;
use strata::memstore::BTreeMapStore;

#[test]
fn put_get_delete_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();

    engine.put(b"user:alice", b"admin").unwrap();
    assert_eq!(engine.get(b"user:alice").unwrap(), Some(b"admin".to_vec()));

    engine.delete(b"user:alice").unwrap();
    assert_eq!(engine.get(b"user:alice").unwrap(), None);
}

#[test]
fn data_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();

    {
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        engine.put(b"config:theme", b"dark").unwrap();
        engine.put(b"config:lang", b"en").unwrap();
        engine.delete(b"config:lang").unwrap();
    }

    let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
    assert_eq!(engine.get(b"config:theme").unwrap(), Some(b"dark".to_vec()));
    assert_eq!(engine.get(b"config:lang").unwrap(), None);
}

/// Write enough data to trigger many compactions, filling L0 with many runs.
/// Every key should remain readable via get.
#[test]
fn heavy_writes_fill_l0_all_readable() {
    let tmp = tempfile::tempdir().unwrap();
    // Small memtable forces frequent compaction. 256 bytes fits ~20 entries
    // before compacting, so 500 keys => ~25 runs in L0 (well under max 64).
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(256)).unwrap();

    let n = 500;
    for i in 0..n {
        engine
            .put(format!("k:{i:06}").as_bytes(), format!("v:{i}").as_bytes())
            .unwrap();
    }

    // Every key should be readable (from memtable or L0 runs).
    for i in 0..n {
        let val = engine.get(format!("k:{i:06}").as_bytes()).unwrap();
        assert_eq!(val, Some(format!("v:{i}").into_bytes()), "missing k:{i:06}");
    }
}

/// Scan should merge entries from memtable and all L0 runs, returning
/// a consistent sorted view.
#[test]
fn scan_merges_across_compactions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();

    let n = 200;
    for i in 0..n {
        engine
            .put(format!("k:{i:06}").as_bytes(), format!("v:{i}").as_bytes())
            .unwrap();
    }

    let results = engine
        .scan(b"k:000000".to_vec()..=b"k:999999".to_vec())
        .unwrap();

    // Should have exactly n unique keys, sorted.
    assert_eq!(results.len(), n);
    for i in 0..n {
        assert_eq!(results[i].0, format!("k:{i:06}").into_bytes());
        assert_eq!(results[i].1, format!("v:{i}").into_bytes());
    }
}

/// Overwrite the same keys across multiple compaction cycles.
/// Get and scan should resolve to the latest version regardless of
/// which L0 run (or memtable) holds it.
#[test]
fn version_resolution_across_l0_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();

    // Round 1: write keys with "old" values, trigger compaction.
    for i in 0..50u32 {
        engine.put(format!("k:{i:04}").as_bytes(), b"old").unwrap();
    }

    // Round 2: overwrite same keys with "new" values in later compactions.
    for i in 0..50u32 {
        engine.put(format!("k:{i:04}").as_bytes(), b"new").unwrap();
    }

    // Get should return the latest version for every key.
    for i in 0..50u32 {
        let val = engine.get(format!("k:{i:04}").as_bytes()).unwrap();
        assert_eq!(val, Some(b"new".to_vec()), "k:{i:04} not updated");
    }

    // Scan should also resolve to latest versions only.
    let results = engine
        .scan(b"k:0000".to_vec()..=b"k:0049".to_vec())
        .unwrap();
    assert_eq!(results.len(), 50);
    for (_, val) in &results {
        assert_eq!(val, b"new");
    }
}

/// Delete a key that was compacted to L0, then verify it's gone from
/// both get and scan.
#[test]
fn delete_shadows_compacted_data() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();

    // Write keys, trigger compaction so they land in L0.
    for i in 0..50u32 {
        engine
            .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
            .unwrap();
    }

    // Delete every other key (tombstones live in memtable or later L0 run).
    for i in (0..50u32).step_by(2) {
        engine.delete(format!("k:{i:04}").as_bytes()).unwrap();
    }

    // Deleted keys should be gone.
    for i in (0..50u32).step_by(2) {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            None,
            "k:{i:04} should be deleted"
        );
    }

    // Remaining keys should still be present.
    for i in (1..50u32).step_by(2) {
        assert!(
            engine
                .get(format!("k:{i:04}").as_bytes())
                .unwrap()
                .is_some(),
            "k:{i:04} should exist"
        );
    }

    // Scan should only return the surviving keys.
    let results = engine
        .scan(b"k:0000".to_vec()..=b"k:0049".to_vec())
        .unwrap();
    assert_eq!(results.len(), 25);
}
