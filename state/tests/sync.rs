//! Following a foreign transition: sequencer B rebuilds A's state from the
//! DA blob content alone and lands on the same root.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arbo::{MemoryStorage, Storage, WriteBatch};
use common::*;
use davinci_state::ProcessState;
use davinci_zkvm_sdk::blob::decode_blobs;
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
    bad.updates[0].1 = bad.accumulator;
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
