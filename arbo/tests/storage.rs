//! Storage-level behavior: two RedbStorage handles on one table stay
//! coherent (the read snapshot refreshes on a miss).
#![cfg(feature = "redb")]

use std::sync::Arc;

use arbo::{RedbStorage, Sha256, Storage, Tree};

#[test]
fn second_redb_handle_sees_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("t.redb")).unwrap());
    let h1 = RedbStorage::new(db.clone(), "t").unwrap();
    let h2 = RedbStorage::new(db, "t").unwrap();

    let mut t1 = Tree::new(h1, 64, Sha256).unwrap();
    t1.add(&1u64.to_le_bytes(), &[1u8; 32]).unwrap();
    // Pin h2's snapshot to this point in time.
    assert!(h2.get(&t1.root()).unwrap().is_some());
    let old_root = t1.root();

    // Writes through h1 after h2's snapshot was opened.
    for i in 2..20u64 {
        t1.add(&i.to_le_bytes(), &[i as u8; 32]).unwrap();
    }

    // h2 must see the new nodes (miss -> snapshot refresh), and the old
    // root's nodes are still there (content-addressed, never deleted).
    assert!(h2.get(&t1.root()).unwrap().is_some(), "stale second handle");
    assert!(h2.get(&old_root).unwrap().is_some());
}

#[test]
fn second_redb_handle_metadata_never_stale() {
    // Regression: read root through h2 (pins its snapshot), write 18
    // adds through h1, then open a Tree on h2 — it must resume at the new
    // root with the new leaf count, not the pinned snapshot's.
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("t.redb")).unwrap());
    let h1 = RedbStorage::new(db.clone(), "t").unwrap();
    let h2 = RedbStorage::new(db, "t").unwrap();

    let mut t1 = Tree::new(h1, 64, Sha256).unwrap();
    t1.add(&1u64.to_le_bytes(), &[1u8; 32]).unwrap();
    assert!(h2.get(b"root").unwrap().is_some());
    // Pin h2's node snapshot too; metadata must still read fresh.
    assert!(h2.get(&t1.root()).unwrap().is_some());

    for i in 2..20u64 {
        t1.add(&i.to_le_bytes(), &[i as u8; 32]).unwrap();
    }

    let t2 = Tree::new(h2, 64, Sha256).unwrap();
    assert_eq!(t2.root(), t1.root(), "second handle opened at stale root");
    assert_eq!(t2.n_leafs().unwrap(), 19, "stale nleafs");
}

#[test]
fn released_snapshot_frees_pages_and_reads_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let db = Arc::new(redb::Database::create(&path).unwrap());
    let reader = RedbStorage::new(db.clone(), "idle").unwrap();
    let mut idle = Tree::new(reader.clone(), 64, Sha256).unwrap();
    idle.add(&1u64.to_le_bytes(), &[1u8; 32]).unwrap();
    assert!(reader.get(&idle.root()).unwrap().is_some());
    assert!(reader.snapshot_open());
    reader.release_snapshot();
    assert!(!reader.snapshot_open());

    // Commits elsewhere while the idle handle holds no snapshot reuse the
    // freed pages instead of growing the file.
    let mut busy = Tree::new(RedbStorage::new(db, "busy").unwrap(), 64, Sha256).unwrap();
    for i in 0..400u64 {
        busy.add(&i.to_le_bytes(), &[i as u8; 32]).unwrap();
    }
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len < 4 << 20, "file grew to {len} B");
    // The released handle reads on (a fresh snapshot opens on demand).
    assert!(reader.get(&idle.root()).unwrap().is_some());
}
