//! Replaying hostile transitions: whatever a peer's blobs decode to, the
//! follower applies only what lands on the published root, and a refused
//! transition leaves nothing behind, in memory or in storage.

mod common;

use arbo::MemoryStorage;
use common::*;
use davinci_state::{Committed, ProcessState};
use davinci_zkvm_sdk::blob::{TransitionData, decode_blobs};
use rand::SeedableRng;
use rand::rngs::StdRng;

/// The follower's whole view: memory, and the state reopened from storage.
fn snapshot(b: &ProcessState<MemoryStorage>, store: &MemoryStorage, env: &Env) -> Committed {
    let reopened = ProcessState::open_stored(env.cfg.clone(), store.clone()).unwrap();
    assert_eq!(
        reopened.committed(),
        b.committed(),
        "storage and memory differ"
    );
    assert_eq!(reopened.occupied(), b.occupied());
    assert_eq!(reopened.root(), b.root());
    b.committed()
}

/// A leader and a follower one honest transition in, with occupied slots.
fn one_in(
    env: &Env,
) -> (
    ProcessState<MemoryStorage>,
    ProcessState<MemoryStorage>,
    MemoryStorage,
) {
    let mut a = new_state(env);
    let store = MemoryStorage::new();
    let mut b = ProcessState::create(env.cfg.clone(), store.clone()).unwrap();
    let votes: Vec<_> = (0..3)
        .map(|i| fake_vote(env, i, &[1, 2], 30 + i as u64))
        .collect();
    let p = a
        .prepare(&votes, &mut StdRng::seed_from_u64(1), &none())
        .unwrap();
    a.commit(&p).unwrap();
    b.apply_synced(&decode_blobs(&p.blobs.blobs, 2).unwrap(), p.new_root)
        .unwrap();
    (a, b, store)
}

#[test]
fn hostile_transitions_are_refused_and_leave_nothing_behind() {
    let env = env(2, 8);
    let (mut a, mut b, store) = one_in(&env);
    let before = snapshot(&b, &store, &env);
    let slots_before: Vec<_> = (0..3)
        .map(|i| {
            b.slot_ballot(fake_vote(&env, i, &[1, 2], 30 + i as u64).slot)
                .unwrap()
        })
        .collect();
    // The leader's honest second transition: an overwrite, a new voter and
    // the refreshes of occupied slots.
    let votes = vec![
        fake_vote(&env, 0, &[2, 2], 40),
        fake_vote(&env, 5, &[1, 0], 41),
    ];
    let p = a
        .prepare(&votes, &mut StdRng::seed_from_u64(2), &none())
        .unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, 2).unwrap();
    assert!(t.updates.len() > t.vote_ids.len(), "carries refreshes");

    let mut cases: Vec<(&str, TransitionData)> = Vec::new();
    // A vote id twice, and one already in the tree.
    let mut d = t.clone();
    d.vote_ids.push(d.vote_ids[0]);
    cases.push(("vote id twice", d));
    let mut d = t.clone();
    d.vote_ids[0] = fake_vote(&env, 1, &[1, 2], 31).pkg.vote_id;
    cases.push(("vote id already settled", d));
    // A slot twice: a new one, and an occupied one.
    let new_slot = votes[1].slot;
    let mut d = t.clone();
    let u = *d.updates.iter().find(|(s, _)| *s == new_slot).unwrap();
    d.updates.push(u);
    cases.push(("new slot twice", d));
    let old_slot = votes[0].slot;
    let mut d = t.clone();
    let u = *d.updates.iter().find(|(s, _)| *s == old_slot).unwrap();
    d.updates.push(u);
    cases.push(("occupied slot twice", d));
    // Updates dropped or added, ballots swapped. (Their order is not part of
    // the state: the tree root is the same in any order.)
    let mut d = t.clone();
    d.updates.pop();
    cases.push(("refresh dropped", d));
    let mut d = t.clone();
    d.updates.push((0x7fff_ffff_ffff_fff0, d.updates[0].1));
    cases.push(("slot added", d));
    let mut d = t.clone();
    let (x, y) = (d.updates[0].1, d.updates[1].1);
    d.updates[0].1 = y;
    d.updates[1].1 = x;
    cases.push(("ballots swapped", d));
    // A slot past the namespace, after valid writes to occupied slots.
    let mut d = t.clone();
    d.updates.push((u64::MAX, d.updates[0].1));
    cases.push(("slot at u64::MAX", d));
    let mut d = t.clone();
    d.updates
        .push((davinci_zkvm_sdk::limits::VOTE_ID_MIN, d.updates[0].1));
    cases.push(("slot at the vote-id namespace", d));
    // Votes dropped or invented.
    let mut d = t.clone();
    d.vote_ids.clear();
    cases.push(("no vote ids", d));
    let mut d = t.clone();
    d.vote_ids.push(u64::MAX);
    cases.push(("vote id added", d));
    // Nothing at all.
    cases.push((
        "empty",
        TransitionData {
            vote_ids: vec![],
            updates: vec![],
            num_fields: 2,
        },
    ));

    for (name, d) in &cases {
        assert!(b.apply_synced(d, p.new_root).is_err(), "{name} applied");
        assert_eq!(snapshot(&b, &store, &env), before, "{name} moved the state");
        for (i, want) in slots_before.iter().enumerate() {
            let slot = fake_vote(&env, i, &[1, 2], 30 + i as u64).slot;
            assert_eq!(&b.slot_ballot(slot).unwrap(), want, "{name} wrote a slot");
        }
        assert!(!b.has_vote_id(votes[0].pkg.vote_id).unwrap(), "{name}");
    }
    // An honest transition onto a root the chain never published.
    let mut wrong = p.new_root;
    wrong[31] ^= 1;
    assert!(b.apply_synced(&t, wrong).is_err());
    assert_eq!(snapshot(&b, &store, &env), before);

    // After every refusal the honest transition still applies.
    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.root(), a.root());
    assert_eq!(snapshot(&b, &store, &env), a.committed());
}

/// The same transition applied twice: the second is refused (its vote ids
/// are in the tree), and the state stays at the first.
#[test]
fn a_transition_replayed_on_its_own_result_is_refused() {
    let env = env(2, 8);
    let (a, mut b, store) = one_in(&env);
    let at = snapshot(&b, &store, &env);
    assert_eq!(at, a.committed());
    let p_again = {
        let mut a2 = new_state(&env);
        let votes: Vec<_> = (0..3)
            .map(|i| fake_vote(&env, i, &[1, 2], 30 + i as u64))
            .collect();
        a2.prepare(&votes, &mut StdRng::seed_from_u64(1), &none())
            .unwrap()
    };
    let t = decode_blobs(&p_again.blobs.blobs, 2).unwrap();
    assert!(b.apply_synced(&t, p_again.new_root).is_err());
    assert_eq!(snapshot(&b, &store, &env), at);
}
