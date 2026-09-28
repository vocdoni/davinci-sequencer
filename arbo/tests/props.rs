//! Property tests: history independence, batch/sequential equivalence,
//! historical roots, proof round-trips and mutation rejection.

use std::collections::BTreeMap;

use arbo::{MemoryStorage, Sha256, Tree, check_proof, pack_siblings, unpack_siblings};
use proptest::prelude::*;

const LEVELS: usize = 64;

fn new_tree() -> Tree<MemoryStorage, Sha256> {
    Tree::new(MemoryStorage::new(), LEVELS, Sha256).unwrap()
}

/// Unique (key, value) pairs; u64 keys rendered as the 8-byte LE arbo key.
fn kvs_strategy(max: usize) -> impl Strategy<Value = Vec<([u8; 8], [u8; 32])>> {
    proptest::collection::btree_map(any::<u64>(), any::<[u8; 32]>(), 1..max).prop_map(
        |m: BTreeMap<u64, [u8; 32]>| m.into_iter().map(|(k, v)| (k.to_le_bytes(), v)).collect(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn root_is_order_independent(kvs in kvs_strategy(40).prop_shuffle(), kvs2_seed in any::<u64>()) {
        let mut sorted = kvs.clone();
        sorted.sort_by_key(|kv| kv.0);
        // A second, differently keyed shuffle.
        let mut other = sorted.clone();
        let n = other.len();
        for i in (1..n).rev() {
            other.swap(i, (kvs2_seed as usize).wrapping_mul(i + 7) % (i + 1));
        }

        let mut t1 = new_tree();
        let mut t2 = new_tree();
        for (k, v) in &kvs { t1.add(k, v).unwrap(); }
        for (k, v) in &other { t2.add(k, v).unwrap(); }
        prop_assert_eq!(t1.root(), t2.root());
        prop_assert_eq!(t1.n_leafs().unwrap(), kvs.len() as u64);
    }

    #[test]
    fn add_batch_equals_sequential(kvs in kvs_strategy(60)) {
        let mut seq = new_tree();
        for (k, v) in &kvs { seq.add(k, v).unwrap(); }

        let mut bat = new_tree();
        let pairs: Vec<(Vec<u8>, Vec<u8>)> =
            kvs.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        let invalid = bat.add_batch(&pairs).unwrap();
        prop_assert!(invalid.is_empty());
        prop_assert_eq!(seq.root(), bat.root());
        prop_assert_eq!(seq.n_leafs().unwrap(), bat.n_leafs().unwrap());
    }

    #[test]
    fn set_root_reopens_history(kvs in kvs_strategy(40)) {
        prop_assume!(kvs.len() >= 2);
        let split = kvs.len() / 2;
        let mut t = new_tree();
        for (k, v) in &kvs[..split] { t.add(k, v).unwrap(); }
        let old_root = t.root();
        for (k, v) in &kvs[split..] { t.add(k, v).unwrap(); }

        // Late keys visible now, gone after rewinding to the old root.
        let (lk, lv) = &kvs[kvs.len() - 1];
        let got = t.get(lk).unwrap();
        prop_assert_eq!(got.as_deref(), Some(lv.as_slice()));
        t.set_root(&old_root).unwrap();
        prop_assert_eq!(t.root(), old_root);
        prop_assert!(t.get(lk).unwrap().is_none());
        let (ek, ev) = &kvs[0];
        let got = t.get(ek).unwrap();
        prop_assert_eq!(got.as_deref(), Some(ev.as_slice()));
    }

    #[test]
    fn gen_check_roundtrip(kvs in kvs_strategy(40), probe in any::<u64>()) {
        let mut t = new_tree();
        for (k, v) in &kvs { t.add(k, v).unwrap(); }
        let root = t.root();

        // Every existing key proves inclusion.
        for (k, v) in &kvs {
            let p = t.gen_proof(k).unwrap();
            prop_assert!(p.exists);
            prop_assert!(check_proof(&Sha256, k, v, &root, &p.packed).unwrap());
        }
        // A probe key not in the tree yields a non-inclusion proof for the
        // leaf actually found (if any).
        let pk = probe.to_le_bytes();
        if !kvs.iter().any(|(k, _)| k == &pk) {
            let p = t.gen_proof(&pk).unwrap();
            prop_assert!(!p.exists);
            if !p.key.is_empty() {
                prop_assert!(check_proof(&Sha256, &p.key, &p.value, &root, &p.packed).unwrap());
            }
            // And the probe (key, value=found) must NOT check as included.
            prop_assert!(!check_proof(&Sha256, &pk, b"bogus", &root, &p.packed).unwrap());
        }
    }

    #[test]
    fn pack_unpack_roundtrip(mut sibs in proptest::collection::vec(
        prop_oneof![3 => Just([0u8; 32]), 2 => any::<[u8; 32]>()], 0..70))
    {
        // Go's UnpackSiblings drops trailing empty siblings, so make the
        // last one non-zero for exact round-tripping.
        if let Some(last) = sibs.last_mut() { last[0] |= 1; }
        let packed = pack_siblings(&Sha256, &sibs).unwrap();
        let un = unpack_siblings(&Sha256, &packed).unwrap();
        prop_assert_eq!(un, sibs);
    }

    #[test]
    fn check_proof_rejects_mutation(kvs in kvs_strategy(20), idx in any::<usize>(), bit in 0usize..8) {
        let mut t = new_tree();
        for (k, v) in &kvs { t.add(k, v).unwrap(); }
        let root = t.root();
        let (k, v) = &kvs[0];
        let p = t.gen_proof(k).unwrap();
        prop_assert!(check_proof(&Sha256, k, v, &root, &p.packed).unwrap());

        let mut bad = p.packed.clone();
        let pos = idx % bad.len();
        bad[pos] ^= 1 << bit;
        // Mutations of spare bitmap bits can decode to the same siblings;
        // only semantically different packings must fail.
        if let Ok(un) = unpack_siblings(&Sha256, &bad) {
            if un == unpack_siblings(&Sha256, &p.packed).unwrap() {
                return Ok(());
            }
            prop_assert!(!check_proof(&Sha256, k, v, &root, &bad).unwrap());
        }
        // A decode error is also a rejection.
    }
}

#[test]
fn trailing_empty_siblings_are_dropped_by_unpack() {
    let mut sibs = vec![[0u8; 32]; 5];
    sibs[1][3] = 9;
    sibs.push([0u8; 32]);
    sibs.push([0u8; 32]);
    let packed = pack_siblings(&Sha256, &sibs).unwrap();
    let un = unpack_siblings(&Sha256, &packed).unwrap();
    // Go quirk: unpack stops once the non-zero siblings are consumed.
    assert_eq!(un.len(), 2);
    assert_eq!(un[1], sibs[1]);
}

#[test]
fn historical_proof_verifies_after_later_adds() {
    let mut t = new_tree();
    for i in 0..10u64 {
        t.add(&i.to_le_bytes(), &[i as u8; 32]).unwrap();
    }
    let old_root = t.root();
    for i in 10..30u64 {
        t.add(&i.to_le_bytes(), &[i as u8; 32]).unwrap();
    }
    // A proof generated at the old root still verifies against it.
    let p = t.gen_proof_at(&old_root, &3u64.to_le_bytes()).unwrap();
    assert!(p.exists);
    assert!(
        check_proof(
            &Sha256,
            &3u64.to_le_bytes(),
            &[3u8; 32],
            &old_root,
            &p.packed
        )
        .unwrap()
    );
    // And a key added later does not exist under the old root.
    let p = t.gen_proof_at(&old_root, &20u64.to_le_bytes()).unwrap();
    assert!(!p.exists);
}

#[test]
fn empty_tree_proofs() {
    let t = new_tree();
    let p = t.gen_proof(&5u64.to_le_bytes()).unwrap();
    assert!(!p.exists);
    assert!(p.key.is_empty() && p.value.is_empty() && p.siblings.is_empty());

    let cp = t.circom_verifier_proof(&5u64.to_le_bytes()).unwrap();
    assert_eq!(cp.fnc, 1, "non-inclusion on empty tree");
    assert_eq!(cp.root, [0u8; 32]);
    assert_eq!(cp.siblings.len(), LEVELS);
    assert!(cp.siblings.iter().all(|s| *s == [0u8; 32]));
}

/// One incremental-vs-sequential run: same base, then `add_batch` on one
/// tree vs per-key `add` on the other. Root, n_leafs and the full
/// (index, error-variant) invalid list must match.
fn check_incremental_matches<S: arbo::Storage>(
    s_batch: S,
    s_seq: S,
    levels: usize,
    base: &[(Vec<u8>, Vec<u8>)],
    batch: &[(Vec<u8>, Vec<u8>)],
) -> Result<(), TestCaseError> {
    let mut a = Tree::new(s_batch, levels, Sha256).unwrap();
    let mut b = Tree::new(s_seq, levels, Sha256).unwrap();
    for (k, v) in base {
        let ra = a.add(k, v);
        let rb = b.add(k, v);
        prop_assert_eq!(ra.is_ok(), rb.is_ok());
    }
    prop_assert!(a.root() != arbo::EMPTY_HASH, "base must populate the tree");

    let invalids = a.add_batch(batch).unwrap();
    let mut expected = Vec::new();
    for (i, (k, v)) in batch.iter().enumerate() {
        if let Err(e) = b.add(k, v) {
            expected.push((i, std::mem::discriminant(&e)));
        }
    }
    let got: Vec<_> = invalids
        .iter()
        .map(|iv| (iv.index, std::mem::discriminant(&iv.error)))
        .collect();
    prop_assert_eq!(got, expected);
    prop_assert_eq!(a.root(), b.root());
    prop_assert_eq!(a.n_leafs().unwrap(), b.n_leafs().unwrap());
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn incremental_add_batch_matches_sequential(
        base in proptest::collection::btree_map(any::<u64>(), any::<[u8; 32]>(), 1..25usize),
        extra in proptest::collection::vec((any::<u64>(), any::<[u8; 32]>()), 1..25usize),
    ) {
        for levels in [10usize, 64, 256] {
            let kl = levels.div_ceil(8).min(8);
            let base_kvs: Vec<(Vec<u8>, Vec<u8>)> = base
                .iter()
                .map(|(k, v)| (k.to_le_bytes()[..kl].to_vec(), v.to_vec()))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect();
            let mut batch: Vec<(Vec<u8>, Vec<u8>)> = extra
                .iter()
                .map(|(k, v)| (k.to_le_bytes()[..kl].to_vec(), v.to_vec()))
                .collect();
            // Inject an existing key, an in-batch duplicate, a too-long key
            // and (at 10 levels, where the path is shorter than the key) a
            // same-path MaxVirtualLevel collision.
            batch.push((base_kvs[0].0.clone(), vec![0xA1; 32]));
            batch.push((batch[0].0.clone(), vec![0xA2; 32]));
            batch.push((vec![0xEE; 40], vec![0xA3; 32]));
            if levels == 10 {
                let mut k = base_kvs[0].0.clone();
                k[1] ^= 0x10;
                batch.push((k, vec![0xA4; 32]));
            }

            check_incremental_matches(
                MemoryStorage::new(),
                MemoryStorage::new(),
                levels,
                &base_kvs,
                &batch,
            )?;
            #[cfg(feature = "redb")]
            {
                let dir = tempfile::tempdir().unwrap();
                let db = std::sync::Arc::new(
                    redb::Database::create(dir.path().join("t.redb")).unwrap(),
                );
                let mut sa = arbo::RedbStorage::new(db.clone(), "a").unwrap();
                let mut sb = arbo::RedbStorage::new(db, "b").unwrap();
                sa.set_durability(redb::Durability::None);
                sb.set_durability(redb::Durability::None);
                check_incremental_matches(sa, sb, levels, &base_kvs, &batch)?;
            }
        }
    }
}
