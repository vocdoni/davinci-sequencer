//! Keys that share their first 63 path bits. The state tree is a 64-level
//! arbo SMT whose path is the key's bits from bit 0 up, so a vote id
//! (`2^63 + h`, 63 hash bits) and the key `h` below 2^63 (a config key or a
//! ballot slot) part only at the last bit: once both are in the tree, both
//! leaves sit at depth 64. Every proof the node builds carries 65 sibling
//! slots, so such a leaf's proof holds its twin as the 64th sibling and the
//! zero sentinel circomlib `SMTLevIns` (the guest) and `Sha256SmtLib` (the
//! registry) need as the 65th: the node settles twins like any other keys,
//! and both leaves stay readable, writable and refreshable.
//!
//! The ZISKEMU test needs `ZISKEMU=1`, the circuit ELF and the circom
//! artifacts (`CIRCOM_ARTIFACTS`) for proofs not in `tests/cache`.

mod common;
mod emu;

use common::*;
use davinci_state::{CensusOrigin, ProcessConfig, ProcessState, VerifiedVote, validate_vote};
use davinci_zkvm_sdk::census::{CensusWitness, csp_sign, eth_address};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::{BALLOT_MIN, SMT_LEVELS, SMT_PROOF_LEVELS, VOTE_ID_MIN};
use davinci_zkvm_sdk::release;
use davinci_zkvm_sdk::types::SmtEntryJson;
use rand::SeedableRng;
use rand::rngs::StdRng;

const MASK: u64 = VOTE_ID_MIN - 1;

fn rng() -> StdRng {
    StdRng::seed_from_u64(0xdee9)
}

fn zero() -> String {
    format!("0x{}", "00".repeat(32))
}

fn entries(b: &davinci_state::PreparedBatch) -> Vec<&SmtEntryJson> {
    let st = b.request.state.as_ref().unwrap();
    st.vote_id_smt
        .iter()
        .chain(&st.ballot_smt)
        .chain(&st.refresh_smt)
        .chain(&st.results_smt)
        .chain(&st.process_smt)
        .collect()
}

/// Every entry carries 65 sibling slots, the last one the zero sentinel;
/// how many of them prove a leaf at depth 64 (a non-zero 64th sibling).
fn deep_entries(b: &davinci_state::PreparedBatch) -> usize {
    let z = zero();
    let all = entries(b);
    for e in &all {
        assert_eq!(e.siblings.len(), SMT_PROOF_LEVELS);
        assert_eq!(e.siblings[SMT_LEVELS], z, "sentinel");
    }
    all.iter()
        .filter(|e| e.siblings[SMT_LEVELS - 1] != z)
        .count()
}

fn selected(st: &ProcessState<arbo::MemoryStorage>, pending: &[VerifiedVote]) -> Vec<u64> {
    let (sel, _) = st
        .select_batch(pending, u64::MAX, &none(), usize::MAX)
        .unwrap();
    sel.iter().map(|v| v.pkg.vote_id).collect()
}

// A vote id whose low 63 bits are a config key is sealed. Settled, it puts
// that config leaf at depth 64, and the next batch (which reads every config
// key) proves it with the twin as the 64th sibling. The results leaf (0x04)
// keeps a registry-valid inclusion proof for the DKG modes.
#[test]
fn config_twin_vote_ids_settle_and_later_batches_prove() {
    let env = env(2, 4);
    for key in [0u64, 2, 3, 4, 6, 7] {
        let mut st = new_state(&env);
        let mut v = fake_vote(&env, 0, &[1, 2], 70);
        v.pkg.vote_id = VOTE_ID_MIN | key;
        assert_eq!(selected(&st, std::slice::from_ref(&v)), vec![v.pkg.vote_id]);
        let b = st
            .prepare(std::slice::from_ref(&v), &mut rng(), &none())
            .unwrap();
        // The insert splits at level 63; only 0x04 is updated in the same
        // batch, already at depth 64.
        let first = usize::from(key == 4);
        assert_eq!(deep_entries(&b), first, "key {key:#x}: the twin's batch");
        st.commit(&b).unwrap();
        assert_eq!(st.vote_id_proof(v.pkg.vote_id).unwrap().siblings.len(), 64);
        let next = st
            .prepare(&[fake_vote(&env, 1, &[2, 1], 71)], &mut rng(), &none())
            .unwrap();
        // The config read (0x04: the results update).
        assert_eq!(deep_entries(&next), 1, "key {key:#x}: the next batch");
        if key == 4 {
            st.rollback(&next).unwrap();
            let (_, sibs) = st.dkg_results_inputs().unwrap();
            assert_eq!(sibs.len(), SMT_PROOF_LEVELS);
            assert_ne!(sibs[SMT_LEVELS - 1], [0u8; 32]);
            assert_eq!(sibs[SMT_LEVELS], [0u8; 32]);
        }
    }
}

// A vote id twin of an occupied slot and a slot twin of a settled vote id
// are both sealed, and the slot they share a path with is rewritten later.
#[test]
fn twins_of_settled_keys_are_sealed_and_rewritten() {
    let env = env(2, 4);
    let mut st = new_state(&env);
    let a = fake_vote(&env, 0, &[1, 2], 72);
    let b = st
        .prepare(std::slice::from_ref(&a), &mut rng(), &none())
        .unwrap();
    st.commit(&b).unwrap();

    let mut v = fake_vote(&env, 1, &[2, 1], 73);
    v.pkg.vote_id = VOTE_ID_MIN | a.slot;
    let mut w = fake_vote(&env, 2, &[0, 1], 74);
    w.slot = a.pkg.vote_id & MASK;
    assert!(w.slot >= BALLOT_MIN);
    assert_eq!(
        selected(&st, &[v.clone(), w.clone()]),
        vec![v.pkg.vote_id, w.pkg.vote_id]
    );
    let b2 = st
        .prepare(std::slice::from_ref(&v), &mut rng(), &none())
        .unwrap();
    st.commit(&b2).unwrap();
    // a's revote updates its slot, now a leaf at depth 64.
    let again = st
        .prepare(&[fake_vote(&env, 0, &[0, 0], 75)], &mut rng(), &none())
        .unwrap();
    assert_eq!(again.overwrites, 1);
    assert!(deep_entries(&again) >= 1, "a's revote is not deep");
}

// Twins inside one queue are sealed together, and so is a vote whose own
// two keys are twins.
#[test]
fn twins_inside_a_batch_are_sealed_together() {
    let env = env(2, 4);
    let mut st = new_state(&env);
    let a = fake_vote(&env, 0, &[1, 2], 77);
    let mut b = fake_vote(&env, 1, &[2, 1], 78);
    b.slot = a.pkg.vote_id & MASK;
    let mut own = fake_vote(&env, 2, &[1, 1], 79);
    own.slot = own.pkg.vote_id & MASK;
    let all = [a.clone(), b.clone(), own.clone()];
    assert_eq!(
        selected(&st, &all),
        vec![a.pkg.vote_id, b.pkg.vote_id, own.pkg.vote_id]
    );
    let batch = st.prepare(&all, &mut rng(), &none()).unwrap();
    deep_entries(&batch);
    st.commit(&batch).unwrap();
}

/// Two honest CSP votes with real proofs: A's vote id and B's slot (the
/// CSP's index for B) share their 63 path bits. The guest accepts the batch
/// that holds both, then B's revote (an update of the slot at depth 64) and
/// another voter's batch that refreshes B's slot.
#[test]
fn csp_twins_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let verifier = BallotVerifier::from_snarkjs_json(release::ballot_vk_json()).unwrap();
    let csp = emu::key(88_001);
    let mut root = [0u8; 32];
    root[12..].copy_from_slice(&eth_address(csp.verifying_key()));
    let env = env(2, 1);
    let c = ProcessConfig {
        census_origin: CensusOrigin::Csp,
        census_root: root,
        ..env.cfg.clone()
    };
    let vote = |sk: &k256::ecdsa::SigningKey, k: u64, index: u64, fields: &[u64]| {
        let addr = eth_address(sk.verifying_key());
        let att = csp_sign(&csp, &c.process_id, &addr, 1, index);
        let (v, ok) = emu::vote(&c, sk, fields, k, 1, CensusWitness::Csp(att));
        assert!(ok);
        validate_vote(&c, &verifier, v.pkg).unwrap()
    };
    let a = vote(&emu::key(88_002), 41, 0, &[1, 0]);
    let twin = a.pkg.vote_id & MASK;
    let bk = emu::key(88_003);
    let b = vote(&bk, 42, twin - BALLOT_MIN, &[1, 0]);
    assert_eq!(b.slot, twin);

    let mut st = ProcessState::create(c.clone(), arbo::MemoryStorage::new()).unwrap();
    let both = [a.clone(), b.clone()];
    assert_eq!(selected(&st, &both), vec![a.pkg.vote_id, b.pkg.vote_id]);
    let pair = st.prepare(&both, &mut rng(), &none()).unwrap();
    emu::assert_accepted(&pair, "the twin pair");
    st.commit(&pair).unwrap();
    // B's revote: an update of the slot now at depth 64.
    let b2 = vote(&bk, 43, twin - BALLOT_MIN, &[0, 1]);
    let batch = st
        .prepare(std::slice::from_ref(&b2), &mut rng(), &none())
        .unwrap();
    assert!(deep_entries(&batch) >= 1);
    emu::assert_accepted(&batch, "B's revote at depth 64");
    st.commit(&batch).unwrap();
    // Another voter's batch that refreshes B's slot (and A's).
    let x = vote(&emu::key(88_004), 44, 7, &[1, 1]);
    let mut slots = vec![a.slot, b.slot];
    slots.sort_unstable();
    let batch = st
        .prepare_with(std::slice::from_ref(&x), [9; 32], &slots, &none())
        .unwrap();
    assert!(batch.refresh_slots().contains(&b.slot));
    assert!(deep_entries(&batch) >= 1);
    emu::assert_accepted(&batch, "a refresh of B's slot at depth 64");
}

// A pair another sequencer settled: the refresh draw picks the slot like any
// other, and the batches that refresh it are well formed.
#[test]
fn refresh_draws_pick_a_twin_slot_like_any_other() {
    let env = env(2, 48);
    let mut st = new_state(&env);
    let votes: Vec<VerifiedVote> = (0..40)
        .map(|i| fake_vote(&env, i, &[1, 1], 200 + i as u64))
        .collect();
    let b = st.prepare(&votes, &mut rng(), &none()).unwrap();
    st.commit(&b).unwrap();
    let deep = votes[0].slot;
    let mut twin = fake_vote(&env, 40, &[0, 1], 300);
    twin.pkg.vote_id = VOTE_ID_MIN | deep;
    let b = st
        .prepare(std::slice::from_ref(&twin), &mut rng(), &none())
        .unwrap();
    st.commit(&b).unwrap();
    assert_eq!(
        st.vote_id_proof(twin.pkg.vote_id).unwrap().siblings.len(),
        64
    );

    let next = fake_vote(&env, 41, &[1, 0], 301);
    let mut hit = 0;
    for s in 0..64 {
        let mut r = StdRng::seed_from_u64(1000 + s);
        let sel = st
            .refresh_selection(std::slice::from_ref(&next), &mut r, &none())
            .unwrap();
        assert_eq!(sel.len(), 16);
        if sel.contains(&deep) {
            hit += 1;
            let b = st
                .prepare_with(std::slice::from_ref(&next), [7; 32], &sel, &none())
                .unwrap();
            assert!(deep_entries(&b) >= 1);
            st.rollback(&b).unwrap();
        }
    }
    // 16 of 41 candidates a draw: about 25 of 64 draws.
    assert!(hit >= 10, "{hit} of 64 draws refresh the twin slot");
}
