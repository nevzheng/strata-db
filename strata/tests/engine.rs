use strata::memstore::BTreeMapStore;
use strata::{LevelConfig, StorageEngine};

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

/// A small max_sst_size splits runs into multiple SSTable files.
#[test]
fn max_sst_size_splits_sstables() {
    let tmp = tempfile::tempdir().unwrap();
    // Tiny max_sst_size (64 bytes) forces multiple SST files per run.
    let mut engine = StorageEngine::with_levels(
        tmp.path(),
        BTreeMapStore::with_capacity(64),
        vec![LevelConfig {
            max_runs: 64,
            max_run_size_bytes: usize::MAX,
        }],
        64,
    )
    .unwrap();

    for i in 0..50u32 {
        engine
            .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
            .unwrap();
    }

    // Multiple SST files should exist due to the small max_sst_size.
    // Each entry is ~22 bytes (2 + 6 key + 8 seq + 1 op + 2 + 3 value).
    // At 64 bytes max data per file, ~2-3 entries fit before splitting.
    // Footer is appended after data, so file on disk is slightly larger.
    let sst_dir = tmp.path().join("sst");
    let mut sst_sizes: Vec<u64> = std::fs::read_dir(&sst_dir)
        .unwrap()
        .filter_map(|e| {
            let e = e.unwrap();
            if e.path().extension().is_some_and(|ext| ext == "sst") {
                Some(e.metadata().unwrap().len())
            } else {
                None
            }
        })
        .collect();
    sst_sizes.sort();
    assert!(
        sst_sizes.len() > 1,
        "expected multiple SST files with small max_sst_size, got {}",
        sst_sizes.len()
    );
    // Each file's data section should be at most max_sst_size (64).
    // Total file = data + footer. Footer is ~20-30 bytes for short keys.
    for size in &sst_sizes {
        assert!(
            *size <= 120,
            "SST file too large ({size} bytes), expected data ≤64 + footer"
        );
    }

    // All data should still be readable.
    for i in 0..50u32 {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            Some(format!("v:{i}").into_bytes()),
        );
    }
}

/// Helper: 3-level engine (L0: 2 runs, L1: 2 runs, L2: 1 run) with a tiny memtable.
fn three_level_engine(dir: &std::path::Path) -> StorageEngine<BTreeMapStore> {
    StorageEngine::with_levels(
        dir,
        BTreeMapStore::with_capacity(32),
        vec![
            LevelConfig {
                max_runs: 2,
                max_run_size_bytes: usize::MAX,
            },
            LevelConfig {
                max_runs: 2,
                max_run_size_bytes: usize::MAX,
            },
            LevelConfig {
                max_runs: 1,
                max_run_size_bytes: usize::MAX,
            },
        ],
        usize::MAX,
    )
    .unwrap()
}

#[test]
fn compaction_fills_l0_then_cascades_to_l1() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = three_level_engine(tmp.path());

    // Write enough to trigger multiple memtable compactions.
    // L0 max_runs=2, so once it fills it cascades to L1.
    for i in 0..20u32 {
        engine.put(format!("k:{i:04}").as_bytes(), b"v").unwrap();
    }

    // L0 should have been drained at least once into L1.
    assert!(
        !engine.level_is_empty(1),
        "L1 should have runs from L0 cascade"
    );
    // L0 should not have grown unbounded (max_runs=2).
    assert!(
        engine.level_run_count(0) <= 2,
        "L0 should stay bounded, got {} runs",
        engine.level_run_count(0)
    );

    for i in 0..20u32 {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            Some(b"v".to_vec()),
            "missing k:{i:04}"
        );
    }
}

#[test]
fn compaction_cascades_from_l1_to_l2() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = three_level_engine(tmp.path());

    // 4 memtable compactions → 2 L0→L1 cascades → L1 full → cascades to L2.
    for i in 0..40u32 {
        engine.put(format!("k:{i:04}").as_bytes(), b"v").unwrap();
    }

    assert!(
        !engine.level_is_empty(2),
        "L2 should have data after L1 cascade"
    );

    for i in 0..40u32 {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            Some(b"v".to_vec()),
            "missing k:{i:04}"
        );
    }
}

#[test]
fn last_level_merges_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = three_level_engine(tmp.path());

    // Write enough to cascade to L2 multiple times.
    for round in 0..3u32 {
        for i in 0..40u32 {
            engine
                .put(
                    format!("k:{i:04}").as_bytes(),
                    format!("r{round}").as_bytes(),
                )
                .unwrap();
        }
    }

    // L2 (max_runs=1) should merge in place, staying compact.
    assert!(
        engine.level_run_count(2) <= 2,
        "L2 should stay compact, got {} runs",
        engine.level_run_count(2)
    );

    // Latest values should win.
    for i in 0..40u32 {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            Some(b"r2".to_vec()),
            "k:{i:04} should have latest value"
        );
    }
}

#[test]
fn compaction_shrinks_levels() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = three_level_engine(tmp.path());

    let mut l0_peaked = 0;
    let mut l0_shrank = false;
    let mut l1_peaked = 0;
    let mut l1_shrank = false;

    for i in 0..500u32 {
        engine.put(format!("k:{i:04}").as_bytes(), b"v").unwrap();

        let l0 = engine.level_run_count(0);
        if l0 > l0_peaked {
            l0_peaked = l0;
        }
        if l0 < l0_peaked && l0_peaked >= 2 {
            l0_shrank = true;
        }

        let l1 = engine.level_run_count(1);
        if l1 > l1_peaked {
            l1_peaked = l1;
        }
        if l1 < l1_peaked && l1_peaked >= 2 {
            l1_shrank = true;
        }

        if l0_shrank && l1_shrank {
            break;
        }
    }

    assert!(
        l0_shrank,
        "L0 should have shrunk after cascading to L1 (peaked at {l0_peaked})"
    );
    assert!(
        l1_shrank,
        "L1 should have shrunk after cascading to L2 (peaked at {l1_peaked})"
    );
}

/// get_at returns the version visible at a given sequence number.
#[test]
fn get_at_returns_version_at_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();

    engine.put(b"key", b"v1").unwrap(); // seq 1
    engine.put(b"key", b"v2").unwrap(); // seq 2
    engine.put(b"key", b"v3").unwrap(); // seq 3

    assert_eq!(engine.get_at(b"key", 1).unwrap(), Some(b"v1".to_vec()));
    assert_eq!(engine.get_at(b"key", 2).unwrap(), Some(b"v2".to_vec()));
    assert_eq!(engine.get_at(b"key", 3).unwrap(), Some(b"v3".to_vec()));
    assert_eq!(engine.get_at(b"key", 0).unwrap(), None);
}

/// get_at respects tombstones at the given sequence number.
#[test]
fn get_at_respects_tombstones() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();

    engine.put(b"key", b"val").unwrap(); // seq 1
    engine.delete(b"key").unwrap(); // seq 2
    engine.put(b"key", b"revived").unwrap(); // seq 3

    assert_eq!(engine.get_at(b"key", 1).unwrap(), Some(b"val".to_vec()));
    assert_eq!(engine.get_at(b"key", 2).unwrap(), None);
    assert_eq!(engine.get_at(b"key", 3).unwrap(), Some(b"revived".to_vec()));
}

/// get_at works across compaction boundaries.
#[test]
fn get_at_across_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();

    engine.put(b"key", b"v1").unwrap(); // seq 1

    // Trigger compaction so "key" lands in L0.
    for i in 0..20u32 {
        engine
            .put(format!("z:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
            .unwrap();
    }

    // Overwrite in the fresh memtable.
    let seq_before = engine.seq();
    engine.put(b"key", b"v2").unwrap();

    // Should see old value at seq 1, new value at current seq.
    assert_eq!(engine.get_at(b"key", 1).unwrap(), Some(b"v1".to_vec()));
    assert_eq!(
        engine.get_at(b"key", seq_before).unwrap(),
        Some(b"v1".to_vec())
    );
    assert_eq!(
        engine.get_at(b"key", engine.seq()).unwrap(),
        Some(b"v2".to_vec())
    );
}

/// Data compacted to SSTables survives engine reopen via manifest reconstruction.
#[test]
fn data_survives_reopen_after_compaction() {
    let tmp = tempfile::tempdir().unwrap();

    {
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();
        for i in 0..100u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }
        // Verify compaction happened.
        assert!(!engine.level_is_empty(0), "L0 should have runs");
    }

    // Reopen — levels reconstructed from manifest.
    let engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();
    assert!(
        !engine.level_is_empty(0),
        "L0 should be restored from manifest"
    );

    for i in 0..100u32 {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            Some(format!("v:{i}").into_bytes()),
            "missing k:{i:04} after reopen"
        );
    }

    let results = engine
        .scan(b"k:0000".to_vec()..=b"k:0099".to_vec())
        .unwrap();
    assert_eq!(results.len(), 100);
}

/// Data cascaded to deeper levels survives reopen.
#[test]
fn data_survives_reopen_after_cascade() {
    let tmp = tempfile::tempdir().unwrap();

    {
        let mut engine = three_level_engine(tmp.path());
        for i in 0..40u32 {
            engine.put(format!("k:{i:04}").as_bytes(), b"v").unwrap();
        }
        assert!(!engine.level_is_empty(1), "L1 should have data");
    }

    // Reopen with same level config.
    let engine = three_level_engine(tmp.path());
    for i in 0..40u32 {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            Some(b"v".to_vec()),
            "missing k:{i:04} after reopen"
        );
    }
}

/// Deletes that were compacted to SSTables persist across reopen.
#[test]
fn deleted_data_stays_deleted_after_reopen() {
    let tmp = tempfile::tempdir().unwrap();

    {
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();
        for i in 0..50u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }
        // Delete even keys.
        for i in (0..50u32).step_by(2) {
            engine.delete(format!("k:{i:04}").as_bytes()).unwrap();
        }
    }

    let engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();
    for i in (0..50u32).step_by(2) {
        assert_eq!(
            engine.get(format!("k:{i:04}").as_bytes()).unwrap(),
            None,
            "k:{i:04} should still be deleted after reopen"
        );
    }
    for i in (1..50u32).step_by(2) {
        assert!(
            engine
                .get(format!("k:{i:04}").as_bytes())
                .unwrap()
                .is_some(),
            "k:{i:04} should still exist after reopen"
        );
    }
}

/// Sequence number is recovered from manifest after compaction + WAL truncate.
#[test]
fn seq_recovered_from_manifest_after_compaction() {
    let tmp = tempfile::tempdir().unwrap();

    let seq_before_reopen;
    {
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();
        for i in 0..50u32 {
            engine
                .put(format!("k:{i:04}").as_bytes(), format!("v:{i}").as_bytes())
                .unwrap();
        }
        seq_before_reopen = engine.seq();
        assert!(seq_before_reopen >= 50);
    }

    // Reopen — seq should be at least as high as before.
    let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::with_capacity(64)).unwrap();
    assert!(
        engine.seq() >= seq_before_reopen,
        "seq after reopen ({}) should be >= seq before ({})",
        engine.seq(),
        seq_before_reopen
    );

    // New writes should get higher seq numbers and not collide.
    engine.put(b"new_key", b"new_val").unwrap();
    assert!(engine.seq() > seq_before_reopen);
    assert_eq!(engine.get(b"new_key").unwrap(), Some(b"new_val".to_vec()));
}
