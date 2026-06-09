//! End-to-end tests over the public tree API: build a config, build the
//! tree, then set and read values.

use lsm::{Lsm, LsmConfig};

#[test]
fn set_then_read_values() {
    let mut tree: Lsm = Lsm::new(LsmConfig::leveled(4));

    tree.put(b"alice", b"admin").unwrap();
    tree.put(b"bob", b"editor").unwrap();

    assert_eq!(tree.get(b"alice").unwrap(), Some(b"admin".to_vec()));
    assert_eq!(tree.get(b"bob").unwrap(), Some(b"editor".to_vec()));
    assert_eq!(tree.get(b"carol").unwrap(), None);
}

#[test]
fn overwrite_returns_latest_version() {
    let mut tree: Lsm = Lsm::new(LsmConfig::default());

    tree.put(b"k", b"v1").unwrap();
    tree.put(b"k", b"v2").unwrap();

    assert_eq!(tree.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn delete_hides_value() {
    let mut tree: Lsm = Lsm::new(LsmConfig::default());

    tree.put(b"k", b"v").unwrap();
    tree.delete(b"k").unwrap();

    assert_eq!(tree.get(b"k").unwrap(), None);
}

#[test]
fn config_is_carried_on_the_tree() {
    let tree: Lsm = Lsm::new(LsmConfig::leveled(5));
    assert_eq!(tree.config().num_levels(), 5);
}

#[test]
fn scan_returns_sorted_resolved_pairs() {
    let mut tree: Lsm = Lsm::new(LsmConfig::default());
    tree.put(b"b", b"2").unwrap();
    tree.put(b"a", b"1").unwrap();
    tree.put(b"a", b"1b").unwrap(); // overwrite a
    tree.put(b"c", b"3").unwrap();
    tree.delete(b"b").unwrap(); // delete b

    let got: Vec<(Vec<u8>, Vec<u8>)> = tree.scan(..).map(|r| r.unwrap()).collect();

    // sorted by key; `a` resolved to its newest value; `b` dropped (tombstone).
    assert_eq!(
        got,
        vec![
            (b"a".to_vec(), b"1b".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ]
    );
}

#[test]
fn scan_sub_range() {
    let mut tree: Lsm = Lsm::new(LsmConfig::default());
    for k in [b"a", b"b", b"c", b"d"] {
        tree.put(k, b"v").unwrap();
    }
    let got: Vec<Vec<u8>> = tree
        .scan(b"b".to_vec()..=b"c".to_vec())
        .map(|r| r.unwrap().0)
        .collect();
    assert_eq!(got, vec![b"b".to_vec(), b"c".to_vec()]);
}
