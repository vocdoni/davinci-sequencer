//! Processor-proof tests: every CircomProcessorProof the tree emits must be
//! accepted by the zkVM guest's verifier (ported verbatim in `oracle.rs`),
//! and tampered proofs must be rejected.
//!
//! The guest tree is 64 levels with 8-byte keys, so everything here runs at
//! `max_levels = 64`.

#[path = "oracle.rs"]
mod oracle;

use arbo::{CircomProcessorProof, MemoryStorage, Sha256, Tree};
use oracle::{FrRaw, SmtTransition, le_to_fr, verify_inclusion, verify_transition};
use sha2::Digest;

const LEVELS: usize = 64;

fn fr_from_bytes(b: &[u8]) -> FrRaw {
    assert!(b.len() <= 32, "field too long for FrRaw");
    let mut buf = [0u8; 32];
    buf[..b.len()].copy_from_slice(b);
    le_to_fr(&buf)
}

fn transition(p: &CircomProcessorProof) -> SmtTransition {
    SmtTransition {
        old_root: fr_from_bytes(&p.old_root),
        new_root: fr_from_bytes(&p.new_root),
        old_key: fr_from_bytes(&p.old_key),
        old_value: fr_from_bytes(&p.old_value),
        is_old0: p.is_old0,
        new_key: fr_from_bytes(&p.new_key),
        new_value: fr_from_bytes(&p.new_value),
        fnc0: p.fnc0,
        fnc1: p.fnc1,
        siblings: p.siblings.iter().map(|s| fr_from_bytes(s)).collect(),
    }
}

fn key8(i: u64) -> [u8; 8] {
    i.to_le_bytes()
}

fn val32(tag: &str, i: u64) -> [u8; 32] {
    sha2::Sha256::digest(format!("{tag}-{i}")).into()
}

fn new_tree() -> Tree<MemoryStorage, Sha256> {
    Tree::new(MemoryStorage::new(), LEVELS, Sha256).unwrap()
}

#[test]
fn insert_into_empty_tree_is_old0() {
    let mut t = new_tree();
    let p = t.insert_with_proof(&key8(7), &val32("v", 7)).unwrap();
    assert!(p.fnc0 && !p.fnc1, "insert fnc bits");
    assert!(p.is_old0, "first insert lands on an empty slot");
    assert_eq!(p.siblings.len(), LEVELS, "siblings padded to levels");
    assert_eq!(p.new_root.to_vec(), t.root().to_vec());
    assert!(
        verify_transition(&transition(&p)),
        "guest rejects first insert"
    );
}

#[test]
fn insert_sequence_verifies_and_hits_displaced_leaves() {
    let mut t = new_tree();
    let mut saw_displaced = false;
    // sha256-derived keys collide in low bits often enough to displace leaves.
    for i in 0..60u64 {
        let kd: [u8; 32] = sha2::Sha256::digest(format!("pk-{i}")).into();
        let key: [u8; 8] = kd[..8].try_into().unwrap();
        let p = t.insert_with_proof(&key, &val32("pv", i)).unwrap();
        assert!(p.fnc0 && !p.fnc1);
        assert_eq!(p.siblings.len(), LEVELS);
        assert_eq!(p.new_root.to_vec(), t.root().to_vec());
        saw_displaced |= !p.is_old0;
        assert!(
            verify_transition(&transition(&p)),
            "guest rejects insert {i}"
        );
    }
    assert!(saw_displaced, "no displaced-leaf insert exercised");
}

#[test]
fn insert_deep_shared_prefix() {
    // Keys sharing 62/63 low bits force divergence at the deepest levels.
    let mut t = new_tree();
    let base = 5u64;
    for (i, k) in [base, base | (1 << 62), base | (1 << 63)]
        .iter()
        .enumerate()
    {
        let p = t
            .insert_with_proof(&key8(*k), &val32("deep", i as u64))
            .unwrap();
        assert!(
            verify_transition(&transition(&p)),
            "guest rejects deep insert {i}"
        );
    }
}

#[test]
fn update_proof_verifies() {
    let mut t = new_tree();
    for i in 0..20u64 {
        t.add(&key8(i * 3 + 1), &val32("u0", i)).unwrap();
    }
    for i in 0..20u64 {
        let key = key8(i * 3 + 1);
        let p = t.update_with_proof(&key, &val32("u1", i)).unwrap();
        assert!(!p.fnc0 && p.fnc1, "update fnc bits");
        assert!(!p.is_old0);
        assert_eq!(p.old_key, key.to_vec());
        assert_eq!(p.new_key, key.to_vec());
        assert_eq!(p.old_value, val32("u0", i).to_vec());
        assert_eq!(p.new_root.to_vec(), t.root().to_vec());
        assert!(
            verify_transition(&transition(&p)),
            "guest rejects update {i}"
        );
    }
}

#[test]
fn read_proof_passes_guest_inclusion() {
    let mut t = new_tree();
    for i in 0..30u64 {
        t.add(&key8(i * 7 + 2), &val32("r", i)).unwrap();
    }
    for i in 0..30u64 {
        let key = key8(i * 7 + 2);
        let cp = t.circom_verifier_proof(&key).unwrap();
        assert_eq!(cp.fnc, 0, "inclusion fnc");
        assert_eq!(cp.siblings.len(), LEVELS);
        let sib: Vec<FrRaw> = cp.siblings.iter().map(|s| fr_from_bytes(s)).collect();
        let ok = verify_inclusion(
            &fr_from_bytes(&cp.root),
            &fr_from_bytes(&key),
            &fr_from_bytes(&val32("r", i)),
            &sib,
        );
        assert!(ok, "guest rejects inclusion {i}");
    }
}

#[test]
fn tampered_proofs_are_rejected() {
    let mut t = new_tree();
    t.add(&key8(1), &val32("t", 1)).unwrap();
    let p = t.insert_with_proof(&key8(9), &val32("t", 9)).unwrap();
    assert!(verify_transition(&transition(&p)), "honest proof must pass");

    // Wrong new value.
    let mut bad = transition(&p);
    bad.new_value[0] ^= 1;
    assert!(!verify_transition(&bad), "tampered new_value accepted");

    // Flipped is_old0.
    let mut bad = transition(&p);
    bad.is_old0 = !bad.is_old0;
    assert!(!verify_transition(&bad), "flipped is_old0 accepted");

    // Wrong old root.
    let mut bad = transition(&p);
    bad.old_root[0] ^= 1;
    assert!(!verify_transition(&bad), "tampered old_root accepted");

    // Tampered sibling.
    let mut bad = transition(&p);
    bad.siblings[0][0] ^= 1;
    assert!(!verify_transition(&bad), "tampered sibling accepted");
}
