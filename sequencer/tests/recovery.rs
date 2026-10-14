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
        // The params write and `skip - 1` genesis leaves go through, then
        // no tree write does (the crash): the bootstrap retry cannot spawn.
        db.fail_arbo_writes_after(skip, u32::MAX);
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

/// Opens a node of 24-vote batches with the timers of the batching tests.
async fn timed_node(
    db: Db,
    dir: &Path,
    s: &TestSetup,
    prover: Arc<FakeProver>,
    shutdown: &CancellationToken,
) -> (Node, ActorHandle) {
    let extra = ["--batch-time", "100s", "--solo-wait", "300s"];
    let cfg = test_config_with(dir, &s.census_dir, 24, "0s", &extra);
    let node = start_node_cfg(db, s.chain.clone(), prover, cfg, shutdown.clone()).await;
    let h = handle(&node).await;
    (node, h)
}

/// Waits up to `secs` (virtual) seconds, one second at a time.
async fn wait_secs(what: &str, secs: u64, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..secs {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timeout waiting for {what}");
}

/// The repair of a revote's lost transition record waits while its blobs
/// cannot be fetched: no record without its archive, and the batch's
/// exposed set is kept meanwhile. Once the record is written with its
/// blobs, a reorg that takes the revote rebuilds, and the re-seal covers
/// every slot the revote's batch published (24 occupied slots: a batch
/// refreshes a random 16 of them).
#[tokio::test(start_paused = true)]
async fn repair_waits_for_the_blobs_and_keeps_the_exposed_set() {
    let s = setup(2, 26, None);
    let nf = s.env.cfg.ballot_mode.num_fields;
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, prover.clone(), &shutdown).await;
    let mut first = Vec::new();
    for i in 0..24 {
        let v = fake_vote(&s.env, i, &[1, 2], 1700 + i as u64);
        first.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    prover.release(1);
    wait_secs("24 settled", 600, async || all_settled(&h, &first).await).await;
    // A revote, sealed alone; its landing's record writes fail (the crash).
    let rv = fake_vote(&s.env, 0, &[2, 1], 1800);
    let rvid = rv.pkg.vote_id;
    h.submit(rv).await.unwrap();
    s.chain.advance_time(400);
    wait_secs("revote sealed", 600, async || prover.calls() >= 2).await;
    let exposed = db.exposed(&pid_fr()).unwrap().unwrap().slots;
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_secs("landed in the tree", 600, async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    let head = s.chain.head_block();
    wait_secs("event routed", 600, async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    shutdown.cancel();
    drop((h, node, db));

    // The restart cannot fetch the revote's blobs.
    s.chain.fail_blobs(u32::MAX);
    let db = reopen_db(dir.path()).await;
    let shutdown = CancellationToken::new();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, FakeProver::open(), &shutdown).await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(
        db.transitions(&pid_fr()).unwrap().len(),
        1,
        "the revote's record was written without its blobs"
    );
    let kept = db.exposed(&pid_fr()).unwrap().map(|r| r.slots);
    assert_eq!(kept.as_ref(), Some(&exposed), "the exposed set was dropped");
    s.chain.fail_blobs(0);
    wait_secs("the record with its blobs", 600, async || {
        db.transitions(&pid_fr()).unwrap().len() == 2
    })
    .await;
    let t = &db.transitions(&pid_fr()).unwrap()[1];
    assert_eq!(db.blobs(&pid_fr(), 1).unwrap().len() as u64, t.n_blobs);

    // A reorg takes the revote; the next batch reverts and the node rebuilds.
    s.chain.reorg_out_last_transition();
    let w = fake_vote(&s.env, 25, &[1, 2], 1801);
    let wid = w.pkg.vote_id;
    h.submit(w).await.unwrap();
    s.chain.advance_time(400);
    wait_secs("rebuilt", 600, async || {
        node.metrics.rebuilds.load(Ordering::SeqCst) == 1
    })
    .await;
    s.chain.advance_time(400);
    wait_secs("all settled on the chain's root", 600, async || {
        all_settled(&h, &[rvid, wid]).await && h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    let reseal = updated_slots(&db, nf, transition_of(&db, nf, rvid));
    for slot in exposed {
        assert!(
            reseal.contains(&slot),
            "the re-seal left out an exposed slot"
        );
    }
    shutdown.cancel();
}

/// The crash window of `crash_between_tree_commit_and_record_write_reopens`
/// (the tree committed the landing, its record never written), and a reorg
/// takes that landing before the restart. The tree is one transition ahead
/// of the log, and the chain sits at the log's last root: the node rebuilds
/// from there, re-seals the vote on the chain and covers every slot the
/// orphaned batch published (kept in its exposed set: it has no record).
#[tokio::test(start_paused = true)]
async fn crash_window_landing_reorged_out_rebuilds() {
    let s = setup(2, 26, None);
    let nf = s.env.cfg.ballot_mode.num_fields;
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, prover.clone(), &shutdown).await;
    let mut first = Vec::new();
    for i in 0..24 {
        let v = fake_vote(&s.env, i, &[1, 2], 1900 + i as u64);
        first.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    prover.release(1);
    wait_secs("24 settled", 600, async || all_settled(&h, &first).await).await;
    let v = fake_vote(&s.env, 24, &[2, 1], 1924);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    s.chain.advance_time(400);
    wait_secs("sealed", 600, async || prover.calls() >= 2).await;
    let exposed = db.exposed(&pid_fr()).unwrap().unwrap().slots;
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_secs("landed in the tree", 600, async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    let head = s.chain.head_block();
    wait_secs("event routed", 600, async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    shutdown.cancel();
    drop((h, node, db));

    s.chain.reorg_out_last_transition();
    let db = reopen_db(dir.path()).await;
    let shutdown = CancellationToken::new();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, FakeProver::open(), &shutdown).await;
    wait_secs("rebuilt", 600, async || {
        node.metrics.rebuilds.load(Ordering::SeqCst) == 1
    })
    .await;
    s.chain.advance_time(400);
    wait_secs("re-sealed on the chain", 600, async || {
        all_settled(&h, &[vid]).await && h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(s.chain.voters(), 25);
    let (p, root) = h.vote_id_proof(vid).await.unwrap();
    assert!(p.exists);
    assert_eq!(root, s.chain.root());
    let reseal = updated_slots(&db, nf, transition_of(&db, nf, vid));
    for slot in exposed {
        assert!(
            reseal.contains(&slot),
            "the re-seal left out an exposed slot"
        );
    }
    shutdown.cancel();
}

/// The same crash window, but before the restart a peer lands on the new
/// branch from the orphan's old root (the log's last root). That transition
/// is not our missing record (it does not reach our tree's root): the node
/// rebuilds from the log's last root, syncs the peer and re-seals the
/// orphan's vote, covering every slot the orphan batch published.
#[tokio::test(start_paused = true)]
async fn crash_window_peer_forks_from_the_orphan_root_rebuilds() {
    let s = setup(2, 26, None);
    let nf = s.env.cfg.ballot_mode.num_fields;
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, prover.clone(), &shutdown).await;
    let mut first = Vec::new();
    for i in 0..24 {
        let v = fake_vote(&s.env, i, &[1, 2], 2900 + i as u64);
        first.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    prover.release(1);
    wait_secs("24 settled", 600, async || all_settled(&h, &first).await).await;
    let v = fake_vote(&s.env, 24, &[2, 1], 2924);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    s.chain.advance_time(400);
    wait_secs("sealed", 600, async || prover.calls() >= 2).await;
    let exposed = db.exposed(&pid_fr()).unwrap().unwrap().slots;
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_secs("landed in the tree", 600, async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    let head = s.chain.head_block();
    wait_secs("event routed", 600, async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    shutdown.cancel();
    drop((h, node, db));

    s.chain.reorg_out_last_transition();
    let db = reopen_db(dir.path()).await;
    // A peer lands on the new branch, from the orphan's old root.
    let peer = fake_vote(&s.env, 25, &[1, 2], 2925);
    let pvid = peer.pkg.vote_id;
    s.chain
        .settle_externally(&peer_batch_of(&db, &s, std::slice::from_ref(&peer)));
    let shutdown = CancellationToken::new();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, FakeProver::open(), &shutdown).await;
    wait_secs("rebuilt", 300, async || {
        node.metrics.rebuilds.load(Ordering::SeqCst) == 1
    })
    .await;
    s.chain.advance_time(400);
    wait_secs("re-sealed on the chain", 600, async || {
        all_settled(&h, &[vid]).await && h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(s.chain.voters(), 26);
    // The peer's vote was never ours: no local record, but in the tree.
    for id in [vid, pvid] {
        let (p, root) = h.vote_id_proof(id).await.unwrap();
        assert!(p.exists);
        assert_eq!(root, s.chain.root());
    }
    let ts = db.transitions(&pid_fr()).unwrap();
    assert_eq!(
        ts.iter().map(|t| t.by_self).collect::<Vec<_>>(),
        [false, false, true],
        "the log is the chain's: ours replayed, the peer's, our re-seal"
    );
    let reseal = updated_slots(&db, nf, transition_of(&db, nf, vid));
    for slot in exposed {
        assert!(
            reseal.contains(&slot),
            "the re-seal left out an exposed slot"
        );
    }
    assert_eq!(node.metrics.rebuilds.load(Ordering::SeqCst), 1);
    shutdown.cancel();
}

/// Our landing and then a peer's, both in the tree and neither recorded:
/// what an earlier build left after its record writes failed (this one no
/// longer applies past a missing record). At the restart the log is
/// repaired in order: each transition leaving the log's last root reaches a
/// root our tree holds, so it is ours to record, not a fork. Nothing
/// rebuilds, and the next vote seals and settles.
#[tokio::test(start_paused = true)]
async fn two_unrecorded_landings_are_repaired_in_order() {
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
    let v0 = fake_vote(&s.env, 0, &[1, 2], 3000);
    h.submit(v0.clone()).await.unwrap();
    wait_secs("sealed", 600, async || prover.calls() >= 1).await;
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_secs("ours in the tree", 600, async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    let peer = fake_vote(&s.env, 5, &[2, 1], 3001);
    let b = s.chain.peer_batch(&s, std::slice::from_ref(&peer));
    s.chain.settle_externally(&b);
    let head = s.chain.head_block();
    wait_secs("the peer's event routed", 600, async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    shutdown.cancel();
    drop((h, node, db));
    // The earlier build applied the peer's transition too.
    let db = reopen_db(dir.path()).await;
    {
        let mut st = davinci_state::ProcessState::open_stored(
            s.env.cfg.clone(),
            db.arbo_storage(&pid_fr()).unwrap(),
        )
        .unwrap();
        let nf = s.env.cfg.ballot_mode.num_fields;
        let td = davinci_zkvm_sdk::blob::decode_blobs(&b.blobs.blobs, nf).unwrap();
        st.apply_synced(&td, b.new_root).unwrap();
    }
    assert!(db.transitions(&pid_fr()).unwrap().is_empty());

    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
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
    wait_secs("both records", 300, async || {
        db.transitions(&pid_fr()).unwrap().len() == 2
    })
    .await;
    let ts = db.transitions(&pid_fr()).unwrap();
    assert_eq!(
        ts.iter().map(|t| t.by_self).collect::<Vec<_>>(),
        [true, false]
    );
    assert_eq!(ts[1].old_root, ts[0].new_root);
    assert_eq!(ts[1].new_root, s.chain.root());
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(
        vote_status(&h, v0.pkg.vote_id).await,
        Some(VoteStatus::Settled)
    );
    let v1 = fake_vote(&s.env, 1, &[1, 2], 3002);
    h.submit(v1.clone()).await.unwrap();
    wait_secs("the next vote settled", 300, async || {
        all_settled(&h, &[v1.pkg.vote_id]).await
    })
    .await;
    assert_eq!(s.chain.voters(), 3);
    tokio::time::sleep(Duration::from_secs(200)).await;
    assert_eq!(node.metrics.rebuilds.load(Ordering::SeqCst), 0);
    shutdown.cancel();
}

/// Our landing's record waits on its blobs (R4) when a peer lands on top:
/// the peer's transition must not be recorded at our index ahead of it. The
/// log repairs in block order once our blobs come: ours, then the peer's.
#[tokio::test(start_paused = true)]
async fn a_peer_landing_waits_for_our_pending_record() {
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
    let v0 = fake_vote(&s.env, 0, &[1, 2], 3100);
    h.submit(v0.clone()).await.unwrap();
    wait_secs("sealed", 600, async || prover.calls() >= 1).await;
    db.fail_next_writes(u32::MAX);
    prover.release(1);
    wait_secs("ours in the tree", 600, async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    let ours = s.chain.root();
    s.chain.withhold_blobs(Some(s.chain.last_transition_tx()));
    db.fail_next_writes(0);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(db.transitions(&pid_fr()).unwrap().is_empty());
    let peer = fake_vote(&s.env, 5, &[2, 1], 3101);
    s.chain
        .settle_externally(&s.chain.peer_batch(&s, std::slice::from_ref(&peer)));
    tokio::time::sleep(Duration::from_secs(30)).await;
    let ts = db.transitions(&pid_fr()).unwrap();
    assert!(
        ts.iter().all(|t| t.old_root != ours),
        "the peer's transition was recorded ahead of ours"
    );
    s.chain.withhold_blobs(None);
    wait_secs("both records", 300, async || {
        db.transitions(&pid_fr()).unwrap().len() == 2
    })
    .await;
    let ts = db.transitions(&pid_fr()).unwrap();
    assert_eq!(
        ts.iter().map(|t| t.by_self).collect::<Vec<_>>(),
        [true, false]
    );
    assert_eq!(ts[0].new_root, ours);
    assert_eq!(ts[1].old_root, ours);
    assert_eq!(db.blobs(&pid_fr(), 0).unwrap().len() as u64, ts[0].n_blobs);
    wait_secs("on the chain's root", 300, async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(
        vote_status(&h, v0.pkg.vote_id).await,
        Some(VoteStatus::Settled)
    );
    shutdown.cancel();
}

/// The crash window of `apply_sync_crash_window_reopens` with a populated
/// election and one of our votes in the peer's batch: our revote waits here,
/// a peer includes it with its refreshes, the sync commits the tree but not
/// the record, and a reorg takes the peer transition before the restart. The
/// rebuild re-seals the revote, and the re-seal covers every slot the peer's
/// public batch changed (24 occupied slots: a batch refreshes a random 16).
#[tokio::test(start_paused = true)]
async fn synced_peer_batch_lost_at_a_crash_keeps_its_slots_covered() {
    let s = setup(2, 26, None);
    let nf = s.env.cfg.ballot_mode.num_fields;
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, prover.clone(), &shutdown).await;
    let mut first = Vec::new();
    for i in 0..24 {
        let v = fake_vote(&s.env, i, &[1, 2], 3200 + i as u64);
        first.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    prover.release(1);
    wait_secs("24 settled", 600, async || all_settled(&h, &first).await).await;
    // Our revote waits for company; a peer includes it first.
    let x = fake_vote(&s.env, 3, &[2, 1], 3300);
    let xid = x.pkg.vote_id;
    h.submit(x.clone()).await.unwrap();
    let b = peer_batch_of(&db, &s, std::slice::from_ref(&x));
    let peer_slots: Vec<u64> = davinci_zkvm_sdk::blob::decode_blobs(&b.blobs.blobs, nf)
        .unwrap()
        .updates
        .iter()
        .map(|(s, _)| *s)
        .collect();
    db.fail_transition_writes(true);
    let block = s.chain.settle_externally(&b);
    wait_secs("synced in the tree", 600, async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    wait_secs("event routed", 600, async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(block)
    })
    .await;
    assert_eq!(db.transitions(&pid_fr()).unwrap().len(), 1);
    shutdown.cancel();
    drop((h, node, db));

    s.chain.reorg_out_last_transition();
    let db = reopen_db(dir.path()).await;
    let shutdown = CancellationToken::new();
    let (node, h) = timed_node(db.clone(), dir.path(), &s, FakeProver::open(), &shutdown).await;
    wait_secs("rebuilt", 600, async || {
        node.metrics.rebuilds.load(Ordering::SeqCst) == 1
    })
    .await;
    s.chain.advance_time(400);
    wait_secs("re-sealed on the chain", 600, async || {
        all_settled(&h, &[xid]).await && h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    let (p, root) = h.vote_id_proof(xid).await.unwrap();
    assert!(p.exists);
    assert_eq!(root, s.chain.root());
    let reseal = updated_slots(&db, nf, transition_of(&db, nf, xid));
    for slot in peer_slots {
        assert!(
            reseal.contains(&slot),
            "the re-seal left out a slot the peer published"
        );
    }
    shutdown.cancel();
}
