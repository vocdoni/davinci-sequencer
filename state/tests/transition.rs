//! Genesis root against the shared vectors, and the prepare/commit/rollback
//! and batch-selection behaviour of `ProcessState`.

mod common;

use std::collections::BTreeSet;

use common::*;
use davinci_state::{
    CensusOrigin, Coverage, Error, ProcessConfig, ProcessState, VerifiedVote, genesis_root,
};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::blob::blob_count;
use davinci_zkvm_sdk::census::slot_key_address;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_from_dec, fr_to_le};
use davinci_zkvm_sdk::limits::{MAX_BLOBS, MAX_REFRESH, TX_BLOB_CAP, required_refresh};
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::Value;

fn hex32(s: &str) -> [u8; 32] {
    hex::decode(s.trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap()
}

// A genesis vector case (both repos share the shape).
fn cfg_from_case(v: &Value) -> ProcessConfig {
    let pid = hex::decode(v["process_id"].as_str().unwrap().trim_start_matches("0x")).unwrap();
    let mut be = [0u8; 32];
    be[32 - pid.len()..].copy_from_slice(&pid);
    let mode: BallotMode = serde_json::from_value(v["ballot_mode"].clone()).unwrap();
    let origin = match v["census_origin"].as_u64().unwrap() {
        1 => CensusOrigin::MerkleStatic,
        2 => CensusOrigin::MerkleOffchainDynamic,
        3 => CensusOrigin::MerkleOnchainDynamic,
        4 => CensusOrigin::Csp,
        o => panic!("origin {o}"),
    };
    ProcessConfig {
        process_id: fr_from_be(&be).unwrap(),
        ballot_mode: mode,
        enc_key: Point {
            x: fr_from_dec(v["enc_key"]["x"].as_str().unwrap()).unwrap(),
            y: fr_from_dec(v["enc_key"]["y"].as_str().unwrap()).unwrap(),
        },
        census_origin: origin,
        // Not part of the genesis tree.
        census_root: Fr::from(0u64),
        ballot_vk_hash: hex32(v["ballot_vk_hash"].as_str().unwrap()),
    }
}

fn check_cases(cases: &[Value]) {
    assert!(!cases.is_empty());
    for c in cases {
        let cfg = cfg_from_case(c);
        let want = hex32(c["root"].as_str().unwrap());
        assert_eq!(
            genesis_root(&cfg).unwrap(),
            want,
            "case {}",
            c["process_id"]
        );
    }
}

#[test]
fn genesis_root_matches_rust_sdk_vectors() {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../davinci-zkvm/rust-sdk/testdata/genesis.json"
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    check_cases(v.as_array().unwrap());
}

#[test]
fn genesis_root_matches_contracts_vectors() {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../davinci-contracts/test/vectors/genesis.json"
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let cases = v["cases"].as_array().unwrap();
    check_cases(cases);
    // The dynamic Merkle origins carry their own value in the 0x06 leaf.
    for o in 1..=4u64 {
        assert!(
            cases.iter().any(|c| c["census_origin"].as_u64() == Some(o)),
            "no vector for origin {o}"
        );
    }
}

#[test]
fn genesis_root_depends_on_the_origin() {
    let cfg = env(2, 4).cfg;
    let roots: Vec<[u8; 32]> = [
        CensusOrigin::MerkleStatic,
        CensusOrigin::MerkleOffchainDynamic,
        CensusOrigin::MerkleOnchainDynamic,
        CensusOrigin::Csp,
    ]
    .into_iter()
    .map(|o| {
        genesis_root(&ProcessConfig {
            census_origin: o,
            ..cfg.clone()
        })
        .unwrap()
    })
    .collect();
    for i in 0..roots.len() {
        for j in i + 1..roots.len() {
            assert_ne!(roots[i], roots[j], "origins {i} and {j}");
        }
    }
}

#[test]
fn set_census_root_moves_register_20() {
    let env = env(2, 8);
    let mut st = new_state(&env);
    let votes = vec![fake_vote(&env, 0, &[1, 2], 7)];
    let mut rng = StdRng::seed_from_u64(1);

    let b = st.prepare(&votes, &mut rng, &none()).unwrap();
    assert_eq!(b.expected.census_root, fr_to_le(&env.cfg.census_root));
    st.rollback(&b).unwrap();

    // A dynamic census re-roots between batches: the next prepare commits
    // to the new root, and the tree (whose 0x06 leaf holds only the
    // origin) does not move.
    let g = st.root();
    let new_root = Fr::from(4242u64);
    st.set_census_root(new_root);
    assert_eq!(st.config().census_root, new_root);
    assert_eq!(st.root(), g);
    let b = st.prepare(&votes, &mut rng, &none()).unwrap();
    assert_eq!(b.expected.census_root, fr_to_le(&new_root));
    b.check_publics(&b.expected).unwrap();
    st.commit(&b).unwrap();
}

#[test]
fn create_prepare_commit_and_rollback() {
    let env = env(2, 8);
    let mut st = new_state(&env);
    let g = genesis_root(&env.cfg).unwrap();
    assert_eq!(st.root(), g);
    assert_eq!(st.committed().root, g);

    let votes: Vec<_> = (0..3)
        .map(|i| fake_vote(&env, i, &[1, 2], 100 + i as u64))
        .collect();
    let mut rng = StdRng::seed_from_u64(1);

    // Prepare, then drop it: the tree must rewind to genesis.
    let b = st.prepare(&votes, &mut rng, &none()).unwrap();
    assert_eq!(b.old_root, g);
    assert_eq!(st.root(), b.new_root);
    assert_ne!(b.new_root, g);
    st.rollback(&b).unwrap();
    assert_eq!(st.root(), g);
    assert_eq!(st.occupied(), 0);
    assert!(st.slot_ballot(votes[0].slot).unwrap().is_none());

    // Prepare again and commit.
    let b = st.prepare(&votes, &mut rng, &none()).unwrap();
    let e = &b.expected;
    assert!(e.ok && e.fail_mask == 0);
    assert_eq!(e.root_before, g);
    assert_eq!(e.root_after, b.new_root);
    assert_eq!((e.voters, e.overwrites, e.occupied_before), (3, 0, 0));
    assert!(e.n_blobs >= 1);
    b.check_publics(&b.expected).unwrap();
    st.commit(&b).unwrap();
    assert_eq!(st.committed().root, b.new_root);
    assert_eq!(st.committed().voters, 3);
    assert_eq!(st.occupied(), 3);
    for v in &votes {
        assert!(st.has_vote_id(v.pkg.vote_id).unwrap());
        assert!(st.slot_ballot(v.slot).unwrap().is_some());
        st.vote_id_proof(v.pkg.vote_id).unwrap();
    }
    // The stored ballot is the re-encryption, not the submitted one.
    assert_ne!(
        st.slot_ballot(votes[0].slot).unwrap().unwrap(),
        votes[0].pkg.ballot
    );

    // Second batch: voter 0 overwrites, voter 3 is new. Two occupied slots
    // stay untouched, so the guest demands 2 refreshes.
    let votes2 = vec![
        fake_vote(&env, 0, &[3, 0], 999),
        fake_vote(&env, 3, &[0, 1], 998),
    ];
    let b2 = st.prepare(&votes2, &mut rng, &none()).unwrap();
    let e = &b2.expected;
    assert_eq!((e.voters, e.overwrites, e.occupied_before), (2, 1, 3));
    assert_eq!(
        b2.request.state.as_ref().unwrap().refresh_smt.len(),
        required_refresh(2, 1, 3)
    );
    st.commit(&b2).unwrap();
    assert_eq!(st.committed().voters, 5);
    assert_eq!(st.committed().overwrites, 1);
    assert_eq!(st.occupied(), 4);

    // Tally: decrypting the accumulator gives the field sums.
    let (_req, tally) = st.results_request(&env.sk, &mut rng).unwrap();
    // Votes standing: v1 [1,2], v2 [1,2], v0 overwrite [3,0], v3 [0,1].
    assert_eq!(&tally[..2], &[1 + 1 + 3, 2 + 2 + 1]);
    assert_eq!(&tally[2..], &[0u64; 14]);
}

/// `open_stored` reopens at the committed record the storage holds (the
/// one written with the slot store), rewinding a prepared batch; an empty
/// storage holds no process, and `open` still refuses another record.
#[test]
fn open_stored_reopens_at_the_stored_record() {
    let env = env(2, 8);
    let store = arbo::MemoryStorage::new();
    let err = ProcessState::open_stored(env.cfg.clone(), store.clone())
        .err()
        .unwrap();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("no process")),
        "{err:?}"
    );

    let mut st = ProcessState::create(env.cfg.clone(), store.clone()).unwrap();
    let mut rng = StdRng::seed_from_u64(5);
    let votes: Vec<_> = (0..3)
        .map(|i| fake_vote(&env, i, &[1, 2], 300 + i as u64))
        .collect();
    let b = st.prepare(&votes, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();
    let committed = st.committed();
    // A second batch prepared but never committed: the tree moved on.
    let more = vec![fake_vote(&env, 4, &[0, 1], 310)];
    let b2 = st.prepare(&more, &mut rng, &none()).unwrap();
    assert_eq!(st.root(), b2.new_root);
    drop(st);

    let st = ProcessState::open_stored(env.cfg.clone(), store.clone()).unwrap();
    assert_eq!(st.committed(), committed);
    assert_eq!(st.root(), committed.root);
    assert_eq!(st.occupied(), 3);
    assert!(st.has_vote_id(votes[0].pkg.vote_id).unwrap());
    assert!(!st.has_vote_id(more[0].pkg.vote_id).unwrap());
    drop(st);

    let mut stale = committed.clone();
    stale.root = genesis_root(&env.cfg).unwrap();
    assert!(ProcessState::open(env.cfg.clone(), store.clone(), &stale).is_err());
    ProcessState::open(env.cfg.clone(), store, &committed).unwrap();
}

/// Membership is the committed tree's: a prepared batch's vote ids are
/// not in it until the commit, though the tree's root already moved.
#[test]
fn has_vote_id_reads_the_committed_root() {
    let env = env(2, 8);
    let mut st = new_state(&env);
    let votes = vec![fake_vote(&env, 0, &[1, 2], 330)];
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(4), &none())
        .unwrap();
    assert_eq!(st.root(), b.new_root);
    assert!(!st.has_vote_id(votes[0].pkg.vote_id).unwrap());
    st.commit(&b).unwrap();
    assert!(st.has_vote_id(votes[0].pkg.vote_id).unwrap());
}

/// Shared storage whose `n`-th write from now fails (a crash there).
#[derive(Clone)]
struct CrashAt {
    inner: arbo::MemoryStorage,
    left: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

impl arbo::Storage for CrashAt {
    fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>, arbo::Error> {
        self.inner.get(k)
    }
    fn write(&self, b: arbo::WriteBatch) -> Result<(), arbo::Error> {
        if self.left.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Err(arbo::Error::Storage("crash".into()));
        }
        self.inner.write(b)
    }
    fn release_snapshot(&self) {}
}

/// A crash at any write of `create` (the genesis leaves go in one by one,
/// the record last) leaves a storage the next `create` finishes.
#[test]
fn create_recovers_from_a_crash_mid_genesis() {
    let env = env(2, 8);
    let genesis = genesis_root(&env.cfg).unwrap();
    for at in 0..10 {
        let s = CrashAt {
            inner: arbo::MemoryStorage::new(),
            left: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(at)),
        };
        let crashed = ProcessState::create(env.cfg.clone(), s.clone()).is_err();
        let st = match ProcessState::open_stored(env.cfg.clone(), s.clone()) {
            Ok(st) => {
                assert!(!crashed, "crash at write {at} left a record");
                st
            }
            Err(_) => ProcessState::create(env.cfg.clone(), s.clone())
                .unwrap_or_else(|e| panic!("crash at write {at}: {e}")),
        };
        assert_eq!(st.root(), genesis, "crash at write {at}");
        assert_eq!(st.committed().root, genesis);
        drop(st);
        assert_eq!(
            ProcessState::open_stored(env.cfg.clone(), s)
                .unwrap()
                .root(),
            genesis
        );
    }
}

#[test]
fn check_publics_bites_on_tampering() {
    let env = env(2, 4);
    let mut st = new_state(&env);
    let votes = vec![fake_vote(&env, 0, &[1], 42)];
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(2), &none())
        .unwrap();
    b.check_publics(&b.expected).unwrap();
    let mut bad = b.expected;
    bad.root_after[0] ^= 1;
    assert!(b.check_publics(&bad).is_err());
    let mut bad = b.expected;
    bad.fail_mask = 1 << 24;
    assert!(b.check_publics(&bad).is_err());
    let mut bad = b.expected;
    bad.occupied_before += 1;
    assert!(b.check_publics(&bad).is_err());
}

#[test]
fn prepare_rejects_bad_batches() {
    let env = env(2, 4);
    let mut st = new_state(&env);
    let g = st.root();
    let mut rng = StdRng::seed_from_u64(3);

    // Empty batch.
    assert!(st.prepare(&[], &mut rng, &none()).is_err());
    // Duplicate slot (same voter twice).
    let dup = vec![fake_vote(&env, 0, &[1], 1), fake_vote(&env, 0, &[2], 2)];
    assert!(st.prepare(&dup, &mut rng, &none()).is_err());
    assert_eq!(st.root(), g, "failed prepare must rewind the tree");
    // Foreign process id.
    let mut alien = fake_vote(&env, 1, &[1], 3);
    alien.pkg.process_id = Fr::from(99u64);
    assert!(st.prepare(&[alien], &mut rng, &none()).is_err());
    assert_eq!(st.root(), g);
}

#[test]
fn prepare_with_rejects_bad_refresh_selections() {
    let env = env(2, 8);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(5);

    // Occupy three slots.
    let first: Vec<_> = (0..3)
        .map(|i| fake_vote(&env, i, &[1], 200 + i as u64))
        .collect();
    let occ: Vec<u64> = first.iter().map(|v| v.slot).collect();
    let b = st.prepare(&first, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();
    let g = st.root();

    // Overwrite of voter 0: required refreshes = min(target, occ - w) = 2.
    let over = vec![fake_vote(&env, 0, &[2], 300)];
    let mut rest: Vec<u64> = occ[1..].to_vec();
    rest.sort_unstable();

    // Non-ascending selection.
    let desc: Vec<u64> = rest.iter().rev().copied().collect();
    assert!(st.prepare_with(&over, [7u8; 32], &desc, &none()).is_err());
    assert_eq!(st.root(), g, "failed prepare must rewind");

    // Selection overlapping the batch's own slot.
    let mut overlap = vec![occ[0], rest[0]];
    overlap.sort_unstable();
    assert!(
        st.prepare_with(&over, [7u8; 32], &overlap, &none())
            .is_err()
    );
    assert_eq!(st.root(), g);

    // The honest selection still works.
    st.prepare_with(&over, [7u8; 32], &rest, &none()).unwrap();
}

#[test]
fn select_batch_dedups_and_respects_max_voters() {
    let env = env(2, 8);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(4);

    // Commit voter 0 so a later vote by it is an overwrite.
    let first = vec![fake_vote(&env, 0, &[1], 10)];
    let b = st.prepare(&first, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();

    let pending = vec![
        fake_vote(&env, 1, &[1], 11),
        fake_vote(&env, 1, &[2], 12), // same slot: dropped
        fake_vote(&env, 2, &[1], 13),
        fake_vote(&env, 3, &[1], 14),
        fake_vote(&env, 0, &[2], 15), // overwrite: free under max_voters
        fake_vote(&env, 4, &[1], 16),
        first[0].clone(), // vote id already in the tree: dropped
    ];
    // max_voters 3: one already voted, so 2 new voters fit; the overwrite
    // always fits.
    let sel = st.select_batch(&pending, 3, &none(), usize::MAX).unwrap().0;
    let slots: Vec<u64> = sel.iter().map(|v| v.slot).collect();
    assert_eq!(
        slots,
        vec![pending[0].slot, pending[2].slot, pending[4].slot]
    );

    // Unlimited: everything but the duplicates.
    let (sel, blocked) = st
        .select_batch(&pending, 1_000, &none(), usize::MAX)
        .unwrap();
    assert_eq!(sel.len(), 5);
    assert!(!blocked);

    // A cap takes the oldest and does not count as a full transaction.
    let (sel, blocked) = st.select_batch(&pending, 1_000, &none(), 2).unwrap();
    let slots: Vec<u64> = sel.iter().map(|v| v.slot).collect();
    assert_eq!(slots, vec![pending[0].slot, pending[2].slot]);
    assert!(!blocked);
}

#[test]
fn select_batch_respects_blob_cap() {
    // 900 occupied 16-field slots, then a long overwrite queue: the batch
    // plus the refreshes the guest will demand must fit in 6 blobs.
    let env = env(16, 900);
    let mut st = new_state(&env);
    let seed: Vec<_> = (0..900)
        .map(|i| fake_vote(&env, i, &[1], 1_000 + i as u64))
        .collect();
    let b = st.prepare_with(&seed, [9u8; 32], &[], &none()).unwrap();
    st.commit(&b).unwrap();

    let pending: Vec<_> = (0..400)
        .map(|i| fake_vote(&env, i, &[2], 10_000 + i as u64))
        .collect();
    let (sel, blocked) = st
        .select_batch(&pending, 1_000_000, &none(), usize::MAX)
        .unwrap();
    let n = sel.len();
    assert!(n < 400 && blocked, "cap never hit");
    let r = |n: usize| required_refresh(n, n, 900);
    assert!(blob_count(n, n + r(n), 16) <= TX_BLOB_CAP);
    assert!(
        blob_count(n + 1, n + 1 + r(n + 1), 16) > TX_BLOB_CAP,
        "selection stopped early: {n}"
    );
}

#[test]
fn select_batch_respects_a_lower_blob_cap() {
    // A chain that takes two blobs per transaction: 400 new 16-field votes
    // fit six blobs, not two. The cap goes up to the guest's MAX_BLOBS.
    let env = env(16, 400);
    let mut st = new_state(&env);
    assert_eq!(st.blob_cap(), TX_BLOB_CAP);
    for bad in [0, MAX_BLOBS + 1] {
        let err = st.set_blob_cap(bad).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
    }
    assert_eq!(st.blob_cap(), TX_BLOB_CAP);
    st.set_blob_cap(MAX_BLOBS).unwrap();
    assert_eq!(st.blob_cap(), MAX_BLOBS);
    st.set_blob_cap(TX_BLOB_CAP).unwrap();

    let pending: Vec<VerifiedVote> = (0..400)
        .map(|i| fake_vote(&env, i, &[1], 30_000 + i as u64))
        .collect();
    let six = st
        .select_batch(&pending, u64::MAX, &none(), usize::MAX)
        .unwrap()
        .0
        .len();
    assert_eq!(six, 400);
    st.set_blob_cap(2).unwrap();
    let sel: Vec<VerifiedVote> = st
        .select_batch(&pending, u64::MAX, &none(), usize::MAX)
        .unwrap()
        .0
        .into_iter()
        .cloned()
        .collect();
    let n = sel.len();
    assert!(n > 0 && n < six, "no shrink: {n}");
    // An empty tree demands no refreshes.
    assert!(blob_count(n, n, 16) <= 2 && blob_count(n + 1, n + 1, 16) > 2);
    let b = st
        .prepare(&sel, &mut StdRng::seed_from_u64(14), &none())
        .unwrap();
    assert!(b.blobs.blobs.len() <= 2, "{} blobs", b.blobs.blobs.len());
    assert_eq!(b.expected.n_blobs as usize, b.blobs.blobs.len());
}

#[test]
fn exposed_refreshes_above_a_lower_blob_cap_are_refused() {
    // 260 exposed 16-field slots fit next to one vote in six blobs, not two.
    let env = env(16, 301);
    let mut st = committed_state(&env, 300);
    let vote = vec![fake_vote(&env, 300, &[1], 910_000)];
    let must = slots_of(0..260);
    assert!(blob_count(1, 261, 16) <= TX_BLOB_CAP && blob_count(1, 261, 16) > 2);
    let b = st
        .prepare(&vote, &mut StdRng::seed_from_u64(15), &must)
        .unwrap();
    st.rollback(&b).unwrap();

    st.set_blob_cap(2).unwrap();
    let err = st
        .select_batch(&vote, u64::MAX, &must, usize::MAX)
        .unwrap_err();
    assert!(matches!(err, Error::RefreshOverflow { .. }), "{err}");
    let g = st.root();
    let err = st
        .prepare(&vote, &mut StdRng::seed_from_u64(16), &must)
        .unwrap_err();
    assert!(matches!(err, Error::RefreshOverflow { .. }), "{err}");
    assert_eq!(st.root(), g);
}

#[test]
fn select_batch_never_repeats_a_slot() {
    let env = env(2, 8);
    let st = new_state(&env);
    // Two different voters forced onto one slot (an address-slot collision
    // the census check should have refused): only the first goes in.
    let a = fake_vote(&env, 1, &[1], 20);
    let mut b = fake_vote(&env, 2, &[1], 21);
    b.slot = a.slot;
    let c = fake_vote(&env, 3, &[1], 22);
    let pending = vec![a.clone(), b, c.clone()];
    let sel = st
        .select_batch(&pending, 1_000, &none(), usize::MAX)
        .unwrap()
        .0;
    let got: Vec<u64> = sel.iter().map(|v| v.pkg.vote_id).collect();
    assert_eq!(got, vec![a.pkg.vote_id, c.pkg.vote_id]);
}

// `n` committed voters (fake proofs), in batches the guest could take.
fn committed_state(env: &Env, n: usize) -> ProcessState<arbo::MemoryStorage> {
    let mut st = new_state(env);
    let mut rng = StdRng::seed_from_u64(77);
    let mut i = 0;
    while i < n {
        let end = (i + 1024).min(n);
        let batch: Vec<_> = (i..end)
            .map(|j| fake_vote(env, j, &[1], 100_000 + j as u64))
            .collect();
        let b = st.prepare(&batch, &mut rng, &none()).unwrap();
        st.commit(&b).unwrap();
        i = end;
    }
    st
}

fn slots_of(idx: impl Iterator<Item = usize>) -> BTreeSet<u64> {
    idx.map(|i| slot_key_address(&voter_address(i))).collect()
}

fn assert_sorted_unique(v: &[u64]) {
    assert!(v.windows(2).all(|w| w[0] < w[1]), "not strictly ascending");
}

#[test]
fn prepare_refresh_is_a_superset_of_must_include() {
    let env = env(2, 40);
    let mut st = committed_state(&env, 30);
    let mut rng = StdRng::seed_from_u64(8);
    // Voter 0 overwrites, voter 30 is new: 29 candidates, 16 required.
    let votes = vec![
        fake_vote(&env, 0, &[2], 500),
        fake_vote(&env, 30, &[1], 501),
    ];
    let required = required_refresh(2, 1, 30);
    assert_eq!(required, 16);
    let cands: BTreeSet<u64> = slots_of(1..30);

    // Ten live members, one the batch writes, one never occupied: the two
    // non-candidates are dropped, the rest topped up to the requirement.
    let mut must = slots_of(1..11);
    must.insert(votes[0].slot);
    must.insert(slot_key_address(&voter_address(35)));
    let b = st.prepare(&votes, &mut rng, &must).unwrap();
    let sel = b.refresh_slots().to_vec();
    assert_sorted_unique(&sel);
    assert_eq!(sel.len(), required);
    assert!(must.intersection(&cands).all(|s| sel.contains(s)));
    assert!(sel.iter().all(|s| cands.contains(s)));
    assert!(!sel.contains(&votes[0].slot));
    assert_eq!(
        b.request.state.as_ref().unwrap().refresh_smt.len(),
        required
    );
    st.rollback(&b).unwrap();

    // More live members than required: all of them, nothing else.
    let must = slots_of(1..21);
    let b = st.prepare(&votes, &mut rng, &must).unwrap();
    assert_eq!(b.refresh_slots(), must.iter().copied().collect::<Vec<_>>());
    st.rollback(&b).unwrap();

    // The retry pattern: each attempt feeds the last one's set back in.
    let mut exposed = none();
    for _ in 0..5 {
        let b = st.prepare(&votes, &mut rng, &exposed).unwrap();
        let sel: BTreeSet<u64> = b.refresh_slots().iter().copied().collect();
        assert!(exposed.is_subset(&sel));
        assert_eq!(sel.len(), required);
        st.rollback(&b).unwrap();
        exposed = sel;
    }
}

#[test]
fn prepare_with_rejects_a_selection_missing_must_include() {
    let env = env(2, 40);
    let mut st = committed_state(&env, 30);
    let votes = vec![fake_vote(&env, 30, &[1], 600)];
    let g = st.root();
    let all: Vec<u64> = slots_of(0..30).into_iter().collect();
    let must: BTreeSet<u64> = [all[20]].into();
    let without: Vec<u64> = all[..16].to_vec();
    let err = st
        .prepare_with(&votes, [3u8; 32], &without, &must)
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err}");
    assert_eq!(st.root(), g, "failed prepare must rewind");
    let mut with = all[..15].to_vec();
    with.push(all[20]);
    st.prepare_with(&votes, [3u8; 32], &with, &must).unwrap();
}

#[test]
fn refresh_top_up_is_uniform() {
    let env = env(2, 40);
    let st = committed_state(&env, 30);
    // One new vote: 30 candidates, 16 required, 6 forced, 10 drawn from 24.
    let votes = vec![fake_vote(&env, 30, &[1], 700)];
    let all: Vec<u64> = slots_of(0..30).into_iter().collect();
    let must: BTreeSet<u64> = all.iter().step_by(5).copied().collect();
    assert_eq!(must.len(), 6);
    let mut rng = StdRng::seed_from_u64(9);
    let trials = 3000usize;
    let mut hits = std::collections::HashMap::<u64, usize>::new();
    for _ in 0..trials {
        let sel = st.refresh_selection(&votes, &mut rng, &must).unwrap();
        assert_eq!(sel.len(), 16);
        assert_sorted_unique(&sel);
        assert!(must.iter().all(|s| sel.contains(s)));
        for s in sel {
            *hits.entry(s).or_default() += 1;
        }
    }
    // Each of the 24 others: Binomial(3000, 10/24), mean 1250, sd ~27.
    for s in all.iter().filter(|s| !must.contains(s)) {
        let h = hits[s];
        assert!((1090..=1410).contains(&h), "slot {s:#x}: {h} hits");
    }
}

#[test]
fn select_batch_shrinks_for_exposed_refreshes() {
    // 900 occupied 16-field slots, 400 new voters waiting. One update is
    // 32 cells and a quarter of an integer cell, so six blobs carry about 760.
    let env = env(16, 1300);
    let mut st = committed_state(&env, 900);
    let pending: Vec<VerifiedVote> = (900..1300)
        .map(|i| fake_vote(&env, i, &[1], 20_000 + i as u64))
        .collect();
    let fits = |n: usize, r: usize| blob_count(n, n + r, 16) <= TX_BLOB_CAP;

    let free = st
        .select_batch(&pending, u64::MAX, &none(), usize::MAX)
        .unwrap()
        .0
        .len();
    // 500 exposed slots outweigh the n refreshes a batch of new votes needs,
    // so the batch shrinks until votes plus exposed slots fit.
    let must = slots_of(0..500);
    let sel: Vec<VerifiedVote> = st
        .select_batch(&pending, u64::MAX, &must, usize::MAX)
        .unwrap()
        .0
        .into_iter()
        .cloned()
        .collect();
    let n = sel.len();
    assert!(n > 0 && n < free, "no shrink: {n} vs {free}");
    assert!(fits(n, 500) && !fits(n + 1, 500), "n = {n}");
    let b = st
        .prepare(&sel, &mut StdRng::seed_from_u64(10), &must)
        .unwrap();
    assert_eq!(b.refresh_slots(), must.iter().copied().collect::<Vec<_>>());
    assert!(b.blobs.blobs.len() <= TX_BLOB_CAP);
    st.rollback(&b).unwrap();

    // Exposed slots no single vote can carry: a typed error, from both.
    let must = slots_of(0..900);
    let err = st
        .select_batch(&pending, u64::MAX, &must, usize::MAX)
        .unwrap_err();
    assert!(matches!(err, Error::RefreshOverflow { .. }), "{err}");
    let g = st.root();
    let err = st
        .prepare(&pending[..1], &mut StdRng::seed_from_u64(11), &must)
        .unwrap_err();
    assert!(matches!(err, Error::RefreshOverflow { .. }), "{err}");
    assert_eq!(st.root(), g);
}

#[test]
fn select_covered_takes_only_votes_whose_history_it_read() {
    let env = env(2, 40);
    let st = committed_state(&env, 24);
    let a = fake_vote(&env, 30, &[1], 32_000);
    let b = fake_vote(&env, 31, &[1], 32_001);
    let cov = Coverage::for_votes(&[a.pkg.vote_id]);
    let pending = [b, a.clone()];
    let (sel, _, _) = st
        .select_covered(&pending, u64::MAX, &none(), &cov, usize::MAX, TX_BLOB_CAP)
        .unwrap();
    let ids: Vec<u64> = sel.iter().map(|v| v.pkg.vote_id).collect();
    assert_eq!(ids, vec![a.pkg.vote_id]);
}

#[test]
fn select_covered_charges_each_vote_its_own_history() {
    // 900 occupied 16-field slots; six blobs carry about 760 updates.
    let env = env(16, 1300);
    let mut st = committed_state(&env, 900);
    let fresh: Vec<VerifiedVote> = (900..1000)
        .map(|i| fake_vote(&env, i, &[1], 30_000 + i as u64))
        .collect();
    // A revote of slot 0 whose history is 700 live slots, queued last, and
    // a later revote of the same slot with no history.
    let heavy = fake_vote(&env, 0, &[2], 31_000);
    let later = fake_vote(&env, 0, &[3], 31_001);
    let hist = slots_of(0..700);
    let mut cov = Coverage::default();
    cov.add(hist.clone(), &[heavy.pkg.vote_id]);
    let mut pending = fresh.clone();
    pending.push(heavy.clone());
    pending.push(later.clone());
    let (sel, blocked, cover) = st
        .select_covered(&pending, u64::MAX, &none(), &cov, usize::MAX, TX_BLOB_CAP)
        .unwrap();
    // The fresh votes are not charged the heavy history; it waits, and the
    // later vote of its slot waits behind it.
    assert_eq!(sel.len(), 100);
    assert!(blocked);
    assert!(cover.is_empty());
    // As before: the history as one set shrinks the fresh batch.
    let legacy = st
        .select_batch(&pending, u64::MAX, &hist, usize::MAX)
        .unwrap()
        .0
        .len();
    assert!(legacy < 100, "{legacy}");
    // Alone, the heavy vote fits and its cover is exactly its history.
    let (sel, _, cover) = st
        .select_covered(
            &pending[100..],
            u64::MAX,
            &none(),
            &cov,
            usize::MAX,
            TX_BLOB_CAP,
        )
        .unwrap();
    assert_eq!(sel.len(), 1);
    assert_eq!(sel[0].pkg.vote_id, heavy.pkg.vote_id);
    assert_eq!(cover, hist);
    let votes: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(14), &cover)
        .unwrap();
    assert!(hist.iter().all(|s| b.updated_slots().contains(s)));
    st.rollback(&b).unwrap();
    // A history no transition can carry names its vote.
    let mut cov = Coverage::default();
    cov.add(slots_of(0..900), &[heavy.pkg.vote_id]);
    let err = st
        .select_covered(&pending, u64::MAX, &none(), &cov, usize::MAX, TX_BLOB_CAP)
        .unwrap_err();
    assert!(
        matches!(err, Error::CoverageOverflow { vote_id, .. } if vote_id == heavy.pkg.vote_id),
        "{err}"
    );
}

#[test]
fn exposed_refreshes_above_max_refresh_are_refused() {
    // One field per ballot, so MAX_REFRESH binds before the blob cap.
    let env = env(1, 2100);
    let mut st = committed_state(&env, MAX_REFRESH + 2);
    let vote = vec![fake_vote(&env, MAX_REFRESH + 2, &[1], 900_000)];
    let must = slots_of(0..MAX_REFRESH + 1);
    assert!(blob_count(1, 1 + must.len(), 1) <= TX_BLOB_CAP);
    let err = st
        .select_batch(&vote, u64::MAX, &must, usize::MAX)
        .unwrap_err();
    assert!(matches!(err, Error::RefreshOverflow { .. }), "{err}");
    let err = st
        .prepare(&vote, &mut StdRng::seed_from_u64(12), &must)
        .unwrap_err();
    assert!(matches!(err, Error::RefreshOverflow { .. }), "{err}");
    // Exactly MAX_REFRESH still goes through.
    let must = slots_of(0..MAX_REFRESH);
    assert_eq!(
        st.select_batch(&vote, u64::MAX, &must, usize::MAX)
            .unwrap()
            .0
            .len(),
        1
    );
    let b = st
        .prepare(&vote, &mut StdRng::seed_from_u64(13), &must)
        .unwrap();
    assert_eq!(b.refresh_slots().len(), MAX_REFRESH);
}

#[test]
fn reseal_without_a_vote_keeps_its_overwrite_hidden() {
    // Attempt 1: voter 0 overwrites, voter 30 is new. Its blobs go public
    // and the actor keeps every key they list as the exposed set.
    let env = env(2, 40);
    let mut st = committed_state(&env, 30);
    let mut rng = StdRng::seed_from_u64(15);
    let a = fake_vote(&env, 0, &[2], 800);
    let fresh = fake_vote(&env, 30, &[1], 801);
    let b1 = st
        .prepare(&[a.clone(), fresh.clone()], &mut rng, &none())
        .unwrap();
    let exposed: BTreeSet<u64> = b1.updated_slots().into_iter().collect();
    assert_eq!(exposed.len(), 2 + b1.refresh_slots().len());
    assert!(exposed.contains(&a.slot) && exposed.contains(&fresh.slot));
    st.rollback(&b1).unwrap();

    // Attempt 2 drops A's vote. A's slot must still change, or `U1 \ U2`
    // names it as an attempt-1 overwrite.
    let b2 = st.prepare(&[fresh], &mut rng, &exposed).unwrap();
    assert!(b2.refresh_slots().contains(&a.slot));
    let u2: BTreeSet<u64> = b2.updated_slots().into_iter().collect();
    let occupied = slots_of(0..30);
    assert!(exposed.intersection(&occupied).all(|s| u2.contains(s)));
}

// Sha256SmtLib.verifyInclusion, line for line: siblings root to leaf, leaf
// depth one past the last non-zero sibling (which must not be the last
// slot), key bits LSB first, leaf = sha256(key_le8 ‖ value_le32 ‖ 0x01).
fn registry_verify(root: &[u8; 32], key: u64, value_be: &[u8; 32], sibs: &[[u8; 32]]) -> bool {
    use sha2::{Digest, Sha256};
    let n = sibs.len();
    if n == 0 || n > 64 {
        return false;
    }
    let d = sibs
        .iter()
        .rposition(|s| *s != [0u8; 32])
        .map_or(0, |i| i + 1);
    if d == n {
        return false;
    }
    let mut value_le = *value_be;
    value_le.reverse();
    let mut h: [u8; 32] = Sha256::new()
        .chain_update(key.to_le_bytes())
        .chain_update(value_le)
        .chain_update([1u8])
        .finalize()
        .into();
    for i in (0..d).rev() {
        let (l, r) = if (key >> i) & 1 == 0 {
            (h, sibs[i])
        } else {
            (sibs[i], h)
        };
        h = Sha256::new()
            .chain_update(l)
            .chain_update(r)
            .finalize()
            .into();
    }
    h == *root
}

// `uint256(sha256(abi.encode(accumulator)))` as BE bytes.
fn acc_value(acc: &[davinci_zkvm_sdk::crypto::field::U256]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for c in acc {
        h.update(davinci_zkvm_sdk::crypto::field::u256_to_be(c));
    }
    h.finalize().into()
}

#[test]
fn dkg_results_inputs_verify_like_the_registry() {
    let env = env(3, 40);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(9);
    let check = |st: &ProcessState<arbo::MemoryStorage>| {
        let root = st.committed().root;
        let (acc, sibs) = st.dkg_results_inputs().unwrap();
        assert_eq!(sibs.len(), 64);
        assert_eq!(sibs[63], [0u8; 32]);
        // Leaf-hash order: field i is coords 4i..4i+4 (c1x, c1y, c2x, c2y).
        let coords = st.committed().accumulator.coords();
        for (a, c) in acc.iter().zip(coords.iter()) {
            assert_eq!(*a, davinci_zkvm_sdk::crypto::field::fr_to_u256(c));
        }
        assert!(registry_verify(&root, 4, &acc_value(&acc), &sibs));
        // It bites: a wrong coordinate, sibling, key or root fails.
        let mut bad = acc;
        bad[5] = davinci_zkvm_sdk::crypto::field::U256::from(7u64);
        assert!(!registry_verify(&root, 4, &acc_value(&bad), &sibs));
        let d = sibs.iter().rposition(|s| *s != [0u8; 32]).unwrap();
        let mut s2 = sibs.clone();
        s2[d][0] ^= 1;
        assert!(!registry_verify(&root, 4, &acc_value(&acc), &s2));
        assert!(!registry_verify(&root, 4, &acc_value(&acc), &sibs[..d + 1]));
        assert!(!registry_verify(&root, 5, &acc_value(&acc), &sibs));
        let mut r2 = root;
        r2[0] ^= 1;
        assert!(!registry_verify(&r2, 4, &acc_value(&acc), &sibs));
    };
    // Genesis: the identity accumulator.
    check(&st);
    for round in 0..2 {
        let votes: Vec<_> = (0..12)
            .map(|i| fake_vote(&env, i + 12 * round, &[1, 2, 0], 500 + i as u64))
            .collect();
        let b = st.prepare(&votes, &mut rng, &none()).unwrap();
        st.commit(&b).unwrap();
        check(&st);
    }
}

#[test]
fn a_512_vote_queue_at_sixteen_fields_seals_what_six_blobs_hold() {
    // The node's default --batch-max is 512. At 16 fields a steady batch
    // (one refresh per new vote) fits six blobs only up to 379 votes, so the
    // selection stops there and reports the transaction full.
    let env = env(16, 1024);
    let mut st = committed_state(&env, 512);
    let pending: Vec<VerifiedVote> = (512..1024)
        .map(|i| fake_vote(&env, i, &[1], 50_000 + i as u64))
        .collect();
    let (sel, blocked) = st.select_batch(&pending, u64::MAX, &none(), 512).unwrap();
    assert_eq!((sel.len(), blocked), (379, true));
    let sel: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
    let b = st
        .prepare(&sel, &mut StdRng::seed_from_u64(17), &none())
        .unwrap();
    assert_eq!(b.blobs.blobs.len(), TX_BLOB_CAP);
}

#[test]
fn a_512_vote_steady_batch_at_sixteen_fields_takes_nine_blobs_in_l1_mode() {
    // In L1-blob mode the per-batch cap is MAX_BLOBS: the node posts the
    // blobs in several transactions. A steady 512 x 16 batch (one refresh per
    // new vote) is nine blobs, all revotes thirteen: --batch-max 512 stays
    // under MAX_BLOBS at any field count.
    assert_eq!(blob_count(512, 512 + 512, 16), 9);
    assert_eq!(blob_count(512, 512 + 1024, 16), 13);
    let env = env(16, 1024);
    let mut st = committed_state(&env, 512);
    st.set_blob_cap(MAX_BLOBS).unwrap();
    let pending: Vec<VerifiedVote> = (512..1024)
        .map(|i| fake_vote(&env, i, &[1], 60_000 + i as u64))
        .collect();
    let (sel, blocked) = st.select_batch(&pending, u64::MAX, &none(), 512).unwrap();
    assert_eq!((sel.len(), blocked), (512, false));
    // A landing window that fits one six-blob transaction sizes it down to
    // what six blobs hold, as a one-transaction chain does.
    let (six, blocked) = st
        .select_batch_within(&pending, u64::MAX, &none(), 512, TX_BLOB_CAP)
        .unwrap();
    assert_eq!((six.len(), blocked), (379, true));
    // Never above the blob cap.
    let (all, _) = st
        .select_batch_within(&pending, u64::MAX, &none(), 512, MAX_BLOBS + 10)
        .unwrap();
    assert_eq!(all.len(), 512);
    let sel: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
    let b = st
        .prepare(&sel, &mut StdRng::seed_from_u64(18), &none())
        .unwrap();
    assert_eq!(b.blobs.blobs.len(), 9);
    assert_eq!(b.expected.n_blobs, 9);
}

#[test]
fn a_window_shortfall_is_not_a_refresh_overflow() {
    // 800 exposed 16-field slots overflow six blobs but fit 32: with a
    // six-blob window nothing fits for now, which is not a permanent error.
    let env = env(16, 1300);
    let mut st = committed_state(&env, 900);
    st.set_blob_cap(MAX_BLOBS).unwrap();
    let pending: Vec<VerifiedVote> = (900..910)
        .map(|i| fake_vote(&env, i, &[1], 70_000 + i as u64))
        .collect();
    let must = slots_of(0..800);
    assert!(blob_count(1, 801, 16) > TX_BLOB_CAP && blob_count(1, 801, 16) <= MAX_BLOBS);
    let (sel, blocked) = st
        .select_batch_within(&pending, u64::MAX, &must, usize::MAX, TX_BLOB_CAP)
        .unwrap();
    assert!(sel.is_empty() && blocked);
    let (sel, _) = st
        .select_batch(&pending, u64::MAX, &must, usize::MAX)
        .unwrap();
    assert!(!sel.is_empty());
}

#[test]
fn select_batch_within_clamps_to_the_blob_cap_and_rejects_zero() {
    let env = env(16, 1024);
    let st = committed_state(&env, 512);
    assert_eq!(st.blob_cap(), TX_BLOB_CAP);
    let pending: Vec<VerifiedVote> = (512..1024)
        .map(|i| fake_vote(&env, i, &[1], 80_000 + i as u64))
        .collect();
    let (sel, blocked) = st
        .select_batch_within(&pending, u64::MAX, &none(), 512, MAX_BLOBS)
        .unwrap();
    assert_eq!((sel.len(), blocked), (379, true));
    let err = st
        .select_batch_within(&pending, u64::MAX, &none(), 512, 0)
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err}");
}

#[test]
fn a_refresh_set_over_the_cap_still_overflows_in_a_short_window() {
    // MAX_REFRESH binds whatever the window, so a short window must not
    // turn the permanent overflow into a wait.
    let env = env(1, 2100);
    let mut st = committed_state(&env, MAX_REFRESH + 2);
    st.set_blob_cap(MAX_BLOBS).unwrap();
    let vote = vec![fake_vote(&env, MAX_REFRESH + 2, &[1], 900_001)];
    let must = slots_of(0..MAX_REFRESH + 1);
    let err = st
        .select_batch_within(&vote, u64::MAX, &must, usize::MAX, TX_BLOB_CAP)
        .unwrap_err();
    assert!(matches!(err, Error::RefreshOverflow { .. }), "{err}");
}
