//! The actor in L1-blob mode on the scripted fakes: blobs posted on a
//! `FakeL1` while proving, settlement through `FakeChain`'s L1 mode, sync
//! from Ethereum blobs.

mod common;

use common::*;

async fn submit(
    h: &ActorHandle,
    s: &TestSetup,
    voters: std::ops::Range<usize>,
    kseed: u64,
) -> Vec<u64> {
    let mut vids = Vec::new();
    for i in voters {
        let v = fake_vote(&s.env, i, &[1, 2], kseed + i as u64);
        vids.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    vids
}

// `wait_until` with a longer (virtual) horizon.
async fn wait_long(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timeout waiting for {what}");
}

async fn l1_node(
    s: &TestSetup,
    l1: &FakeL1,
    prover: Arc<FakeProver>,
    dir: &TempDir,
    shutdown: &CancellationToken,
) -> (Node, ActorHandle) {
    let node = start_l1_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        s,
        s.chain.clone(),
        l1.clone(),
        prover,
        3,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    (node, h)
}

#[tokio::test]
async fn l1_happy_path_posts_proves_and_settles() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 100).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(s.chain.voters(), 3);
    assert_eq!(l1.mined_posts(), 1);
    assert_eq!(s.chain.l1_records(), 0, "the proof was ready first");
    assert!(s.chain.l1_last_headers() <= 3);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    let ts = node.db.transitions(&pid_fr()).unwrap();
    assert_eq!(ts.len(), 1);
    assert!(ts[0].by_self);
    assert_eq!(
        node.db.blobs(&pid_fr(), 0).unwrap().len() as u64,
        ts[0].n_blobs
    );
    assert!(node.db.l1_posts().unwrap().is_empty());
    shutdown.cancel();
}

// ~10 s per vote at nf 2 (prove_base 30 s).
fn slow(node: &Node) {
    for _ in 0..60 {
        node.metrics.prove.observe(2, 1, 40.0, 30.0);
    }
}

/// The live OP setup: a 150 s floor, the default 30 s prove base and the
/// L1-blob default margin (30 s). The 110 s Ethereum leg overlaps the proof,
/// so the batch is sized by 150 − 30 − 30 = 90 s: 9 votes at 10 s each.
#[tokio::test]
async fn l1_floor_budget_seals_with_the_default_margin() {
    let s = setup(2, 16, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let cfg = test_config_with(
        dir.path(),
        &s.census_dir,
        20,
        "30s",
        &["--batch-time", "100s", "--solo-wait", "300s"],
    );
    let node = start_l1_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        l1.clone(),
        prover.clone(),
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let vids = submit(&h, &s, 0..15, 1200).await;
    slow(&node);
    s.chain.advance_time(115);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![9, 6]);
    assert_eq!(s.chain.voters(), 15);
    assert_eq!(l1.mined_posts(), 2);
    shutdown.cancel();
}

/// 110 s of Ethereum leg plus a 60 s margin exceed the 150 s floor: nothing
/// seals while the window is open. Past the end a raised grace fits them.
#[tokio::test]
async fn l1_lag_over_the_floor_waits_for_the_end() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let cfg = test_config(dir.path(), &s.census_dir, 3, "60s");
    let node = start_l1_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        l1.clone(),
        prover.clone(),
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    s.chain.set_grace(600);
    let vids = submit(&h, &s, 0..3, 1300).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prover.calls(), 0);
    assert!(l1.sends().is_empty());
    for vid in &vids {
        assert_eq!(vote_status(&h, *vid).await, Some(VoteStatus::Pending));
    }
    s.chain.advance_time(DURATION + 1);
    wait_until("settled in the grace", async || {
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(prover.sizes(), vec![3]);
    assert_eq!(s.chain.voters(), 3);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_origin_lag_records_once_and_anchors_at_the_record() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    l1.pin_origin(Some(0));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 200).await;
    wait_until("blobs posted", async || l1.mined_posts() == 1).await;
    let n = l1.block_of(&l1.sends()[0].hash).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        s.chain.l1_records(),
        0,
        "OP's origin is not at the blobs yet"
    );
    l1.pin_origin(Some(n));
    wait_until("origin recorded", async || s.chain.l1_records() == 1).await;
    // The origin runs 30 blocks ahead before the proof is ready.
    l1.mine(30);
    l1.pin_origin(Some(n + 30));
    prover.release(10);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(s.chain.l1_records(), 1, "recorded once");
    assert_eq!(
        s.chain.l1_last_headers(),
        1,
        "anchored at the record, not 33 headers"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn l1_origin_mismatch_is_rebuilt_not_reposted() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.revert_simulates("L1OriginMismatch", 2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 300).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert!(s.chain.l1_simulates() >= 3);
    assert_eq!(l1.mined_posts(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_tx_too_old_posts_again() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.revert_simulates("TxTooOld", 1);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 400).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(l1.mined_posts(), 2);
    let second = l1.sends().last().unwrap().hash;
    assert_eq!(
        s.chain.l1_last().unwrap().blocks[0],
        l1.block_of(&second).unwrap()
    );
    shutdown.cancel();
}

#[tokio::test]
async fn l1_dropped_post_is_replaced_after_three_blocks() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    l1.drop_next(1);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 500).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    let sends = l1.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].nonce, sends[1].nonce);
    assert_eq!(sends[1].fees, sends[0].fees.doubled());
    assert_eq!(l1.mined_posts(), 1);
    shutdown.cancel();
}

// Paused: whether the poster or the settle loop sees the reorg first, the
// latter waits GONE_FOR before posting again.
#[tokio::test(start_paused = true)]
async fn l1_reorged_post_is_posted_again() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 600).await;
    wait_until("blobs posted", async || l1.mined_posts() == 1).await;
    let first = l1.sends()[0].clone();
    // An Ethereum reorg drops the block with the blob transaction.
    l1.reorg(1);
    assert_eq!(l1.mined_posts(), 0);
    prover.release(10);
    wait_long("settled", async || all_settled(&h, &vids).await).await;
    let sends = l1.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1].nonce, first.nonce, "the freed nonce");
    assert_eq!(
        sends[1].fees,
        first.fees.doubled(),
        "priced over the reorged tx"
    );
    assert_eq!(l1.mined_posts(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_lost_race_syncs_from_ethereum_and_settles_next() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let (dir_a, dir_b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let shutdown = CancellationToken::new();
    let (prover_a, prover_b) = (FakeProver::gated(), FakeProver::gated());
    let node_a = start_l1_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        l1.clone(),
        prover_a.clone(),
        2,
        shutdown.clone(),
    )
    .await;
    let node_b = start_l1_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        l1.with_sender(0x2b),
        prover_b.clone(),
        2,
        shutdown.clone(),
    )
    .await;
    let (ha, hb) = (handle(&node_a).await, handle(&node_b).await);
    let vids_a = submit(&ha, &s, 0..2, 10).await;
    let vids_b = submit(&hb, &s, 2..4, 20).await;
    wait_until("both proving", async || {
        prover_a.calls() >= 1 && prover_b.calls() >= 1
    })
    .await;
    prover_a.release(100);
    wait_until("A settled", async || s.chain.voters() == 2).await;
    prover_b.release(100);
    wait_until("B settled", async || {
        all_settled(&hb, &vids_b).await && s.chain.voters() == 4
    })
    .await;
    assert_eq!(node_b.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(node_b.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    let b_posts = l1
        .sends()
        .iter()
        .filter(|x| x.from == Address::repeat_byte(0x2b))
        .count();
    assert!(b_posts >= 2, "the losing post is wasted: {b_posts}");
    wait_until("A synced B's transition from Ethereum", async || {
        ha.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert!(all_settled(&ha, &vids_a).await);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_restart_replaces_the_pending_post() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    // No blocks: the first run's post stays pending.
    l1.set_auto_mine(false);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 700).await;
    wait_until("post recorded", async || {
        node.db.l1_posts().unwrap().len() == 1
    })
    .await;
    let rec = node.db.l1_posts().unwrap().remove(0);
    assert_eq!(l1.mined_posts(), 0);
    shutdown.cancel();
    drop(h);
    drop(node);

    let db = reopen_db(dir.path()).await;
    l1.set_auto_mine(true);
    let shutdown2 = CancellationToken::new();
    let node2 = start_l1_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        l1.clone(),
        FakeProver::open(),
        3,
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    // Re-proved (the batch secrets died with the first run) and re-posted
    // over the pending transaction, at its nonce.
    wait_until("recovered votes settled", async || {
        all_settled(&h2, &vids).await
    })
    .await;
    let last = l1.sends().pop().unwrap();
    assert_eq!(last.nonce, rec.nonce);
    assert!(last.fees.max_fee >= 2 * rec.fees.max_fee);
    assert!(last.fees.tip >= 2 * rec.fees.tip);
    assert!(last.fees.blob_fee >= 2 * rec.fees.blob_fee);
    assert_eq!(l1.mined_posts(), 1);
    assert!(node2.db.l1_posts().unwrap().is_empty());
    shutdown2.cancel();
}

#[tokio::test(start_paused = true)]
async fn l1_settlement_that_never_shows_is_rolled_back() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.vanish_next_l1_submit();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 800).await;
    // The receipt came back but no event ever does: after the landing wait
    // the votes go back to the queue, are proved again and settle.
    wait_long("settled after the rollback", async || {
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(l1.mined_posts(), 2);
    assert_eq!(s.chain.voters(), 3);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    shutdown.cancel();
}

#[tokio::test(start_paused = true)]
async fn l1_late_event_keeps_the_landed_flight() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    // The landing's events stay out of the event pages for now.
    s.chain.hide_block(Some(s.chain.head_block() + 1));
    let vids = submit(&h, &s, 0..3, 900).await;
    wait_long("landed", async || s.chain.voters() == 3).await;
    // Twice past LANDED_EVENT_WAIT (90 s), the registry showing the new root.
    tokio::time::sleep(Duration::from_secs(200)).await;
    assert!(h.snapshot().await.unwrap().in_flight, "the flight is kept");
    assert!(!all_settled(&h, &vids).await);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 0);
    // The event arrives: the flight commits, nothing is proved or posted again.
    s.chain.hide_block(None);
    s.chain.set_replay_all(true);
    s.chain.set_head_block(s.chain.head_block() + 1);
    wait_long("settled", async || all_settled(&h, &vids).await).await;
    s.chain.set_replay_all(false);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    assert_eq!(prover.calls(), 1, "no rollback, no re-prove");
    assert_eq!(l1.sends().len(), 1, "no re-post");
    assert_eq!(s.chain.voters(), 3);
    assert_eq!(node.db.transitions(&pid_fr()).unwrap().len(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_event_before_the_receipt_commits_once() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    // The settlement lands and its event is out while submit_transition_l1
    // still waits for its receipt.
    s.chain.gate_l1_submits(true);
    let vids = submit(&h, &s, 0..3, 1000).await;
    wait_until("committed at the event", async || {
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    assert!(!h.snapshot().await.unwrap().in_flight);
    // The dropped flight cancelled the job mid-send; the next batch settles.
    s.chain.gate_l1_submits(false);
    let next = submit(&h, &s, 3..6, 1100).await;
    wait_until("next batch settled", async || all_settled(&h, &next).await).await;
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 2);
    assert_eq!(s.chain.voters(), 6);
    assert_eq!(node.db.transitions(&pid_fr()).unwrap().len(), 2);
    assert_eq!((prover.calls(), l1.mined_posts()), (2, 2));
    shutdown.cancel();
}
