//! End-to-end tests over the public tree API: configure a tree rooted at a
//! temp dir, then set / read / scan — including across a flush to disk.

use lsm::{Lsm, LsmConfig};
use tempfile::TempDir;

/// A tree configured by `config`, rooted at a fresh temp dir (kept alive by
/// the returned handle).
fn tree_with(config: LsmConfig) -> (TempDir, Lsm) {
    let tmp = tempfile::tempdir().unwrap();
    let lsm = Lsm::new(tmp.path(), config);
    (tmp, lsm)
}

fn tree() -> (TempDir, Lsm) {
    tree_with(LsmConfig::default())
}

#[test]
fn set_then_read_values() {
    let (_tmp, mut tree) = tree();
    tree.put(b"alice", b"admin").unwrap();
    tree.put(b"bob", b"editor").unwrap();

    assert_eq!(tree.get(b"alice").unwrap(), Some(b"admin".to_vec()));
    assert_eq!(tree.get(b"bob").unwrap(), Some(b"editor".to_vec()));
    assert_eq!(tree.get(b"carol").unwrap(), None);
}

#[test]
fn overwrite_returns_latest_version() {
    let (_tmp, mut tree) = tree();
    tree.put(b"k", b"v1").unwrap();
    tree.put(b"k", b"v2").unwrap();
    assert_eq!(tree.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn delete_hides_value() {
    let (_tmp, mut tree) = tree();
    tree.put(b"k", b"v").unwrap();
    tree.delete(b"k").unwrap();
    assert_eq!(tree.get(b"k").unwrap(), None);
}

#[test]
fn config_is_carried_on_the_tree() {
    let (_tmp, tree) = tree_with(LsmConfig::leveled(5));
    assert_eq!(tree.config().num_levels(), 5);
}

#[test]
fn scan_returns_sorted_resolved_pairs() {
    let (_tmp, mut tree) = tree();
    tree.put(b"b", b"2").unwrap();
    tree.put(b"a", b"1").unwrap();
    tree.put(b"a", b"1b").unwrap(); // overwrite a
    tree.put(b"c", b"3").unwrap();
    tree.delete(b"b").unwrap(); // delete b

    let got: Vec<(Vec<u8>, Vec<u8>)> = tree.scan(..).map(|r| r.unwrap()).collect();
    assert_eq!(
        got,
        vec![
            (b"a".to_vec(), b"1b".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ]
    );
}

#[test]
fn flush_persists_then_reads_from_disk() {
    let (_tmp, mut tree) = tree();
    tree.put(b"a", b"1").unwrap();
    tree.put(b"b", b"2").unwrap();
    tree.flush().unwrap(); // memtable -> L0 SSTable; memtable now empty

    // Reads now come from the on-disk level.
    assert_eq!(tree.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(tree.get(b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(tree.get(b"missing").unwrap(), None);
}

#[test]
fn reads_merge_memtable_over_on_disk_levels() {
    let (_tmp, mut tree) = tree();
    tree.put(b"a", b"1").unwrap();
    tree.put(b"b", b"2").unwrap();
    tree.flush().unwrap();

    // Newer writes live in the memtable and must win over the flushed L0.
    tree.put(b"c", b"3").unwrap();
    tree.put(b"a", b"1b").unwrap(); // overwrite the flushed `a`

    let got: Vec<(Vec<u8>, Vec<u8>)> = tree.scan(..).map(|r| r.unwrap()).collect();
    assert_eq!(
        got,
        vec![
            (b"a".to_vec(), b"1b".to_vec()), // memtable wins over L0
            (b"b".to_vec(), b"2".to_vec()),  // from L0
            (b"c".to_vec(), b"3".to_vec()),  // from memtable
        ]
    );
}

#[test]
fn delete_in_memtable_shadows_flushed_value() {
    let (_tmp, mut tree) = tree();
    tree.put(b"k", b"v").unwrap();
    tree.flush().unwrap();
    tree.delete(b"k").unwrap(); // tombstone in the memtable

    assert_eq!(tree.get(b"k").unwrap(), None);
}
