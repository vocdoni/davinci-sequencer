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

/// Restarts a node on `dir` after the window was hit: the tree is at the
/// chain root, the record writes after it never happened. The node must
/// reopen, heal the pin, write the missing transition record and settle a
/// new vote on the healed root.
async fn restart_after_the_window(
    s: &TestSetup,
    dir: &Path,
    by_self: bool,
    settled: &[u64],
    n_votes: u64,
) {
    let db = reopen_db(dir).await;
    let shutdown = CancellationToken::new();
    let node = start_node(
        db,
        dir,
        s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    wait_until("settled after restart", async || {
        all_settled(&h, settled).await
    })
    .await;
    wait_until("pin healed", async || h.snapshot().await.unwrap().synced).await;
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    let pid = pid_fr();
    wait_until("transition recorded", async || {
        node.db.transitions(&pid).unwrap().len() == 1
    })
    .await;
    let t = &node.db.transitions(&pid).unwrap()[0];
    assert_eq!(
        (t.index, t.new_root, t.by_self),
        (0, s.chain.root(), by_self)
    );
    assert_eq!((t.n_votes, t.n_overwrites), (n_votes, 0));
    assert!(!node.db.blobs(&pid, 0).unwrap().is_empty());
    assert_eq!(
        node.db.process(&pid).unwrap().unwrap().onchain.state_root,
        s.chain.root()
    );
    // A new vote seals on the healed root and lands.
    let v = fake_vote(&s.env, 5, &[0, 1], 1_005);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("new vote settled", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(node.db.transitions(&pid).unwrap().len(), 2);
    shutdown.cancel();
}

/// Crash between the tree commit of our own landing and the record writes
/// after it (votes, transition, pin): the restart reopens at the tree's
/// record and repairs the rest.
#[tokio::test]
async fn crash_between_tree_commit_and_record_write_reopens() {
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
    let v = fake_vote(&s.env, 0, &[1, 2], 1_000);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("batch sealed", async || prover.calls() >= 1).await;
    // From here on no record write persists: the crash.
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_until("landed and committed in the tree", async || {
        s.chain.voters() == 1 && h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    // The monitor moved past the landing's event before the crash.
    let head = s.chain.head_block();
    wait_until("event routed", async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    shutdown.cancel();
    drop((h, node, db));
    restart_after_the_window(&s, dir.path(), true, &[vid], 1).await;
}

/// The same window after a synced foreign transition (tree commit of
/// `apply_synced`, then the transition and record writes).
#[tokio::test]
async fn apply_sync_crash_window_reopens() {
    let s = setup(2, 8, None);
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
    let h = handle(&node).await;
    db.fail_next_writes(u32::MAX);
    let block = s.chain.settle_externally(&foreign_batch(&s, 0, 1));
    wait_until("synced in the tree", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    wait_until("event routed", async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(block)
    })
    .await;
    shutdown.cancel();
    drop((h, node, db));
    restart_after_the_window(&s, dir.path(), false, &[], 2).await;
}

/// Results set on-chain while the node was down, their events lost (the
/// cursor already past them): the actor's first read finalizes the process
/// locally, whether or not the first chain read after the restart fails.
#[tokio::test]
async fn results_lost_at_a_crash_finalize_locally() {
    for boot_read_fails in [false, true] {
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
        let h = handle(&node).await;
        let v = fake_vote(&s.env, 0, &[1, 2], 1_000);
        let vid = v.pkg.vote_id;
        h.submit(v).await.unwrap();
        wait_until("settled", async || all_settled(&h, &[vid]).await).await;
        shutdown.cancel();
        drop((h, node));

        s.chain.set_status(ProcessStatus::Ended);
        s.chain.pass_grace();
        s.chain.set_results_externally(vec![1, 2]);
        let db = reopen_db(dir.path()).await;
        db.set_meta_u64("monitor_last_block", s.chain.head_block())
            .unwrap();
        if boot_read_fails {
            s.chain.fail_process(1);
        }
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
        wait_until("finalized locally", async || {
            h.snapshot().await.unwrap().local == LocalStatus::Finalized
        })
        .await;
        let rec = node.db.process(&pid_fr()).unwrap().unwrap();
        let ctx = format!("boot_read_fails={boot_read_fails}");
        assert_eq!(rec.onchain.status, ProcessStatus::Results, "{ctx}");
        assert_eq!(rec.local, LocalStatus::Finalized, "{ctx}");
        assert_eq!(rec.onchain.results, vec![1, 2], "{ctx}");
        assert!(all_settled(&h, &[vid]).await, "{ctx}");
        shutdown.cancel();
    }
}

/// A crash part way through the genesis of a new process (its tree written,
/// no committed record yet): the restart re-creates it and the process
/// spawns and settles.
#[tokio::test]
async fn crash_inside_genesis_creation_respawns() {
    for skip in [2, 4] {
        let s = setup(2, 8, None);
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        // The params write and `skip - 1` genesis leaves go through.
        db.fail_arbo_writes_after(skip, 1);
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
        wait_until("bootstrap stored the record", async || {
            db.process(&pid_fr()).unwrap().is_some()
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !node.processes.read().await.contains_key(&pid_fr()),
            "the cut genesis spawned"
        );
        shutdown.cancel();
        drop((node, db));

        let shutdown = CancellationToken::new();
        let node = start_node(
            reopen_db(dir.path()).await,
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
        assert_eq!(
            h.snapshot().await.unwrap().root,
            genesis_root(&s.env.cfg).unwrap()
        );
        let v = fake_vote(&s.env, 0, &[1, 2], 1_100);
        let vid = v.pkg.vote_id;
        h.submit(v).await.unwrap();
        wait_until("settled", async || all_settled(&h, &[vid]).await).await;
        shutdown.cancel();
    }
}

/// The rebuild after a reorg drops the tree and the log in one write and
/// re-creates the genesis after it. A crash in between (here: the genesis
/// writes fail) reopens at genesis and the resync takes it to the chain.
#[tokio::test(start_paused = true)]
async fn crash_after_the_rebuild_dropped_the_tree_reopens() {
    let s = setup(2, 8, None);
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
    let h = handle(&node).await;
    let mut vids = Vec::new();
    for i in 0..2 {
        let v = fake_vote(&s.env, i, &[1, 2], 1200 + i as u64);
        vids.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
        wait_until("settled", async || all_settled(&h, &vids).await).await;
    }
    s.chain.reorg_out_last_transition();
    let v2 = fake_vote(&s.env, 2, &[1, 2], 1202);
    vids.push(v2.pkg.vote_id);
    h.submit(v2).await.unwrap();
    // Diverged, and the rebuild a while off: from here no tree write lands.
    wait_until("the revert", async || {
        node.metrics.lost_races.load(Ordering::SeqCst) == 1
    })
    .await;
    db.fail_next_arbo_writes(u32::MAX);
    for _ in 0..200 {
        if node.metrics.rebuilds.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(node.metrics.rebuilds.load(Ordering::SeqCst), 1);
    assert!(db.transitions(&pid_fr()).unwrap().is_empty());
    shutdown.cancel();
    drop((h, node, db));

    let shutdown = CancellationToken::new();
    let node = start_node(
        reopen_db(dir.path()).await,
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
    for _ in 0..200 {
        if all_settled(&h, &vids).await && h.snapshot().await.unwrap().root == s.chain.root() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(all_settled(&h, &vids).await, "not every vote settled");
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(s.chain.voters(), 3);
    shutdown.cancel();
}
