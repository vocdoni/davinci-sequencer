//! Following a foreign transition: sequencer B rebuilds A's state from the
//! DA blob content alone and lands on the same root.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arbo::{MemoryStorage, Storage, WriteBatch};
use common::*;
use davinci_state::{PreparedBatch, ProcessState, VerifiedVote};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{TransitionData, decode_blobs};
use davinci_zkvm_sdk::census::slot_key_address;
use davinci_zkvm_sdk::crypto::elgamal::decrypt;
use davinci_zkvm_sdk::reenc::{ReencChain, reencrypt_ballot};
use rand::SeedableRng;
use rand::rngs::StdRng;

/// Storage whose write of the committed record fails while `fail` is set.
struct FlakyStorage {
    inner: MemoryStorage,
    fail: Arc<AtomicBool>,
}

impl Storage for FlakyStorage {
    fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>, arbo::Error> {
        self.inner.get(k)
    }
    fn write(&self, batch: WriteBatch) -> Result<(), arbo::Error> {
        let puts = batch.into_puts();
        if self.fail.load(Ordering::SeqCst) && puts.iter().any(|(k, _)| k == b"committed") {
            self.fail.store(false, Ordering::SeqCst);
            return Err(arbo::Error::Storage("injected failure".into()));
        }
        let mut b = WriteBatch::new();
        for (k, v) in puts {
            b.put(k, v);
        }
        self.inner.write(b)
    }
}

#[test]
fn follower_reaches_the_same_state_from_blobs() {
    let env = env(2, 8);
    let mut a = new_state(&env);
    let mut b = new_state(&env);
    let mut rng = StdRng::seed_from_u64(11);

    // Transition 1: three fresh votes.
    let votes: Vec<_> = (0..3)
        .map(|i| fake_vote(&env, i, &[1, 2], 50 + i as u64))
        .collect();
    let p1 = a.prepare(&votes, &mut rng, &none()).unwrap();
    a.commit(&p1).unwrap();
    let t1 = decode_blobs(&p1.blobs.blobs, 2).unwrap();
    b.apply_synced(&t1, p1.new_root).unwrap();
    assert_eq!(b.root(), a.root());
    assert_eq!(b.committed(), a.committed());
    assert_eq!(b.occupied(), a.occupied());
    for v in &votes {
        assert_eq!(
            b.slot_ballot(v.slot).unwrap(),
            a.slot_ballot(v.slot).unwrap()
        );
        assert!(b.has_vote_id(v.pkg.vote_id).unwrap());
    }

    // Transition 2: an overwrite plus a new voter (carries refreshes).
    let votes2 = vec![
        fake_vote(&env, 0, &[2, 1], 77),
        fake_vote(&env, 4, &[1, 0], 78),
    ];
    let p2 = a.prepare(&votes2, &mut rng, &none()).unwrap();
    a.commit(&p2).unwrap();
    let t2 = decode_blobs(&p2.blobs.blobs, 2).unwrap();
    b.apply_synced(&t2, p2.new_root).unwrap();
    assert_eq!(b.root(), a.root());
    assert_eq!(b.committed(), a.committed());
    assert_eq!(b.committed().voters, 5);
    assert_eq!(b.committed().overwrites, 1);
}

#[test]
fn tampered_blob_content_is_rejected() {
    let env = env(2, 4);
    let mut a = new_state(&env);
    let mut b = new_state(&env);
    let mut rng = StdRng::seed_from_u64(12);

    let votes = vec![
        fake_vote(&env, 0, &[1, 1], 60),
        fake_vote(&env, 1, &[2, 0], 61),
    ];
    let p = a.prepare(&votes, &mut rng, &none()).unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, 2).unwrap();

    // Swap one published ballot for another: the root cannot match.
    let mut bad = t.clone();
    bad.updates[0].1 = bad.updates[1].1;
    let g = b.root();
    let err = b.apply_synced(&bad, p.new_root).unwrap_err();
    assert!(matches!(err, davinci_state::Error::RootMismatch { .. }));
    assert_eq!(b.root(), g, "failed apply must rewind");
    assert_eq!(b.committed().voters, 0);

    // Wrong target root with honest content.
    let mut wrong = p.new_root;
    wrong[0] ^= 1;
    assert!(b.apply_synced(&t, wrong).is_err());
    assert_eq!(b.root(), g);

    // The honest transition still applies afterwards.
    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.root(), a.root());
}

#[test]
fn apply_rejects_malformed_transitions() {
    let env = env(2, 4);
    let mut a = new_state(&env);
    let mut b = new_state(&env);
    let votes = vec![fake_vote(&env, 0, &[1, 1], 80)];
    let p = a
        .prepare(&votes, &mut StdRng::seed_from_u64(13), &none())
        .unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, 2).unwrap();
    let g = b.root();
    let unchanged = |b: &ProcessState<MemoryStorage>| {
        assert_eq!(b.root(), g);
        assert_eq!(b.committed().voters, 0);
        assert_eq!(b.occupied(), 0);
    };

    // Update targeting a config key below the ballot namespace.
    let mut bad = t.clone();
    bad.updates[0].0 = 0x04;
    assert!(b.apply_synced(&bad, p.new_root).is_err());
    unchanged(&b);

    // Vote id below 2^63.
    let mut bad = t.clone();
    bad.vote_ids[0] = 5;
    assert!(b.apply_synced(&bad, p.new_root).is_err());
    unchanged(&b);

    // Field count not this process's.
    let mut bad = t.clone();
    bad.num_fields = 3;
    assert!(b.apply_synced(&bad, p.new_root).is_err());
    unchanged(&b);

    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.root(), a.root());
}

#[test]
fn failed_persist_leaves_memory_at_the_committed_root() {
    let env = env(2, 4);
    let mut a = new_state(&env);
    let fail = Arc::new(AtomicBool::new(false));
    let mut b = ProcessState::create(
        env.cfg.clone(),
        FlakyStorage {
            inner: MemoryStorage::new(),
            fail: fail.clone(),
        },
    )
    .unwrap();

    let votes = vec![
        fake_vote(&env, 0, &[1, 2], 90),
        fake_vote(&env, 1, &[2, 0], 91),
    ];
    let p = a
        .prepare(&votes, &mut StdRng::seed_from_u64(14), &none())
        .unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, 2).unwrap();

    // The persist write fails once: memory must not run ahead of the tree.
    let g = b.root();
    fail.store(true, Ordering::SeqCst);
    assert!(b.apply_synced(&t, p.new_root).is_err());
    assert_eq!(b.root(), g);
    assert_eq!(b.committed().voters, 0);
    assert_eq!(b.occupied(), 0);

    // The honest transition then applies on the same state.
    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.root(), a.root());
    assert_eq!(b.committed(), a.committed());
    assert_eq!(b.occupied(), a.occupied());
}

// Leader `a` proves and commits `votes`; follower `b` recomputes the Results
// ballot from the blobs and its stored ballots, then applies the transition.
fn step(
    a: &mut ProcessState<MemoryStorage>,
    b: &mut ProcessState<MemoryStorage>,
    votes: &[VerifiedVote],
    rng: &mut StdRng,
) -> PreparedBatch {
    let p = a.prepare(votes, rng, &none()).unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, a.config().ballot_mode.num_fields).unwrap();
    assert_eq!(b.results_after(&t).unwrap(), a.committed().accumulator);
    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.committed(), a.committed());
    p
}

fn slot(i: usize) -> u64 {
    slot_key_address(&voter_address(i))
}

#[test]
fn results_after_matches_the_guest_leaf() {
    let env = env(2, 8);
    let mut a = new_state(&env);
    let mut b = new_state(&env);
    let mut rng = StdRng::seed_from_u64(31);
    let vote = |i: usize, f: &[u64], k: u64| fake_vote(&env, i, f, k);

    // First batch: new votes on the genesis (identity) Results ballot.
    let p = step(
        &mut a,
        &mut b,
        &[
            vote(0, &[1, 0], 400),
            vote(1, &[0, 1], 401),
            vote(2, &[1, 1], 402),
        ],
        &mut rng,
    );
    assert!(p.refresh_slots().is_empty());

    // A new vote and three refreshes, voter 0's slot among them.
    let p = step(&mut a, &mut b, &[vote(3, &[1, 0], 403)], &mut rng);
    assert!(p.refresh_slots().contains(&slot(0)));

    // Voter 0 revotes on the slot refreshed one batch ago, voter 4 is new.
    let p = step(
        &mut a,
        &mut b,
        &[vote(0, &[0, 1], 404), vote(4, &[1, 0], 405)],
        &mut rng,
    );
    assert_eq!((p.overwrites, p.refresh_slots().len()), (1, 3));

    // Only an overwrite, plus refreshes.
    let p = step(&mut a, &mut b, &[vote(1, &[1, 1], 406)], &mut rng);
    assert_eq!((p.overwrites, p.refresh_slots().len()), (1, 4));

    // Only refreshes. The guest wants at least one vote, so no leader builds
    // this; the follower still computes it, and re-encryption keeps the tally.
    let mut chain = ReencChain::new(&[7u8; 32], &b.root());
    let mut slots: Vec<u64> = (0..5).map(slot).collect();
    slots.sort_unstable();
    let updates = slots
        .into_iter()
        .map(|k| {
            let old = b.slot_ballot(k).unwrap().unwrap();
            (k, reencrypt_ballot(&old, &env_pk(&env), 2, &mut chain))
        })
        .collect();
    let t = TransitionData {
        vote_ids: vec![],
        updates,
        num_fields: 2,
    };
    let before = b.committed().accumulator;
    let after = b.results_after(&t).unwrap();
    assert_ne!(after, before);
    for f in 0..2 {
        let tally = decrypt(&env.sk, &before.0[f], 100);
        assert!(tally.is_some());
        assert_eq!(decrypt(&env.sk, &after.0[f], 100), tally, "field {f}");
    }
}

fn slot_db_key(i: usize) -> Vec<u8> {
    [b"slot/".as_slice(), &slot(i).to_be_bytes()].concat()
}

fn put(store: &MemoryStorage, k: Vec<u8>, v: Vec<u8>) {
    let mut w = WriteBatch::new();
    w.put(k, v);
    store.write(w).unwrap();
}

#[test]
fn corrupted_stored_ballot_fails_the_root_check() {
    let env = env(2, 4);
    let mut a = new_state(&env);
    let store = MemoryStorage::new();
    let mut b = ProcessState::create(env.cfg.clone(), store.clone()).unwrap();
    let mut rng = StdRng::seed_from_u64(32);
    let votes = vec![
        fake_vote(&env, 0, &[1, 2], 500),
        fake_vote(&env, 1, &[2, 0], 501),
    ];
    step(&mut a, &mut b, &votes, &mut rng);

    // Voter 0's stored ballot becomes voter 1's: a valid encoding, the wrong value.
    let honest = store.get(&slot_db_key(0)).unwrap().unwrap();
    put(
        &store,
        slot_db_key(0),
        store.get(&slot_db_key(1)).unwrap().unwrap(),
    );

    // Voter 0 revotes: the follower subtracts the wrong old ballot.
    let p = a
        .prepare(&[fake_vote(&env, 0, &[2, 2], 502)], &mut rng, &none())
        .unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, 2).unwrap();
    let g = b.root();
    let err = b.apply_synced(&t, p.new_root).unwrap_err();
    assert!(
        matches!(err, davinci_state::Error::RootMismatch { .. }),
        "{err}"
    );
    assert_eq!(b.root(), g);

    // With the honest ballot back the same transition applies.
    put(&store, slot_db_key(0), honest);
    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.committed(), a.committed());
}

#[test]
fn occupied_slot_without_a_ballot_is_an_error() {
    let env = env(2, 4);
    let mut a = new_state(&env);
    let store = MemoryStorage::new();
    let mut b = ProcessState::create(env.cfg.clone(), store.clone()).unwrap();
    let votes = vec![fake_vote(&env, 0, &[1, 2], 600)];
    step(&mut a, &mut b, &votes, &mut StdRng::seed_from_u64(33));

    // List a slot as occupied without storing its ballot, then reopen.
    let ghost = 0x20u64;
    let mut occ = store.get(b"occ").unwrap().unwrap();
    occ.extend_from_slice(&ghost.to_le_bytes());
    let mut slots: Vec<u64> = occ
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    slots.sort_unstable();
    put(
        &store,
        b"occ".to_vec(),
        slots.iter().flat_map(|s| s.to_le_bytes()).collect(),
    );
    let b = ProcessState::open(env.cfg.clone(), store.clone(), &b.committed()).unwrap();

    let t = TransitionData {
        vote_ids: vec![],
        updates: vec![(ghost, Ballot::identity())],
        num_fields: 2,
    };
    let err = b.results_after(&t).unwrap_err();
    assert!(
        matches!(&err, davinci_state::Error::Invalid(m) if m.contains("no stored ballot")),
        "{err:?}"
    );
}
