//! Crash-recovery tests: a node killed mid-flight restarts from its
//! database and converges with the chain.

mod common;

use common::*;

/// Killed after sealing a batch (the prover holds it): the restart recovers
/// the vote to pending, keeps the committed root, and settles it later.
#[tokio::test]
async fn killed_after_seal_restarts_pending_with_committed_root() {
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
    let v = fake_vote(&s.env, 0, &[1, 2], 700);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("batch sealed", async || prover.calls() >= 1).await;
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Aggregated));
    // Kill mid-prove: nothing landed on chain.
    shutdown.cancel();
    drop(h);
    drop(node);

    // Restart with a batch cap the lone vote does not reach: it must sit
    // in pending, on the committed (genesis) root.
    let db = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let node2 = start_node(
        db,
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
    wait_until("vote recovered to pending", async || {
        vote_status(&h2, vid).await == Some(VoteStatus::Pending)
    })
    .await;
    assert_eq!(h2.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(s.chain.voters(), 0);

    // A second vote fills the batch; both settle.
    let v2 = fake_vote(&s.env, 1, &[2, 3], 701);
    let vid2 = v2.pkg.vote_id;
    h2.submit(v2).await.unwrap();
    wait_until("both settled", async || {
        all_settled(&h2, &[vid, vid2]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 2);
    assert_eq!(h2.snapshot().await.unwrap().root, s.chain.root());
    shutdown2.cancel();
}

/// The chain moved while the node was down (another sequencer settled a
/// transition): the restart syncs it from the blobs and then settles its
/// own pending vote on the new root.
#[tokio::test]
async fn restart_syncs_transition_settled_by_another() {
    let s = setup(2, 8, None);
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    // B seals a batch on genesis and dies before proving it.
    let shutdown_b = CancellationToken::new();
    let prover_b = FakeProver::gated();
    let node_b = start_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        prover_b.clone(),
        1,
        "0s",
        shutdown_b.clone(),
    )
    .await;
    let hb = handle(&node_b).await;
    let vb = fake_vote(&s.env, 0, &[1, 2], 800);
    let vid_b = vb.pkg.vote_id;
    hb.submit(vb).await.unwrap();
    wait_until("B sealed", async || prover_b.calls() >= 1).await;
    shutdown_b.cancel();
    drop(hb);
    drop(node_b);

    // While B is down, A settles a transition: the chain root moves.
    let shutdown_a = CancellationToken::new();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        FakeProver::open(),
        2,
        "0s",
        shutdown_a.clone(),
    )
    .await;
    let ha = handle(&node_a).await;
    for i in 1..3usize {
        ha.submit(fake_vote(&s.env, i, &[2, 1], 810 + i as u64))
            .await
            .unwrap();
    }
    wait_until("A settled", async || s.chain.voters() == 2).await;
    shutdown_a.cancel();

    // B restarts: replays A's transition from the blobs, re-proves its own
    // vote on the new root and settles it.
    let db_b = reopen_db(dir_b.path()).await;
    let shutdown_b2 = CancellationToken::new();
    let node_b2 = start_node(
        db_b,
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        FakeProver::open(),
        1,
        "0s",
        shutdown_b2.clone(),
    )
    .await;
    let hb2 = handle(&node_b2).await;
    wait_until("B's vote settled after sync", async || {
        vote_status(&hb2, vid_b).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(hb2.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(s.chain.voters(), 3);
    assert!(node_b2.metrics.synced_from_others.load(Ordering::SeqCst) >= 1);
    shutdown_b2.cancel();
}

/// Killed after its own submit landed but before it saw the receipt: the
/// restart finds the transition on chain and settles the vote without
/// proving it again.
#[tokio::test]
async fn killed_after_own_submit_lands_settles_without_reproving() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    // The tx lands but the submitter never learns (times out).
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
    let v = fake_vote(&s.env, 0, &[1, 2], 900);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("batch sealed", async || prover.calls() >= 1).await;
    // Hide the block the transition event lands in, so the first node can
    // never settle from the event: the kill happens mid-flight for sure.
    s.chain.hide_block(Some(3));
    prover.release(1);
    // Kill the node as soon as its transition is on chain.
    wait_until("own submit landed", async || s.chain.voters() == 1).await;
    assert_eq!(
        vote_status(&h, vid).await,
        Some(VoteStatus::Processed),
        "vote must still be unsettled at kill time"
    );
    shutdown.cancel();
    drop(h);
    drop(node);

    // Restart: the vote ends settled with no new prover call.
    s.chain.hide_block(None);
    let db = reopen_db(dir.path()).await;
    let prover2 = FakeProver::open();
    let shutdown2 = CancellationToken::new();
    let node2 = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover2.clone(),
        2,
        "0s",
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    wait_until("vote settled after restart", async || {
        vote_status(&h2, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(prover2.calls(), 0);
    assert_eq!(h2.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(s.chain.voters(), 1);
    shutdown2.cancel();
}

/// A fresh datadir with --start-block scans from that block, one
/// `eth_getLogs` page at a time.
#[tokio::test]
async fn fresh_datadir_scans_from_the_start_block() {
    let s = setup(2, 2, None);
    let dir = TempDir::new().unwrap();
    s.chain.set_head_block(20_000);
    let cfg = test_config_with(
        dir.path(),
        &s.census_dir,
        1,
        "0s",
        &["--start-block", "10000"],
    );
    let shutdown = CancellationToken::new();
    let node = start_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        FakeProver::open(),
        cfg,
        shutdown.clone(),
    )
    .await;
    wait_until("caught up", async || {
        node.db.meta_u64("monitor_last_block").unwrap() == Some(20_000)
    })
    .await;
    shutdown.cancel();
    assert_eq!(
        s.chain.event_calls()[..3],
        [(10_000, 14_999), (15_000, 19_999), (20_000, 20_000)]
    );
}

/// A page failing mid-scan resumes from the last persisted page, not
/// from the start of the range.
#[tokio::test]
async fn failed_page_resumes_from_the_last_good_one() {
    let s = setup(2, 2, None);
    let dir = TempDir::new().unwrap();
    s.chain.set_head_block(12_000);
    s.chain.fail_events_at(5_001);
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
    wait_until("caught up", async || {
        node.db.meta_u64("monitor_last_block").unwrap() == Some(12_000)
    })
    .await;
    // The process created in block 2 was still picked up.
    assert!(handle(&node).await.snapshot().await.is_ok());
    shutdown.cancel();
    assert_eq!(
        s.chain.event_calls()[..4],
        [
            (1, 5_000),
            (5_001, 10_000),
            (5_001, 10_000),
            (10_001, 12_000)
        ]
    );
}

/// Shutdown stops the monitor while `eth_getLogs` hangs.
#[tokio::test]
async fn shutdown_cuts_a_stalled_catch_up() {
    let s = setup(2, 2, None);
    let dir = TempDir::new().unwrap();
    s.chain.set_head_block(1_000_000);
    s.chain.events_delay(Duration::from_secs(3600));
    let shutdown = CancellationToken::new();
    let cfg = test_config(dir.path(), &s.census_dir, 1, "0s");
    let (_node, tasks) = start_node_tracked(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        FakeProver::open(),
        cfg,
        shutdown.clone(),
    )
    .await;
    wait_until("scan started", async || !s.chain.event_calls().is_empty()).await;
    shutdown.cancel();
    tasks.close();
    tokio::time::timeout(Duration::from_secs(2), tasks.wait())
        .await
        .expect("monitor still running 2s after shutdown");
}
