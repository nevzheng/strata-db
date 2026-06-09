//! Integration tests over the new `lsm`-backed engine: put / get / delete /
//! scan, manual flush into on-disk L0, and recovery across reopen.
//!
//! Note: compaction is still manual (`flush` is explicit); there's no automatic
//! compaction yet.

use strata_store::memstore::BTreeMapStore;
use strata_store::{KVPair, StorageEngine};

fn engine(tmp: &tempfile::TempDir) -> StorageEngine<BTreeMapStore> {
    StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap()
}

#[test]
fn put_get_delete_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine(&tmp);

    engine.put(b"user:alice", b"admin").unwrap();
    assert_eq!(engine.get(b"user:alice").unwrap(), Some(b"admin".to_vec()));

    engine.delete(b"user:alice").unwrap();
    assert_eq!(engine.get(b"user:alice").unwrap(), None);
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

    let results: Vec<KVPair> = engine.scan(..).collect::<Result<_, _>>().unwrap();
    assert_eq!(
        results,
        vec![
            (b"k:a".to_vec(), b"1b".to_vec()), // memtable wins over L0
            (b"k:b".to_vec(), b"2".to_vec()),
            (b"k:c".to_vec(), b"3".to_vec()),
        ]
    );
}

/// Write many keys with periodic flushes (several L0 runs), then read every
/// one back — exercising the memtable + multi-run merge on the read path.
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
        let val = engine.get(format!("k:{i:06}").as_bytes()).unwrap();
        assert_eq!(val, Some(format!("v:{i}").into_bytes()), "missing k:{i:06}");
    }
}

#[test]
fn data_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
        engine.put(b"config:theme", b"dark").unwrap();
        engine.put(b"config:lang", b"en").unwrap();
        engine.flush().unwrap(); // flushed to L0 + manifest
        engine.put(b"config:lang", b"fr").unwrap(); // unflushed override
        engine.delete(b"config:theme").unwrap(); // unflushed tombstone
    }
    // Reopen: manifest rebuilds L0, the journal replays the unflushed tail.
    let engine = StorageEngine::new(tmp.path(), BTreeMapStore::new()).unwrap();
    assert_eq!(engine.get(b"config:theme").unwrap(), None);
    assert_eq!(engine.get(b"config:lang").unwrap(), Some(b"fr".to_vec()));
}

#[test]
fn delete_after_flush_is_hidden() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = engine(&tmp);

    engine.put(b"k", b"v").unwrap();
    engine.flush().unwrap();
    engine.delete(b"k").unwrap();
    assert_eq!(engine.get(b"k").unwrap(), None);

    // The tombstone is still authoritative after it too is flushed.
    engine.flush().unwrap();
    assert_eq!(engine.get(b"k").unwrap(), None);
}
