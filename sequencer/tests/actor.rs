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

#[tokio::test]
async fn process_closed_during_flight() {
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
    let genesis = genesis_root(&s.env.cfg).unwrap();

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
    prover.release(100);
    wait_until("votes errored", async || {
        for vid in &vids {
            match h.status(*vid).await.unwrap() {
                Some(v) if v.status == VoteStatus::Error => {}
                _ => return false,
            }
        }
        true
    })
    .await;
    let sv = h.status(vids[0]).await.unwrap().unwrap();
    assert!(sv.error.unwrap().contains("closed"), "wrong error");
    // The local state rolled back to the committed (genesis) root.
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.root, genesis);
    assert!(!snap.in_flight);
    assert_eq!(snap.pending, 0);
    assert_eq!(s.chain.voters(), 0);
    // New votes are refused.
    let late = fake_vote(&s.env, 3, &[1, 1], 99);
    assert!(matches!(h.submit(late).await, Err(ActorError::Closed(_))));
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
    shutdown.cancel();
}

#[tokio::test]
async fn tampered_program_vk_rejected() {
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
    wait_until("vote errored", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Error)
    })
    .await;
    let sv = h.status(vid).await.unwrap().unwrap();
    assert!(sv.error.unwrap().contains("vk"), "vk pin named");
    assert_eq!(s.chain.voters(), 0);
    shutdown.cancel();
}

#[tokio::test]
async fn deadline_margin_blocks_sealing() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    // Margin larger than the whole election: nothing ever seals.
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
    // Past the end the pending vote becomes an error.
    s.chain.advance_time(DURATION + 1);
    wait_until("vote errored at end", async || {
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

/// FW-1: the contract accepts results for a process paused past its
/// end, so the key holder finalizes it like a Ready one.
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
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    shutdown.cancel();
}

/// FW-2: a paused process still accepts votes; they queue and settle
/// once the process resumes.
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

/// Minor: a transition event the monitor never delivered (a gap) is
/// replayed from the registry when a later event reveals it, resuming from
/// the last applied transition. Tail: a vote settled by another
/// sequencer is a duplicate here even without a local record.
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

/// Minor: a transient submit failure requeues the votes and the next seal
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

/// Minor: a receipt timeout on a tx that actually landed commits from the
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
    // The first attempt fails transiently; after the ~2s cooldown a fresh
    // attempt succeeds.
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    shutdown.cancel();
}

/// A slot with a pending vote refuses a second one until it settles.
#[tokio::test]
async fn second_pending_vote_for_slot_rejected() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        10, // nothing seals: the queue stays pending
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v1 = fake_vote(&s.env, 0, &[1, 2], 70);
    let v2 = fake_vote(&s.env, 0, &[2, 1], 71); // same voter, new vote id
    h.submit(v1).await.unwrap();
    assert!(matches!(h.submit(v2).await, Err(ActorError::SlotBusy(_))));
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

/// A prover 4xx (refused input) errors the votes once — no retry.
#[tokio::test]
async fn permanent_prover_refusal_errors_votes_once() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches_permanent.lock().unwrap() = 1;
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
    let sv = h.status(vid).await.unwrap().unwrap();
    assert!(sv.error.unwrap().contains("400"), "reason recorded");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prover.calls(), 1, "a refused input must not be resubmitted");
    assert_eq!(s.chain.submit_count(), 0);
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
    s.chain.advance_time(0);
    s.chain.push_event(EventKind::CensusUpdated {
        pid: pid31(),
        root: fr_to_be(&root),
        uri,
    });
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
    // The first broadcast reverts; after the cooldown a fresh attempt lands.
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    shutdown.cancel();
}

/// A duration extension that lands while the results proof
/// is in flight must keep the plaintext tally out of the mempool entirely.
/// The pre-broadcast window re-check in `finalize.rs` catches it before
/// `submit_results`; the node finalizes only once the new end passes.
#[tokio::test]
async fn extension_during_results_proof_blocks_broadcast() {
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
    // Close the window by time; the results proof hangs on the gate.
    prover.gate_results();
    s.chain.advance_time(DURATION + 1);
    wait_until("results proving", async || prover.results_calls() == 1).await;
    // The organizer extends the election while the proof is in flight,
    // then the proof completes: the re-check must stop the broadcast.
    s.chain.extend_duration(500_000);
    prover.release_results(10);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(s.chain.results_submits(), 0, "tally broadcast while open");
    assert_ne!(h.snapshot().await.unwrap().local, LocalStatus::Finalized);
    // Once the extended window closes too, finalize goes through.
    s.chain.advance_time(500_000);
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    assert_eq!(s.chain.results_submits(), 1);
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
    wait_until("finalize lost", async || s.chain.dkg_calls().1.1 == 1).await;
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
