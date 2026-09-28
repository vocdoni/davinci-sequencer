//! Error-path tests: typed errors instead of panics on malformed input.

use arbo::{Error, MemoryStorage, Sha256, Storage, Tree, WriteBatch, unpack_siblings};

fn new_tree(levels: usize) -> Tree<MemoryStorage, Sha256> {
    Tree::new(MemoryStorage::new(), levels, Sha256).unwrap()
}

#[test]
fn key_too_long_rejected() {
    let mut t = new_tree(64);
    let err = t.add(&[1u8; 9], &[0u8; 32]).unwrap_err();
    assert!(matches!(err, Error::KeyTooLong { .. }), "{err}");
}

#[test]
fn add_existing_key_fails() {
    let mut t = new_tree(64);
    t.add(&7u64.to_le_bytes(), b"a").unwrap();
    let err = t.add(&7u64.to_le_bytes(), b"b").unwrap_err();
    assert!(matches!(err, Error::KeyAlreadyExists), "{err}");
}

#[test]
fn update_missing_key_fails() {
    let mut t = new_tree(64);
    t.add(&7u64.to_le_bytes(), b"a").unwrap();
    let err = t.update(&8u64.to_le_bytes(), b"b").unwrap_err();
    assert!(matches!(err, Error::KeyNotFound), "{err}");
}

#[test]
fn max_virtual_level_reached() {
    // Keys 0x01 and 0x11 share the low 4 bits; at max_levels=4 the virtual
    // extension exceeds the tree depth, as in Go's ErrMaxVirtualLevel.
    let mut t = new_tree(4);
    t.add(&[0x01], b"a").unwrap();
    let err = t.add(&[0x11], b"b").unwrap_err();
    assert!(matches!(err, Error::MaxVirtualLevel), "{err}");
}

#[test]
fn set_root_unknown_fails() {
    let mut t = new_tree(64);
    t.add(&1u64.to_le_bytes(), b"a").unwrap();
    let err = t.set_root(&[0xAB; 32]).unwrap_err();
    assert!(matches!(err, Error::RootNotFound), "{err}");
}

#[test]
fn corrupted_storage_returns_error() {
    let storage = MemoryStorage::new();
    let mut t = Tree::new(storage.clone(), 64, Sha256).unwrap();
    t.add(&1u64.to_le_bytes(), b"a").unwrap();
    t.add(&2u64.to_le_bytes(), b"b").unwrap();

    // Overwrite the root node bytes with garbage of every problematic shape.
    for garbage in [vec![], vec![0x02], vec![0x02, 32, 1, 2, 3], vec![0xFF; 66]] {
        let mut wb = WriteBatch::new();
        wb.put(t.root().to_vec(), garbage);
        storage.write(wb).unwrap();
        // Walking must fail typed, not panic.
        let res = t.get(&1u64.to_le_bytes());
        assert!(res.is_err(), "corrupted node accepted: {res:?}");
    }
}

#[test]
fn intermediate_chain_deeper_than_levels_errors() {
    // Fabricate a chain of intermediates longer than max_levels: node i's
    // left child is node i+1. Go would panic indexing path bits; we must
    // return a typed error from every walk.
    let storage = MemoryStorage::new();
    let levels = 4;
    let keys: Vec<[u8; 32]> = (0..=levels + 1).map(|i| [i as u8 + 1; 32]).collect();
    let mut wb = WriteBatch::new();
    for i in 0..=levels {
        let mut v = vec![2u8, 32];
        v.extend_from_slice(&keys[i + 1]);
        v.extend_from_slice(&[0u8; 32]);
        wb.put(keys[i].to_vec(), v);
    }
    wb.put(b"root".to_vec(), keys[0].to_vec());
    // Params must be crafted too: opening a rooted tree without them fails.
    wb.put(b"params".to_vec(), vec![1, levels as u8, 0, 0, 0]);
    storage.write(wb).unwrap();

    let mut t = Tree::new(storage, levels, Sha256).unwrap();
    let e = t.get(&[0]).unwrap_err();
    assert!(matches!(e, Error::Corrupted(_)), "get: {e}");
    let e = t.gen_proof(&[0]).unwrap_err();
    assert!(matches!(e, Error::Corrupted(_)), "gen_proof: {e}");
    let e = t.add(&[0], b"x").unwrap_err();
    assert!(matches!(e, Error::Corrupted(_)), "add: {e}");
    let e = t.update(&[0], b"x").unwrap_err();
    assert!(matches!(e, Error::Corrupted(_)), "update: {e}");
}

#[test]
fn reopen_with_different_params_fails() {
    let storage = MemoryStorage::new();
    let mut t = Tree::new(storage.clone(), 64, Sha256).unwrap();
    t.add(&1u64.to_le_bytes(), b"a").unwrap();
    drop(t);
    match Tree::new(storage, 4, Sha256) {
        Err(Error::ParamsMismatch("max_levels")) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("mismatched reopen accepted"),
    }
}

#[test]
fn reopen_with_different_hash_fails() {
    // A hash function with a different id: behavior is irrelevant, the
    // open must fail before any hashing.
    #[derive(Clone)]
    struct AltHash;
    impl arbo::HashFunction for AltHash {
        fn id(&self) -> u8 {
            9
        }
        fn hash(&self, parts: &[&[u8]]) -> arbo::Hash {
            Sha256.hash(parts)
        }
    }

    let storage = MemoryStorage::new();
    let mut t = Tree::new(storage.clone(), 64, Sha256).unwrap();
    t.add(&1u64.to_le_bytes(), b"a").unwrap();
    drop(t);
    match Tree::new(storage, 64, AltHash) {
        Err(Error::ParamsMismatch("hash function")) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("mismatched hash reopen accepted"),
    }
}

#[test]
fn open_rooted_tree_without_params_fails() {
    // A tree that has nodes but no params key is from an unknown writer;
    // it must not be silently adopted (backfilled).
    let bare = MemoryStorage::new();
    let mut wb = WriteBatch::new();
    wb.put(b"root".to_vec(), vec![0xAB; 32]);
    bare.write(wb).unwrap();
    match Tree::new(bare, 64, Sha256) {
        Err(Error::ParamsMismatch("params missing")) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("rooted tree without params accepted"),
    }
}

#[test]
fn add_batch_aborts_on_corrupted_storage() {
    // Stored-data corruption must abort the incremental batch with nothing
    // written, not be filed as a per-key invalid.
    let storage = MemoryStorage::new();
    let mut t = Tree::new(storage.clone(), 64, Sha256).unwrap();
    t.add(&1u64.to_le_bytes(), b"a").unwrap();
    t.add(&2u64.to_le_bytes(), b"b").unwrap();
    let root_before = t.root();

    let mut wb = WriteBatch::new();
    wb.put(t.root().to_vec(), vec![0xFF; 66]);
    storage.write(wb).unwrap();

    let e = t
        .add_batch(&[(3u64.to_le_bytes().to_vec(), b"c".to_vec())])
        .unwrap_err();
    assert!(matches!(e, Error::InvalidValuePrefix), "{e}");
    assert_eq!(t.n_leafs().unwrap(), 2, "batch must write nothing");
    assert_eq!(t.root(), root_before, "root must be unchanged");
}

#[test]
fn dump_oversized_stored_leaf_errors() {
    // A stored leaf whose value cannot fit the dump's u16 length must fail
    // typed instead of truncating.
    let storage = MemoryStorage::new();
    let t = Tree::new(storage.clone(), 64, Sha256).unwrap();
    drop(t);
    let mut leaf = vec![1u8, 8];
    leaf.extend_from_slice(&1u64.to_le_bytes());
    leaf.extend_from_slice(&vec![0u8; 70_000]);
    let mut wb = WriteBatch::new();
    wb.put(vec![0xAB; 32], leaf);
    wb.put(b"root".to_vec(), vec![0xAB; 32]);
    storage.write(wb).unwrap();

    let t = match Tree::new(storage, 64, Sha256) {
        Ok(t) => t,
        Err(e) => panic!("reopen failed: {e}"),
    };
    let root = t.root();
    let e = t.dump(&root).unwrap_err();
    assert!(matches!(e, Error::Corrupted(_)), "{e}");
}

#[test]
fn pack_siblings_over_u16_errors() {
    // > u16::MAX packed bytes: 2048 non-empty siblings.
    let sibs = vec![[1u8; 32]; 2048];
    let err = arbo::pack_siblings(&Sha256, &sibs).unwrap_err();
    assert!(matches!(err, Error::MalformedSiblings(_)), "{err}");
}

#[test]
fn malformed_packed_siblings_rejected() {
    // Truncated header.
    assert!(unpack_siblings(&Sha256, &[1, 0]).is_err());
    // full_len larger than the buffer.
    assert!(unpack_siblings(&Sha256, &[200, 0, 1, 0, 1]).is_err());
    // bitmap promises a sibling the payload cuts short (Go would panic):
    // [full u16][bitmap_len u16][bitmap 0b1][16 bytes instead of 32]
    let mut p = Vec::new();
    p.extend_from_slice(&((4 + 1 + 16) as u16).to_le_bytes());
    p.extend_from_slice(&1u16.to_le_bytes());
    p.push(0b0000_0001);
    p.extend_from_slice(&[9u8; 16]);
    assert!(unpack_siblings(&Sha256, &p).is_err());
}

#[test]
fn malformed_dump_rejected() {
    let mut t = new_tree(64);
    // Truncated record: says 8-byte key, 32-byte value, delivers 3 bytes.
    let mut d = vec![8u8];
    d.extend_from_slice(&32u16.to_le_bytes());
    d.extend_from_slice(&[1, 2, 3]);
    assert!(matches!(t.import_dump(&d), Err(Error::MalformedDump(_))));
}

#[test]
fn import_into_non_empty_tree_fails() {
    let mut src = new_tree(64);
    src.add(&1u64.to_le_bytes(), b"a").unwrap();
    let dump = src.dump(&src.root()).unwrap();

    let mut dst = new_tree(64);
    dst.add(&2u64.to_le_bytes(), b"b").unwrap();
    assert!(matches!(dst.import_dump(&dump), Err(Error::TreeNotEmpty)));
}
