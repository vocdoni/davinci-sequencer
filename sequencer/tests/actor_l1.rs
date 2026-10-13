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

// ------------------------------------------------- several transactions

/// A 16-field node whose Ethereum takes one blob per transaction: a batch
/// of 127 or more fresh votes needs two blobs, so two transactions.
fn chunked_cfg(dir: &TempDir, s: &TestSetup, batch_max: usize, extra: &[&str]) -> Config {
    let mut args = vec!["--max-blobs-per-tx", "1"];
    args.extend_from_slice(extra);
    test_config_with(dir.path(), &s.census_dir, batch_max, "0s", &args)
}

async fn chunked_node(
    s: &TestSetup,
    l1: &FakeL1,
    prover: Arc<FakeProver>,
    dir: &TempDir,
    cfg: Config,
    shutdown: &CancellationToken,
) -> (Node, ActorHandle) {
    let node = start_l1_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        l1.clone(),
        prover,
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    (node, h)
}

#[tokio::test]
async fn l1_a_two_blob_batch_goes_out_in_two_transactions() {
    let s = setup(16, 140, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let (node, h) = chunked_node(&s, &l1, prover.clone(), &dir, cfg, &shutdown).await;
    let vids = submit(&h, &s, 0..140, 2000).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    // One batch (the per-batch cap is MAX_BLOBS), two blobs, two posts.
    assert_eq!(prover.sizes(), vec![140]);
    let sends = l1.sends();
    assert_eq!(sends.iter().map(|x| x.nonce).collect::<Vec<_>>(), [0, 1]);
    let blocks: Vec<u64> = sends
        .iter()
        .map(|x| l1.block_of(&x.hash).unwrap())
        .collect();
    assert!(blocks[1] > blocks[0]);
    assert_eq!(s.chain.l1_last().unwrap().blocks, blocks);
    assert_eq!(s.chain.voters(), 140);
    assert!(node.db.l1_posts().unwrap().is_empty());
    shutdown.cancel();
}

/// The fake registry's 150 s floor, a 140 s Ethereum leg and no margin:
/// the leg of a second transaction (24 s more) does not fit while voting is
/// open, so the batch is sized down to one blob, not held.
#[tokio::test]
async fn l1_the_seal_budget_sizes_a_batch_down_to_fewer_transactions() {
    let s = setup(16, 140, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let cfg = chunked_cfg(
        &dir,
        &s,
        140,
        &["--l1-settle-lag", "140s", "--batch-time", "100s"],
    );
    let (_node, h) = chunked_node(&s, &l1, prover.clone(), &dir, cfg, &shutdown).await;
    let vids = submit(&h, &s, 0..140, 3000).await;
    // One blob holds 126 fresh 16-field votes.
    wait_until("one batch settled", async || s.chain.voters() == 126).await;
    assert_eq!(prover.sizes(), vec![126]);
    assert_eq!(l1.mined_posts(), 1);
    s.chain.advance_time(115);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![126, 14]);
    assert_eq!(l1.mined_posts(), 2);
    shutdown.cancel();
}

/// An exposed set of 126 live slots plus one new vote needs 127 entries: two
/// blobs. In a window that fits one blob this is a window-limited shortfall,
/// not a `RefreshOverflow`: no vote errors and the set stays until the grace
/// after the end opens a window for two (grace raised to 180 s).
#[tokio::test]
async fn l1_a_short_window_waits_for_an_exposed_set_that_needs_more_blobs() {
    let s = setup(16, 150, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    // Phase 1: 140 votes settle, so their slots are live.
    let shutdown = CancellationToken::new();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let (node, h) = chunked_node(&s, &l1, FakeProver::open(), &dir, cfg, &shutdown).await;
    let first = submit(&h, &s, 0..140, 3500).await;
    wait_until("first votes settled", async || {
        all_settled(&h, &first).await
    })
    .await;
    shutdown.cancel();
    drop(h);
    drop(node);

    // Phase 2: an exposed record over 126 of those slots, as a lost race
    // leaves it, then one new vote whose slot is not in it.
    let db = reopen_db(dir.path()).await;
    let late = fake_vote(&s.env, 140, &[1, 2], 3700);
    let late_vid = late.pkg.vote_id;
    db.put_exposed(
        &pid_fr(),
        &davinci_sequencer::storage::ExposedRecord {
            slots: (0..126)
                .map(|i| fake_vote(&s.env, i, &[1, 2], 3500 + i as u64).slot)
                .collect(),
            vote_ids: vec![late_vid],
        },
    )
    .unwrap();
    // The fake registry's 150 s floor and no margin: one chunk of one blob
    // while voting is open.
    let shutdown2 = CancellationToken::new();
    let cfg = chunked_cfg(
        &dir,
        &s,
        140,
        &["--l1-settle-lag", "140s", "--batch-time", "0s"],
    );
    let node2 = start_l1_node_cfg(
        db,
        s.chain.clone(),
        l1.clone(),
        FakeProver::open(),
        cfg,
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    // Read before the submit: a seal tick follows it.
    let before = node2.db.exposed(&pid_fr()).unwrap().unwrap();
    h2.submit(late).await.unwrap();
    // A few seal ticks in the short window: nothing errors, nothing clears.
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            vote_status(&h2, late_vid).await,
            Some(VoteStatus::Pending),
            "a window-limited overflow touched the vote"
        );
        // Still queued, so the loop's ticks do reach the selection.
        assert_eq!(h2.snapshot().await.unwrap().pending, 1);
        assert_eq!(node2.db.exposed(&pid_fr()).unwrap().as_ref(), Some(&before));
    }
    // The end opens a grace of 180 s: window 180 - 140 fits two chunks.
    s.chain.set_grace(180);
    s.chain.advance_time(DURATION + 1);
    wait_until("settled", async || all_settled(&h2, &[late_vid]).await).await;
    // The settlement after the end carries two blobs in two transactions.
    assert_eq!(s.chain.l1_last().unwrap().blocks.len(), 2);
    shutdown2.cancel();
}

#[tokio::test]
async fn l1_restart_with_one_chunk_pending_replaces_it() {
    let s = setup(16, 140, None);
    let l1 = FakeL1::new();
    // The first chunk is mined, the second stays in the pool.
    l1.limit_mined(Some(1));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let (node, h) = chunked_node(&s, &l1, FakeProver::open(), &dir, cfg, &shutdown).await;
    let vids = submit(&h, &s, 0..140, 4000).await;
    wait_until("second chunk pending", async || {
        l1.sends().len() == 2 && node.db.l1_posts().unwrap().len() == 1
    })
    .await;
    let rec = node.db.l1_posts().unwrap().remove(0);
    assert_eq!((rec.nonce, l1.mined_posts()), (1, 1));
    shutdown.cancel();
    drop(h);
    drop(node);

    let db = reopen_db(dir.path()).await;
    l1.limit_mined(None);
    let shutdown2 = CancellationToken::new();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let node2 = start_l1_node_cfg(
        db,
        s.chain.clone(),
        l1.clone(),
        FakeProver::open(),
        cfg,
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    // Proved again; its first chunk replaces the pending one at nonce 1
    // with every fee doubled, its second follows at nonce 2.
    wait_until("recovered votes settled", async || {
        all_settled(&h2, &vids).await
    })
    .await;
    let sends = l1.sends();
    assert_eq!(
        sends.iter().map(|x| x.nonce).collect::<Vec<_>>(),
        [0, 1, 1, 2]
    );
    assert!(sends[2].fees.max_fee >= 2 * rec.fees.max_fee);
    assert!(sends[2].fees.tip >= 2 * rec.fees.tip);
    assert!(sends[2].fees.blob_fee >= 2 * rec.fees.blob_fee);
    assert_eq!(
        l1.block_of(&rec.tx_hash),
        None,
        "the pending chunk was replaced"
    );
    // The first run's mined chunk is wasted; the settlement carries the new pair.
    assert_eq!(l1.mined_posts(), 3);
    let (b2, b3) = (
        l1.block_of(&sends[2].hash).unwrap(),
        l1.block_of(&sends[3].hash).unwrap(),
    );
    assert_eq!(s.chain.l1_last().unwrap().blocks, vec![b2, b3]);
    assert!(node2.db.l1_posts().unwrap().is_empty());
    shutdown2.cancel();
}

// Ethereum's pace whatever the poll: a block every 12 s.
fn mine_every_12s(l1: &FakeL1) -> tokio::task::JoinHandle<()> {
    l1.set_auto_mine(false);
    let l1 = l1.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(12)).await;
            l1.mine(1);
        }
    })
}

async fn all_pending(h: &ActorHandle, vids: &[u64]) {
    for vid in vids {
        assert_eq!(vote_status(h, *vid).await, Some(VoteStatus::Pending));
    }
}

/// A proof no post fixes, every time: after three proved batches whose
/// settlement ends without landing the node seals nothing more for the
/// process and the votes stay pending. A restart clears the stop.
#[tokio::test(start_paused = true)]
async fn l1_repeated_settle_failures_stop_sealing_until_a_restart() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    l1.fault_proofs(Some(ProofFault::Data));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 1500).await;
    wait_long("three attempts", async || prover.calls() == 3).await;
    // Past the longest cooldown (300 s): a fourth attempt would have started.
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(prover.calls(), 3, "no seal after the third failure");
    assert_eq!(l1.sends().len(), 3, "one post per attempt, no re-post");
    all_pending(&h, &vids).await;
    let snap = h.snapshot().await.unwrap();
    assert_eq!((snap.pending, snap.in_flight), (3, false));
    shutdown.cancel();
    drop(h);
    drop(node);

    l1.fault_proofs(None);
    let db = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let node2 = start_l1_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        l1.clone(),
        prover.clone(),
        3,
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    wait_long("settled after the restart", async || {
        all_settled(&h2, &vids).await
    })
    .await;
    assert_eq!(prover.calls(), 4);
    assert_eq!(s.chain.voters(), 3);
    shutdown2.cancel();
    miner.abort();
}

/// A landing resets the count: two failures, a landing, then three more
/// failures before the stop.
#[tokio::test(start_paused = true)]
async fn l1_a_landing_resets_the_settle_failure_count() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    l1.fault_proofs(Some(ProofFault::Data));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 1600).await;
    // The third attempt is proved; its blobs are not mined yet.
    wait_long("third attempt", async || prover.calls() == 3).await;
    l1.fault_proofs(None);
    wait_long("settled", async || all_settled(&h, &vids).await).await;
    l1.fault_proofs(Some(ProofFault::Data));
    let more = submit(&h, &s, 3..6, 1700).await;
    wait_long("three more attempts", async || prover.calls() == 6).await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(prover.calls(), 6, "three failures after the landing");
    all_pending(&h, &more).await;
    shutdown.cancel();
    miner.abort();
}
