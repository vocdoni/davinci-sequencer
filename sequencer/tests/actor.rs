//! Actor/monitor/finalizer tests on the shared scripted fakes in
//! `common/` (fake chain, prover, blobs, clock).

mod common;

use common::*;

#[tokio::test]
async fn happy_path_five_votes_one_transition() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        5,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;

    let votes: Vec<VerifiedVote> = (0..5)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    // A resubmit of the same vote id is refused.
    assert!(matches!(
        h.submit(votes[0].clone()).await,
        Err(ActorError::Duplicate(_))
    ));

    wait_until("all settled", async || all_settled(&h, &vids).await).await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.root, s.chain.root());
    assert_eq!(snap.voters, 5);
    assert_eq!(snap.pending, 0);
    assert!(!snap.in_flight);
    assert_eq!(s.chain.voters(), 5);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 0);
    assert_eq!(prover.calls(), 1);
    // The settled vote has an inclusion proof under the committed root.
    h.vote_id_proof(vids[0]).await.unwrap();
    assert!(matches!(
        h.vote_id_proof(42).await,
        Err(ActorError::NotFound)
    ));
    shutdown.cancel();
}

#[tokio::test]
async fn race_loser_syncs_and_settles_next() {
    let s = setup(2, 8, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::gated();
    let prover_b = FakeProver::gated();
    let chain_a = s.chain.with_signer(0xa1);
    let chain_b = s.chain.with_signer(0xb2);
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        chain_a,
        prover_a.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        chain_b,
        prover_b.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    let va: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 10 + i as u64))
        .collect();
    let vb: Vec<VerifiedVote> = (2..4)
        .map(|i| fake_vote(&s.env, i, &[3, 1], 20 + i as u64))
        .collect();
    let vids_a: Vec<u64> = va.iter().map(|v| v.pkg.vote_id).collect();
    let vids_b: Vec<u64> = vb.iter().map(|v| v.pkg.vote_id).collect();
    for v in &va {
        ha.submit(v.clone()).await.unwrap();
    }
    for v in &vb {
        hb.submit(v.clone()).await.unwrap();
    }

    // Both sealed a batch on the genesis root and sit in the prover.
    wait_until("both proving", async || {
        prover_a.calls() >= 1 && prover_b.calls() >= 1
    })
    .await;
    // A wins the race.
    prover_a.release(100);
    wait_until("A settled", async || s.chain.voters() == 2).await;
    // B loses, syncs A's blobs, rebuilds and settles its votes next.
    prover_b.release(100);
    wait_until("B settled", async || {
        all_settled(&hb, &vids_b).await && s.chain.voters() == 4
    })
    .await;

    assert_eq!(node_b.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(node_b.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(node_b.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    assert_eq!(node_a.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    assert_eq!(node_a.metrics.lost_races.load(Ordering::SeqCst), 0);
    // A follows B's transition; both nodes land on the chain root.
    wait_until("A synced", async || {
        ha.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(node_a.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(hb.snapshot().await.unwrap().root, s.chain.root());
    // No vote lost or duplicated: 4 voters on chain, everything settled.
    assert!(all_settled(&ha, &vids_a).await);
    assert_eq!(s.chain.voters(), 4);
    shutdown.cancel();
}

#[tokio::test]
async fn duplicate_vote_settles_once() {
    let s = setup(2, 8, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::gated();
    let prover_b = FakeProver::gated();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        prover_b.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    let v = fake_vote(&s.env, 0, &[2, 2], 7);
    let vid = v.pkg.vote_id;
    ha.submit(v.clone()).await.unwrap();
    hb.submit(v.clone()).await.unwrap();
    wait_until("both proving", async || {
        prover_a.calls() >= 1 && prover_b.calls() >= 1
    })
    .await;
    prover_a.release(100);
    wait_until("A settled", async || s.chain.voters() == 1).await;
    prover_b.release(100);
    // B marks the vote settled after syncing the winner's transition;
    // no second transition happens.
    wait_until("B settled via sync", async || {
        vote_status(&hb, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    assert_eq!(node_b.metrics.settled_by_self.load(Ordering::SeqCst), 0);
    assert_eq!(node_b.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(node_b.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(vote_status(&ha, vid).await, Some(VoteStatus::Settled));
    shutdown.cancel();
}

/// An organizer END does not close out: the batch in flight lands in the
/// grace, admission closes at the (collapsed) end.
#[tokio::test]
async fn end_event_flushes_instead_of_closing() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;

    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 0], 30 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("proving", async || prover.calls() >= 1).await;
    // The election ends while the batch is in flight.
    s.chain.set_status(ProcessStatus::Ended);
    wait_until("actor sees end", async || {
        h.snapshot().await.unwrap().status == ProcessStatus::Ended
    })
    .await;
    // New votes are refused.
    let late = fake_vote(&s.env, 3, &[1, 1], 99);
    assert!(matches!(h.submit(late).await, Err(ActorError::Closed(_))));
    prover.release(100);
    wait_until("settled in the grace", async || {
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(s.chain.voters(), 2);
    let snap = h.snapshot().await.unwrap();
    assert!(!snap.in_flight);
    assert_eq!(snap.pending, 0);
    assert!(snap.synced);
    shutdown.cancel();
}

#[tokio::test]
async fn guest_failure_errors_votes_no_retry() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_mask.lock().unwrap() = Some(1 << 16); // census bit
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let genesis = genesis_root(&s.env.cfg).unwrap();

    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("vote errored", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Error)
    })
    .await;
    let sv = h.status(vid).await.unwrap().unwrap();
    assert!(sv.error.unwrap().contains("census"), "fail bits named");
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.root, genesis);
    assert_eq!(snap.pending, 0);
    // No retry: the prover is not called again.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(prover.calls(), 1);
    assert_eq!(s.chain.voters(), 0);
    assert_eq!(node.metrics.guest_rejections.load(Ordering::SeqCst), 1);
    assert_eq!(node.metrics.bisections.load(Ordering::SeqCst), 0);
    shutdown.cancel();
}

/// A proof our own checks refuse (here the program vk pin) holds the votes
/// pending: requeued, never errored, settled once a proof passes.
#[tokio::test]
async fn tampered_program_vk_holds_the_votes() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.tamper_vk.lock().unwrap() = true;
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("vote settled on the retry", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(prover.calls(), 2);
    assert_eq!(node.metrics.held_batches.load(Ordering::SeqCst), 1);
    assert_eq!(node.metrics.bisections.load(Ordering::SeqCst), 0);
    assert_eq!(s.chain.voters(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn deadline_margin_blocks_sealing() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    // Landing margin larger than the whole election: the time budget fits
    // no vote, so nothing ever seals.
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "48h",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prover.calls(), 0);
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Pending));
    // Past the end it still waits: the grace may extend.
    s.chain.advance_time(DURATION + 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Pending));
    // Once the grace closes the pending vote becomes an error.
    s.chain.pass_grace();
    wait_until("vote errored at grace end", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Error)
    })
    .await;
    assert_eq!(prover.calls(), 0);
    shutdown.cancel();
}

#[tokio::test]
async fn finalize_with_key_sets_results() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;

    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    let mut want = vec![0u64; 16];
    want[0] = 3;
    want[1] = 5;
    assert_eq!(s.chain.results(), want);
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    // Sequencer mode never touches the DKG calls.
    assert_eq!(s.chain.dkg_calls(), ((0, 0), (0, 0)));
    shutdown.cancel();
}

/// The contract accepts results for a process paused past its end, so
/// the key holder finalizes it like a Ready one.
#[tokio::test]
async fn paused_past_end_still_finalizes() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    // Paused, then the window closes: finalize must still fire.
    s.chain.set_status(ProcessStatus::Paused);
    s.chain.advance_time(DURATION + 1);
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    shutdown.cancel();
}

/// A paused process still accepts votes; they queue and settle once the
/// process resumes.
#[tokio::test]
async fn paused_process_queues_votes_until_resume() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    s.chain.set_status(ProcessStatus::Paused);
    wait_until("actor sees the pause", async || {
        h.snapshot().await.unwrap().status == ProcessStatus::Paused
    })
    .await;
    let v = fake_vote(&s.env, 0, &[1, 2], 100);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    // Queued, not sealed: no prover call while paused.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prover.calls(), 0, "sealed a batch while paused");
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Pending));
    s.chain.set_status(ProcessStatus::Ready);
    wait_until("settled after resume", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    shutdown.cancel();
}

/// The organizer pauses while a batch is in flight: its settlement reverts
/// `InvalidStatus` and the votes go back to pending, not to an error. They
/// wait out the pause (still reported, still accepting) and settle on
/// resume. `seen`: whether the node has the pause event by then, or only a
/// fresh chain read shows it.
async fn pause_during_flight(seen: bool) {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 0], 130 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("proving", async || prover.calls() >= 1).await;
    if !seen {
        s.chain.hide_block(Some(s.chain.head_block() + 1));
    }
    s.chain.set_status(ProcessStatus::Paused);
    if seen {
        wait_until("actor sees the pause", async || {
            h.snapshot().await.unwrap().status == ProcessStatus::Paused
        })
        .await;
    }
    prover.release(100);
    let no_error = async || {
        for vid in &vids {
            assert_ne!(
                vote_status(&h, *vid).await,
                Some(VoteStatus::Error),
                "a paused batch errored its votes"
            );
        }
    };
    wait_until("votes back to pending", async || {
        no_error().await;
        let snap = h.snapshot().await.unwrap();
        !snap.in_flight && snap.pending == 2
    })
    .await;
    // Past the requeue cooldown: still no second proof, event or not.
    tokio::time::sleep(Duration::from_secs(3)).await;
    no_error().await;
    assert_eq!(s.chain.voters(), 0);
    assert_eq!(prover.calls(), 1, "sealed while paused");
    for vid in &vids {
        assert_eq!(vote_status(&h, *vid).await, Some(VoteStatus::Pending));
    }
    if seen {
        let snap = h.snapshot().await.unwrap();
        assert_eq!(snap.status, ProcessStatus::Paused);
        assert!(snap.accepting, "a paused process takes votes");
    }
    s.chain.hide_block(None);
    s.chain.set_status(ProcessStatus::Ready);
    wait_until("settled after the resume", async || {
        no_error().await;
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(s.chain.voters(), 2);
    shutdown.cancel();
}

#[tokio::test]
async fn pause_during_flight_requeues_until_resume() {
    pause_during_flight(true).await;
}

#[tokio::test]
async fn pause_the_node_has_not_seen_requeues_too() {
    pause_during_flight(false).await;
}

/// A batch refused by a pause, then the process ends instead of resuming:
/// pause defers, so the requeued vote settles in the grace.
#[tokio::test]
async fn end_from_paused_flushes() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 0], 140);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("proving", async || prover.calls() >= 1).await;
    s.chain.set_status(ProcessStatus::Paused);
    wait_until("actor sees the pause", async || {
        h.snapshot().await.unwrap().status == ProcessStatus::Paused
    })
    .await;
    prover.release(100);
    wait_until("vote back to pending", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Pending)
            && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    s.chain.set_status(ProcessStatus::Ended);
    prover.release(100);
    wait_until("settled in the grace", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    shutdown.cancel();
}

/// Before its start time a process refuses votes as not open yet (a sealed
/// batch would revert `InvalidTimeBounds`); from the start it takes them.
#[tokio::test]
async fn votes_before_the_start_are_refused() {
    let s = setup(2, 8, None);
    s.chain.set_start_time(T0 + 1000);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 100);
    let vid = v.pkg.vote_id;
    assert!(matches!(
        h.submit(v.clone()).await,
        Err(ActorError::NotStarted(t)) if t == T0 + 1000
    ));
    assert!(!h.snapshot().await.unwrap().accepting);
    assert_eq!(vote_status(&h, vid).await, None, "nothing stored");
    s.chain.advance_time(1000);
    wait_until("open at the start", async || {
        h.snapshot().await.unwrap().accepting
    })
    .await;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    assert_eq!(prover.calls(), 1);
    shutdown.cancel();
}

/// Queued votes are not sealed while the head reads before the start (a
/// lagging endpoint after a restart): the batch could only revert.
#[tokio::test]
async fn no_seal_while_the_head_is_before_the_start() {
    let s = setup(2, 8, None);
    s.chain.set_start_time(T0);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        3,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 110 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    shutdown.cancel();
    drop(node);

    // Restarted behind an endpoint whose head predates the start, with a
    // batch size the two queued votes fill.
    s.chain.set_time(T0 - 60);
    let prover = FakeProver::open();
    let shutdown = CancellationToken::new();
    let node = start_node(
        reopen_db(dir.path()).await,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prover.calls(), 0, "sealed before the start");
    assert_eq!(h.snapshot().await.unwrap().pending, 2);
    s.chain.set_time(T0);
    wait_until("settled from the start", async || {
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(s.chain.voters(), 2);
    shutdown.cancel();
}

#[tokio::test]
async fn node_without_key_does_not_finalize() {
    // Not a derived key at all.
    does_not_finalize(setup(2, 8, None)).await;
}

/// Another node's key for the very same pid, chain and registry is
/// not ours: its master secret differs.
#[tokio::test]
async fn other_nodes_key_is_not_ours() {
    let other = TempDir::new().unwrap();
    let pk = node_key(&Db::open_in(other.path()).unwrap());
    does_not_finalize(setup(2, 8, Some(pk))).await;
}

async fn does_not_finalize(s: TestSetup) {
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(s.chain.results().is_empty());
    assert_ne!(h.snapshot().await.unwrap().local, LocalStatus::Finalized);
    // Not even attempted: the SDK would also refuse the wrong key, which
    // hides a missing ownership check behind a failed attempt.
    assert_eq!(node.metrics.finalize_attempts.load(Ordering::SeqCst), 0);
    shutdown.cancel();
}

// ------------------------------- bootstrap, sync and admission recovery

/// A transient failure fetching the process on ProcessCreated must not
/// lose the election; the monitor keeps last_block and retries the range.
#[tokio::test]
async fn bootstrap_retries_after_process_fetch_failure() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    s.chain.fail_process(1); // the first bootstrap attempt fails
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    shutdown.cancel();
}

/// A transient census fetch failure retries instead of permanently
/// ignoring the process.
#[tokio::test]
async fn census_transient_failure_retries_not_ignores() {
    let s = setup(2, 8, None);
    let census_path = s.census_dir.join("census.json");
    std::fs::remove_file(&census_path).unwrap();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Not spawned yet, but crucially not recorded as ignored either.
    assert!(node.processes.read().await.is_empty());
    assert!(node.db.process(&pid_fr()).unwrap().is_none());
    // The census appears. Seed the store through a fresh handle (same db)
    // to skip the store's own 30s download backoff.
    write_census(&s.census_dir, 8);
    let store = CensusStore::with_options(
        node.db.clone(),
        CensusOptions {
            dir: Some(s.census_dir.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let uri = format!("file://{}", census_path.display());
    store.fetch(&uri, &s.env.cfg.census_root).await.unwrap();
    let h = handle(&node).await;
    assert_eq!(
        node.db.process(&pid_fr()).unwrap().unwrap().local,
        LocalStatus::Active
    );
    drop(h);
    shutdown.cancel();
}

/// `ProcessMetadataUpdated` reaches the stored record, which mirrors
/// `getProcess`.
#[tokio::test]
async fn metadata_update_reaches_the_record() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let _h = handle(&node).await;
    s.chain.set_metadata("ipfs://metadata-2", [0x5a; 32]);
    wait_until("metadata stored", async || {
        node.db.process(&pid_fr()).unwrap().is_some_and(|r| {
            r.onchain.metadata_uri == "ipfs://metadata-2" && r.onchain.metadata_hash == [0x5a; 32]
        })
    })
    .await;
    shutdown.cancel();
}

/// A failed blob fetch during sync must not wedge the actor; the
/// heartbeat replays the gap and the pending votes settle.
#[tokio::test]
async fn blob_fetch_failure_recovers_on_heartbeat() {
    let s = setup(2, 8, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::gated();
    let prover_b = FakeProver::gated();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        prover_b.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    let v = fake_vote(&s.env, 0, &[2, 2], 7);
    let vid = v.pkg.vote_id;
    ha.submit(v.clone()).await.unwrap();
    hb.submit(v.clone()).await.unwrap();
    wait_until("both proving", async || {
        prover_a.calls() >= 1 && prover_b.calls() >= 1
    })
    .await;
    // B's first attempt to sync the winner's transition fails.
    s.chain.fail_blobs(1);
    prover_a.release(100);
    wait_until("A settled", async || s.chain.voters() == 1).await;
    prover_b.release(100);
    wait_until("B recovered and settled via sync", async || {
        vote_status(&hb, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    assert_eq!(node_b.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(node_b.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(node_b.metrics.settled_by_self.load(Ordering::SeqCst), 0);
    assert_eq!(hb.snapshot().await.unwrap().root, s.chain.root());
    shutdown.cancel();
}

/// A transition event the monitor never delivered (a gap) is replayed
/// from the registry when a later event reveals it, resuming from the last
/// applied transition. A vote settled by another sequencer is then a
/// duplicate here even without a local record.
#[tokio::test]
async fn gap_replay_applies_missed_transitions() {
    let s = setup(2, 8, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::gated();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        FakeProver::open(),
        10,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    // A settles v1 in a block hidden from every events() query, so B's
    // monitor advances past it without delivering it.
    let v1 = fake_vote(&s.env, 0, &[1, 2], 61);
    ha.submit(v1.clone()).await.unwrap();
    wait_until("A proving", async || prover_a.calls() >= 1).await;
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    prover_a.release(100);
    wait_until("T1 landed", async || s.chain.voters() == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await; // B polls past T1
    s.chain.hide_block(None);
    // A settles a second transition; B sees only T2 -> gap -> replay both.
    let v2 = fake_vote(&s.env, 1, &[2, 1], 62);
    ha.submit(v2).await.unwrap();
    wait_until("T2 landed", async || s.chain.voters() == 2).await;
    wait_until("B replayed the gap", async || {
        hb.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(node_b.metrics.synced_from_others.load(Ordering::SeqCst), 2);
    // v1's id is in B's synced tree although B never stored it.
    assert!(matches!(hb.submit(v1).await, Err(ActorError::Duplicate(_))));
    shutdown.cancel();
}

/// A transient submit failure requeues the votes and the next seal
/// retries; no lost race is counted and no vote is lost.
#[tokio::test]
async fn transient_submit_failure_requeues_and_retries() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    s.chain.fail_submits(1);
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled after retry", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    assert_eq!(s.chain.submit_count(), 1);
    assert!(prover.calls() >= 2, "the batch was re-proved after requeue");
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 0);
    shutdown.cancel();
}

/// A receipt timeout on a tx that actually landed commits from the
/// transition event instead of double-proving or faking a lost race.
#[tokio::test]
async fn receipt_timeout_on_landed_tx_commits_from_event() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    s.chain.timeout_submits(1);
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled from the event", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    assert_eq!(s.chain.submit_count(), 1);
    assert_eq!(prover.calls(), 1, "no re-prove of a landed batch");
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 0);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    shutdown.cancel();
}

/// A lagging RPC endpoint reverts InvalidStateRoot while the chain still
/// shows our committed root: cool down and retry, never latch await_sync.
#[tokio::test]
async fn invalid_state_root_at_our_root_is_transient() {
    stale_root_revert_settles(false, 0).await;
}

/// Same, but the root read in the revert arm fails too: the flag latches
/// and the resync heartbeat must release it once the chain shows our root.
#[tokio::test]
async fn resync_clears_await_sync_at_our_root() {
    stale_root_revert_settles(true, 1).await;
}

async fn stale_root_revert_settles(fail_read: bool, lost: u64) {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    s.chain.revert_simulates("InvalidStateRoot", 1);
    if fail_read {
        // After the boot read of the landing time, so the revert arm's
        // read is the one that fails.
        beats().await;
        s.chain.fail_process(1);
    }
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled after the stale revert", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    assert_eq!(s.chain.submit_count(), 1);
    assert!(prover.calls() >= 2, "the batch was re-proved after requeue");
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), lost);
    shutdown.cancel();
}

/// Two sequencers settle in the same block and ours loses, but the replay
/// cannot name the revert (`Lost`): a lost race like `InvalidStateRoot`.
/// The votes go back to pending, the node syncs the winner and settles
/// them next; none is ever errored.
#[tokio::test]
async fn unnamed_revert_in_a_same_block_race_requeues() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    // The winner's batch, built on the same genesis root.
    let mut other =
        davinci_state::ProcessState::create(s.env.cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let theirs = [
        fake_vote(&s.env, 2, &[1, 1], 60),
        fake_vote(&s.env, 3, &[2, 1], 61),
    ];
    let b = other
        .prepare(&theirs, &mut StdRng::seed_from_u64(5), &Default::default())
        .unwrap();
    s.chain.race_next_submit(b);
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let ours: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 50 + i as u64))
        .collect();
    let vids: Vec<u64> = ours.iter().map(|v| v.pkg.vote_id).collect();
    for v in &ours {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled after the lost race", async || {
        for vid in &vids {
            let st = vote_status(&h, *vid).await;
            assert_ne!(st, Some(VoteStatus::Error), "a lost race errored a vote");
            if st != Some(VoteStatus::Settled) {
                return false;
            }
        }
        true
    })
    .await;
    assert_eq!(s.chain.voters(), 4);
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(node.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    shutdown.cancel();
}

/// After a restart, `recover()` finds the settled state root in the
/// persisted record, so the key holder still finalizes when the election ends.
#[tokio::test]
async fn restart_persists_root_and_finalizes() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown1 = CancellationToken::new();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        2,
        "0s",
        shutdown1.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    // Stop the node, drop every handle, restart on the same datadir.
    shutdown1.cancel();
    drop(h);
    drop(node);
    drop(db);
    let db2 = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let node2 = start_node(
        db2,
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        2,
        "0s",
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    // Recovery kept the settled votes and the committed root.
    assert!(all_settled(&h2, &vids).await);
    assert_eq!(h2.snapshot().await.unwrap().root, s.chain.root());
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    let mut want = vec![0u64; 16];
    want[0] = 3;
    want[1] = 5;
    assert_eq!(s.chain.results(), want);
    shutdown2.cancel();
}

/// A transient finalize failure re-arms after a cooldown instead of
/// latching forever.
#[tokio::test]
async fn finalize_rearms_after_transient_failure() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_results.lock().unwrap() = 1;
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 40);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    // The first attempt fails transiently; after the ~2s cooldown a fresh
    // attempt succeeds.
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    shutdown.cancel();
}

/// A slot holds at most `slot_depth` (3) queued votes, in flight included;
/// a fourth is refused, and the three settle.
#[tokio::test]
async fn slot_depth_cap_refuses_fourth() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    // Same voter, new vote ids.
    let votes: Vec<VerifiedVote> = (0..4)
        .map(|i| fake_vote(&s.env, 0, &[1 + i as u64, 1], 70 + i as u64))
        .collect();
    h.submit(votes[0].clone()).await.unwrap();
    wait_until("first in flight", async || prover.calls() == 1).await;
    h.submit(votes[1].clone()).await.unwrap();
    h.submit(votes[2].clone()).await.unwrap();
    assert!(matches!(
        h.submit(votes[3].clone()).await,
        Err(ActorError::SlotBusy(_))
    ));
    prover.release(3);
    let vids: Vec<u64> = votes[..3].iter().map(|v| v.pkg.vote_id).collect();
    wait_until("three settled", async || all_settled(&h, &vids).await).await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!((snap.occupied, snap.overwrites), (1, 2));
    shutdown.cancel();
}

/// A new-slot vote is refused once distinct voters + queued new slots
/// reach max_voters.
#[tokio::test]
async fn max_voters_blocks_new_slot_votes() {
    let s = setup(2, 8, None);
    s.chain.set_max_voters(2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        10,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    h.submit(fake_vote(&s.env, 0, &[1, 2], 80)).await.unwrap();
    h.submit(fake_vote(&s.env, 1, &[1, 2], 81)).await.unwrap();
    match h.submit(fake_vote(&s.env, 2, &[1, 2], 82)).await {
        Err(ActorError::MaxVoters) => {}
        other => panic!("expected MaxVoters, got {other:?}"),
    }
    shutdown.cancel();
}

/// An unservable process (its `process()` read fails forever) must not
/// stall event routing — the valid process created in the same block spawns
/// and settles — and the bad one ends `ignored` after the attempt cap.
#[tokio::test]
async fn bad_process_does_not_stall_routing_and_ends_ignored() {
    let s = setup(2, 8, None);
    s.chain.add_bad_process(BadKind::FetchFails);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await; // the valid process spawned despite the bad one
    let v = fake_vote(&s.env, 0, &[1, 2], 300);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("vote settled", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    // Routing advanced past the bad process's block.
    assert!(db.meta_u64("monitor_last_block").unwrap().unwrap() >= 2);
    // The bad process runs out of bootstrap attempts and is recorded ignored.
    wait_until("bad process ignored", async || {
        matches!(
            db.process(&bad_pid_fr()).unwrap(),
            Some(rec) if rec.local == LocalStatus::Ignored
        )
    })
    .await;
    let rec = db.process(&bad_pid_fr()).unwrap().unwrap();
    assert!(rec.note.unwrap().contains("gave up"), "note names the cap");
    // The placeholder does not fabricate an on-chain status.
    assert_eq!(rec.onchain.status, ProcessStatus::Unknown);
    shutdown.cancel();
}

/// A census policy refusal (unsupported scheme) is permanent — the
/// process is ignored on the first bootstrap attempt, no retries.
#[tokio::test]
async fn refused_census_uri_is_ignored_immediately() {
    let s = setup(2, 8, None);
    let mut cfg2 = s.env.cfg.clone();
    cfg2.process_id = bad_pid_fr();
    s.chain.add_bad_process(BadKind::CensusUri {
        uri: "ftp://x".into(),
        state_root: genesis_root(&cfg2).unwrap(),
    });
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let _h = handle(&node).await;
    wait_until("bad process ignored", async || {
        matches!(
            db.process(&bad_pid_fr()).unwrap(),
            Some(rec) if rec.local == LocalStatus::Ignored
        )
    })
    .await;
    let rec = db.process(&bad_pid_fr()).unwrap().unwrap();
    assert!(rec.note.unwrap().contains("refused"), "refusal in the note");
    shutdown.cancel();
}

/// Replayed (already-applied) transition events neither abort a live
/// flight nor roll the pinned root back.
#[tokio::test]
async fn replayed_events_do_not_abort_flights() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;

    let v1 = fake_vote(&s.env, 0, &[1, 2], 400);
    h.submit(v1).await.unwrap();
    wait_until("first proof running", async || prover.calls() >= 1).await;
    prover.release(1);
    wait_until("first settled", async || s.chain.voters() == 1).await;

    // From here every poll re-delivers every event since genesis.
    s.chain.set_replay_all(true);
    let v2 = fake_vote(&s.env, 1, &[1, 2], 401);
    let vid2 = v2.pkg.vote_id;
    h.submit(v2).await.unwrap();
    wait_until("second proof running", async || prover.calls() >= 2).await;
    // Let the monitor re-deliver the first transition into the live flight
    // a few times.
    for _ in 0..3 {
        s.chain.advance_time(0);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    prover.release(1);
    wait_until("second settled", async || {
        vote_status(&h, vid2).await == Some(VoteStatus::Settled)
    })
    .await;

    // Still replaying: the first transition, now two roots behind, must be
    // dropped by the transition log, not treated as a foreign transition.
    let v3 = fake_vote(&s.env, 2, &[1, 2], 402);
    let vid3 = v3.pkg.vote_id;
    h.submit(v3).await.unwrap();
    wait_until("third proof running", async || prover.calls() >= 3).await;
    for _ in 0..3 {
        s.chain.advance_time(0);
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    prover.release(1);
    wait_until("third settled", async || {
        vote_status(&h, vid3).await == Some(VoteStatus::Settled)
    })
    .await;

    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 0);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 3);
    assert_eq!(prover.calls(), 3, "no batch was aborted and re-proved");
    shutdown.cancel();
}

/// A commit-persist failure rolls the tree back before requeueing, so
/// the follow-up resync from our own blobs applies and the votes settle.
#[tokio::test]
async fn commit_failure_rolls_back_and_resyncs() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 500);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("proof running", async || prover.calls() >= 1).await;
    // The next arbo write is the commit's persist: it fails once.
    db.fail_next_arbo_writes(1);
    prover.release(1);
    wait_until("vote settled after resync", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.root, s.chain.root());
    // The repair went through the blob-sync path.
    assert_eq!(node.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    shutdown.cancel();
}

/// Restart with a sealed-but-unproved batch: recover() puts the aggregated
/// votes back to pending and the new node settles them.
#[tokio::test]
async fn restart_recovers_aggregated_votes_to_pending() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 600);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("batch sealed", async || prover.calls() >= 1).await;
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Aggregated));
    shutdown.cancel();
    drop(node);

    let db = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let node2 = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    wait_until("recovered vote settled", async || {
        vote_status(&h2, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(s.chain.voters(), 1);
    shutdown2.cancel();
}

/// A pending new-slot vote holds a capacity slot only while queued —
/// once its batch fails permanently, the slot is admitted to someone else.
#[tokio::test]
async fn failed_batch_releases_capacity() {
    let s = setup(2, 8, None);
    s.chain.set_max_voters(2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;

    let v1 = fake_vote(&s.env, 0, &[1, 2], 700);
    let vid1 = v1.pkg.vote_id;
    h.submit(v1).await.unwrap();
    wait_until("first proof running", async || prover.calls() >= 1).await;
    prover.release(1);
    wait_until("first settled", async || {
        vote_status(&h, vid1).await == Some(VoteStatus::Settled)
    })
    .await;

    // The second vote's batch fails in the guest (permanent, not requeued).
    *prover.fail_mask.lock().unwrap() = Some(1);
    let v2 = fake_vote(&s.env, 1, &[1, 2], 701);
    let vid2 = v2.pkg.vote_id;
    h.submit(v2).await.unwrap();
    wait_until("second proof running", async || prover.calls() >= 2).await;
    // While v2 is queued it holds the last capacity slot.
    match h.submit(fake_vote(&s.env, 2, &[1, 2], 702)).await {
        Err(ActorError::MaxVoters) => {}
        other => panic!("expected MaxVoters, got {other:?}"),
    }
    prover.release(1);
    wait_until("second errored", async || {
        vote_status(&h, vid2).await == Some(VoteStatus::Error)
    })
    .await;

    // The failed vote no longer counts against capacity.
    let v3 = fake_vote(&s.env, 2, &[1, 2], 703);
    let vid3 = v3.pkg.vote_id;
    h.submit(v3).await.unwrap();
    wait_until("third proof running", async || prover.calls() >= 3).await;
    prover.release(1);
    wait_until("third settled", async || {
        vote_status(&h, vid3).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(s.chain.voters(), 2);
    shutdown.cancel();
}

/// Transient prover failures cool the batch off instead of
/// hot-looping, and the same votes settle once the prover recovers.
#[tokio::test]
async fn transient_prover_failure_backs_off_and_recovers() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches.lock().unwrap() = 1;
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 100);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("first attempt", async || prover.calls() >= 1).await;
    // One failure arms a 2 s cooldown: ~25 heartbeats must not re-prove,
    // and the vote is held pending, not errored.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(prover.calls(), 1, "re-proved during the cooldown");
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Pending));
    wait_until("settled after the cooldown", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(prover.calls(), 2);
    shutdown.cancel();
}

/// A prover 4xx (refused input) on a lone vote: one full-size retry, then
/// the vote is isolated and errored with the prover's reason.
#[tokio::test]
async fn permanent_prover_refusal_isolates_a_lone_vote() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches_permanent.lock().unwrap() = 2;
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 100);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("vote errored", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Error)
    })
    .await;
    let e = h.status(vid).await.unwrap().unwrap().error.unwrap();
    assert!(e.contains("400"), "reason recorded: {e}");
    assert!(e.contains("isolated by bisection"), "{e}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prover.calls(), 2, "one retry, then isolated");
    assert_eq!(s.chain.submit_count(), 0);
    assert_eq!(node.metrics.bisections.load(Ordering::SeqCst), 2);
    assert_eq!(node.metrics.isolated_votes.load(Ordering::SeqCst), 1);
    shutdown.cancel();
}

/// Consecutive transient failures grow the cooldown (2 s, 4 s,
/// 8 s) instead of resetting to 2 s on every retry. Paused runtime:
/// the gaps are virtual-time exact.
#[tokio::test(start_paused = true)]
async fn transient_backoff_grows_across_failures() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches.lock().unwrap() = 3;
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 100);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    for n in 1..=4 {
        wait_until("next attempt", async || prover.calls() >= n).await;
    }
    wait_until("settled after three cooldowns", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    let times = prover.call_times.lock().unwrap().clone();
    assert_eq!(times.len(), 4);
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[1] > gaps[0], "backoff did not grow: {gaps:?}");
    assert!(gaps[2] > gaps[1], "backoff did not grow: {gaps:?}");
    assert!(gaps[0] >= Duration::from_secs(2), "gaps {gaps:?}");
    assert!(gaps[1] >= Duration::from_secs(4), "gaps {gaps:?}");
    assert!(gaps[2] >= Duration::from_secs(8), "gaps {gaps:?}");
    shutdown.cancel();
}

/// A prover that never comes back sees a bounded number of
/// attempts (saturating backoff), and the vote stays pending.
#[tokio::test(start_paused = true)]
async fn prover_always_down_bounds_the_attempts() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches.lock().unwrap() = u32::MAX;
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 100);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("first attempt", async || prover.calls() >= 1).await;
    // Cooldowns 2+4+8+16+32 s: one virtual minute fits at most 6 calls.
    tokio::time::sleep(Duration::from_secs(60)).await;
    let calls = prover.calls();
    assert!((3..=6).contains(&calls), "calls in a minute: {calls}");
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Pending));
    shutdown.cancel();
}

/// A node bootstrapping after transitions already settled validates
/// genesis against the first transition's old_root, replays the gap from
/// blobs and then settles new votes.
#[tokio::test]
async fn late_bootstrap_syncs_past_transitions_and_settles() {
    let s = setup(2, 8, None);
    let dir_a = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::open();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    for i in 0..2u64 {
        let v = fake_vote(&s.env, i as usize, &[1, 2], 100 + i);
        let vid = v.pkg.vote_id;
        ha.submit(v).await.unwrap();
        wait_until("A settled", async || all_settled(&ha, &[vid]).await).await;
    }
    assert_eq!(s.chain.voters(), 2);
    assert_ne!(s.chain.root(), genesis_root(&s.env.cfg).unwrap());

    // B starts fresh; the on-chain root is two transitions past genesis.
    let dir_b = TempDir::new().unwrap();
    let prover_b = FakeProver::open();
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        prover_b.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let hb = handle(&node_b).await;
    wait_until("B synced to the chain root", async || {
        hb.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(node_b.metrics.synced_from_others.load(Ordering::SeqCst), 2);
    let v = fake_vote(&s.env, 2, &[1, 2], 300);
    let vid = v.pkg.vote_id;
    hb.submit(v).await.unwrap();
    wait_until("B settled a new vote", async || {
        all_settled(&hb, &[vid]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 3);
    assert_eq!(prover_b.calls(), 1);
    shutdown.cancel();
}

/// Negative: a process whose root moved but shows no transition from
/// this config's genesis exhausts its bootstrap retries and ends ignored.
#[tokio::test]
async fn foreign_root_without_genesis_transition_is_ignored() {
    let s = setup(2, 8, None);
    s.chain.add_bad_process(BadKind::CensusUri {
        uri: "file:///unused.json".into(),
        state_root: [0xdd; 32],
    });
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let _h = handle(&node).await;
    wait_until("bad process ignored", async || {
        matches!(
            db.process(&bad_pid_fr()).unwrap(),
            Some(rec) if rec.local == LocalStatus::Ignored
        )
    })
    .await;
    let rec = db.process(&bad_pid_fr()).unwrap().unwrap();
    assert!(
        rec.note.unwrap().contains("no transition event"),
        "reason in the note"
    );
    shutdown.cancel();
}

/// A process whose first transition departs from a root that is
/// not our config's genesis is ignored immediately (no retry cap).
#[tokio::test]
async fn foreign_first_transition_is_ignored_immediately() {
    let s = setup(2, 8, None);
    s.chain.add_bad_process(BadKind::CensusUri {
        uri: "file:///unused.json".into(),
        state_root: [0xdd; 32],
    });
    // The visible first transition starts from a foreign root, so the
    // genesis check concludes (Ok(false)) instead of retrying blind.
    s.chain.push_event(EventKind::StateTransitioned {
        pid: bad_pid31(),
        sender: Address::repeat_byte(0xEE),
        old_root: [0xaa; 32],
        new_root: [0xdd; 32],
        voters: 1,
        overwrites: 0,
        n_blobs: 1,
        l1: None,
    });
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let _h = handle(&node).await;
    wait_until("foreign process ignored", async || {
        matches!(
            db.process(&bad_pid_fr()).unwrap(),
            Some(rec) if rec.local == LocalStatus::Ignored
        )
    })
    .await;
    let rec = db.process(&bad_pid_fr()).unwrap().unwrap();
    assert!(
        rec.note.unwrap().contains("not the genesis"),
        "reason in the note"
    );
    shutdown.cancel();
}

// ----------------------------------- dynamic census and the exposed set

use davinci_zkvm_sdk::blob::decode_blobs;
use davinci_zkvm_sdk::crypto::field::fr_to_le;

/// Origin-2 setup: `n_voters` in the bootstrap `census.json`.
fn offchain_setup(n_voters: usize) -> TestSetup {
    let mut e = env(2, n_voters, None);
    e.cfg.census_origin = CensusOrigin::MerkleOffchainDynamic;
    let census_dir = TempDir::new().unwrap();
    let uri = write_census(census_dir.path(), n_voters);
    let chain = FakeChain::new(&e, uri);
    let dir = census_dir.path().to_path_buf();
    TestSetup {
        env: e,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    }
}

fn census_root_of(voters: impl IntoIterator<Item = usize>) -> Fr {
    LeanImt::from_leaves(
        voters
            .into_iter()
            .map(|i| census_leaf(&voter_address(i), 1).unwrap())
            .collect(),
    )
    .root()
}

/// Writes `voters` under `<census dir>/<sub>/` and announces the update.
fn update_census(s: &TestSetup, sub: &str, voters: &[usize]) -> Fr {
    let root = census_root_of(voters.iter().copied());
    let dir = s.census_dir.join(sub);
    std::fs::create_dir(&dir).unwrap();
    let uri = write_census_of(&dir, voters.iter().copied());
    s.chain.set_census(fr_to_be(&root), uri);
    root
}

/// The slots this node's latest own transition changed, from its blobs.
fn own_updated_slots(db: &Db, nf: u8) -> Vec<u64> {
    let ts = db.transitions(&pid_fr()).unwrap();
    let t = ts.iter().rev().find(|t| t.by_self).unwrap();
    let blobs = db.blobs(&pid_fr(), t.index).unwrap();
    decode_blobs(&blobs, nf)
        .unwrap()
        .updates
        .iter()
        .map(|(s, _)| *s)
        .collect()
}

/// A batch that lost the settlement race re-seals covering every slot
/// the first attempt exposed (writes and refreshes as one set), so the
/// broadcast sets stay indistinguishable.
#[tokio::test]
async fn reseal_covers_first_attempts_slots() {
    let s = setup(2, 24, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::open();
    let prover_b = FakeProver::gated();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        prover_b.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    // 20 settled votes: more occupied slots than the refresh floor (16),
    // so refresh selection is a proper subset.
    for i in 0..20 {
        ha.submit(fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
            .await
            .unwrap();
    }
    wait_until("20 votes settled", async || s.chain.voters() == 20).await;
    wait_until("B synced", async || {
        hb.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;

    // B seals one vote: one write plus 16 refreshed slots, persisted as
    // one union list before the batch can broadcast.
    let vb = fake_vote(&s.env, 20, &[1, 2], 300);
    let vid_b = vb.pkg.vote_id;
    hb.submit(vb).await.unwrap();
    wait_until("B proving", async || prover_b.calls() >= 1).await;
    let e1 = node_b
        .db
        .exposed(&pid_fr())
        .unwrap()
        .expect("exposed set persisted at seal");
    assert_eq!(e1.vote_ids, vec![vid_b]);
    assert_eq!(e1.slots.len(), 17, "one write + 16 refreshes: {e1:?}");
    assert!(
        e1.slots.windows(2).all(|w| w[0] < w[1]),
        "one strictly sorted union list"
    );

    // A wins the race with another vote.
    ha.submit(fake_vote(&s.env, 21, &[1, 2], 400))
        .await
        .unwrap();
    wait_until("A settled again", async || s.chain.voters() == 21).await;

    // B's proof loses; the re-seal must change a superset of the slots
    // the first attempt exposed.
    prover_b.release(100);
    wait_until("B settled", async || {
        vote_status(&hb, vid_b).await == Some(VoteStatus::Settled)
    })
    .await;
    let u2 = own_updated_slots(&node_b.db, 2);
    for slot in &e1.slots {
        assert!(u2.contains(slot), "exposed slot {slot} not re-covered");
    }
    // Every recorded vote settled: the set clears.
    wait_until("exposed cleared", async || {
        node_b.db.exposed(&pid_fr()).unwrap().is_none()
    })
    .await;
    shutdown.cancel();
}

/// A restart between seal and re-seal keeps the exposed set; the
/// recovered batch still covers the recorded slots.
#[tokio::test]
async fn exposed_set_survives_restart() {
    let s = setup(2, 24, None);
    let dir = TempDir::new().unwrap();

    // Phase 1: 20 settled votes.
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    for i in 0..20 {
        h.submit(fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
            .await
            .unwrap();
    }
    wait_until("20 votes settled", async || s.chain.voters() == 20).await;
    shutdown.cancel();
    drop(node);

    // Phase 2: seal one vote and stop before its proof completes.
    let db = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let prover = FakeProver::gated();
    let node2 = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    let v = fake_vote(&s.env, 20, &[1, 2], 300);
    let vid = v.pkg.vote_id;
    h2.submit(v).await.unwrap();
    wait_until("sealed", async || prover.calls() >= 1).await;
    let e1 = node2
        .db
        .exposed(&pid_fr())
        .unwrap()
        .expect("exposed set persisted at seal");
    assert_eq!(e1.slots.len(), 17);
    shutdown2.cancel();
    drop(node2);

    // Phase 3: the set survives the restart and the re-seal covers it.
    let db = reopen_db(dir.path()).await;
    assert_eq!(db.exposed(&pid_fr()).unwrap(), Some(e1.clone()));
    let shutdown3 = CancellationToken::new();
    let node3 = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown3.clone(),
    )
    .await;
    let h3 = handle(&node3).await;
    wait_until("recovered vote settled", async || {
        vote_status(&h3, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    let u2 = own_updated_slots(&node3.db, 2);
    for slot in &e1.slots {
        assert!(u2.contains(slot), "exposed slot {slot} lost by the restart");
    }
    wait_until("exposed cleared", async || {
        node3.db.exposed(&pid_fr()).unwrap().is_none()
    })
    .await;
    shutdown3.cancel();
}

/// Origin 2: a census update during a flight rolls it back and re-seals at
/// the new root once its tree loads; the stale proof never lands (the
/// simulate revert gates the broadcast).
#[tokio::test]
async fn census_update_during_flight_reseals_at_new_root() {
    let s = offchain_setup(4);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 40 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("first seal", async || prover.calls() >= 1).await;

    // The organizer re-roots the census while the batch is in the prover;
    // the contract now expects the new root.
    let root1 = update_census(&s, "v2", &[0, 1, 2, 3, 4]);
    s.chain.set_census_root_check(Some(fr_to_le(&root1)));

    // The actor aborts the flight and re-seals once the new tree loads.
    wait_until("re-seal at the new root", async || prover.calls() >= 2).await;
    prover.release(100);
    wait_until("votes settled", async || all_settled(&h, &vids).await).await;
    // Only the re-sealed batch landed; the stale one never broadcast.
    assert_eq!(s.chain.submit_count(), 1);
    assert_eq!(s.chain.last_census_root(), Some(fr_to_le(&root1)));
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 0);
    shutdown.cancel();
}

/// Origin 2: pending votes are re-checked on every census update, flight
/// or not; a voter dropped from the census is errored with a recast hint.
#[tokio::test]
async fn census_update_rechecks_pending_votes() {
    let s = offchain_setup(4);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        5,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v0 = fake_vote(&s.env, 0, &[1, 2], 50);
    let v1 = fake_vote(&s.env, 1, &[1, 2], 51);
    let (vid0, vid1) = (v0.pkg.vote_id, v1.pkg.vote_id);
    h.submit(v0).await.unwrap();
    h.submit(v1).await.unwrap();

    // Voter 1 is dropped from the census.
    update_census(&s, "v2", &[0, 2, 3]);
    wait_until("changed vote errored", async || {
        vote_status(&h, vid1).await == Some(VoteStatus::Error)
    })
    .await;
    let rec = h.status(vid1).await.unwrap().unwrap();
    assert_eq!(rec.error.as_deref(), Some("census changed, recast"));
    // The unchanged voter's ballot stays queued.
    assert_eq!(vote_status(&h, vid0).await, Some(VoteStatus::Pending));
    shutdown.cancel();
}

/// `InvalidCensusRoot` at settlement re-roots and re-seals — the
/// votes are requeued, never errored — and three consecutive reverts of
/// the same root start the transient backoff.
#[tokio::test(start_paused = true)]
async fn invalid_census_root_requeues_with_backoff() {
    let s = offchain_setup(4);
    // The contract expects a root this node never learns about.
    s.chain.set_census_root_check(Some([0x99; 32]));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 60);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    // Three reverts of the same root: each requeues (no error) and
    // re-seals; the third starts the cooldown.
    for n in 1..=3u64 {
        wait_until("attempt", async || prover.calls() >= n).await;
        assert_ne!(vote_status(&h, vid).await, Some(VoteStatus::Error));
        prover.release(1);
    }
    wait_until("attempt after the backoff", async || prover.calls() >= 4).await;
    let times = prover.call_times.lock().unwrap().clone();
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[0] < Duration::from_secs(2), "gaps {gaps:?}");
    assert!(gaps[1] < Duration::from_secs(2), "gaps {gaps:?}");
    assert!(gaps[2] >= Duration::from_secs(2), "gaps {gaps:?}");
    // Nothing broadcast while reverting; once the contract accepts the
    // recorded root again, the same vote settles.
    assert_eq!(s.chain.submit_count(), 0);
    s.chain.set_census_root_check(None);
    prover.release(100);
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    assert_eq!(s.chain.submit_count(), 1);
    shutdown.cancel();
}

/// Origin 3 on anvil: the census grows past `max_voters`; batches seal at
/// the contract's latest root, new-slot votes beyond the cap are refused
/// and overwrites still settle.
#[tokio::test]
async fn onchain_census_growth_respects_max_voters() {
    if !common::anvil::enabled() {
        return;
    }
    let ch = common::anvil::CensusChain::start().await;
    ch.add(&[(voter_address(0), 1), (voter_address(1), 1)])
        .await;
    let (recorded, _) = ch.state().await;

    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::MerkleOnchainDynamic;
    e.cfg.census_root = fr_from_be(&recorded).unwrap();
    let census_dir = TempDir::new().unwrap();
    let chain = FakeChain::new(&e, String::new());
    let c = ch.census.into_array();
    chain.set_census_contract(c);
    chain.set_max_voters(2);
    while chain.head_block() < ch.head().await {
        chain.advance_time(0);
    }
    let dir = census_dir.path().to_path_buf();
    let s = TestSetup {
        env: e,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    };

    let dbdir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let url = ch.anvil.endpoint();
    let cfg = test_config_with(dbdir.path(), &s.census_dir, 1, "0s", &["--rpc-url", &url]);
    let node = start_node_cfg(
        Db::open_in(dbdir.path()).unwrap(),
        s.chain.clone(),
        FakeProver::open(),
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;

    h.submit(fake_vote(&s.env, 0, &[1, 2], 70)).await.unwrap();
    wait_until("voter 0 settled", async || s.chain.voters() == 1).await;

    // The census grows past the cap; the index follows the contract.
    ch.add(&[(voter_address(2), 1), (voter_address(3), 1)])
        .await;
    while s.chain.head_block() < ch.head().await {
        s.chain.advance_time(0);
    }
    wait_until("index synced", async || {
        node.onchain
            .latest(&c)
            .is_some_and(|(_, size, _)| size == 4)
    })
    .await;

    // A second member settles at the latest confirmed root.
    h.submit(fake_vote(&s.env, 1, &[1, 2], 71)).await.unwrap();
    wait_until("voter 1 settled", async || s.chain.voters() == 2).await;
    let latest = node.onchain.latest(&c).unwrap().0;
    assert_eq!(s.chain.last_census_root(), Some(fr_to_le(&latest)));

    // A third distinct voter exceeds max_voters.
    match h.submit(fake_vote(&s.env, 2, &[1, 2], 72)).await {
        Err(ActorError::MaxVoters) => {}
        other => panic!("expected MaxVoters, got {other:?}"),
    }
    // An overwrite by an existing voter still works.
    let v0b = fake_vote(&s.env, 0, &[3, 1], 73);
    let vid0b = v0b.pkg.vote_id;
    h.submit(v0b).await.unwrap();
    wait_until("overwrite settled", async || {
        all_settled(&h, &[vid0b]).await
    })
    .await;
    // The cap tracks distinct voters, not ballots: still full.
    match h.submit(fake_vote(&s.env, 3, &[1, 2], 74)).await {
        Err(ActorError::MaxVoters) => {}
        other => panic!("expected MaxVoters, got {other:?}"),
    }
    shutdown.cancel();
}

// --------------------------------- refresh overflow and census indexing

use davinci_sequencer::census::onchain::UsableError;
use davinci_sequencer::storage::ExposedRecord;

/// Refresh overflow, unit path: only the pending votes the exposed set records are
/// errored, the set clears everywhere, and later seals start a fresh set.
#[tokio::test]
async fn refresh_overflow_errors_only_exposed_votes() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();

    // Phase 1: three pending votes, no seal (batch_max 5, batch_time 1h).
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        5,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes: Vec<VerifiedVote> = (0..3)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 60 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    shutdown.cancel();
    drop(node);

    // Phase 2: seed an exposed record naming votes 0 and 1.
    let db = reopen_db(dir.path()).await;
    db.put_exposed(
        &pid_fr(),
        &ExposedRecord {
            slots: vec![1_000_001, 1_000_002],
            vote_ids: vec![vids[0], vids[1]],
        },
    )
    .unwrap();

    // Phase 3: force the overflow path.
    let shutdown2 = CancellationToken::new();
    let prover = FakeProver::gated();
    let node2 = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        5,
        "0s",
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    h2.force_refresh_overflow().await;
    wait_until("recorded votes errored", async || {
        vote_status(&h2, vids[0]).await == Some(VoteStatus::Error)
            && vote_status(&h2, vids[1]).await == Some(VoteStatus::Error)
    })
    .await;
    for vid in &vids[..2] {
        let rec = h2.status(*vid).await.unwrap().unwrap();
        assert_eq!(
            rec.error.as_deref(),
            Some("exposed refresh set too large, recast")
        );
    }
    // The unrecorded vote is untouched and the set is gone from the db.
    assert_eq!(vote_status(&h2, vids[2]).await, Some(VoteStatus::Pending));
    assert!(node2.db.exposed(&pid_fr()).unwrap().is_none());

    // The in-memory set cleared too: the next seal records a fresh one.
    let mut more: Vec<u64> = Vec::new();
    for i in 3..7 {
        let v = fake_vote(&s.env, i, &[1, 2], 60 + i as u64);
        more.push(v.pkg.vote_id);
        h2.submit(v).await.unwrap();
    }
    wait_until("sealed", async || prover.calls() >= 1).await;
    let e2 = node2
        .db
        .exposed(&pid_fr())
        .unwrap()
        .expect("fresh exposed set at seal");
    assert!(!e2.slots.contains(&1_000_001) && !e2.slots.contains(&1_000_002));
    assert!(!e2.vote_ids.contains(&vids[0]) && !e2.vote_ids.contains(&vids[1]));
    let mut want = vec![vids[2]];
    want.extend(&more);
    want.sort_unstable();
    assert_eq!(e2.vote_ids, want);
    prover.release(100);
    wait_until("survivors settled", async || all_settled(&h2, &want).await).await;
    shutdown2.cancel();
}

/// Exposed-set sizing: a batch_max trim that sheds an overwrite of a live exposed
/// slot errors nothing — the shed votes stay pending and settle later.
#[tokio::test]
async fn trimmed_exposed_overwrite_errors_nothing() {
    let s = setup(2, 24, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::open();
    let prover_b = FakeProver::gated();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        prover_b.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    // 20 settled votes so the exposed set has live slots beyond the floor.
    for i in 0..20 {
        ha.submit(fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
            .await
            .unwrap();
    }
    wait_until("20 votes settled", async || s.chain.voters() == 20).await;
    wait_until("B synced", async || {
        hb.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;

    // B seals one vote; the flight holds while more votes queue behind it.
    let vb = fake_vote(&s.env, 20, &[1, 2], 300);
    let vid_b = vb.pkg.vote_id;
    hb.submit(vb).await.unwrap();
    wait_until("B proving", async || prover_b.calls() >= 1).await;
    let e1 = node_b.db.exposed(&pid_fr()).unwrap().unwrap();

    // vx: a fresh voter. vc: an overwrite of a live exposed slot; with
    // batch_max 1 the re-seal keeps only vx, so vc is the trimmed one.
    let vx = fake_vote(&s.env, 21, &[1, 2], 400);
    let vid_x = vx.pkg.vote_id;
    let vc = (0..20)
        .map(|j| fake_vote(&s.env, j, &[3, 1], 500 + j as u64))
        .find(|v| e1.slots.contains(&v.slot))
        .expect("some settled slot was refreshed");
    let vid_c = vc.pkg.vote_id;
    hb.submit(vx).await.unwrap();
    hb.submit(vc).await.unwrap();

    // A wins the race; B requeues [vx, vc, vb] and re-seals one at a time.
    ha.submit(fake_vote(&s.env, 22, &[1, 2], 600))
        .await
        .unwrap();
    wait_until("A settled again", async || s.chain.voters() == 21).await;
    prover_b.release(100);
    wait_until("all of B's votes settled", async || {
        all_settled(&hb, &[vid_b, vid_x, vid_c]).await
    })
    .await;
    // The trimmed exposed overwrite was never errored.
    let rec = hb.status(vid_c).await.unwrap().unwrap();
    assert!(rec.error.is_none(), "trimmed vote errored: {:?}", rec.error);
    wait_until("exposed cleared", async || {
        node_b.db.exposed(&pid_fr()).unwrap().is_none()
    })
    .await;
    shutdown.cancel();
}

/// Origin 3: an unindexed contract (a resume whose register failed) is
/// transient — votes stay pending, admit stays open, and the actor
/// re-registers on its next tick and seals.
#[tokio::test]
async fn onchain_unindexed_census_is_transient() {
    if !common::anvil::enabled() {
        return;
    }
    let ch = common::anvil::CensusChain::start().await;
    ch.add(&[(voter_address(0), 1), (voter_address(1), 1)])
        .await;
    let (recorded, _) = ch.state().await;

    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::MerkleOnchainDynamic;
    e.cfg.census_root = fr_from_be(&recorded).unwrap();
    let census_dir = TempDir::new().unwrap();
    let chain = FakeChain::new(&e, String::new());
    let c = ch.census.into_array();
    chain.set_census_contract(c);
    while chain.head_block() < ch.head().await {
        chain.advance_time(0);
    }
    let dir = census_dir.path().to_path_buf();
    let s = TestSetup {
        env: e,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    };

    let dbdir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let url = ch.anvil.endpoint();
    let cfg = test_config_with(dbdir.path(), &s.census_dir, 1, "0s", &["--rpc-url", &url]);
    let node = start_node_cfg(
        Db::open_in(dbdir.path()).unwrap(),
        s.chain.clone(),
        FakeProver::open(),
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;

    // Forget the registration, as if the resume-time register had failed.
    node.onchain.unregister(&c);
    assert_eq!(node.onchain.usable(&c), Err(UsableError::NotIndexed));

    // The vote is admitted and settles: the actor re-registers on its
    // tick instead of latching the process unusable.
    let v0 = fake_vote(&s.env, 0, &[1, 2], 70);
    let vid0 = v0.pkg.vote_id;
    h.submit(v0).await.unwrap();
    wait_until("voter 0 settled", async || all_settled(&h, &[vid0]).await).await;
    let rec = h.status(vid0).await.unwrap().unwrap();
    assert!(rec.error.is_none(), "{:?}", rec.error);

    // Admit never closed: a second voter goes through as well.
    let v1 = fake_vote(&s.env, 1, &[1, 2], 71);
    let vid1 = v1.pkg.vote_id;
    h.submit(v1).await.unwrap();
    wait_until("voter 1 settled", async || all_settled(&h, &[vid1]).await).await;
    shutdown.cancel();
}

// -------------------------------- appended blobs and the results window

/// A settlement tx padded with a junk blob past the event's n_blobs
/// still syncs on other nodes — only the registry-checked prefix is decoded.
#[tokio::test]
async fn synced_transition_ignores_appended_blob() {
    let s = setup(2, 8, None);
    let shutdown = CancellationToken::new();
    let dir_a = TempDir::new().unwrap();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 40);
    let vid = v.pkg.vote_id;
    ha.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&ha, &[vid]).await).await;

    // The attack: one junk blob appended to the settling transaction.
    s.chain.append_junk_blob();

    // A fresh node syncs the transition from its DA blobs regardless.
    let dir_b = TempDir::new().unwrap();
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        FakeProver::gated(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let hb = handle(&node_b).await;
    wait_until("B synced", async || {
        hb.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    shutdown.cancel();
}

/// A results revert that means "not ended / time bounds" (an extension
/// racing the broadcast) is transient — finalize re-arms instead of latching.
#[tokio::test]
async fn results_revert_on_reopened_window_rearms() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 40);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    s.chain.revert_results_once("InvalidTimeBounds");
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    // The first broadcast reverts; after the cooldown a fresh attempt lands.
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    shutdown.cancel();
}

/// Eager results: proved during the grace at the quiescent root and held.
/// Another sequencer's landing moves the root (and the grace end): the held
/// proof is dropped and redone, then submitted once the grace ends. An
/// `InvalidStateRoot` on that submit drops it: the chain is re-read and the
/// retry proves again.
#[tokio::test]
async fn eager_results_submits_at_grace_end_reproves_on_root_move() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    prover.gate_results();
    s.chain.advance_time(DURATION + 1);
    wait_until("eager proof", async || prover.results_calls() == 1).await;
    // Another sequencer lands in the grace while the proof runs.
    let mut other =
        davinci_state::ProcessState::create(s.env.cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let theirs = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let b = other
        .prepare(&theirs, &mut StdRng::seed_from_u64(3), &Default::default())
        .unwrap();
    s.chain.advance_time(60);
    s.chain.settle_externally(&b);
    wait_until("synced", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    prover.release_results(2);
    wait_until("proved again", async || prover.results_calls() == 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.chain.results_submits(), 0, "tally broadcast in the grace");
    // Held until the grace end the landing moved.
    assert_eq!(
        s.chain.grace_end(),
        T0 + DURATION + 61 + GRACE.default_grace
    );
    s.chain.revert_results_once("InvalidStateRoot");
    s.chain.pass_grace();
    prover.release_results(1);
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    let mut want = vec![0u64; 16];
    want[0] = 3;
    want[1] = 5;
    assert_eq!(s.chain.results(), want);
    assert_eq!(s.chain.results_submits(), 2);
    assert_eq!(prover.results_calls(), 3, "the held proof was dropped");
    shutdown.cancel();
}

// ------------------------------------------------------------ DKG modes

/// A DKG-mode election with two settled votes, ended (explicitly, or by
/// time with `by_time`), on a node that does not hold (and could not hold)
/// the key. `chain` is the node's view.
async fn dkg_node_ended(
    mode: KeyMode,
    chain: impl FnOnce(&FakeChain) -> FakeChain,
    votes: bool,
    by_time: bool,
) -> (TestSetup, Node, ActorHandle, CancellationToken, TempDir) {
    let s = setup(2, 8, None);
    s.chain.set_key_mode(mode);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        chain(&s.chain),
        FakeProver::open(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    if votes {
        let votes = [
            fake_vote(&s.env, 0, &[1, 2], 40),
            fake_vote(&s.env, 1, &[2, 3], 41),
        ];
        let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
        for v in &votes {
            h.submit(v.clone()).await.unwrap();
        }
        wait_until("settled", async || all_settled(&h, &vids).await).await;
    }
    if by_time {
        s.chain.advance_time(DURATION + 1);
    } else {
        s.chain.set_status(ProcessStatus::Ended);
    }
    s.chain.pass_grace();
    (s, node, h, shutdown, dir)
}

async fn dkg_node(
    mode: KeyMode,
    chain: impl FnOnce(&FakeChain) -> FakeChain,
    votes: bool,
) -> (TestSetup, Node, ActorHandle, CancellationToken, TempDir) {
    dkg_node_ended(mode, chain, votes, false).await
}

// `wait_until` with a longer (virtual) horizon, for tests that sit through
// finalize backoffs.
async fn wait_long(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timeout waiting for {what}");
}

// DKG results carry one entry per field; these processes have two.
fn tally(a: u64, b: u64) -> Vec<u64> {
    vec![a, b]
}

// The request goes out once, from the committed tree; nothing more until
// the committee has combined every ciphertext, then one finalize.
async fn dkg_finalizes_after_the_committee(mode: KeyMode) {
    let (s, node, h, shutdown, _dir) = dkg_node(mode, FakeChain::clone, true).await;
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    // The request carries the committed accumulator (the fake registry has
    // already checked its inclusion under the root).
    let (acc, sibs) = s.chain.dkg_request().unwrap();
    assert_eq!(sibs.len(), 64);
    let d = s.chain.dkg_state();
    assert!(d.requested);
    assert_eq!((d.first_index, d.count), (1, 2));
    assert_ne!(acc[0], [0u8; 32], "field 0 is an active ciphertext");
    // A locked process sits here until the organizer reveals: no resend,
    // no finalize, no results.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (0, 0)));
    assert!(s.chain.results().is_empty());
    assert_ne!(h.snapshot().await.unwrap().local, LocalStatus::Finalized);
    s.chain.dkg_combine(vec![3, 5]);
    wait_long("results on chain", async || !s.chain.results().is_empty()).await;
    assert_eq!(s.chain.results(), tally(3, 5));
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (1, 1)));
    assert_eq!(s.chain.results_submits(), 0, "no results proof in DKG mode");
    // Readiness polls are not attempts: one request, one count.
    assert_eq!(node.metrics.finalize_attempts.load(Ordering::SeqCst), 1);
    shutdown.cancel();
}

#[tokio::test(start_paused = true)]
async fn dkg_automatic_requests_once_and_finalizes_when_ready() {
    dkg_finalizes_after_the_committee(KeyMode::DkgAutomatic).await;
}

#[tokio::test(start_paused = true)]
async fn dkg_locked_waits_for_the_reveal() {
    dkg_finalizes_after_the_committee(KeyMode::DkgLocked).await;
}

// A Council key takes the same path: request once, finalize when combined.
#[tokio::test(start_paused = true)]
async fn council_requests_once_and_finalizes_when_ready() {
    dkg_finalizes_after_the_committee(KeyMode::Council).await;
}

/// A zero-vote Council process whose ceremony has not opened decryption:
/// the request lands and ends it, but nothing is published. The node keeps
/// reading the gate on a backoff that tops out at five minutes, never
/// latches, and finalizes the zeros once the gate opens.
#[tokio::test(start_paused = true)]
async fn council_zero_vote_waits_for_the_gate() {
    let closed = |c: &FakeChain| {
        c.set_council_gate(false);
        c.clone()
    };
    let (s, _node, h, shutdown, _dir) = dkg_node(KeyMode::Council, closed, false).await;
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    assert_eq!(s.chain.dkg_state().count, 0);
    // An hour with the gate closed: no finalize goes out, the reads slow
    // down (15 s doubling to 300 s: about 16 in an hour), the process stays
    // ENDED and the actor active.
    tokio::time::sleep(Duration::from_secs(3600)).await;
    let reads = s.chain.council_gate_reads();
    assert!((10..=25).contains(&reads), "{reads} gate reads in an hour");
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (0, 0)));
    assert_eq!(s.chain.status(), ProcessStatus::Ended);
    assert!(s.chain.results().is_empty());
    assert_eq!(h.snapshot().await.unwrap().local, LocalStatus::Active);
    // A day on, still capped at one read per 300 s; then the ceremony
    // opens and the next read publishes the zeros.
    tokio::time::sleep(Duration::from_secs(3600 * 24)).await;
    let before = s.chain.council_gate_reads();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert!(
        s.chain.council_gate_reads() - before <= 13,
        "capped at 300 s"
    );
    s.chain.set_council_gate(true);
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), vec![0u64; 2]);
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (1, 1)));
    shutdown.cancel();
}

/// A finalize the registry refuses with `DecryptionNotOpen` (the gate view
/// read open on a lagging endpoint) is pending, not a failure: the node
/// retries and publishes the combined tally once the gate is open.
#[tokio::test(start_paused = true)]
async fn council_decryption_not_open_is_retried_not_latched() {
    let lagging = |c: &FakeChain| {
        c.set_council_gate(false);
        c.set_council_gate_view(Some(true));
        c.clone()
    };
    let (s, _node, h, shutdown, _dir) = dkg_node(KeyMode::Council, lagging, true).await;
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    s.chain.dkg_combine(vec![3, 5]);
    wait_long("finalize refused twice", async || {
        s.chain.dkg_calls().1.1 >= 2
    })
    .await;
    assert_eq!(s.chain.dkg_calls().1.0, 0);
    assert_eq!(s.chain.status(), ProcessStatus::Ended);
    assert_eq!(h.snapshot().await.unwrap().local, LocalStatus::Active);
    s.chain.set_council_gate(true);
    s.chain.set_council_gate_view(None);
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), tally(3, 5));
    assert_eq!(s.chain.dkg_calls().0, (1, 1));
    assert_eq!(s.chain.dkg_calls().1.0, 1);
    shutdown.cancel();
}

/// Another node's request lands between our read and our send: the revert
/// counts as done, and this node still publishes the plaintexts.
#[tokio::test(start_paused = true)]
async fn dkg_request_race_is_tolerated() {
    let race = |c: &FakeChain| {
        c.race_dkg_request();
        c.clone()
    };
    let (s, _node, h, shutdown, _dir) = dkg_node(KeyMode::DkgAutomatic, race, true).await;
    wait_until("request attempted", async || s.chain.dkg_calls().0.1 == 1).await;
    assert_eq!(s.chain.dkg_calls().0.0, 0, "the other node's request won");
    assert!(s.chain.dkg_state().requested);
    s.chain.dkg_combine(vec![3, 5]);
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), tally(3, 5));
    // Never retried the request.
    assert_eq!(s.chain.dkg_calls(), ((0, 1), (1, 1)));
    shutdown.cancel();
}

/// Another node publishes the plaintexts first: this node finalizes from
/// the events and never latches a failure.
#[tokio::test(start_paused = true)]
async fn dkg_finalize_race_is_tolerated() {
    let (s, _node, h, shutdown, _dir) =
        dkg_node(KeyMode::DkgAutomatic, FakeChain::clone, true).await;
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    s.chain.set_results_externally(tally(3, 5));
    s.chain.dkg_combine(vec![3, 5]);
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), tally(3, 5));
    assert_eq!(s.chain.dkg_calls().1.0, 0, "results were already set");
    shutdown.cancel();
}

/// Two signing nodes finalize the same DKG process from the same heartbeat.
/// Each waits a random moment and reads again before sending, so only one
/// request and one finalize reach the chain; the other node skips both.
#[tokio::test(start_paused = true)]
async fn two_dkg_nodes_send_each_call_once() {
    let s = setup(2, 8, None);
    s.chain.set_key_mode(KeyMode::DkgAutomatic);
    let shutdown = CancellationToken::new();
    let (dir_a, dir_b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut nodes = Vec::new();
    for (dir, signer) in [(&dir_a, 0xa1), (&dir_b, 0xb2)] {
        let node = start_node(
            Db::open_in(dir.path()).unwrap(),
            dir.path(),
            &s,
            s.chain.with_signer(signer),
            FakeProver::open(),
            2,
            "0s",
            shutdown.clone(),
        )
        .await;
        let h = handle(&node).await;
        nodes.push((node, h));
    }
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        nodes[0].1.submit(v.clone()).await.unwrap();
    }
    wait_until("settled and synced", async || {
        all_settled(&nodes[0].1, &vids).await
            && nodes[1].1.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (0, 0)), "a second request");
    s.chain.dkg_combine(vec![3, 5]);
    for (_, h) in &nodes {
        wait_long("finalized", async || {
            h.snapshot().await.unwrap().local == LocalStatus::Finalized
        })
        .await;
    }
    assert_eq!(s.chain.results(), tally(3, 5));
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (1, 1)), "a second finalize");
    shutdown.cancel();
}

/// Another node's request lands while this node pauses before sending its
/// own: the fresh read after the pause shows it, so none goes out.
#[tokio::test(start_paused = true)]
async fn dkg_request_seen_after_the_pause_is_not_sent() {
    let s = setup(2, 8, None);
    s.chain.set_key_mode(KeyMode::DkgAutomatic);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    // The attempt's first read sees no request; the other node's lands
    // before its second, the one right before sending.
    s.chain.race_dkg_request_on_read(2);
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("the other request seen", async || {
        s.chain.dkg_state().requested
    })
    .await;
    s.chain.dkg_combine(vec![3, 5]);
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), tally(3, 5));
    assert_eq!(s.chain.dkg_calls(), ((0, 0), (1, 1)), "request sent anyway");
    shutdown.cancel();
}

/// Another node's finalize lands between this node's readiness check and
/// its send: the read after the pause shows RESULTS, and it sends nothing.
#[tokio::test(start_paused = true)]
async fn dkg_finalize_seen_after_the_pause_is_not_sent() {
    let (s, _node, h, shutdown, _dir) =
        dkg_node(KeyMode::DkgAutomatic, FakeChain::clone, true).await;
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    s.chain.race_dkg_finalize_after_ready();
    s.chain.dkg_combine(vec![3, 5]);
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), tally(3, 5));
    assert_eq!(
        s.chain.dkg_calls(),
        ((1, 1), (0, 0)),
        "finalize sent anyway"
    );
    shutdown.cancel();
}

/// Nothing to decrypt: the request itself finalizes to zeros.
#[tokio::test(start_paused = true)]
async fn dkg_zero_vote_process_finalizes_on_the_request() {
    let (s, _node, h, shutdown, _dir) =
        dkg_node(KeyMode::DkgAutomatic, FakeChain::clone, false).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), vec![0u64; 2]);
    assert_eq!(s.chain.dkg_state().count, 0);
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (0, 0)));
    shutdown.cancel();
}

/// Observers never send, DKG mode included.
#[tokio::test(start_paused = true)]
async fn dkg_observer_stays_silent() {
    let (s, node, h, shutdown, _dir) =
        dkg_node(KeyMode::DkgAutomatic, FakeChain::observer, false).await;
    s.chain.dkg_combine(vec![]);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(s.chain.dkg_calls(), ((0, 0), (0, 0)));
    assert_ne!(h.snapshot().await.unwrap().local, LocalStatus::Finalized);
    assert_eq!(node.metrics.finalize_attempts.load(Ordering::SeqCst), 0);
    shutdown.cancel();
}

/// The request moves a READY-past-end process to ENDED in the same tx
/// (`StatusChanged` before `ResultsDecryptionRequested`): the node still
/// requests once and finalizes once.
#[tokio::test(start_paused = true)]
async fn dkg_request_on_ready_past_end_moves_to_ended() {
    let (s, _node, h, shutdown, _dir) =
        dkg_node_ended(KeyMode::DkgAutomatic, FakeChain::clone, true, true).await;
    assert_eq!(s.chain.status(), ProcessStatus::Ready);
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    assert_eq!(s.chain.status(), ProcessStatus::Ended);
    wait_until("actor saw ENDED", async || {
        h.snapshot().await.unwrap().status == ProcessStatus::Ended
    })
    .await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (0, 0)));
    s.chain.dkg_combine(vec![3, 5]);
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), tally(3, 5));
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (1, 1)));
    shutdown.cancel();
}

/// A finalize that loses to another node reverts `InvalidStatus` before
/// the chain shows RESULTS: transient, not a latch. The winner's events
/// never reach this node, so only its own retry can finalize it.
#[tokio::test(start_paused = true)]
async fn dkg_lost_finalize_is_retried_not_latched() {
    let (s, _node, h, shutdown, _dir) = dkg_node(
        KeyMode::DkgAutomatic,
        |c| {
            c.dkg_combine(vec![3, 5]);
            c.lose_dkg_finalize();
            c.clone()
        },
        true,
    )
    .await;
    // Behind the request's and the finalize's random delays: up to 20 s.
    wait_long("finalize lost", async || s.chain.dkg_calls().1.1 == 1).await;
    assert_ne!(s.chain.status(), ProcessStatus::Results);
    s.chain.set_results_externally(tally(3, 5));
    s.chain.hide_block(Some(s.chain.head_block()));
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    // The retry read RESULTS and did not send again.
    assert_eq!(s.chain.dkg_calls(), ((1, 1), (0, 1)));
    shutdown.cancel();
}

/// Another sequencer's last transition is on-chain but not yet in this
/// node's tree: no request (its proof would be for a stale root); once the
/// node resyncs, the request lands.
#[tokio::test(start_paused = true)]
async fn dkg_lagging_root_holds_the_request() {
    let s = setup(2, 8, None);
    s.chain.set_key_mode(KeyMode::DkgAutomatic);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let mut other =
        davinci_state::ProcessState::create(s.env.cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let b = other
        .prepare(&votes, &mut StdRng::seed_from_u64(3), &Default::default())
        .unwrap();
    // The transition event is hidden from this node for now.
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    s.chain.settle_externally(&b);
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(s.chain.dkg_calls(), ((0, 0), (0, 0)));
    assert_ne!(h.snapshot().await.unwrap().root, s.chain.root());
    // Resync: the event is delivered (replayed), the tree catches up.
    s.chain.hide_block(None);
    s.chain.set_replay_all(true);
    s.chain.set_head_block(s.chain.head_block() + 1);
    wait_long("resynced", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    s.chain.set_replay_all(false);
    wait_long("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    assert_eq!(s.chain.dkg_calls().0, (1, 1));
    assert_eq!(s.chain.dkg_state().count, 2);
    s.chain.dkg_combine(vec![3, 5]);
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results(), tally(3, 5));
    shutdown.cancel();
}

// ------------------------------------------------------ batching policy

/// A node with the given batching flags; `batch_max` 10 and a zero landing
/// margin unless the flags say otherwise.
async fn batching_node(
    db: Db,
    dir: &Path,
    s: &TestSetup,
    prover: Arc<FakeProver>,
    batch_max: usize,
    extra: &[&str],
    shutdown: CancellationToken,
) -> (Node, ActorHandle) {
    let cfg = test_config_with(dir, &s.census_dir, batch_max, "0s", extra);
    let node = start_node_cfg(db, s.chain.clone(), prover, cfg, shutdown).await;
    let h = handle(&node).await;
    (node, h)
}

/// Lets the actor run a few heartbeats.
async fn beats() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

const TIMERS: &[&str] = &["--batch-time", "100s", "--solo-wait", "300s"];

/// A lone vote has no company: past batch_time it still waits, and seals
/// alone once it is solo_wait old (both jittered by ±10%).
#[tokio::test]
async fn solo_vote_waits_past_batch_time_seals_at_solo_wait() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    s.chain.advance_time(115);
    beats().await;
    assert_eq!(prover.calls(), 0, "no seal past batch_time without company");
    s.chain.advance_time(150); // 265 s
    beats().await;
    assert_eq!(prover.calls(), 0, "no seal before solo_wait");
    s.chain.advance_time(70); // 335 s
    wait_until("solo seal", async || all_settled(&h, &[vid]).await).await;
    assert_eq!(prover.sizes(), vec![1]);
    shutdown.cancel();
}

/// A second arrival gives the old vote company: the pair is past
/// batch_time and seals at once.
#[tokio::test]
async fn second_arrival_makes_the_pair_due() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let a = fake_vote(&s.env, 0, &[1, 2], 5);
    let b = fake_vote(&s.env, 1, &[2, 1], 6);
    let vids = [a.pkg.vote_id, b.pkg.vote_id];
    h.submit(a).await.unwrap();
    s.chain.advance_time(115);
    beats().await;
    assert_eq!(prover.calls(), 0);
    h.submit(b).await.unwrap();
    wait_until("pair settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![2]);
    shutdown.cancel();
}

/// The snapshot advertises the seal bound: a lone vote at the solo
/// floor, a pair at the timer floor (both at the 0.9 jitter floor).
#[tokio::test]
async fn snapshot_advertises_the_seal_bound() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    // The fake clock starts at T0 and only advance_time moves it.
    let t0 = T0;
    let a = fake_vote(&s.env, 0, &[1, 2], 5);
    h.submit(a).await.unwrap();
    beats().await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.pending, 1);
    assert_eq!(snap.next_seal_not_before, Some(t0 + 270));
    let b = fake_vote(&s.env, 1, &[2, 1], 6);
    h.submit(b).await.unwrap();
    beats().await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.next_seal_not_before, Some(t0 + 90));
    shutdown.cancel();
}

/// A queue at --batch-max seals without waiting for any timer: the
/// bound collapses to now.
#[tokio::test]
async fn capacity_ready_queue_advertises_now() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        4,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    // The pair seals and is held in flight by the gated prover, so the
    // queue below can grow to batch_max without sealing under it: four
    // distinct voters, one per slot.
    let a = fake_vote(&s.env, 0, &[1, 2], 5);
    let b = fake_vote(&s.env, 1, &[2, 1], 6);
    h.submit(a).await.unwrap();
    h.submit(b).await.unwrap();
    s.chain.advance_time(115);
    wait_until("pair sealed", async || prover.calls() == 1).await;
    for i in 2..6usize {
        let v = fake_vote(&s.env, i, &[1, 2], 100 + i as u64);
        h.submit(v).await.unwrap();
    }
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.pending, 4);
    assert_eq!(snap.next_seal_not_before, Some(T0 + 115));
    shutdown.cancel();
}

/// A queue that no longer fits one transaction seals the largest batch
/// that does, without waiting for any timer.
#[tokio::test]
async fn capacity_full_seals_without_timer() {
    let nf = 16u8;
    // New slots only (no refreshes): n vote ids and n slot updates.
    let n_fit = (1..)
        .take_while(|&n| davinci_zkvm_sdk::blob::blob_count(n, n, nf) <= 1)
        .last()
        .unwrap();
    let s = setup(nf, n_fit + 1, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        1024,
        &["--max-blobs-per-tx", "1"],
        shutdown.clone(),
    )
    .await;
    for i in 0..=n_fit {
        h.submit(fake_vote(&s.env, i, &[1, 2], 1000 + i as u64))
            .await
            .unwrap();
    }
    wait_until("capacity seal", async || s.chain.voters() == n_fit as u64).await;
    beats().await;
    assert_eq!(
        prover.sizes(),
        vec![n_fit],
        "one full batch, the rest waits"
    );
    assert_eq!(h.snapshot().await.unwrap().pending, 1);
    shutdown.cancel();
}

/// The seal jitter stays within ±10% and spreads over the band.
#[test]
fn jitter_within_bounds() {
    use davinci_sequencer::actor::seal_jitter;
    let mut rng = StdRng::seed_from_u64(7);
    let js: Vec<f64> = (0..10_000).map(|_| seal_jitter(&mut rng)).collect();
    assert!(js.iter().all(|j| (0.9..=1.1).contains(j)));
    let lo = js.iter().copied().fold(f64::MAX, f64::min);
    let hi = js.iter().copied().fold(f64::MIN, f64::max);
    assert!(lo < 0.91 && hi > 1.09, "spread {lo}..{hi}");
}

/// Company counts distinct admissible slots, not queue length: two votes
/// of one slot are a solo batch each, in cast order.
#[tokio::test]
async fn min_mix_counts_trial_not_pending() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let a = fake_vote(&s.env, 0, &[1, 2], 5);
    let b = fake_vote(&s.env, 0, &[2, 1], 6);
    let (va, vb) = (a.pkg.vote_id, b.pkg.vote_id);
    h.submit(a).await.unwrap();
    h.submit(b).await.unwrap();
    s.chain.advance_time(115);
    beats().await;
    assert_eq!(prover.calls(), 0, "two votes of one slot are no company");
    s.chain.advance_time(220); // 335 s
    wait_until("solo seal", async || prover.calls() == 1).await;
    prover.release(1);
    wait_until("first settled", async || all_settled(&h, &[va]).await).await;
    assert_ne!(vote_status(&h, vb).await, Some(VoteStatus::Settled));
    prover.release(1);
    wait_until("second settled", async || all_settled(&h, &[vb]).await).await;
    assert_eq!(prover.sizes(), vec![1, 1]);
    shutdown.cancel();
}

/// Two queued votes of one slot settle one after the other, oldest first.
#[tokio::test]
async fn slot_fifo_settles_both_in_order() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    // Another voter's batch holds the prover while the slot's votes queue.
    h.submit(fake_vote(&s.env, 1, &[1, 1], 4)).await.unwrap();
    wait_until("first in flight", async || prover.calls() == 1).await;
    let a = fake_vote(&s.env, 0, &[1, 2], 5);
    let b = fake_vote(&s.env, 0, &[2, 1], 6);
    let (va, vb) = (a.pkg.vote_id, b.pkg.vote_id);
    h.submit(a).await.unwrap();
    h.submit(b).await.unwrap();
    prover.release(2);
    wait_until("older settled", async || all_settled(&h, &[va]).await).await;
    assert_ne!(vote_status(&h, vb).await, Some(VoteStatus::Settled));
    prover.release(1);
    wait_until("newer settled", async || all_settled(&h, &[vb]).await).await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!((snap.occupied, snap.overwrites), (2, 1));
    shutdown.cancel();
}

/// A lost race puts the flight back ahead of the later votes of its slots
/// with its original age: the older vote re-seals first.
#[tokio::test]
async fn requeue_preserves_age_and_order() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let mut other =
        davinci_state::ProcessState::create(s.env.cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let theirs = [
        fake_vote(&s.env, 2, &[1, 1], 60),
        fake_vote(&s.env, 3, &[2, 1], 61),
    ];
    let b = other
        .prepare(&theirs, &mut StdRng::seed_from_u64(5), &Default::default())
        .unwrap();
    s.chain.race_next_submit(b);
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    let a = fake_vote(&s.env, 0, &[1, 2], 5);
    let a2 = fake_vote(&s.env, 0, &[2, 1], 6);
    let (va, va2) = (a.pkg.vote_id, a2.pkg.vote_id);
    h.submit(a).await.unwrap();
    wait_until("a in flight", async || prover.calls() == 1).await;
    s.chain.advance_time(50);
    h.submit(a2).await.unwrap();
    // Requeued at a later clock: an age reset would sort a behind a2.
    s.chain.advance_time(50);
    prover.release(1);
    wait_until("re-sealed", async || prover.calls() == 2).await;
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(vote_status(&h, va).await, Some(VoteStatus::Aggregated));
    assert_eq!(vote_status(&h, va2).await, Some(VoteStatus::Pending));
    assert_eq!(h.status(va).await.unwrap().unwrap().created_at, T0);
    prover.release(2);
    wait_until("both settled", async || all_settled(&h, &[va, va2]).await).await;
    assert_eq!(prover.sizes(), vec![1, 1, 1]);
    shutdown.cancel();
}

/// Restart variant: recovery requeues the in-flight vote ahead of the
/// later vote of its slot, with its original age.
#[tokio::test]
async fn requeue_preserves_age_and_order_across_restart() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    let a = fake_vote(&s.env, 0, &[1, 2], 5);
    let a2 = fake_vote(&s.env, 0, &[2, 1], 6);
    let (va, va2) = (a.pkg.vote_id, a2.pkg.vote_id);
    h.submit(a).await.unwrap();
    wait_until("a in flight", async || prover.calls() == 1).await;
    s.chain.advance_time(50);
    h.submit(a2).await.unwrap();
    s.chain.advance_time(50);
    shutdown.cancel();
    drop(h);
    drop(node);

    let db = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let prover2 = FakeProver::gated();
    let (_node2, h2) = batching_node(
        db,
        dir.path(),
        &s,
        prover2.clone(),
        1,
        &[],
        shutdown2.clone(),
    )
    .await;
    wait_until("re-sealed", async || prover2.calls() == 1).await;
    assert_eq!(vote_status(&h2, va).await, Some(VoteStatus::Aggregated));
    assert_eq!(vote_status(&h2, va2).await, Some(VoteStatus::Pending));
    prover2.release(1);
    wait_until("older settled", async || all_settled(&h2, &[va]).await).await;
    assert_ne!(vote_status(&h2, va2).await, Some(VoteStatus::Settled));
    prover2.release(1);
    wait_until("newer settled", async || all_settled(&h2, &[va2]).await).await;
    shutdown2.cancel();
}

// ~10 s per vote at nf 2 (prove_base 30 s).
fn slow(node: &Node) {
    for _ in 0..60 {
        node.metrics.prove.observe(2, 1, 40.0, 30.0);
    }
}

/// In the grace the budget sizes a batch to land before the grace end, and
/// each landing moves that end: 70 s left fits 4 votes, the landing's fresh
/// 150 s fits the rest.
#[tokio::test]
async fn budget_shrinks_flush_batches() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let votes: Vec<VerifiedVote> = (0..8)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    slow(&node);
    // Paused until 80 s past the end: the grace end (end + 150) binds.
    s.chain.set_status(ProcessStatus::Paused);
    s.chain.advance_time(DURATION + 80);
    wait_until("all settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![4, 4]);
    assert_eq!(s.chain.last_vote_at(), T0 + DURATION + 80);
    shutdown.cancel();
}

/// Past the end only the grace end binds, not now + floor: with the grace
/// raised to 600 s, 590 s left fits all 15 votes (the floor would cap at 12).
#[tokio::test]
async fn budget_past_end_uses_grace_end() {
    let s = setup(2, 16, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        20,
        // Only the flush seals, so the head time is past the end.
        &["--batch-time", "1000000s", "--solo-wait", "3000000s"],
        shutdown.clone(),
    )
    .await;
    s.chain.set_grace(600);
    beats().await;
    let votes: Vec<VerifiedVote> = (0..15)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    slow(&node);
    s.chain.set_status(ProcessStatus::Paused);
    s.chain.advance_time(DURATION + 10);
    wait_until("all settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![15]);
    shutdown.cancel();
}

/// The END events arrive before a head shows the END block's time: the
/// registry's `Ended` status already says the end passed, so the first
/// flush is sized by the 600 s grace (all 15 votes), not by the open
/// window's now + 150 s floor (12).
#[tokio::test]
async fn first_flush_after_an_end_uses_the_grace() {
    let s = setup(2, 16, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        20,
        // Only the flush seals.
        &["--batch-time", "1000000s", "--solo-wait", "3000000s"],
        shutdown.clone(),
    )
    .await;
    s.chain.set_grace(600);
    beats().await;
    let votes: Vec<VerifiedVote> = (0..15)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    slow(&node);
    s.chain.end_ahead_of_head(5);
    wait_until("all settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![15]);
    shutdown.cancel();
}

/// Far from the end a batch is still sized to survive the earliest legal
/// close, an END now under the floor grace: 150 − 30 s fits 12 votes, and
/// the END right after sealing still lands them.
#[tokio::test]
async fn budget_respects_worst_case_close() {
    let s = setup(2, 16, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        20,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let votes: Vec<VerifiedVote> = (0..15)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    slow(&node);
    s.chain.advance_time(115);
    wait_until("sealed", async || prover.calls() == 1).await;
    assert_eq!(prover.sizes(), vec![12]);
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.advance_time(149);
    prover.release(10);
    wait_until("all settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![12, 3]);
    shutdown.cancel();
}

/// `spv ← 0.7·spv + 0.3·max(0.05, (secs − base)/n)`, seeded at
/// `max(0.2, 0.05·nf)`, fed by every finished proving job.
#[tokio::test]
async fn ewma_update_rule() {
    use davinci_sequencer::metrics::ProveModel;
    assert_eq!(ProveModel::seed(2), 0.2);
    assert_eq!(ProveModel::seed(16), 0.8);
    let m = ProveModel::default();
    assert_eq!(m.spv(4), 0.2);
    m.observe(4, 10, 130.0, 30.0); // 10 s/vote
    assert!((m.spv(4) - (0.7 * 0.2 + 0.3 * 10.0)).abs() < 1e-12);
    let before = m.spv(4);
    m.observe(4, 10, 1.0, 30.0); // faster than the base: floored
    assert!((m.spv(4) - (0.7 * before + 0.3 * ProveModel::FLOOR)).abs() < 1e-12);
    assert_eq!(m.spv(8), 0.4, "per field count");
    // A stalled job counts as at most CEIL_SEEDS seeds per vote.
    let before = m.spv(4);
    m.observe(4, 1, 1830.0, 30.0);
    let ceil = ProveModel::CEIL_SEEDS * ProveModel::seed(4);
    assert!((m.spv(4) - (0.7 * before + 0.3 * ceil)).abs() < 1e-12);
    assert!((m.estimate(4, 3, 30.0) - (30.0 + 3.0 * m.spv(4))).abs() < 1e-12);

    // Wiring: a job's proving seconds, on the node clock, update the model.
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("proving", async || prover.calls() == 1).await;
    s.chain.advance_time(40);
    prover.release(1);
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    let spv = node.metrics.prove.spv(2);
    assert!((spv - (0.7 * 0.2 + 0.3 * 10.0)).abs() < 1e-9, "spv {spv}");
    // The prover's own proving time wins over the wall time (queue wait).
    *prover.reported.lock().unwrap() = Some(31.0);
    let v = fake_vote(&s.env, 1, &[1, 2], 6);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("proving", async || prover.calls() == 2).await;
    s.chain.advance_time(1800);
    prover.release(1);
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    let want = 0.7 * spv + 0.3 * 1.0;
    let got = node.metrics.prove.spv(2);
    assert!((got - want).abs() < 1e-9, "spv {got}");
    shutdown.cancel();
}

/// A job stalled behind the prover's queue must not starve the window: with
/// no proving time reported, the capped observation still fits a vote.
#[tokio::test]
async fn stalled_job_does_not_starve_the_window() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    for _ in 0..60 {
        node.metrics.prove.observe(2, 1, 1830.0, 30.0);
    }
    s.chain.advance_time(DURATION - 100);
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("flushed", async || all_settled(&h, &[vid]).await).await;
    shutdown.cancel();
}

/// Within flush_horizon of the end a lone, young vote seals at once.
#[tokio::test]
async fn flush_mode_ignores_min_mix() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    s.chain.advance_time(DURATION - 400);
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    beats().await;
    assert_eq!(prover.calls(), 0, "outside the flush horizon");
    s.chain.advance_time(300); // 100 s left, horizon 180 s
    wait_until("flushed", async || all_settled(&h, &[vid]).await).await;
    assert_eq!(prover.sizes(), vec![1]);
    shutdown.cancel();
}

/// Another sequencer fills max_voters: this node's queued new-slot vote
/// errors as soon as it syncs, while its overwrite stays and settles.
#[tokio::test]
async fn max_voters_loser_errors_promptly() {
    let s = setup(2, 8, None);
    s.chain.set_max_voters(2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let keep = fake_vote(&s.env, 0, &[1, 2], 90);
    let lose = fake_vote(&s.env, 1, &[1, 2], 91);
    let (vk, vl) = (keep.pkg.vote_id, lose.pkg.vote_id);
    h.submit(keep).await.unwrap();
    h.submit(lose).await.unwrap();
    // The winner occupies voter 0's slot (ours becomes an overwrite) and a
    // second one: the process is full.
    let mut other =
        davinci_state::ProcessState::create(s.env.cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let theirs = [
        fake_vote(&s.env, 0, &[2, 2], 92),
        fake_vote(&s.env, 2, &[2, 1], 93),
    ];
    let b = other
        .prepare(&theirs, &mut StdRng::seed_from_u64(9), &Default::default())
        .unwrap();
    s.chain.settle_externally(&b);
    wait_until("loser errored", async || {
        vote_status(&h, vl).await == Some(VoteStatus::Error)
    })
    .await;
    let e = h.status(vl).await.unwrap().unwrap().error.unwrap();
    assert_eq!(e, "max voters reached");
    assert_eq!(prover.calls(), 0, "errored before any seal");
    assert_eq!(vote_status(&h, vk).await, Some(VoteStatus::Pending));
    // The overwrite seals on its solo timer (3 h ±10%).
    s.chain.advance_time(4 * 3600);
    wait_until("overwrite settled", async || all_settled(&h, &[vk]).await).await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!((snap.occupied, snap.overwrites), (2, 1));
    shutdown.cancel();
}

/// A revote of a voter whose new slot is still queued reserves no second
/// slot; a third distinct voter is still refused.
#[tokio::test]
async fn revote_of_queued_new_slot_is_not_max_voters() {
    let s = setup(2, 8, None);
    s.chain.set_max_voters(2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        FakeProver::open(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    h.submit(fake_vote(&s.env, 0, &[1, 2], 80)).await.unwrap();
    h.submit(fake_vote(&s.env, 1, &[1, 2], 81)).await.unwrap();
    h.submit(fake_vote(&s.env, 0, &[2, 2], 82))
        .await
        .expect("revote of a queued voter");
    match h.submit(fake_vote(&s.env, 2, &[1, 2], 83)).await {
        Err(ActorError::MaxVoters) => {}
        other => panic!("expected MaxVoters, got {other:?}"),
    }
    shutdown.cancel();
}

/// A resend of a queued vote id is a duplicate even when its slot is at depth.
#[tokio::test]
async fn resend_at_full_slot_depth_is_duplicate() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        FakeProver::open(),
        10,
        &["--slot-depth", "1"],
        shutdown.clone(),
    )
    .await;
    let v = fake_vote(&s.env, 0, &[1, 2], 80);
    h.submit(v.clone()).await.unwrap();
    match h.submit(v).await {
        Err(ActorError::Duplicate(_)) => {}
        other => panic!("expected Duplicate, got {other:?}"),
    }
    shutdown.cancel();
}

// Voter 0 lands from another sequencer (occupied 1 of 2), voter 1 queues here.
async fn one_occupied_one_queued(s: &TestSetup, h: &ActorHandle) -> u64 {
    let v = fake_vote(&s.env, 1, &[1, 2], 91);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    let mut other =
        davinci_state::ProcessState::create(s.env.cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let b = other
        .prepare(
            &[fake_vote(&s.env, 0, &[2, 2], 92)],
            &mut StdRng::seed_from_u64(9),
            &Default::default(),
        )
        .unwrap();
    s.chain.settle_externally(&b);
    wait_until("synced", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(vote_status(h, vid).await, Some(VoteStatus::Pending));
    vid
}

/// Lowering max_voters to the occupied count errors queued new-slot votes
/// at once.
#[tokio::test]
async fn max_voters_lowered_errors_new_slots() {
    let s = setup(2, 8, None);
    s.chain.set_max_voters(2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        FakeProver::open(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let vid = one_occupied_one_queued(&s, &h).await;
    s.chain.set_max_voters(1);
    s.chain.advance_time(0);
    s.chain.push_event(EventKind::MaxVotersChanged {
        pid: pid31(),
        max_voters: 1,
    });
    wait_until("errored", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Error)
    })
    .await;
    shutdown.cancel();
}

/// A restart over a full tree errors the recovered new-slot votes.
#[tokio::test]
async fn max_voters_full_on_recover_errors_new_slots() {
    let s = setup(2, 8, None);
    s.chain.set_max_voters(2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db.clone(),
        dir.path(),
        &s,
        FakeProver::open(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let vid = one_occupied_one_queued(&s, &h).await;
    shutdown.cancel();
    drop((h, node));
    // The cap drops while the node is down (its event already consumed).
    s.chain.set_max_voters(1);
    drop(db);
    let db = reopen_db(dir.path()).await;
    let mut rec = db.process(&pid_fr()).unwrap().unwrap();
    rec.onchain.max_voters = 1;
    db.put_process(&rec).unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        FakeProver::open(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Error));
    shutdown.cancel();
}

/// Idle heartbeats with a queue below the capacity estimate and no
/// deadline due read no storage; a due timer does.
#[tokio::test]
async fn capacity_trial_skipped_below_estimate() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db.clone(),
        dir.path(),
        &s,
        prover.clone(),
        10,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let votes: Vec<VerifiedVote> = (0..3)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    beats().await;
    let reads = db.arbo_reads();
    beats().await;
    assert_eq!(db.arbo_reads(), reads, "idle heartbeats read storage");
    s.chain.advance_time(115);
    wait_until("timer seal", async || all_settled(&h, &vids).await).await;
    assert!(db.arbo_reads() > reads);
    assert_eq!(prover.sizes(), vec![3]);
    shutdown.cancel();
}

// --------------------------------------------------------- grace window

// Sequencer-key results carry all 16 fields.
fn key_tally(a: u64, b: u64) -> Vec<u64> {
    let mut t = vec![0u64; 16];
    (t[0], t[1]) = (a, b);
    t
}

/// Two votes for voters `a` and `b`, prepared by another sequencer on the
/// genesis root.
fn foreign_batch(s: &TestSetup, a: usize, b: usize) -> davinci_state::PreparedBatch {
    let mut other =
        davinci_state::ProcessState::create(s.env.cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let theirs = [
        fake_vote(&s.env, a, &[1, 2], 70),
        fake_vote(&s.env, b, &[2, 3], 71),
    ];
    other
        .prepare(&theirs, &mut StdRng::seed_from_u64(7), &Default::default())
        .unwrap()
}

/// Past the end with no event the queue flushes; admission is closed.
#[tokio::test]
async fn time_ended_election_flushes_without_event() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    beats().await;
    assert_eq!(prover.calls(), 0);
    s.chain.advance_time(DURATION + 1);
    wait_until("flushed", async || all_settled(&h, &vids).await).await;
    assert_eq!(s.chain.voters(), 2);
    assert_eq!(s.chain.status(), ProcessStatus::Ready);
    assert!(!h.snapshot().await.unwrap().accepting);
    let late = fake_vote(&s.env, 3, &[1, 1], 99);
    assert!(matches!(h.submit(late).await, Err(ActorError::Closed(_))));
    shutdown.cancel();
}

/// A vote admitted a second before the end lands in the grace, which then
/// runs from its landing.
#[tokio::test]
async fn vote_admitted_at_end_minus_epsilon_settles_in_grace() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    s.chain.advance_time(DURATION - 1);
    wait_until("head seen", async || prover.calls() == 0).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.advance_time(2);
    prover.release(1);
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    let end = T0 + DURATION;
    assert_eq!(s.chain.last_vote_at(), end + 1);
    assert_eq!(s.chain.grace_end(), end + 1 + GRACE.default_grace);
    shutdown.cancel();
}

/// Each landing moves the grace end; once it passes, it is frozen and a
/// batch still in flight errors.
#[tokio::test]
async fn idle_grace_extends_and_freezes() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    let end = T0 + DURATION;
    s.chain.advance_time(DURATION - 10);
    beats().await;
    let votes: Vec<VerifiedVote> = (0..3)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.advance_time(60); // end + 50
    prover.release(1);
    wait_until("first", async || all_settled(&h, &vids[..1]).await).await;
    assert_eq!(s.chain.grace_end(), end + 200);
    wait_until("resealed", async || prover.calls() == 2).await;
    s.chain.advance_time(110); // end + 160: past end + grace, not the moved end
    beats().await;
    assert_eq!(vote_status(&h, vids[2]).await, Some(VoteStatus::Pending));
    prover.release(1);
    wait_until("second", async || all_settled(&h, &vids[..2]).await).await;
    assert_eq!(s.chain.grace_end(), end + 310);
    wait_until("third sealed", async || prover.calls() == 3).await;
    s.chain.pass_grace();
    beats().await;
    prover.release(1);
    wait_until("third errored", async || {
        vote_status(&h, vids[2]).await == Some(VoteStatus::Error)
    })
    .await;
    let sv = h.status(vids[2]).await.unwrap().unwrap();
    assert!(sv.error.unwrap().contains("closed"));
    assert_eq!(s.chain.grace_end(), end + 310, "frozen");
    assert_eq!(s.chain.voters(), 2);
    shutdown.cancel();
}

/// A backlog wider than one grace drains batch by batch, each landing
/// buying the next its grace.
#[tokio::test]
async fn backlog_drains_across_extended_grace() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 2, &[], shutdown.clone()).await;
    let votes: Vec<VerifiedVote> = (0..6)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.advance_time(DURATION + 1);
    for k in 1..=3u64 {
        s.chain.advance_time(140);
        prover.release(1);
        wait_until("landed", async || s.chain.voters() == 2 * k).await;
        if k < 3 {
            wait_until("next sealed", async || prover.calls() == k + 1).await;
        }
    }
    wait_until("all settled", async || all_settled(&h, &vids).await).await;
    assert!(s.chain.last_vote_at() > T0 + DURATION + GRACE.default_grace);
    shutdown.cancel();
}

/// Losing a race in the grace: the winner's landing moved the grace end,
/// and the reseal lands within it, past the original one.
#[tokio::test]
async fn lost_race_during_grace_flush_reseals_within_extended_window() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    s.chain.advance_time(DURATION + 100);
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.race_next_submit(foreign_batch(&s, 2, 3));
    s.chain.advance_time(40); // end + 140
    prover.release(1);
    wait_until("resealed", async || prover.calls() == 2).await;
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 1);
    s.chain.advance_time(100); // end + 240: past end + grace
    prover.release(1);
    wait_until("all settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(s.chain.voters(), 4);
    assert_eq!(s.chain.last_vote_at(), T0 + DURATION + 240);
    shutdown.cancel();
}

/// Votes the budget cannot fit before the grace end wait, and error when
/// it passes.
#[tokio::test]
async fn grace_end_errors_leftovers() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let votes: Vec<VerifiedVote> = (0..3)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    slow(&node);
    // 25 s left: one vote needs 30 + 10.
    s.chain.set_status(ProcessStatus::Paused);
    s.chain.advance_time(DURATION + 125);
    beats().await;
    assert_eq!(prover.calls(), 0);
    assert_eq!(vote_status(&h, vids[0]).await, Some(VoteStatus::Pending));
    s.chain.pass_grace();
    for vid in &vids {
        wait_until("errored", async || {
            vote_status(&h, *vid).await == Some(VoteStatus::Error)
        })
        .await;
    }
    let sv = h.status(vids[0]).await.unwrap().unwrap();
    assert!(sv.error.unwrap().contains("closed"));
    assert_eq!(prover.calls(), 0);
    shutdown.cancel();
}

/// Eager results start only once nothing is in flight or pending, and are
/// submitted at the grace end.
#[tokio::test]
async fn eager_trigger_quiescence() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    prover.gate_results();
    let (_node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.advance_time(DURATION + 1);
    beats().await;
    assert_eq!(prover.results_calls(), 0, "in flight and pending");
    prover.release(1);
    wait_until("second sealed", async || prover.calls() == 2).await;
    beats().await;
    assert_eq!(prover.results_calls(), 0, "in flight");
    prover.release(1);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    wait_until("eager proof", async || prover.results_calls() == 1).await;
    prover.release_results(1);
    beats().await;
    assert_eq!(s.chain.results_submits(), 0, "tally broadcast in the grace");
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    assert_eq!(s.chain.results(), key_tally(3, 5));
    assert_eq!(prover.results_calls(), 1);
    shutdown.cancel();
}

/// Right after a restart the node is not quiescent: a landing it missed
/// while down must be synced before the eager proof.
#[tokio::test]
async fn eager_waits_for_the_chain_after_recover() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown1 = CancellationToken::new();
    let prover1 = FakeProver::open();
    let (node, h) = batching_node(
        db.clone(),
        dir.path(),
        &s,
        prover1.clone(),
        1,
        &[],
        shutdown1.clone(),
    )
    .await;
    s.chain.advance_time(DURATION + 1);
    s.chain.set_status(ProcessStatus::Ended);
    wait_until("eager proof", async || prover1.results_calls() == 1).await;
    shutdown1.cancel();
    drop(h);
    drop(node);
    drop(db);
    // Another sequencer lands while this node is down.
    s.chain.settle_externally(&foreign_batch(&s, 0, 1));
    let db2 = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let prover2 = FakeProver::open();
    prover2.gate_results();
    let (_node2, h2) = batching_node(
        db2,
        dir.path(),
        &s,
        prover2.clone(),
        1,
        &[],
        shutdown2.clone(),
    )
    .await;
    wait_until("proving", async || prover2.results_calls() == 1).await;
    prover2.release_results(1);
    wait_until("synced", async || {
        h2.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    beats().await;
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    assert_eq!(s.chain.results(), key_tally(3, 5));
    assert_eq!(prover2.results_calls(), 1, "no proof at the stale root");
    shutdown2.cancel();
}

/// A landing-time read that keeps failing, or shows a root behind the
/// events, holds close-out and results, is counted as a stall, and lets
/// them go once it clears.
#[tokio::test]
async fn stalled_landing_read_holds_results_and_is_counted() {
    for lag in [false, true] {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let pk = node_key(&db);
        let s = setup(2, 8, Some(pk));
        let shutdown = CancellationToken::new();
        let prover = FakeProver::open();
        let (node, h) =
            batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
        s.chain.advance_time(DURATION + 1);
        s.chain.set_status(ProcessStatus::Ended);
        beats().await;
        if lag {
            s.chain.lag_process(u32::MAX);
        } else {
            s.chain.fail_process(u32::MAX);
        }
        s.chain.settle_externally(&foreign_batch(&s, 0, 1));
        wait_until("synced", async || {
            h.snapshot().await.unwrap().root == s.chain.root()
        })
        .await;
        s.chain.pass_grace();
        wait_until("stall counted", async || {
            node.metrics.stalled_reads.load(Ordering::SeqCst) >= 1
        })
        .await;
        beats().await;
        assert!(
            s.chain.results().is_empty(),
            "lag={lag}: results on a stale read"
        );
        s.chain.lag_process(0);
        s.chain.fail_process(0);
        wait_until("results on chain", async || !s.chain.results().is_empty()).await;
        assert_eq!(s.chain.results(), key_tally(3, 5), "lag={lag}");
        shutdown.cancel();
    }
}

/// With eager results off nothing is proved before the grace end.
#[tokio::test]
async fn finalize_waits_for_grace_end() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        2,
        &["--eager-results", "false"],
        shutdown.clone(),
    )
    .await;
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    beats().await;
    assert_eq!(prover.results_calls(), 0);
    assert_eq!(s.chain.results_submits(), 0);
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    assert_eq!(s.chain.results(), key_tally(3, 5));
    shutdown.cancel();
}

/// A pause holds the queue before the end; once the end passes the paused
/// process flushes through the grace like a ready one.
#[tokio::test]
async fn paused_at_end_flushes_through_grace() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    // Paused first: with batch_time 0 two votes would seal at once.
    s.chain.set_status(ProcessStatus::Paused);
    beats().await;
    for v in votes {
        h.submit(v).await.unwrap();
    }
    s.chain.advance_time(DURATION - 10); // in the flush horizon
    beats().await;
    assert_eq!(prover.calls(), 0, "paused before the end");
    s.chain.advance_time(20);
    wait_until("flushed", async || all_settled(&h, &vids).await).await;
    assert_eq!(s.chain.status(), ProcessStatus::Paused);
    assert_eq!(s.chain.voters(), 2);
    shutdown.cancel();
}

/// InvalidStatus in the grace requeues the batch; after the grace end the
/// same revert errors the batch in flight.
#[tokio::test]
async fn revert_during_grace_is_transient() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 100 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.advance_time(DURATION + 10);
    beats().await;
    s.chain.revert_simulates("InvalidStatus", 1);
    prover.release(1);
    wait_until("requeued and resealed", async || prover.calls() == 2).await;
    assert_eq!(vote_status(&h, vids[0]).await, Some(VoteStatus::Aggregated));
    prover.release(1);
    wait_until("first", async || all_settled(&h, &vids[..1]).await).await;
    wait_until("second sealed", async || prover.calls() == 3).await;
    s.chain.pass_grace();
    beats().await;
    prover.release(1);
    wait_until("second errored", async || {
        vote_status(&h, vids[1]).await == Some(VoteStatus::Error)
    })
    .await;
    let sv = h.status(vids[1]).await.unwrap().unwrap();
    assert!(sv.error.unwrap().contains("closed"));
    shutdown.cancel();
}

/// A revert in the grace while getProcess fails requeues: the local grace
/// end misses a foreign landing (end + 100 moves it to end + 250), so only
/// the chain can say the window closed, and an RPC error is not that.
#[tokio::test]
async fn revert_with_failing_read_requeues() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 1, &[], shutdown.clone()).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 100);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    s.chain.advance_time(DURATION + 10);
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.fail_process(u32::MAX);
    s.chain.advance_time(90);
    s.chain.settle_externally(&foreign_batch(&s, 3, 4));
    wait_until("lost and resealed", async || prover.calls() == 2).await;
    prover.release(1); // the aborted flight's job
    s.chain.advance_time(60); // end + 160: past the stale local grace end
    beats().await;
    s.chain.revert_simulates("InvalidTimeBounds", 1);
    prover.release(1);
    beats().await;
    beats().await;
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Pending));
    s.chain.fail_process(0);
    prover.release(1);
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    shutdown.cancel();
}

/// A shortened end puts the queue in flush mode at once; admission closes
/// at the new end.
#[tokio::test]
async fn shorten_enters_flush_and_closes_admission() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (_node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    let v0 = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid0 = v0.pkg.vote_id;
    h.submit(v0).await.unwrap();
    beats().await;
    assert_eq!(prover.calls(), 0);
    s.chain.set_end(T0 + 100);
    wait_until("flushed", async || all_settled(&h, &[vid0]).await).await;
    s.chain.advance_time(99);
    beats().await;
    let v1 = fake_vote(&s.env, 1, &[1, 2], 6);
    let vid1 = v1.pkg.vote_id;
    h.submit(v1).await.unwrap();
    wait_until("admitted before the new end", async || {
        all_settled(&h, &[vid1]).await
    })
    .await;
    s.chain.advance_time(2);
    wait_until("closed", async || !h.snapshot().await.unwrap().accepting).await;
    let late = fake_vote(&s.env, 2, &[1, 1], 7);
    assert!(matches!(h.submit(late).await, Err(ActorError::Closed(_))));
    shutdown.cancel();
}

// ------------------------------------------------------- failed batches

// Votes `0..n` of the election, submitted in order.
async fn submit_n(h: &ActorHandle, s: &TestSetup, n: usize, kseed: u64) -> Vec<u64> {
    let mut vids = Vec::new();
    for i in 0..n {
        let v = fake_vote(&s.env, i, &[1, 2], kseed + i as u64);
        vids.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    vids
}

async fn vote_error(h: &ActorHandle, vid: u64) -> String {
    h.status(vid)
        .await
        .unwrap()
        .unwrap()
        .error
        .unwrap_or_default()
}

/// A prover that fails every batch holding vote X: one full-size retry,
/// then halvings down to X alone, which is errored with the prover's
/// reason; every other vote settles. 8 votes: 1 + 1 + log2(8) = 5 failed
/// batches (8, 8, 4, 2, 1).
#[tokio::test(start_paused = true)]
async fn bisection_isolates_the_failing_vote() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        8,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let x = fake_vote(&s.env, 5, &[1, 2], 305).pkg.vote_id;
    *prover.fail_vid.lock().unwrap() = Some(x);
    let vids = submit_n(&h, &s, 8, 300).await;
    assert_eq!(vids[5], x);
    // The batch timer releases the leftovers once the bisection is over.
    s.chain.advance_time(400);
    let others: Vec<u64> = vids.iter().copied().filter(|v| *v != x).collect();
    wait_long("X errored, the rest settled", async || {
        vote_status(&h, x).await == Some(VoteStatus::Error) && all_settled(&h, &others).await
    })
    .await;
    let e = vote_error(&h, x).await;
    assert!(
        e.contains("guest panicked") && e.contains("isolated by bisection"),
        "{e}"
    );
    assert_eq!(prover.sizes(), vec![8, 8, 4, 4, 2, 1, 1, 1, 1]);
    assert_eq!(node.metrics.bisections.load(Ordering::SeqCst), 5);
    assert_eq!(node.metrics.isolated_votes.load(Ordering::SeqCst), 1);
    assert_eq!(s.chain.voters(), 7);
    // The landings ended the bisection: later votes seal whole again.
    let mut more = Vec::new();
    for i in 0..3 {
        let v = fake_vote(&s.env, i, &[2, 1], 350 + i as u64);
        more.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    s.chain.advance_time(400);
    wait_long("revotes settled", async || all_settled(&h, &more).await).await;
    assert_eq!(*prover.sizes().last().unwrap(), 3);
    shutdown.cancel();
}

/// A prover that fails every batch: three votes are isolated and errored,
/// then the actor holds; no further vote errors, and the held votes settle
/// once the prover recovers.
#[tokio::test(start_paused = true)]
async fn prover_failing_every_batch_holds_after_three_isolations() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches_permanent.lock().unwrap() = u32::MAX;
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        5,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let vids = submit_n(&h, &s, 5, 400).await;
    s.chain.advance_time(400);
    wait_long("three isolated", async || {
        node.metrics.isolated_votes.load(Ordering::SeqCst) == 3
    })
    .await;
    // Holding: retries go on at full size, nothing more errors.
    let calls = prover.calls();
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert!(prover.calls() > calls, "still retrying");
    assert_eq!(node.metrics.isolated_votes.load(Ordering::SeqCst), 3);
    for vid in &vids[..3] {
        assert_eq!(vote_status(&h, *vid).await, Some(VoteStatus::Error));
    }
    for vid in &vids[3..] {
        assert_eq!(vote_status(&h, *vid).await, Some(VoteStatus::Pending));
    }
    assert_eq!(
        *prover.sizes().last().unwrap(),
        2,
        "held batches are full size"
    );
    *prover.fail_batches_permanent.lock().unwrap() = 0;
    wait_long("held votes settled", async || {
        all_settled(&h, &vids[3..]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 2);
    shutdown.cancel();
}

/// A close during a bisection errors the pending suspects with the plain
/// close reason.
#[tokio::test(start_paused = true)]
async fn close_during_bisection_reads_process_closed() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches_permanent.lock().unwrap() = u32::MAX;
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 4, &[], shutdown.clone()).await;
    let vids = submit_n(&h, &s, 4, 500).await;
    // Two failures (full size, then the retry): requeued, cap now 2.
    wait_long("bisecting, nothing in flight", async || {
        prover.calls() == 2 && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    for vid in &vids {
        assert_eq!(vote_status(&h, *vid).await, Some(VoteStatus::Pending));
    }
    s.chain.set_status(ProcessStatus::Canceled);
    for vid in &vids {
        wait_long("errored", async || {
            vote_status(&h, *vid).await == Some(VoteStatus::Error)
        })
        .await;
        assert_eq!(vote_error(&h, *vid).await, "process closed");
    }
    assert_eq!(node.metrics.isolated_votes.load(Ordering::SeqCst), 0);
    shutdown.cancel();
}

/// A node-side check failure (the program vk pin) on every proof holds the
/// votes: requeued with the usual backoff, never bisected, never errored,
/// and settled once the proofs pass.
#[tokio::test(start_paused = true)]
async fn held_check_failure_settles_after_the_fix() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.wrong_vk.lock().unwrap() = true;
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 3, &[], shutdown.clone()).await;
    let vids = submit_n(&h, &s, 3, 600).await;
    wait_long("held three times", async || {
        node.metrics.held_batches.load(Ordering::SeqCst) >= 3
    })
    .await;
    for vid in &vids {
        assert_eq!(vote_status(&h, *vid).await, Some(VoteStatus::Pending));
    }
    assert!(prover.sizes().iter().all(|n| *n == 3), "never bisected");
    assert_eq!(node.metrics.bisections.load(Ordering::SeqCst), 0);
    *prover.wrong_vk.lock().unwrap() = false;
    wait_long("settled", async || all_settled(&h, &vids).await).await;
    shutdown.cancel();
}

/// Held votes still pending at the close carry the held reason.
#[tokio::test(start_paused = true)]
async fn held_votes_close_with_the_held_reason() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.wrong_vk.lock().unwrap() = true;
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 2, &[], shutdown.clone()).await;
    let vids = submit_n(&h, &s, 2, 700).await;
    wait_long("held, nothing in flight", async || {
        node.metrics.held_batches.load(Ordering::SeqCst) >= 1
            && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    s.chain.set_status(ProcessStatus::Canceled);
    for vid in &vids {
        wait_long("errored", async || {
            vote_status(&h, *vid).await == Some(VoteStatus::Error)
        })
        .await;
        let e = vote_error(&h, *vid).await;
        assert!(e.starts_with("process closed (held: "), "{e}");
        assert!(e.contains("program vk"), "{e}");
    }
    shutdown.cancel();
}

/// Bisection state is in memory: after a restart the next seal is full size.
#[tokio::test(start_paused = true)]
async fn restart_mid_bisection_seals_full_size() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches_permanent.lock().unwrap() = u32::MAX;
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 4, &[], shutdown.clone()).await;
    let vids = submit_n(&h, &s, 4, 800).await;
    wait_long("bisecting, nothing in flight", async || {
        prover.calls() == 2 && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    shutdown.cancel();
    drop(h);
    drop(node);

    let db = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let prover2 = FakeProver::open();
    let (_node2, h2) = batching_node(
        db,
        dir.path(),
        &s,
        prover2.clone(),
        4,
        &[],
        shutdown2.clone(),
    )
    .await;
    wait_long("settled", async || all_settled(&h2, &vids).await).await;
    assert_eq!(prover2.sizes(), vec![4]);
    shutdown2.cancel();
}

/// A results proof that fails for good once is retried after a cooldown,
/// not latched: the results land without a restart.
#[tokio::test]
async fn failed_results_proof_is_retried_not_latched() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_results_permanent.lock().unwrap() = 1;
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 40);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    assert_eq!(prover.results_calls(), 2);
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    shutdown.cancel();
}

/// Ruling 22: with node A's prover failing every batch, foreign landings
/// (B's, synced by A) never clear A's hold or its isolated count, so no
/// fourth vote errors; A's own landing after the prover recovers does, and
/// a later failing vote is isolated again.
#[tokio::test(start_paused = true)]
async fn foreign_landings_do_not_clear_a_bisection_hold() {
    let s = setup(2, 8, None);
    let (dir_a, dir_b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::open();
    *prover_a.fail_batches_permanent.lock().unwrap() = u32::MAX;
    let node_a = start_node_cfg(
        Db::open_in(dir_a.path()).unwrap(),
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        test_config_with(dir_a.path(), &s.census_dir, 5, "0s", TIMERS),
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let (ha, hb) = (handle(&node_a).await, handle(&node_b).await);
    let isolated = || node_a.metrics.isolated_votes.load(Ordering::SeqCst);
    let vids_a = submit_n(&ha, &s, 5, 900).await;
    s.chain.advance_time(400);
    wait_long("three isolated on A", async || isolated() == 3).await;
    // Two foreign landings, each followed by enough of A's attempts to
    // isolate another vote if the hold had been cleared.
    for (k, i) in [5usize, 6].into_iter().enumerate() {
        let calls = prover_a.calls();
        let v = fake_vote(&s.env, i, &[1, 2], 950 + i as u64);
        let vid = v.pkg.vote_id;
        hb.submit(v).await.unwrap();
        wait_long("B landed, A synced", async || {
            all_settled(&hb, &[vid]).await
                && node_a.metrics.synced_from_others.load(Ordering::SeqCst) == k as u64 + 1
        })
        .await;
        s.chain.advance_time(400);
        wait_long("A retried after the sync", async || {
            prover_a.calls() >= calls + 3
        })
        .await;
        assert_eq!(isolated(), 3, "a foreign landing cleared the hold");
    }
    for vid in &vids_a[3..] {
        assert_eq!(vote_status(&ha, *vid).await, Some(VoteStatus::Pending));
    }
    // The prover recovers: A's own landing clears the hold.
    *prover_a.fail_batches_permanent.lock().unwrap() = 0;
    wait_long("A's held votes landed", async || {
        all_settled(&ha, &vids_a[3..]).await
    })
    .await;
    let x = fake_vote(&s.env, 0, &[2, 1], 990);
    let y = fake_vote(&s.env, 1, &[2, 1], 991);
    let (vx, vy) = (x.pkg.vote_id, y.pkg.vote_id);
    *prover_a.fail_vid.lock().unwrap() = Some(vx);
    ha.submit(x).await.unwrap();
    ha.submit(y).await.unwrap();
    s.chain.advance_time(400);
    wait_long("x isolated, y settled", async || {
        vote_status(&ha, vx).await == Some(VoteStatus::Error) && all_settled(&ha, &[vy]).await
    })
    .await;
    assert_eq!(isolated(), 4);
    shutdown.cancel();
}

/// A foreign landing between bisection cycles keeps the halved cap: 4
/// votes fail twice (cap now 2), another sequencer lands two unrelated
/// votes, and the next seal is still 2, not the failed batch's 4.
#[tokio::test(start_paused = true)]
async fn foreign_landing_keeps_the_halved_bisection_cap() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_vid.lock().unwrap() = Some(fake_vote(&s.env, 3, &[1, 2], 1003).pkg.vote_id);
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 4, &[], shutdown.clone()).await;
    submit_n(&h, &s, 4, 1000).await;
    wait_long("two failures, nothing in flight", async || {
        prover.calls() == 2 && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    s.chain.settle_externally(&foreign_batch(&s, 6, 7));
    wait_long("synced", async || {
        node.metrics.synced_from_others.load(Ordering::SeqCst) == 1
    })
    .await;
    wait_long("third attempt", async || prover.calls() >= 3).await;
    assert_eq!(prover.sizes()[..3], [4, 4, 2]);
    shutdown.cancel();
}

/// A lost race (`InvalidStateRoot`) on a halved bisection batch keeps the
/// halved cap once the winner is synced.
#[tokio::test(start_paused = true)]
async fn lost_race_keeps_the_halved_bisection_cap() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    *prover.fail_vid.lock().unwrap() = Some(fake_vote(&s.env, 3, &[1, 2], 1103).pkg.vote_id);
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) =
        batching_node(db, dir.path(), &s, prover.clone(), 4, &[], shutdown.clone()).await;
    submit_n(&h, &s, 4, 1100).await;
    for k in 1..=2 {
        wait_long("attempt", async || prover.calls() == k).await;
        prover.release(1);
    }
    // The halved batch [0, 1] proves, but another sequencer lands first and
    // its event is not seen yet: the settlement reverts InvalidStateRoot.
    wait_long("halved attempt", async || prover.calls() == 3).await;
    assert_eq!(prover.sizes(), [4, 4, 2]);
    let block = s.chain.settle_externally(&foreign_batch(&s, 6, 7));
    s.chain.hide_block(Some(block));
    prover.release(1);
    wait_long("lost the race", async || {
        node.metrics.lost_races.load(Ordering::SeqCst) == 1
    })
    .await;
    s.chain.hide_block(None);
    wait_long("synced", async || {
        node.metrics.synced_from_others.load(Ordering::SeqCst) == 1
    })
    .await;
    wait_long("next attempt", async || prover.calls() == 4).await;
    assert_eq!(prover.sizes(), [4, 4, 2, 2]);
    shutdown.cancel();
}

/// While held (a node-side check failure, or a prover failing everything)
/// nothing can land, so fresh votes must not join the cycles: their slots
/// would ratchet into the exposed set. Overwrites arriving cycle after
/// cycle leave the exposed set as it was, and nothing errors.
async fn hold_seals_only_exposed_votes(prover_hold: bool) {
    let s = setup(2, 40, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        40,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let first = submit_n(&h, &s, 40, 100).await;
    wait_long("40 settled", async || all_settled(&h, &first).await).await;
    // Overwrites that end up held.
    let n_held = if prover_hold { 5 } else { 2 };
    if prover_hold {
        *prover.fail_batches_permanent.lock().unwrap() = u32::MAX;
    } else {
        *prover.wrong_vk.lock().unwrap() = true;
    }
    let mut vids = Vec::new();
    for i in 0..n_held {
        let v = fake_vote(&s.env, i, &[2, 1], 200 + i as u64);
        vids.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    s.chain.advance_time(400);
    wait_long("holding", async || {
        let held = if prover_hold {
            node.metrics.isolated_votes.load(Ordering::SeqCst) == 3
        } else {
            node.metrics.held_batches.load(Ordering::SeqCst) >= 1
        };
        held && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    let exposed = || node.db.exposed(&pid_fr()).unwrap().unwrap().slots.len();
    let slots: std::collections::BTreeSet<u64> = node
        .db
        .exposed(&pid_fr())
        .unwrap()
        .unwrap()
        .slots
        .into_iter()
        .collect();
    let e0 = slots.len();
    let fresh: Vec<usize> = (n_held..40)
        .filter(|i| !slots.contains(&fake_vote(&s.env, *i, &[1, 2], 1).slot))
        .take(3)
        .collect();
    assert_eq!(fresh.len(), 3);
    for i in fresh {
        let calls = prover.calls();
        let v = fake_vote(&s.env, i, &[2, 1], 300 + i as u64);
        vids.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
        s.chain.advance_time(400);
        wait_long("another held cycle", async || {
            prover.calls() > calls && !h.snapshot().await.unwrap().in_flight
        })
        .await;
        assert_eq!(exposed(), e0, "a fresh vote widened the exposed set");
    }
    let mut held = Vec::new();
    for vid in &vids {
        let st = vote_status(&h, *vid).await;
        if st == Some(VoteStatus::Error) {
            let e = vote_error(&h, *vid).await;
            assert!(prover_hold && e.contains("isolated by bisection"), "{e}");
        } else {
            held.push(*vid);
        }
    }
    *prover.fail_batches_permanent.lock().unwrap() = 0;
    *prover.wrong_vk.lock().unwrap() = false;
    for _ in 0..3 {
        s.chain.advance_time(400);
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
    wait_long("held votes settled", async || all_settled(&h, &held).await).await;
    shutdown.cancel();
}

#[tokio::test(start_paused = true)]
async fn held_process_seals_only_its_exposed_votes() {
    hold_seals_only_exposed_votes(false).await;
}

#[tokio::test(start_paused = true)]
async fn prover_hold_seals_only_its_exposed_votes() {
    hold_seals_only_exposed_votes(true).await;
}

/// Votes closed under a prover hold name it: the close reason carries the
/// prover's error, not the plain `process closed` of an expected close.
#[tokio::test(start_paused = true)]
async fn cancel_under_a_prover_hold_names_the_hold() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches_permanent.lock().unwrap() = u32::MAX;
    let db = Db::open_in(dir.path()).unwrap();
    let (node, h) = batching_node(
        db,
        dir.path(),
        &s,
        prover.clone(),
        5,
        TIMERS,
        shutdown.clone(),
    )
    .await;
    let vids = submit_n(&h, &s, 5, 1200).await;
    s.chain.advance_time(400);
    wait_long("holding, nothing in flight", async || {
        node.metrics.isolated_votes.load(Ordering::SeqCst) == 3
            && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    s.chain.set_status(ProcessStatus::Canceled);
    for vid in &vids[3..] {
        wait_long("errored", async || {
            vote_status(&h, *vid).await == Some(VoteStatus::Error)
        })
        .await;
        let e = vote_error(&h, *vid).await;
        assert!(e.starts_with("process closed (held: prover: "), "{e}");
        assert!(e.contains("bad input"), "{e}");
    }
    shutdown.cancel();
}

// The node's stored (votersCount, overwrittenVotesCount), as the API serves them.
fn stored_counts(db: &Db) -> (u64, u64) {
    let r = db.process(&pid_fr()).unwrap().unwrap();
    (r.onchain.voters_count, r.onchain.overwritten_count)
}

/// The registry counters follow two own landings and one synced transition,
/// overwrites included, without re-reading getProcess; a node bootstrapped
/// mid-election and a restarted one report them too.
#[tokio::test]
async fn onchain_counts_follow_own_and_synced_transitions() {
    let s = setup(2, 8, None);
    let (dir_a, dir_b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let (stop_a, stop_b) = (CancellationToken::new(), CancellationToken::new());
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        FakeProver::open(),
        2,
        "0s",
        stop_a.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    assert_eq!(stored_counts(&node_a.db), (0, 0));
    // From here the counters come from the landings alone.
    s.chain.fail_process(u32::MAX);

    // Own landing 1: two new voters, its event hidden, so the receipt alone counts.
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    for (i, k) in [(0, 10), (1, 11)] {
        ha.submit(fake_vote(&s.env, i, &[1, 2], k)).await.unwrap();
    }
    wait_until("own landing 1", async || {
        stored_counts(&node_a.db) == (2, 0)
    })
    .await;
    s.chain.hide_block(None);
    // Own landing 2: voter 0 overwrites, voter 2 is new.
    for (i, k) in [(0, 12), (2, 13)] {
        ha.submit(fake_vote(&s.env, i, &[2, 1], k)).await.unwrap();
    }
    wait_until("own landing 2", async || {
        stored_counts(&node_a.db) == (3, 1)
    })
    .await;
    assert_eq!(s.chain.voters(), 3);

    // B bootstraps mid-election and replays both landings from blobs.
    s.chain.fail_process(0);
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        FakeProver::open(),
        2,
        "0s",
        stop_b.clone(),
    )
    .await;
    let hb = handle(&node_b).await;
    wait_until("B synced", async || {
        hb.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(stored_counts(&node_b.db), (3, 1));
    s.chain.fail_process(u32::MAX);

    // B lands voter 1's overwrite and new voter 3; A syncs it.
    for (i, k) in [(1, 14), (3, 15)] {
        hb.submit(fake_vote(&s.env, i, &[1, 1], k)).await.unwrap();
    }
    wait_until("synced landing", async || {
        stored_counts(&node_a.db) == (4, 2) && stored_counts(&node_b.db) == (4, 2)
    })
    .await;
    assert_eq!(node_a.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(node_a.metrics.settled_by_self.load(Ordering::SeqCst), 2);
    assert_eq!(node_b.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    s.chain.fail_process(0);
    let chain = s.chain.process(&pid31()).await.unwrap();
    assert_eq!((chain.voters_count, chain.overwritten_count), (4, 2));

    // A restart keeps them.
    stop_a.cancel();
    drop(ha);
    drop(node_a);
    let db = reopen_db(dir_a.path()).await;
    assert_eq!(stored_counts(&db), (4, 2));
    stop_b.cancel();
}

/// A gap replay of older transitions never rewinds the counters the newer
/// event already pinned.
#[tokio::test]
async fn gap_replay_keeps_the_newer_counts() {
    let s = setup(2, 8, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover_a = FakeProver::gated();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        prover_a.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        FakeProver::open(),
        10,
        "0s",
        shutdown.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    // T1 lands in a block hidden from B's monitor.
    ha.submit(fake_vote(&s.env, 0, &[1, 2], 71)).await.unwrap();
    wait_until("A proving", async || prover_a.calls() >= 1).await;
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    prover_a.release(100);
    wait_until("T1 landed", async || s.chain.voters() == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await; // B polls past T1
    s.chain.hide_block(None);
    // T2 reaches B as a gap; the replay from its event and the first resync
    // fail at the blobs, the third attempt waits out a backoff. No getProcess
    // read heals the counts meanwhile.
    s.chain.fail_process(u32::MAX);
    s.chain.fail_blobs(3);
    ha.submit(fake_vote(&s.env, 1, &[2, 1], 72)).await.unwrap();
    prover_a.release(100);
    wait_until("T2 landed", async || s.chain.voters() == 2).await;
    wait_until("B's replays failed", async || {
        s.chain.fail_blobs_left() <= 1
    })
    .await;
    assert_ne!(hb.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(stored_counts(&node_b.db), (2, 0));
    s.chain.fail_process(0);
    wait_until("B replayed the gap", async || {
        hb.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(stored_counts(&node_b.db), (2, 0));
    shutdown.cancel();
}

/// A node restarted on a record with stale counters heals them from the
/// chain on its first tick, with no landing.
#[tokio::test]
async fn stale_stored_counts_heal_on_the_first_tick() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let stop = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        stop.clone(),
    )
    .await;
    let h = handle(&node).await;
    for (i, k) in [(0, 80), (1, 81), (0, 82)] {
        h.submit(fake_vote(&s.env, i, &[1, 2], k)).await.unwrap();
        wait_until("landed", async || h.snapshot().await.unwrap().pending == 0).await;
    }
    wait_until("counted", async || stored_counts(&node.db) == (2, 1)).await;
    stop.cancel();
    drop(h);
    drop(node);

    // A datadir from before the counters followed the landings.
    let db = reopen_db(dir.path()).await;
    let mut rec = db.process(&pid_fr()).unwrap().unwrap();
    (rec.onchain.voters_count, rec.onchain.overwritten_count) = (0, 0);
    db.put_process(&rec).unwrap();
    let submits = s.chain.submit_count();
    let stop = CancellationToken::new();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        stop.clone(),
    )
    .await;
    handle(&node).await;
    wait_until("healed", async || stored_counts(&node.db) == (2, 1)).await;
    assert_eq!(s.chain.submit_count(), submits);
    stop.cancel();
}

// ------------------------------------------------ events lost at a crash

/// Events the monitor had routed into the actor's mailbox die with the
/// process: here an extension of the voting window. The restart reads the
/// process from the chain (at boot, or on the first tick when that read
/// fails), so the pending vote settles inside the new window instead of
/// being closed out at the old grace end.
#[tokio::test]
async fn restart_heals_a_stale_record_from_the_chain() {
    for boot_read_fails in [false, true] {
        let s = setup(2, 8, None);
        let dir = TempDir::new().unwrap();
        let shutdown = CancellationToken::new();
        let (node, h) = batching_node(
            Db::open_in(dir.path()).unwrap(),
            dir.path(),
            &s,
            FakeProver::open(),
            2,
            &[],
            shutdown.clone(),
        )
        .await;
        let v = fake_vote(&s.env, 0, &[1, 2], 1_100);
        let vid = v.pkg.vote_id;
        h.submit(v).await.unwrap();
        shutdown.cancel();
        drop((h, node));

        // While it is down the organizer extends the window, and the clock
        // passes the old grace end; the event never reaches the actor.
        s.chain.extend_duration(5_000);
        s.chain.advance_time(DURATION + GRACE.default_grace + 10);
        assert!(s.chain.time() < s.chain.end_time());
        let db = reopen_db(dir.path()).await;
        db.set_meta_u64("monitor_last_block", s.chain.head_block())
            .unwrap();
        if boot_read_fails {
            s.chain.fail_process(1);
        }
        let shutdown = CancellationToken::new();
        let prover = FakeProver::open();
        let (node, h) =
            batching_node(db, dir.path(), &s, prover.clone(), 2, &[], shutdown.clone()).await;
        wait_until("vote settled in the extended window", async || {
            let st = vote_status(&h, vid).await;
            assert_ne!(
                st,
                Some(VoteStatus::Error),
                "boot_read_fails={boot_read_fails}: closed at the old grace end"
            );
            st == Some(VoteStatus::Settled)
        })
        .await;
        assert!(h.snapshot().await.unwrap().accepting);
        let rec = node.db.process(&pid_fr()).unwrap().unwrap();
        assert_eq!(rec.onchain.end_time(), s.chain.end_time());
        shutdown.cancel();
    }
}

/// A foreign transition whose event was lost (its block is never routed):
/// the boot read shows an unknown root, so the node waits for the sync
/// instead of sealing on the stale root and wasting a cycle on the revert.
#[tokio::test]
async fn restart_behind_an_unseen_transition_resyncs_before_sealing() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        FakeProver::open(),
        1,
        &[],
        shutdown.clone(),
    )
    .await;
    shutdown.cancel();
    drop((h, node));

    let block = s.chain.settle_externally(&foreign_batch(&s, 0, 1));
    s.chain.hide_block(Some(block));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (node, h) = batching_node(
        reopen_db(dir.path()).await,
        dir.path(),
        &s,
        prover.clone(),
        1,
        &[],
        shutdown.clone(),
    )
    .await;
    let v = fake_vote(&s.env, 2, &[1, 0], 1_200);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    beats().await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!(prover.calls(), 0, "sealed on a root the chain has left");
    assert!(!snap.synced);
    assert_ne!(snap.root, s.chain.root());

    s.chain.hide_block(None);
    wait_until("vote settled after the sync", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(node.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 0);
    assert_eq!(prover.calls(), 1);
    shutdown.cancel();
}

/// Origin 2: a `CensusUpdated` lost at a crash. The restart's chain read
/// (at boot, or on the first tick when that read fails) moves the census
/// root and queues its download, so the votes seal at the root the
/// contract now expects.
#[tokio::test]
async fn lost_census_update_loads_after_a_restart() {
    for boot_read_fails in [false, true] {
        let s = offchain_setup(4);
        let dir = TempDir::new().unwrap();
        let shutdown = CancellationToken::new();
        let (node, h) = batching_node(
            Db::open_in(dir.path()).unwrap(),
            dir.path(),
            &s,
            FakeProver::open(),
            2,
            &[],
            shutdown.clone(),
        )
        .await;
        let v0 = fake_vote(&s.env, 0, &[1, 2], 1_300);
        h.submit(v0.clone()).await.unwrap();
        shutdown.cancel();
        drop((h, node));

        let root1 = update_census(&s, "v2", &[0, 1, 2, 3, 4]);
        s.chain.set_census_root_check(Some(fr_to_le(&root1)));
        let db = reopen_db(dir.path()).await;
        db.set_meta_u64("monitor_last_block", s.chain.head_block())
            .unwrap();
        if boot_read_fails {
            s.chain.fail_process(1);
        }
        let shutdown = CancellationToken::new();
        let (_node, h) = batching_node(
            db,
            dir.path(),
            &s,
            FakeProver::open(),
            2,
            &[],
            shutdown.clone(),
        )
        .await;
        let v1 = fake_vote(&s.env, 1, &[2, 1], 1_301);
        h.submit(v1.clone()).await.unwrap();
        wait_until("settled at the new census root", async || {
            all_settled(&h, &[v0.pkg.vote_id, v1.pkg.vote_id]).await
        })
        .await;
        assert_eq!(
            s.chain.last_census_root(),
            Some(fr_to_le(&root1)),
            "boot_read_fails={boot_read_fails}"
        );
        shutdown.cancel();
    }
}

/// A chain read from an endpoint behind the events (its head below the
/// newest block seen, or below the monitor's cursor at boot) is not merged:
/// it would rewind the extension the events delivered.
#[tokio::test]
async fn lagging_read_does_not_rewind_the_record() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        FakeProver::open(),
        2,
        &[],
        shutdown.clone(),
    )
    .await;
    let end = s.chain.end_time();
    s.chain.freeze_view(true);
    s.chain.extend_duration(5_000);
    // A landing makes the actor re-read the chain.
    s.chain.settle_externally(&foreign_batch(&s, 0, 1));
    wait_until("synced", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    beats().await;
    let stored = |db: &Db| db.process(&pid_fr()).unwrap().unwrap().onchain.end_time();
    assert_eq!(stored(&node.db), end + 5_000, "rewound by a lagging read");
    shutdown.cancel();
    drop((h, node));

    // At boot the cursor bounds the endpoint.
    let shutdown = CancellationToken::new();
    let (node, h) = batching_node(
        reopen_db(dir.path()).await,
        dir.path(),
        &s,
        FakeProver::open(),
        2,
        &[],
        shutdown.clone(),
    )
    .await;
    beats().await;
    assert_eq!(stored(&node.db), end + 5_000, "rewound at boot");
    s.chain.freeze_view(false);
    beats().await;
    assert!(h.snapshot().await.unwrap().synced);
    assert_eq!(stored(&node.db), end + 5_000);
    shutdown.cancel();
}

// ------------------------------------------- close-out from chain truth

/// Another sequencer's landing extends the grace while its event is not
/// routed yet: a heartbeat past the old grace end must not error a vote the
/// extended window still settles. Once the event shows, the in-flight vote
/// is requeued by the lost race and both settle in the extension.
#[tokio::test]
async fn close_out_waits_for_a_landing_the_events_have_not_shown() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (_node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        prover.clone(),
        1,
        &[],
        shutdown.clone(),
    )
    .await;
    let end = s.chain.end_time();
    s.chain.advance_time(DURATION - 10);
    beats().await;
    let a = fake_vote(&s.env, 0, &[1, 2], 1_400);
    let b = fake_vote(&s.env, 1, &[1, 2], 1_401);
    let vids = [a.pkg.vote_id, b.pkg.vote_id];
    h.submit(a).await.unwrap();
    h.submit(b).await.unwrap();
    // A seals (flush) and sits in the prover; B waits.
    wait_until("sealed", async || prover.calls() == 1).await;
    s.chain.advance_time(110);
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    s.chain.settle_externally(&foreign_batch_of(
        &s,
        &[
            fake_vote(&s.env, 2, &[1, 2], 70),
            fake_vote(&s.env, 3, &[2, 3], 71),
        ],
    ));
    assert_eq!(s.chain.grace_end(), end + 100 + GRACE.default_grace);
    // Past the old grace end, inside the new one.
    s.chain.advance_time(60);
    beats().await;
    assert!(s.chain.time() < s.chain.grace_end());
    assert_eq!(
        vote_status(&h, vids[1]).await,
        Some(VoteStatus::Pending),
        "closed at the stale grace end"
    );
    s.chain.hide_block(None);
    prover.release(10);
    wait_until("both settled in the extended window", async || {
        for vid in vids {
            assert_ne!(vote_status(&h, vid).await, Some(VoteStatus::Error));
        }
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    shutdown.cancel();
}

/// A pending vote another sequencer settled, whose sync lags (blob fetches
/// fail) when the grace ends: it settles from the sync, never errors.
#[tokio::test]
async fn close_out_settles_synced_votes_before_erroring() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        FakeProver::open(),
        10,
        &[],
        shutdown.clone(),
    )
    .await;
    // Below every seal trigger: it waits.
    let x = fake_vote(&s.env, 0, &[1, 2], 1_450);
    let vid = x.pkg.vote_id;
    h.submit(x.clone()).await.unwrap();
    s.chain.fail_blobs(3);
    s.chain.settle_externally(&foreign_batch_of(&s, &[x]));
    wait_until("first sync attempt failed", async || {
        s.chain.fail_blobs_left() < 3
    })
    .await;
    s.chain.pass_grace();
    wait_until("settled from the sync", async || {
        let st = vote_status(&h, vid).await;
        assert_ne!(st, Some(VoteStatus::Error), "closed out before the sync");
        st == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    shutdown.cancel();
}

/// The close-out takes the chain's word, not the record's: an extension
/// whose event is not routed yet keeps the window open past the old grace
/// end, and at the real one a read from an endpoint behind the chain (an
/// unknown root) closes nothing until a good read confirms it.
#[tokio::test]
async fn close_out_refuses_a_lagging_read() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    // An observer never seals: the vote waits for the close.
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.observer(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 1_500);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    s.chain.extend_duration(500);
    s.chain.advance_time(DURATION + GRACE.default_grace + 10);
    beats().await;
    assert!(s.chain.time() < s.chain.end_time());
    assert_eq!(
        vote_status(&h, vid).await,
        Some(VoteStatus::Pending),
        "closed at the old grace end"
    );

    s.chain.hide_block(None);
    s.chain.lag_process(u32::MAX);
    s.chain.pass_grace();
    beats().await;
    assert_eq!(
        vote_status(&h, vid).await,
        Some(VoteStatus::Pending),
        "closed on a lagging read"
    );
    s.chain.lag_process(0);
    wait_until("closed out", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Error)
    })
    .await;
    let sv = h.status(vid).await.unwrap().unwrap();
    assert_eq!(sv.error.as_deref(), Some("process closed"));
    shutdown.cancel();
}

/// Results land while the node is behind: a foreign transition that
/// settled pending X is not applied yet. The node resyncs first (X settles,
/// the tree reaches the chain's final root) and only then closes out the
/// rest; until then it does not report itself synced.
#[tokio::test]
async fn results_while_behind_resync_first_then_close_out() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (_node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        prover.clone(),
        1,
        &[],
        shutdown.clone(),
    )
    .await;
    s.chain.advance_time(DURATION - 10);
    beats().await;
    let a = fake_vote(&s.env, 0, &[1, 2], 1_600);
    let x = fake_vote(&s.env, 1, &[1, 2], 1_601);
    let (vid_a, vid_x) = (a.pkg.vote_id, x.pkg.vote_id);
    h.submit(a.clone()).await.unwrap();
    h.submit(x.clone()).await.unwrap();
    wait_until("sealed", async || prover.calls() == 1).await;
    // Another sequencer settles X; its event is not routed yet.
    s.chain.advance_time(20);
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    let fb = foreign_batch_of(&s, &[x.clone(), fake_vote(&s.env, 3, &[2, 3], 71)]);
    s.chain.settle_externally(&fb);
    s.chain.pass_grace();
    s.chain.set_results_externally(vec![3, 4]);
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    beats().await;
    assert_ne!(
        vote_status(&h, vid_x).await,
        Some(VoteStatus::Error),
        "closed out while behind"
    );
    assert!(
        !h.snapshot().await.unwrap().synced,
        "synced against the pin alone"
    );

    // A's proof is still out: past RESULTS it can only revert, so the
    // resync does not wait for it.
    s.chain.hide_block(None);
    wait_until("X settled", async || {
        let st = vote_status(&h, vid_x).await;
        assert_ne!(st, Some(VoteStatus::Error), "closed out while behind");
        st == Some(VoteStatus::Settled)
    })
    .await;
    // A can never settle now: closed once the tree is at the final root.
    wait_until("A closed", async || {
        vote_status(&h, vid_a).await == Some(VoteStatus::Error)
    })
    .await;
    prover.release(10);
    beats().await;
    assert_eq!(vote_status(&h, vid_a).await, Some(VoteStatus::Error));
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.root, s.chain.root());
    assert!(snap.synced);
    let (p, root) = h.vote_id_proof(vid_x).await.unwrap();
    let tp = davinci_client::api::TrackerProof {
        process_id: davinci_client::api::ProcessId(pid31()),
        vote_id: vid_x,
        root,
        siblings: p.siblings,
    };
    assert!(davinci_client::api::verify_tracker(&tp, &s.chain.root()));
    assert!(h.slot_ballot(x.slot).await.unwrap().is_some());
    assert!(h.slot_ballot(a.slot).await.unwrap().is_none());
    shutdown.cancel();
}

/// A Finalized process restarted behind the chain (the transition before
/// the results never reached it) catches up to the final root, without
/// sealing anything.
#[tokio::test]
async fn finalized_actor_keeps_following_the_chain() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        FakeProver::open(),
        1,
        &[],
        shutdown.clone(),
    )
    .await;
    beats().await;
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    s.chain.settle_externally(&foreign_batch(&s, 0, 1));
    s.chain.fail_process(u32::MAX);
    s.chain.pass_grace();
    s.chain.set_results_externally(vec![3, 5]);
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    beats().await;
    // No read has shown the final root yet.
    assert!(
        !h.snapshot().await.unwrap().synced,
        "synced against the pin alone"
    );
    // The first read makes the final root the target, persisted.
    s.chain.fail_process(0);
    wait_until("final root pinned", async || {
        node.db
            .process(&pid_fr())
            .unwrap()
            .unwrap()
            .onchain
            .state_root
            == s.chain.root()
    })
    .await;
    assert!(!h.snapshot().await.unwrap().synced);
    shutdown.cancel();
    drop((h, node));

    s.chain.hide_block(None);
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (node, h) = batching_node(
        reopen_db(dir.path()).await,
        dir.path(),
        &s,
        prover.clone(),
        1,
        &[],
        shutdown.clone(),
    )
    .await;
    wait_until("caught up with the final root", async || {
        let snap = h.snapshot().await.unwrap();
        snap.root == s.chain.root() && snap.synced
    })
    .await;
    assert_eq!(h.snapshot().await.unwrap().local, LocalStatus::Finalized);
    assert_eq!(node.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    assert_eq!(prover.calls(), 0);
    shutdown.cancel();
}

/// A results receipt is not a landing: an OP reorg can drop it. The node
/// stays Active until the results event; when none comes within the
/// landing wait it resubmits the held proof (no re-proof), and finalizes
/// at the event.
#[tokio::test(start_paused = true)]
async fn results_receipt_without_event_is_resubmitted() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 40);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    // Eager results: proved in the grace and held.
    s.chain.advance_time(DURATION + 1);
    wait_until("eager proof", async || prover.results_calls() == 1).await;
    s.chain.vanish_next_results();
    s.chain.pass_grace();
    wait_until("first submit", async || s.chain.results_submits() == 1).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(s.chain.results_submits(), 1, "resubmitted before the wait");
    assert_eq!(
        h.snapshot().await.unwrap().local,
        LocalStatus::Active,
        "finalized on a receipt"
    );
    wait_long("results on chain", async || !s.chain.results().is_empty()).await;
    wait_long("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results_submits(), 2);
    assert_eq!(prover.results_calls(), 1, "the held proof was not reused");
    shutdown.cancel();
}

/// A landing whose record write failed in-run (the tree committed it), then
/// a new vote: the log catches up before the next seal, so both transitions
/// are recorded and the first vote settles in the db.
#[tokio::test]
async fn failed_settle_then_new_landing_keeps_the_first_record() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v0 = fake_vote(&s.env, 0, &[1, 2], 1_000);
    let vid0 = v0.pkg.vote_id;
    h.submit(v0).await.unwrap();
    wait_until("batch sealed", async || prover.calls() >= 1).await;
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_until("landed and committed in the tree", async || {
        s.chain.voters() == 1 && h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    // The echo arrives and the repair attempts fail; the backoff grows.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    db.fail_next_writes(0);
    let v1 = fake_vote(&s.env, 1, &[2, 1], 1_001);
    let vid1 = v1.pkg.vote_id;
    h.submit(v1).await.unwrap();
    wait_until("second batch sealed", async || prover.calls() >= 2).await;
    prover.release(1);
    wait_until("both settled", async || {
        all_settled(&h, &[vid0, vid1]).await
    })
    .await;
    let ts = node.db.transitions(&pid_fr()).unwrap();
    assert_eq!(ts.len(), 2, "the first transition was never recorded");
    assert_eq!(ts.iter().map(|t| t.index).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(ts[0].old_root, genesis_root(&s.env.cfg).unwrap());
    assert_eq!(ts[0].new_root, ts[1].old_root);
    assert_eq!(ts[1].new_root, s.chain.root());
    assert!(h.snapshot().await.unwrap().synced);
    shutdown.cancel();
}

/// The refresh read is pinned to the head it read first: a backend whose
/// `getProcess` lags the head (a load-balanced URL) cannot rewind what the
/// events moved; the read fails there and is retried.
#[tokio::test]
async fn process_read_behind_the_head_does_not_rewind_the_record() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        FakeProver::open(),
        2,
        &[],
        shutdown.clone(),
    )
    .await;
    beats().await;
    let end = s.chain.end_time();
    s.chain.split_view(true);
    s.chain.extend_duration(5_000);
    // A landing makes the actor re-read the chain.
    s.chain.settle_externally(&foreign_batch(&s, 0, 1));
    wait_until("synced", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    beats().await;
    let stored = |db: &Db| db.process(&pid_fr()).unwrap().unwrap().onchain.end_time();
    assert_eq!(stored(&node.db), end + 5_000, "rewound by a lagging read");
    s.chain.split_view(false);
    beats().await;
    assert!(h.snapshot().await.unwrap().synced);
    assert_eq!(stored(&node.db), end + 5_000);
    shutdown.cancel();
}

/// A landing whose record write was lost at a crash, restarted while every
/// `getProcess` fails: the log repair moves the pin with the record (as a
/// sync does), so sealing resumes without waiting for a chain read.
#[tokio::test]
async fn log_repair_moves_the_pin_without_a_chain_read() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v0 = fake_vote(&s.env, 0, &[1, 2], 1_700);
    let vid0 = v0.pkg.vote_id;
    h.submit(v0).await.unwrap();
    wait_until("batch sealed", async || prover.calls() >= 1).await;
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_until("committed in the tree", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    shutdown.cancel();
    drop((h, node, db));

    // The echo was routed before the crash: only the gap replay finds it.
    let db = reopen_db(dir.path()).await;
    db.set_meta_u64("monitor_last_block", s.chain.head_block())
        .unwrap();
    s.chain.fail_process(u32::MAX);
    let shutdown = CancellationToken::new();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v1 = fake_vote(&s.env, 1, &[2, 1], 1_701);
    let vid1 = v1.pkg.vote_id;
    h.submit(v1).await.unwrap();
    wait_until("both settled", async || {
        all_settled(&h, &[vid0, vid1]).await
    })
    .await;
    assert_eq!(node.db.transitions(&pid_fr()).unwrap().len(), 2);
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    shutdown.cancel();
}

/// Two extensions past the cursor at a restart, the second not routed yet:
/// the first chain read merges the newest end, and the older event routed
/// after it must not rewind the record (admission closing early, a
/// close-out at the old grace end). The vote waits and flushes.
#[tokio::test]
async fn restart_backlog_of_extensions_does_not_rewind_the_end() {
    const WAITS: &[&str] = &["--solo-wait", "48h"];
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = batching_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        FakeProver::open(),
        10,
        WAITS,
        shutdown.clone(),
    )
    .await;
    let v = fake_vote(&s.env, 0, &[1, 2], 1_800);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    beats().await;
    shutdown.cancel();
    drop((h, node));

    let e0 = s.chain.end_time();
    s.chain.extend_duration(500);
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    s.chain.extend_duration(500);
    let last = s.chain.head_block();
    // Past the first extension's end, inside its grace.
    s.chain.advance_time(DURATION + 550);
    // The first chain read comes before the backlog is paged.
    s.chain.events_delay(Duration::from_millis(300));
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (node, h) = batching_node(
        reopen_db(dir.path()).await,
        dir.path(),
        &s,
        prover.clone(),
        10,
        WAITS,
        shutdown.clone(),
    )
    .await;
    wait_until("backlog routed", async || {
        node.db.meta_u64("monitor_last_block").unwrap().unwrap_or(0) >= last
    })
    .await;
    beats().await;
    let end = |db: &Db| db.process(&pid_fr()).unwrap().unwrap().onchain.end_time();
    assert_eq!(end(&node.db), e0 + 1_000, "rewound by an older event");
    assert!(h.snapshot().await.unwrap().accepting);
    assert_eq!(prover.calls(), 0, "flushed at an older end");
    // Past the first extension's grace end: still open on-chain.
    s.chain.advance_time(150);
    beats().await;
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Pending));
    // Within the flush horizon of the real end.
    prover.release(10);
    s.chain.advance_time(200);
    wait_until("flushed", async || {
        let st = vote_status(&h, vid).await;
        assert_ne!(st, Some(VoteStatus::Error), "closed at an older end");
        st == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(end(&node.db), e0 + 1_000);
    shutdown.cancel();
}
