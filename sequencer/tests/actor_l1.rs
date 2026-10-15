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

/// The R18 setup: 140 settled 16-field votes in one-blob transactions, then
/// X and Y with disjoint 70-slot histories; returns the restarted node (L1
/// lag 110 s, no margin, the 150 s floor, `extra` flags) and its prover,
/// which fails its first batch.
async fn proof_first_setup(
    s: &TestSetup,
    l1: &FakeL1,
    dir: &TempDir,
    extra: &[&str],
) -> (
    Node,
    ActorHandle,
    Arc<FakeProver>,
    [VerifiedVote; 2],
    CancellationToken,
) {
    let shutdown = CancellationToken::new();
    let cfg = chunked_cfg(dir, s, 140, &[]);
    let (node, h) = chunked_node(s, l1, FakeProver::open(), dir, cfg, &shutdown).await;
    let first = submit(&h, s, 0..140, 4000).await;
    wait_until("first votes settled", async || {
        all_settled(&h, &first).await
    })
    .await;
    shutdown.cancel();
    drop(h);
    drop(node);
    let db = reopen_db(dir.path()).await;
    let slot = |i: usize| fake_vote(&s.env, i, &[1, 2], 1).slot;
    let xy = [
        fake_vote(&s.env, 140, &[1, 2], 4100),
        fake_vote(&s.env, 141, &[1, 2], 4101),
    ];
    for (k, range) in [0..70, 70..140].into_iter().enumerate() {
        let entry = davinci_sequencer::storage::ExposedRecord {
            slots: range.map(slot).collect(),
            vote_ids: vec![xy[k].pkg.vote_id],
        };
        db.put_coverage(&pid_fr(), &[k as u8 + 1; 32], &entry)
            .unwrap();
    }
    let shutdown = CancellationToken::new();
    let mut args = vec!["--l1-settle-lag", "110s"];
    args.extend_from_slice(extra);
    let cfg = chunked_cfg(dir, s, 140, &args);
    let prover = FakeProver::open();
    *prover.fail_batches_permanent.lock().unwrap() = 1;
    let node = start_l1_node_cfg(
        db,
        s.chain.clone(),
        l1.clone(),
        prover.clone(),
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    (node, h, prover, xy, shutdown)
}

/// R18: X failed alone once, so the retry is proof-first with a bisection
/// cap of one, and Y joins the queue. The leg is sized for the one vote the
/// retry carries (one chunk: 30 + 110 fits the 150 s floor), not for X and
/// Y together (two chunks would need 30 + 134), so X settles while voting
/// is open.
#[tokio::test]
async fn l1_a_proof_first_retry_sizes_its_leg_for_the_votes_it_carries() {
    let s = setup(16, 150, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let (node, h, prover, [x, y], shutdown) =
        proof_first_setup(&s, &l1, &dir, &["--batch-time", "0s"]).await;
    let (xid, yid) = (x.pkg.vote_id, y.pkg.vote_id);
    h.submit(x).await.unwrap();
    wait_until("X failed once", async || {
        prover.calls() == 1 && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    assert_eq!(vote_status(&h, xid).await, Some(VoteStatus::Pending));
    h.submit(y).await.unwrap();
    wait_until("X settled", async || all_settled(&h, &[xid]).await).await;
    assert_eq!(
        node.db.process(&pid_fr()).unwrap().unwrap().onchain.status,
        ProcessStatus::Ready,
        "settled while voting is open"
    );
    wait_until("Y settled", async || all_settled(&h, &[yid]).await).await;
    shutdown.cancel();
}

/// A vote whose coverage history holds 126 live slots needs 127 entries: two
/// blobs. In a window that fits one blob this is a window-limited shortfall,
/// not an overflow: the vote is not errored and waits until the grace after
/// the end opens a window for two (grace raised to 180 s).
#[tokio::test]
async fn l1_a_short_window_waits_for_a_history_that_needs_more_blobs() {
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

    // Phase 2: a history entry over 126 of those slots, as a reorged-out
    // peer batch that carried it leaves it, then the vote itself.
    let db = reopen_db(dir.path()).await;
    let late = fake_vote(&s.env, 140, &[1, 2], 3700);
    let late_vid = late.pkg.vote_id;
    db.put_coverage(
        &pid_fr(),
        &[3; 32],
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
    }
    // The end opens a grace of 180 s: window 180 - 140 fits two chunks.
    s.chain.set_grace(180);
    s.chain.advance_time(DURATION + 1);
    wait_until("settled", async || all_settled(&h2, &[late_vid]).await).await;
    // The settlement after the end carries two blobs in two transactions.
    assert_eq!(s.chain.l1_last().unwrap().blocks.len(), 2);
    shutdown2.cancel();
}

/// R16: two pending votes whose coverage histories (70 live slots each,
/// disjoint) fit one blob apiece but need two together, in a window that
/// fits one blob transaction: each vote is charged only its own history,
/// so both settle in one-blob batches while voting is open.
#[tokio::test]
async fn l1_a_short_window_seals_each_vote_with_its_own_history() {
    let s = setup(16, 150, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let (node, h) = chunked_node(&s, &l1, FakeProver::open(), &dir, cfg, &shutdown).await;
    let first = submit(&h, &s, 0..140, 3800).await;
    wait_until("first votes settled", async || {
        all_settled(&h, &first).await
    })
    .await;
    shutdown.cancel();
    drop(h);
    drop(node);

    // Peer batches that carried X1 and X2 (both reorged out before they
    // reached this node's queue), as their history entries.
    let db = reopen_db(dir.path()).await;
    let slot = |i: usize| fake_vote(&s.env, i, &[1, 2], 1).slot;
    let xs = [
        fake_vote(&s.env, 140, &[1, 2], 3900),
        fake_vote(&s.env, 141, &[1, 2], 3901),
    ];
    let vids = [xs[0].pkg.vote_id, xs[1].pkg.vote_id];
    for (k, (v, range)) in [(vids[0], 0..70), (vids[1], 70..140)]
        .into_iter()
        .enumerate()
    {
        let entry = davinci_sequencer::storage::ExposedRecord {
            slots: range.map(slot).collect(),
            vote_ids: vec![v],
        };
        db.put_coverage(&pid_fr(), &[k as u8 + 1; 32], &entry)
            .unwrap();
    }
    let shutdown2 = CancellationToken::new();
    let cfg = chunked_cfg(
        &dir,
        &s,
        140,
        &[
            "--l1-settle-lag",
            "140s",
            "--batch-time",
            "100s",
            "--solo-wait",
            "100s",
        ],
    );
    let prover = FakeProver::open();
    let node2 = start_l1_node_cfg(
        db,
        s.chain.clone(),
        l1.clone(),
        prover.clone(),
        cfg,
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    for v in &xs {
        h2.submit(v.clone()).await.unwrap();
    }
    // Both queued before the timer fires.
    s.chain.advance_time(115);
    wait_until("both settled", async || all_settled(&h2, &vids).await).await;
    assert_eq!(prover.sizes(), vec![1, 1], "one vote per batch");
    assert_eq!(
        s.chain.l1_last().unwrap().blocks.len(),
        1,
        "one transaction"
    );
    assert_eq!(
        node2.db.process(&pid_fr()).unwrap().unwrap().onchain.status,
        ProcessStatus::Ready,
        "settled while voting is open"
    );
    for (v, range) in [(vids[0], 0..70), (vids[1], 70..140)] {
        let nf = s.env.cfg.ballot_mode.num_fields;
        let reseal = updated_slots(&node2.db, nf, transition_of(&node2.db, nf, v));
        assert!(
            range.map(slot).all(|s| reseal.contains(&s)),
            "a seal left out its history"
        );
    }
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

// `wait_until` over `secs` virtual seconds.
async fn wait_secs(what: &str, secs: u64, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..secs {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timeout waiting for {what}");
}

// Seconds between batch proof requests `a` and `b` (1-based).
fn gap(prover: &FakeProver, a: usize, b: usize) -> u64 {
    let t = prover.call_times.lock().unwrap();
    (t[b - 1] - t[a - 1]).as_secs()
}

// About how long one failed cycle takes here: two blocks to the reorg
// margin, PROOF_FAIL_FOR, the cooldown.
const CYCLE: u64 = 60;

/// A proof no post fixes, every time: after three failed cycles the node
/// pauses the process 10 min, then 20 after the next one. The votes stay
/// pending and `nextSealNotBefore` shows the pause end.
#[tokio::test(start_paused = true)]
async fn l1_repeated_failed_cycles_pause_sealing_and_the_pause_doubles() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    l1.fault_proofs(Some(ProofFault::Data));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 1500).await;
    wait_secs("the third cycle failed", 600, async || {
        prover.calls() == 3 && !h.snapshot().await.unwrap().in_flight
    })
    .await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(prover.calls(), 3, "paused");
    all_pending(&h, &vids).await;
    let next = h.snapshot().await.unwrap().next_seal_not_before.unwrap();
    assert!(
        next >= s.chain.time() + 200 && next <= s.chain.time() + 300,
        "nextSealNotBefore {next} shows the pause end (now {})",
        s.chain.time()
    );
    wait_secs("sealing resumes", 600, async || prover.calls() == 4).await;
    let g = gap(&prover, 3, 4);
    assert!((600 + CYCLE..1200).contains(&g), "a 10 min pause: {g} s");
    // The fourth cycle fails too: 20 min.
    wait_secs("the pause doubled", 1800, async || prover.calls() == 5).await;
    let g = gap(&prover, 4, 5);
    assert!((1200 + CYCLE..1800).contains(&g), "a 20 min pause: {g} s");
    assert_eq!(l1.sends().len(), 5, "one post per cycle, no re-post");
    wait_secs("the fifth cycle failed", 300, async || {
        !h.snapshot().await.unwrap().in_flight
    })
    .await;
    all_pending(&h, &vids).await;
    shutdown.cancel();
    miner.abort();
}

/// Our landing resets the count: two failures, a landing, then three more
/// failures before the pause.
#[tokio::test(start_paused = true)]
async fn l1_a_landing_resets_the_failed_cycle_count() {
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
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(prover.calls(), 6, "three failures after the landing");
    all_pending(&h, &more).await;
    shutdown.cancel();
    miner.abort();
}

// Nodes A and B on one chain, each posting from its own Ethereum account.
async fn two_l1_nodes(
    s: &TestSetup,
    l1: &FakeL1,
    provers: (Arc<FakeProver>, Arc<FakeProver>),
    dirs: &(TempDir, TempDir),
    shutdown: &CancellationToken,
) -> (Node, Node) {
    let a = start_l1_node(
        Db::open_in(dirs.0.path()).unwrap(),
        dirs.0.path(),
        s,
        s.chain.with_signer(0xa1),
        l1.clone(),
        provers.0,
        3,
        shutdown.clone(),
    )
    .await;
    let b = start_l1_node(
        Db::open_in(dirs.1.path()).unwrap(),
        dirs.1.path(),
        s,
        s.chain.with_signer(0xb2),
        l1.with_sender(0x2b),
        provers.1,
        3,
        shutdown.clone(),
    )
    .await;
    (a, b)
}

/// Another sequencer's landing ends the pause at once but keeps the streak
/// (less one): the next failed cycle pauses again.
#[tokio::test(start_paused = true)]
async fn l1_a_foreign_landing_ends_the_pause_but_keeps_the_streak() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    let dirs = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let shutdown = CancellationToken::new();
    let (prover_a, prover_b) = (FakeProver::open(), FakeProver::open());
    // A's proofs fail after its posts, four times.
    *prover_a.fail_batches.lock().unwrap() = 4;
    let (node_a, node_b) = two_l1_nodes(
        &s,
        &l1,
        (prover_a.clone(), prover_b.clone()),
        &dirs,
        &shutdown,
    )
    .await;
    let (ha, hb) = (handle(&node_a).await, handle(&node_b).await);
    let vids_a = submit(&ha, &s, 0..3, 30).await;
    wait_secs("A paused", 600, async || {
        prover_a.calls() == 3 && !ha.snapshot().await.unwrap().in_flight
    })
    .await;
    let vids_b = submit(&hb, &s, 3..6, 40).await;
    wait_secs("B settled", 120, async || all_settled(&hb, &vids_b).await).await;
    // Well inside A's 10 min pause: A seals again, and fails a fourth time.
    wait_secs("A sealed after syncing B", 120, async || {
        prover_a.calls() == 4 && !ha.snapshot().await.unwrap().in_flight
    })
    .await;
    assert_eq!(node_a.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    // Streak 3 − 1 + 1 = 3: paused again.
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(prover_a.calls(), 4, "paused again");
    all_pending(&ha, &vids_a).await;
    shutdown.cancel();
    miner.abort();
}

/// Foreign landings between our failed cycles do not hold the pause off:
/// two failures, a foreign landing, two more failures reach the streak of 3.
#[tokio::test(start_paused = true)]
async fn l1_foreign_landings_between_failed_cycles_still_pause() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    let dirs = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let shutdown = CancellationToken::new();
    // A's proofs fail after its posts, each when the test releases it.
    let (prover_a, prover_b) = (FakeProver::gated(), FakeProver::open());
    *prover_a.fail_batches.lock().unwrap() = 100;
    let (node_a, node_b) = two_l1_nodes(
        &s,
        &l1,
        (prover_a.clone(), prover_b.clone()),
        &dirs,
        &shutdown,
    )
    .await;
    let (ha, hb) = (handle(&node_a).await, handle(&node_b).await);
    let vids_a = submit(&ha, &s, 0..3, 50).await;
    for k in 1..=2 {
        wait_secs("A proving", 120, async || prover_a.calls() == k).await;
        prover_a.release(1);
    }
    // A's third attempt is in flight when B lands: A loses it and syncs.
    wait_secs("A proving again", 120, async || prover_a.calls() == 3).await;
    let vids_b = submit(&hb, &s, 3..6, 60).await;
    wait_secs("A synced B", 120, async || {
        node_a.metrics.synced_from_others.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(all_settled(&hb, &vids_b).await);
    for k in 4..=5 {
        wait_secs("A proving", 120, async || prover_a.calls() == k).await;
        prover_a.release(1);
    }
    wait_secs("A's fifth attempt failed", 120, async || {
        !ha.snapshot().await.unwrap().in_flight
    })
    .await;
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(prover_a.calls(), 5, "paused");
    all_pending(&ha, &vids_a).await;
    shutdown.cancel();
    miner.abort();
}

/// A proof that fails after the blobs went out counts: the cycle posted.
#[tokio::test(start_paused = true)]
async fn l1_a_proof_failure_after_posting_counts() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_batches.lock().unwrap() = 100;
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 1800).await;
    wait_secs("three attempts", 300, async || prover.calls() == 3).await;
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(prover.calls(), 3, "paused after three posted failures");
    assert_eq!(l1.sends().len(), 3);
    all_pending(&h, &vids).await;
    shutdown.cancel();
    miner.abort();
}

/// The END events arrive before a head shows the END block's time: the
/// first flush is sized by the 600 s grace (all 15 votes, one blob), not
/// by the open window's now + 150 s floor (12).
#[tokio::test]
async fn l1_first_flush_after_an_end_uses_the_grace() {
    let s = setup(2, 16, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let cfg = test_config_with(
        dir.path(),
        &s.census_dir,
        20,
        "0s",
        &["--batch-time", "1000000s", "--solo-wait", "3000000s"],
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
    s.chain.set_grace(600);
    let vids = submit(&h, &s, 0..15, 1900).await;
    slow(&node);
    s.chain.end_ahead_of_head(5);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![15]);
    assert_eq!(l1.mined_posts(), 1);
    shutdown.cancel();
}

/// A settlement that reverts with a name the node does not handle errors
/// its votes, but the cycle posted blobs: three such cycles pause sealing.
#[tokio::test(start_paused = true)]
async fn l1_reverted_posted_cycles_count() {
    let s = setup(2, 16, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    s.chain.revert_simulates("UnhandledRevert", 100);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    for (k, voters) in [0..3, 3..6, 6..9].into_iter().enumerate() {
        let vids = submit(&h, &s, voters, 2000 + 10 * k as u64).await;
        wait_secs("the batch reverted", 300, async || {
            for vid in &vids {
                if vote_status(&h, *vid).await != Some(VoteStatus::Error) {
                    return false;
                }
            }
            true
        })
        .await;
    }
    let fourth = submit(&h, &s, 9..12, 2100).await;
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(prover.calls(), 3, "the fourth batch waits for the pause");
    all_pending(&h, &fourth).await;
    shutdown.cancel();
    miner.abort();
}

/// A prover that fails every batch holding vote X, in L1-blob mode: the
/// first failure posted its blobs (one failed cycle); the bisection cycles
/// are proof-first, so the failing ones post nothing and the streak never
/// reaches the pause. X alone is errored, the rest land, and so does the
/// next vote.
#[tokio::test(start_paused = true)]
async fn l1_bisection_cycles_post_only_after_the_proof() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let x = fake_vote(&s.env, 3, &[1, 2], 2203).pkg.vote_id;
    *prover.fail_vid.lock().unwrap() = Some(x);
    let node = start_l1_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        l1.clone(),
        prover.clone(),
        4,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let vids = submit(&h, &s, 0..4, 2200).await;
    assert_eq!(vids[3], x);
    // Well inside the 10 min pause three posted failures would trigger.
    wait_secs("X errored, the rest settled", 400, async || {
        vote_status(&h, x).await == Some(VoteStatus::Error) && all_settled(&h, &vids[..3]).await
    })
    .await;
    assert_eq!(prover.sizes(), vec![4, 4, 2, 2, 1, 1]);
    // The first failure and the two landings ([0, 1], [2]).
    assert_eq!(l1.sends().len(), 3, "failed bisection cycles posted");
    assert_eq!(node.metrics.bisections.load(Ordering::SeqCst), 4);
    let next = submit(&h, &s, 4..5, 2300).await;
    wait_secs("the next vote landed", 200, async || {
        all_settled(&h, &next).await
    })
    .await;
    assert_eq!(l1.sends().len(), 4);
    assert_eq!(s.chain.voters(), 4);
    shutdown.cancel();
    miner.abort();
}

/// A proof-first retry needs chunks for the exposed set the failed attempt
/// left: 130 live overwritten slots take two one-blob transactions. The
/// 150 s window leaves one chunk after a full-size proof, yet the retry
/// must still seal (fewer votes, both chunks), not wait for a wider window.
#[tokio::test]
async fn l1_proof_first_retry_is_sized_for_its_exposed_set() {
    let s = setup(16, 130, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    // 150 − 30 (base) − 93 (lag) − 24 (second chunk) leaves 3 s to prove.
    let cfg = chunked_cfg(&dir, &s, 130, &["--l1-settle-lag", "93s"]);
    let (_node, h) = chunked_node(&s, &l1, prover.clone(), &dir, cfg, &shutdown).await;
    let first = submit(&h, &s, 0..130, 4000).await;
    wait_until("first votes settled", async || {
        all_settled(&h, &first).await
    })
    .await;
    // Overwrites of every slot; one of them fails in the prover.
    *prover.fail_vid.lock().unwrap() = Some(fake_vote(&s.env, 129, &[1, 2], 5129).pkg.vote_id);
    submit(&h, &s, 0..130, 5000).await;
    wait_until("the failed attempt and its retry", async || {
        prover.calls() >= 3
    })
    .await;
    let sizes = prover.sizes();
    assert_eq!(sizes[..2], [130, 130]);
    assert!(
        sizes[2] < 130,
        "the retry is sized for both chunks: {sizes:?}"
    );
    shutdown.cancel();
}

/// A job task that panics after its blobs were mined still counts a failed
/// cycle: three of them pause sealing, as three posted failures do.
#[tokio::test(start_paused = true)]
async fn l1_a_job_panicking_after_posting_counts() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let miner = mine_every_12s(&l1);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    *prover.panic_batches.lock().unwrap() = 100;
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 1900).await;
    for k in 1..=3u64 {
        wait_secs("proving", 600, async || prover.calls() == k).await;
        wait_secs("posted", 600, async || l1.sends().len() as u64 == k).await;
        // Mined, then the job dies.
        tokio::time::sleep(Duration::from_secs(30)).await;
        prover.release(1);
    }
    wait_secs("the third cycle ended", 600, async || {
        !h.snapshot().await.unwrap().in_flight
    })
    .await;
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(prover.calls(), 3, "paused after three posted failures");
    all_pending(&h, &vids).await;
    shutdown.cancel();
    miner.abort();
}
