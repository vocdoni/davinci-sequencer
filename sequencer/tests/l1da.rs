//! The L1-blob poster and settle loop on the scripted Ethereum (`FakeL1`)
//! and registry (`FakeChain`).

mod common;

use common::*;
use davinci_sequencer::l1da::{
    FeeGate, GONE_FOR, L1Da, L1Job, MAX_PROOF_RETRIES, MAX_REPOSTS, MIN_SETTLE_GAS, PROOF_FAIL_FOR,
    PostError, Posted, Poster, Posting, Settled, chunks, settle, with_post,
};
use davinci_sequencer::storage::L1PostRecord;
use davinci_sequencer::web3::L1Chain;
use davinci_sequencer::web3::L1Receipt;
use davinci_sequencer::web3::L1SendError;
use davinci_sequencer::web3::SignedBlobTx;
use davinci_sequencer::web3::l1proof::{L1BlobProof, verify_local};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{TransitionData, blob_count, build_blobs};
use davinci_zkvm_sdk::limits::{TX_BLOB_CAP, VOTE_ID_MIN};

const PID: [u8; 31] = [7; 31];
const ROOT: [u8; 32] = [1; 32];

fn blobs(seed: u8) -> TransitionBlobs {
    let t = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + u64::from(seed)],
        updates: vec![],
        num_fields: 1,
    };
    build_blobs(&t, &Fr::from(u64::from(seed)), &[seed; 32]).unwrap()
}

/// A transition of `n` blobs (2 or more): 128 (n - 1) 16-field updates.
fn wide_blobs(n: usize) -> TransitionBlobs {
    let u = 128 * (n - 1);
    assert_eq!(blob_count(0, u, 16), n);
    let t = TransitionData {
        vote_ids: vec![],
        updates: (0..u as u64)
            .map(|k| (0x10 + k, Ballot::identity()))
            .collect(),
        num_fields: 16,
    };
    build_blobs(&t, &Fr::from(n as u64), &[n as u8; 32]).unwrap()
}

fn poster(l1: &FakeL1, db: &Db, cap: u128) -> Poster {
    Poster::new(
        Arc::new(l1.clone()),
        db.clone(),
        cap,
        Duration::from_millis(5),
    )
    .unwrap()
}

fn soon() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(20)
}

// For the paused-time tests that wait out GONE_FOR or PROOF_FAIL_FOR.
fn later() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(600)
}

fn fresh_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    (dir, db)
}

#[tokio::test]
async fn posts_are_serial_with_consecutive_nonces() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let p = poster(&l1, &db, u128::MAX);
    let (b1, b2) = (blobs(1), blobs(2));
    let (a, b) = tokio::join!(
        p.post(&PID, &ROOT, &b1, soon()),
        p.post(&PID, &ROOT, &b2, soon())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut nonces = [a.nonce, b.nonce];
    nonces.sort();
    assert_eq!(nonces, [0, 1]);
    let s = l1.sends();
    assert_eq!(s.len(), 2);
    // The second went out only once the first was mined.
    let first = if a.nonce == 0 { a } else { b };
    assert!(s[1].head >= first.block, "{s:?}");
    assert_eq!(l1.mined_posts(), 2);
    assert!(db.l1_posts().unwrap().is_empty());
}

#[tokio::test]
async fn the_fee_cap_waits_for_cheaper_fees() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    // 1,000 gwei base fee; the cap allows about 50 gwei.
    l1.set_market(L1Market {
        base_fee: 1_000_000_000_000,
        tip: 0,
        blob_base_fee: 1,
    });
    let p = poster(&l1, &db, 21_000 * 100_000_000_000);
    let e = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap_err();
    assert!(matches!(e, PostError::FeeCap { .. }), "{e}");
    assert!(l1.sends().is_empty(), "nothing sent above the cap");
    l1.set_market(L1Market {
        base_fee: 1_000_000_000,
        tip: 0,
        blob_base_fee: 1,
    });
    p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap();
}

#[tokio::test]
async fn a_replacement_above_the_cap_waits_at_the_current_price() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    l1.hold(true);
    let first = L1Fees::offer(&l1.market());
    // One doubling fits, a second does not.
    let p = poster(&l1, &db, first.cost(1) * 3);
    let b = blobs(1);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let e = p.post(&PID, &ROOT, &b, deadline).await.unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    let s = l1.sends();
    assert_eq!(s.len(), 2, "the post and one doubled replacement: {s:?}");
    assert_eq!(s[0].nonce, s[1].nonce);
    assert_eq!(s[1].fees, first.doubled());
    // The record keeps the price on the pool, for the next post.
    let rec = db.l1_posts().unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].fees, s[1].fees);
}

#[tokio::test]
async fn a_dropped_post_is_replaced_at_its_nonce() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    l1.drop_next(1);
    let p = poster(&l1, &db, u128::MAX);
    let posted = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap();
    let s = l1.sends();
    assert_eq!(s.len(), 2);
    assert_eq!((s[0].nonce, s[1].nonce, posted.nonce), (0, 0, 0));
    assert_eq!(s[1].fees, s[0].fees.doubled());
    assert!(s[1].head >= s[0].head + davinci_sequencer::l1da::REPLACE_AFTER);
    assert_eq!(posted.tx, s[1].hash);
    assert_eq!(l1.mined_posts(), 1);
}

#[tokio::test]
async fn an_unrecorded_orphan_is_outbid() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let orphan_fees = L1Fees::offer(&l1.market()).doubled().doubled();
    let orphan = l1.inject_orphan(0, orphan_fees);
    let p = poster(&l1, &db, u128::MAX);
    let posted = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap();
    assert_eq!(posted.nonce, 0);
    let s = l1.sends();
    assert_eq!(
        s.len(),
        4,
        "refused until every fee doubles the orphan's: {s:?}"
    );
    let won = s.last().unwrap();
    assert!(won.fees.max_fee >= 2 * orphan_fees.max_fee);
    assert!(won.fees.tip >= 2 * orphan_fees.tip);
    assert!(won.fees.blob_fee >= 2 * orphan_fees.blob_fee);
    assert_eq!(l1.block_of(&orphan), None);
    assert_eq!(l1.mined_posts(), 1);
}

#[tokio::test]
async fn a_restart_replaces_the_recorded_post() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    // No blocks: the post stays pending.
    l1.set_auto_mine(false);
    let p1 = poster(&l1, &db, u128::MAX);
    let b = blobs(1);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    let e = p1.post(&PID, &ROOT, &b, deadline).await.unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    let rec = db.l1_posts().unwrap().pop().unwrap();
    drop(p1);
    // A new poster on the same database: the first post replaces it.
    l1.set_auto_mine(true);
    let p2 = poster(&l1, &db, u128::MAX);
    let posted = p2.post(&PID, &ROOT, &blobs(2), soon()).await.unwrap();
    assert_eq!(posted.nonce, rec.nonce);
    let last = l1.sends().pop().unwrap();
    assert_eq!(last.nonce, rec.nonce);
    assert!(last.fees.max_fee >= 2 * rec.fees.max_fee);
    assert!(last.fees.tip >= 2 * rec.fees.tip);
    assert!(last.fees.blob_fee >= 2 * rec.fees.blob_fee);
    assert_eq!(l1.mined_posts(), 1);
    assert!(db.l1_posts().unwrap().is_empty());
}

#[tokio::test]
async fn no_key_no_post() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let p = poster(&l1.observer(), &db, u128::MAX);
    let e = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap_err();
    assert!(matches!(e, PostError::NoSigner), "{e}");
}

#[tokio::test]
async fn a_record_of_another_sender_is_ignored_and_dropped() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    // A pending post of an older L1 key, at a nonce the new account also needs.
    l1.set_auto_mine(false);
    let old = poster(&l1.with_sender(0x2b), &db, u128::MAX);
    let b = blobs(1);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let e = old.post(&PID, &ROOT, &b, deadline).await.unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    assert_eq!(db.l1_posts().unwrap().len(), 1);
    drop(old);
    l1.set_auto_mine(true);
    let p = poster(&l1, &db, u128::MAX);
    assert!(
        db.l1_posts().unwrap().is_empty(),
        "the foreign record is dropped"
    );
    let before = l1.sends().len();
    let posted = p.post(&PID, &ROOT, &blobs(2), soon()).await.unwrap();
    assert_eq!(posted.nonce, 0);
    // Priced from the market, not from the old account's record.
    let s = l1.sends();
    assert_eq!(s[before].fees, L1Fees::offer(&l1.market()));
    assert_eq!(s.len(), before + 1);
}

#[tokio::test]
async fn a_refused_send_does_not_price_the_next_one() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let p = poster(&l1, &db, u128::MAX);
    // Refused at the door (a 413 or 429 of a public endpoint), twice.
    l1.refuse_next(2);
    for _ in 0..2 {
        let e = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap_err();
        assert!(matches!(e, PostError::L1(_)), "{e}");
        assert!(
            db.l1_posts().unwrap().is_empty(),
            "no record of a refused send"
        );
    }
    let posted = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap();
    assert_eq!(posted.nonce, 0);
    let s = l1.sends();
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(
        s[0].fees,
        L1Fees::offer(&l1.market()),
        "priced from the market"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_replacement_keeps_the_pooled_record() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    // Pooled, never mined: after 3 blocks the post is replaced.
    l1.hold(true);
    let p = poster(&l1, &db, u128::MAX);
    let b = blobs(1);
    let (e, ()) = tokio::join!(p.post(&PID, &ROOT, &b, later()), async {
        wait_until("first send", async || l1.sends().len() == 1).await;
        l1.refuse_next(1);
    });
    assert!(matches!(e, Err(PostError::L1(_))), "{e:?}");
    let first = l1.sends()[0].clone();
    // The record is the pooled transaction's, not the refused replacement's.
    let rec = db.l1_posts().unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!((rec[0].tx_hash, rec[0].fees), (first.hash, first.fees));
    l1.hold(false);
    let posted = p.post(&PID, &ROOT, &b, later()).await.unwrap();
    assert_eq!(posted.nonce, first.nonce);
    let s = l1.sends();
    assert_eq!(s.len(), 2, "{s:?}");
    assert_eq!(
        s[1].fees,
        first.fees.doubled(),
        "one doubling over the pool"
    );
}

#[tokio::test]
async fn an_observer_start_keeps_the_records() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    // No blocks: the post stays pending.
    l1.set_auto_mine(false);
    let p1 = poster(&l1, &db, u128::MAX);
    let b = blobs(1);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let e = p1.post(&PID, &ROOT, &b, deadline).await.unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    drop(p1);
    let rec = db.l1_posts().unwrap();
    assert_eq!(rec.len(), 1);
    // A start without the L1 key leaves the record alone...
    drop(poster(&l1.observer(), &db, u128::MAX));
    assert_eq!(db.l1_posts().unwrap(), rec);
    // ...for the next start with the key, which replaces its transaction.
    l1.set_auto_mine(true);
    let p = poster(&l1, &db, u128::MAX);
    let posted = p.post(&PID, &ROOT, &blobs(2), soon()).await.unwrap();
    assert_eq!(posted.nonce, rec[0].nonce);
    let last = l1.sends().pop().unwrap();
    assert_eq!(last.fees, rec[0].fees.doubled());
    assert_eq!(l1.mined_posts(), 1);
}

// A pending post of an earlier run, recorded at the market price, under a
// cap that fits that price but not its doubling.
async fn pending_record_under_a_tight_cap(l1: &FakeL1, db: &Db) -> L1PostRecord {
    let cap = L1Fees::offer(&l1.market()).cost(1) * 3 / 2;
    l1.set_auto_mine(false);
    let p = poster(l1, db, cap);
    let b = blobs(1);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let e = p.post(&PID, &ROOT, &b, deadline).await.unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    l1.set_auto_mine(true);
    db.l1_posts().unwrap().pop().unwrap()
}

#[tokio::test]
async fn a_pending_post_the_l1_node_forgot_does_not_price_the_next() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let rec = pending_record_under_a_tight_cap(&l1, &db).await;
    // Evicted: nothing of ours holds nonce 0 any more.
    l1.forget(rec.tx_hash);
    let cap = L1Fees::offer(&l1.market()).cost(1) * 3 / 2;
    let p = poster(&l1, &db, cap);
    let posted = p.post(&PID, &ROOT, &blobs(2), soon()).await.unwrap();
    assert_eq!(posted.nonce, rec.nonce);
    let s = l1.sends();
    assert_eq!(s.len(), 2, "{s:?}");
    assert_eq!(s[1].fees, L1Fees::offer(&l1.market()), "the market price");
    assert_eq!(l1.block_of(&rec.tx_hash), None);
    assert_eq!(l1.mined_posts(), 1);
    assert!(db.l1_posts().unwrap().is_empty());
}

#[tokio::test]
async fn a_pending_post_still_known_keeps_the_fee_cap() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let rec = pending_record_under_a_tight_cap(&l1, &db).await;
    let cap = L1Fees::offer(&l1.market()).cost(1) * 3 / 2;
    let p = poster(&l1, &db, cap);
    let e = p.post(&PID, &ROOT, &blobs(2), soon()).await.unwrap_err();
    assert!(matches!(e, PostError::FeeCap { .. }), "{e}");
    assert_eq!(l1.sends().len(), 1, "nothing sent past the cap");
    assert_eq!(db.l1_posts().unwrap(), vec![rec]);
}

#[tokio::test(start_paused = true)]
async fn a_lagging_receipt_does_not_free_the_nonce() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let lag = Arc::new(Lagging::new(&l1));
    let p = Poster::new(lag.clone(), db.clone(), u128::MAX, Duration::from_secs(1)).unwrap();
    // The nonce count moves past ours while our receipt reads as unknown.
    lag.lag_for(Duration::from_secs(20));
    let posted = p.post(&PID, &ROOT, &blobs(1), later()).await.unwrap();
    let s = l1.sends();
    assert_eq!(s.len(), 1, "no second post at the next nonce: {s:?}");
    assert_eq!((posted.nonce, posted.tx), (0, s[0].hash));
}

/// FakeChain in L1 mode over a FakeL1, a poster on a fresh database, and a
/// one-vote batch on the chain's root with the snark FakeProver returns.
struct Rig {
    s: TestSetup,
    l1: FakeL1,
    da: L1Da,
    db: Db,
    snark: PlonkSnark,
    blobs: TransitionBlobs,
    _dir: TempDir,
}

fn rig() -> Rig {
    rig_with(Duration::from_millis(5))
}

// `poll` paces the poster and the settle loop; FakeL1 mines a block per
// poll, so 12 s is Ethereum's pace.
fn rig_with(poll: Duration) -> Rig {
    rig_of(poll, blobs(9), TX_BLOB_CAP)
}

/// A transition of `n` blobs, posted in chunks of `chunk`.
fn rig_chunked(poll: Duration, n: usize, chunk: usize) -> Rig {
    rig_of(poll, wide_blobs(n), chunk)
}

fn rig_of(poll: Duration, blobs: TransitionBlobs, chunk: usize) -> Rig {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.attach_l1(&l1);
    let (dir, db) = fresh_db();
    let da = L1Da::new(Arc::new(l1.clone()), db.clone(), u128::MAX, poll, chunk).unwrap();
    let p = BatchPublics {
        ok: true,
        fail_mask: 0,
        root_before: s.chain.root(),
        root_after: [0x42; 32],
        voters: 1,
        overwrites: 0,
        census_root: [0; 32],
        blobs_digest: blobs.digest,
        n_blobs: blobs.blobs.len() as u32,
        occupied_before: 0,
        nproofs: 1,
        n_public: 3,
        log_n: 0,
    };
    let snark = PlonkSnark {
        program_vk: release::BATCH_PROGRAM_VK,
        root_c_vadcop_final: release::ROOT_C_VADCOP_FINAL,
        public_values: encode_batch(&p),
        proof_bytes: vec![0; 768],
    };
    Rig {
        s,
        l1,
        da,
        db,
        snark,
        blobs,
        _dir: dir,
    }
}

impl Rig {
    fn job(&self) -> L1Job<'_> {
        L1Job {
            pid: pid31(),
            root_before: self.s.chain.root(),
            blobs: &self.blobs,
            deadline: soon(),
            cancel: CancellationToken::new(),
            posted: Default::default(),
        }
    }

    /// Every chunk posted, the proof already done (nothing recorded).
    async fn posting(&self, job: &L1Job<'_>) -> Posting {
        with_post(&self.da, &self.s.chain, job, std::future::ready(()))
            .await
            .1
    }

    fn vhs(&self) -> Vec<B256> {
        self.blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect()
    }
}

#[tokio::test]
async fn settle_lands_with_the_l1_blocks() {
    let r = rig();
    let job = r.job();
    let posting = r.posting(&job).await;
    let posted = posting.posts[0];
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.s.chain.root(), [0x42; 32]);
    assert_eq!(
        r.s.chain.l1_last().unwrap().blocks,
        vec![posted.block; r.blobs.blobs.len()]
    );
    assert!(r.s.chain.l1_last_headers() <= 3);
    assert_eq!(r.s.chain.l1_records(), 0);
}

#[tokio::test]
async fn settle_gives_up_after_bounded_origin_mismatches() {
    let r = rig();
    r.s.chain.revert_simulates("L1OriginMismatch", 100);
    let job = r.job();
    let posting = r.posting(&job).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("L1OriginMismatch")),
        "{out:?}"
    );
    assert_eq!(r.s.chain.l1_simulates(), u64::from(MAX_PROOF_RETRIES) + 1);
    assert_eq!(r.l1.mined_posts(), 1, "a mismatch never re-posts");
}

#[tokio::test]
async fn a_recorded_origin_anchors_a_deep_proof() {
    let r = rig();
    let job = r.job();
    let posting = r.posting(&job).await;
    let n = posting.posts[0].block;
    r.l1.pin_origin(Some(n));
    let rec = r.s.chain.record_l1_origin().await.unwrap();
    assert_eq!(rec.number, n);
    r.l1.mine(30);
    r.l1.pin_origin(Some(n + 30));
    let posting = Posting {
        recorded: Some(rec),
        ..posting
    };
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.s.chain.l1_last_headers(), 1, "headers n..=n, not 33");
    assert_eq!(r.l1.mined_posts(), 1);
}

#[tokio::test]
async fn a_proof_too_deep_without_a_record_reposts() {
    let r = rig();
    let job = r.job();
    let posting = r.posting(&job).await;
    let n = posting.posts[0].block;
    r.l1.mine(80);
    r.l1.pin_origin(Some(n + 70));
    let driver = async {
        wait_until("re-posted", async || r.l1.mined_posts() == 2).await;
        r.l1.pin_origin(None);
    };
    let (out, ()) = tokio::join!(settle(&r.da, &r.s.chain, &job, &r.snark, posting), driver);
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let second = r.l1.sends().last().unwrap().hash;
    let m = r.l1.block_of(&second).unwrap();
    assert!(m > n + 70);
    assert_eq!(r.s.chain.l1_last().unwrap().blocks[0], m);
}

#[tokio::test]
async fn tx_too_old_reposts() {
    let r = rig();
    r.s.chain.revert_simulates("TxTooOld", 1);
    let job = r.job();
    let posting = r.posting(&job).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.mined_posts(), 2);
}

/// An Ethereum whose receipt reads come back empty for a while (a lagging
/// load-balanced endpoint).
struct Lagging {
    inner: FakeL1,
    until: Mutex<Option<tokio::time::Instant>>,
}

impl Lagging {
    fn new(l1: &FakeL1) -> Self {
        Lagging {
            inner: l1.clone(),
            until: Mutex::new(None),
        }
    }

    fn lag_for(&self, d: Duration) {
        *self.until.lock().unwrap() = Some(tokio::time::Instant::now() + d);
    }
}

#[async_trait]
impl L1Chain for Lagging {
    fn sender(&self) -> Option<Address> {
        self.inner.sender()
    }
    async fn head(&self) -> Result<u64, Web3Error> {
        self.inner.head().await
    }
    async fn nonce(&self) -> Result<u64, Web3Error> {
        self.inner.nonce().await
    }
    async fn fees(&self) -> Result<L1Market, Web3Error> {
        self.inner.fees().await
    }
    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx, Web3Error> {
        self.inner.sign_blob_tx(nonce, fees, blobs).await
    }
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> Result<(), L1SendError> {
        self.inner.send_blob_tx(tx).await
    }
    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>, Web3Error> {
        let until = *self.until.lock().unwrap();
        if until.is_some_and(|t| tokio::time::Instant::now() < t) {
            return Ok(None);
        }
        self.inner.receipt(tx).await
    }
    async fn tx_known(&self, tx: B256) -> Result<bool, Web3Error> {
        self.inner.tx_known(tx).await
    }
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        self.inner.build_proof(txs, top).await
    }
}

#[tokio::test(start_paused = true)]
async fn a_lagging_receipt_does_not_repost() {
    let poll = Duration::from_secs(1);
    let mut r = rig_with(poll);
    // Poster and settle loop share the lagging view.
    let lag = Arc::new(Lagging::new(&r.l1));
    let (dir, db) = fresh_db();
    r.da = L1Da::new(lag.clone(), db.clone(), u128::MAX, poll, TX_BLOB_CAP).unwrap();
    (r.db, r._dir) = (db, dir);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    lag.lag_for(GONE_FOR - 2 * poll);
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(
        r.l1.mined_posts(),
        1,
        "a receipt read as null for less than GONE_FOR is not gone"
    );
}

#[tokio::test(start_paused = true)]
async fn a_vanished_post_is_gone_after_gone_for() {
    let r = rig_with(Duration::from_secs(1));
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    // An Ethereum reorg drops the blob transaction's block.
    r.l1.reorg(1);
    assert_eq!(r.l1.mined_posts(), 0);
    let early = async {
        tokio::time::sleep(GONE_FOR - Duration::from_secs(1)).await;
        assert_eq!(r.l1.sends().len(), 1, "posted again before GONE_FOR");
    };
    let (out, ()) = tokio::join!(settle(&r.da, &r.s.chain, &job, &r.snark, posting), early);
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.sends().len(), 2);
    assert_eq!(r.l1.mined_posts(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_proof_failing_past_proof_fail_for_reposts_once() {
    let r = rig_with(Duration::from_secs(12));
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    // Proofs fail a little longer than PROOF_FAIL_FOR.
    r.l1.fail_proofs_for(PROOF_FAIL_FOR + Duration::from_secs(5));
    let early = async {
        tokio::time::sleep(PROOF_FAIL_FOR - Duration::from_secs(1)).await;
        assert_eq!(r.l1.sends().len(), 1, "posted again before PROOF_FAIL_FOR");
    };
    let (out, ()) = tokio::join!(settle(&r.da, &r.s.chain, &job, &r.snark, posting), early);
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 2, "one re-post: {s:?}");
    assert_eq!(r.l1.mined_posts(), 2);
    assert_eq!(
        r.s.chain.l1_last().unwrap().blocks[0],
        r.l1.block_of(&s[1].hash).unwrap(),
        "settled with the second post"
    );
}

#[tokio::test]
async fn reposts_are_bounded() {
    let r = rig();
    r.s.chain.revert_simulates("TxTooOld", 100);
    let job = r.job();
    let posting = r.posting(&job).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("re-posts")),
        "{out:?}"
    );
    assert_eq!(
        r.l1.mined_posts(),
        1 + MAX_REPOSTS as usize,
        "the fourth re-post ends the settle"
    );
}

// A proof no fresh post can fix (a header this build cannot encode, a
// header chain that does not link): retried for PROOF_FAIL_FOR without
// posting, then the settle ends.
async fn a_faulty_proof_does_not_repost(fault: ProofFault) {
    let r = rig_chunked(Duration::from_secs(12), 3, 2);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    assert_eq!(posting.posts.len(), 2);
    r.l1.fault_proofs(Some(fault));
    let t0 = tokio::time::Instant::now();
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("no new post can fix")),
        "{fault:?}: {out:?}"
    );
    assert!(t0.elapsed() >= PROOF_FAIL_FOR, "retried first");
    assert!(t0.elapsed() < PROOF_FAIL_FOR + Duration::from_secs(60));
    assert_eq!(r.l1.sends().len(), 2, "{fault:?}: no re-post");
    assert_eq!(r.l1.mined_posts(), 2);
    assert_eq!(r.s.chain.l1_simulates(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_proof_data_error_does_not_repost() {
    a_faulty_proof_does_not_repost(ProofFault::Data).await;
}

#[tokio::test(start_paused = true)]
async fn a_broken_header_chain_does_not_repost() {
    a_faulty_proof_does_not_repost(ProofFault::BrokenChain).await;
}

// A chain that stops linking for a few polls (a reorg between header
// reads) lands once it links again, with the same posts.
#[tokio::test(start_paused = true)]
async fn a_header_chain_broken_for_a_while_lands_without_reposting() {
    let r = rig_with(Duration::from_secs(12));
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    r.l1.fault_proofs(Some(ProofFault::BrokenChain));
    let heal = async {
        tokio::time::sleep(PROOF_FAIL_FOR / 2).await;
        r.l1.fault_proofs(None);
    };
    let (out, ()) = tokio::join!(settle(&r.da, &r.s.chain, &job, &r.snark, posting), heal);
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.sends().len(), 1);
}

// ---------------------------------------------------------------- chunks

#[tokio::test]
async fn a_chunked_transition_lands_with_every_transaction_in_order() {
    // Three blobs in chunks of two: blobs 0-1 in one transaction, blob 2 in
    // the next, posted once the first is mined.
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    let job = r.job();
    let posting = r.posting(&job).await;
    let p = posting.posts.clone();
    assert_eq!(p.len(), 2, "{posting:?}");
    assert!(posting.error.is_none());
    assert_eq!((p[0].nonce, p[1].nonce), (0, 1));
    assert!(p[1].block > p[0].block);
    let s = r.l1.sends();
    assert_eq!(s.len(), 2);
    assert!(
        s[1].head >= p[0].block,
        "the second waited for the first: {s:?}"
    );
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let last = r.s.chain.l1_last().unwrap();
    assert_eq!(last.vhs, r.vhs(), "versioned hashes in transition order");
    assert_eq!(last.blocks, vec![p[0].block, p[0].block, p[1].block]);
    // One header chain from the anchor down to the oldest chunk's block.
    assert!(r.s.chain.l1_last_headers() as u64 > p[1].block - p[0].block);
    assert_eq!((r.l1.mined_posts(), r.s.chain.l1_records()), (2, 0));
    assert!(r.db.l1_posts().unwrap().is_empty());
}

#[tokio::test]
async fn the_origin_is_recorded_only_once_the_last_chunk_is_mined() {
    // Three blobs, one per transaction.
    let r = rig_chunked(Duration::from_millis(5), 3, 1);
    r.l1.pin_origin(Some(0));
    let job = r.job();
    let (done, proved) = tokio::sync::oneshot::channel::<()>();
    let driver = async {
        wait_until("three chunks mined", async || r.l1.mined_posts() == 3).await;
        let b: Vec<u64> =
            r.l1.sends()
                .iter()
                .map(|x| r.l1.block_of(&x.hash).unwrap())
                .collect();
        // OP's origin at an older chunk's block: a record there could not
        // anchor the newer transactions.
        for n in [b[0], b[1]] {
            r.l1.pin_origin(Some(n));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(r.s.chain.l1_records(), 0, "recorded below the newest chunk");
        }
        r.l1.pin_origin(Some(b[2]));
        wait_until("origin recorded", async || r.s.chain.l1_records() == 1).await;
        // The origin runs 30 blocks ahead before the proof is ready.
        r.l1.mine(30);
        r.l1.pin_origin(Some(b[2] + 30));
        done.send(()).unwrap();
        (b[0], b[2])
    };
    let ((_, posting), (oldest, newest)) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job, async {
            proved.await.ok();
        }),
        driver
    );
    assert_eq!(posting.recorded.map(|o| o.number), Some(newest));
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.s.chain.l1_records(), 1, "recorded once");
    assert_eq!(
        r.s.chain.l1_last_headers() as u64,
        newest - oldest + 1,
        "anchored at the record: headers from the newest chunk down to the oldest"
    );
}

#[tokio::test(start_paused = true)]
async fn one_chunk_gone_is_posted_again_alone() {
    let r = rig_chunked(Duration::from_secs(1), 3, 2);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    let p = posting.posts.clone();
    // An Ethereum reorg drops the block with the second chunk only.
    assert_eq!(r.l1.head_number(), p[1].block);
    r.l1.reorg(1);
    assert_eq!(r.l1.mined_posts(), 1);
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 3, "the lost chunk alone: {s:?}");
    assert_eq!(s[2].nonce, 1, "at the freed nonce");
    assert_eq!(s[2].fees, s[1].fees.doubled(), "priced over the reorged tx");
    let b2 = r.l1.block_of(&s[2].hash).unwrap();
    assert_eq!(r.l1.block_of(&s[0].hash), Some(p[0].block), "chunk 0 kept");
    assert_eq!(
        r.s.chain.l1_last().unwrap().blocks,
        vec![p[0].block, p[0].block, b2]
    );
    assert_eq!(r.l1.mined_posts(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_reposted_first_chunk_may_land_after_the_second() {
    // Chunk 0's transaction is gone; chunk 1 is mined. Chunk 0 goes out
    // again and lands in a newer block than chunk 1: the proof still lists
    // the transactions in chunk order.
    let r = rig_chunked(Duration::from_secs(1), 3, 2);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let c = chunks(&r.blobs, 2);
    let p1 =
        r.da.poster
            .post(&job.pid, &job.root_before, &c[1], job.deadline)
            .await
            .unwrap();
    let lost = Posted {
        tx: B256::repeat_byte(0xdd),
        nonce: 7,
        block: p1.block,
    };
    let posting = Posting {
        posts: vec![lost, p1],
        ..Posting::default()
    };
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 2, "{s:?}");
    let b0 = r.l1.block_of(&s[1].hash).unwrap();
    assert!(b0 > p1.block);
    let last = r.s.chain.l1_last().unwrap();
    assert_eq!(last.vhs, r.vhs());
    assert_eq!(last.blocks, vec![b0, b0, p1.block]);
}

#[tokio::test]
async fn a_proof_too_deep_posts_every_chunk_again() {
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    let job = r.job();
    let posting = r.posting(&job).await;
    let b0 = posting.posts[0].block;
    r.l1.mine(80);
    r.l1.pin_origin(Some(b0 + 70));
    let driver = async {
        wait_until("re-posted", async || r.l1.mined_posts() == 4).await;
        r.l1.pin_origin(None);
    };
    let (out, ()) = tokio::join!(settle(&r.da, &r.s.chain, &job, &r.snark, posting), driver);
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 4, "both chunks again: {s:?}");
    let (n0, n1) = (
        r.l1.block_of(&s[2].hash).unwrap(),
        r.l1.block_of(&s[3].hash).unwrap(),
    );
    assert!(n0 > b0 + 70 && n1 > n0);
    assert_eq!(r.s.chain.l1_last().unwrap().blocks, vec![n0, n0, n1]);
}

#[tokio::test]
async fn reposts_of_a_chunked_transition_are_bounded() {
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    r.s.chain.revert_simulates("TxTooOld", 100);
    let job = r.job();
    let posting = r.posting(&job).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("re-posts")),
        "{out:?}"
    );
    // Two chunks, then MAX_REPOSTS full re-posts of two.
    assert_eq!(r.l1.mined_posts(), 2 * (1 + MAX_REPOSTS as usize));
}

/// An Ethereum whose fees jump above any cap once a post is mined, until
/// `calm`.
struct Spiky {
    inner: FakeL1,
    calm: AtomicBool,
}

#[async_trait]
impl L1Chain for Spiky {
    fn sender(&self) -> Option<Address> {
        self.inner.sender()
    }
    async fn head(&self) -> Result<u64, Web3Error> {
        self.inner.head().await
    }
    async fn nonce(&self) -> Result<u64, Web3Error> {
        self.inner.nonce().await
    }
    async fn fees(&self) -> Result<L1Market, Web3Error> {
        let mut m = self.inner.fees().await?;
        if self.inner.mined_posts() > 0 && !self.calm.load(Ordering::SeqCst) {
            m.base_fee *= 1000;
        }
        Ok(m)
    }
    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx, Web3Error> {
        self.inner.sign_blob_tx(nonce, fees, blobs).await
    }
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> Result<(), L1SendError> {
        self.inner.send_blob_tx(tx).await
    }
    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>, Web3Error> {
        self.inner.receipt(tx).await
    }
    async fn tx_known(&self, tx: B256) -> Result<bool, Web3Error> {
        self.inner.tx_known(tx).await
    }
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        self.inner.build_proof(txs, top).await
    }
}

#[tokio::test]
async fn a_fee_spike_between_chunks_waits_for_the_second() {
    // Chunk 0 goes out; then the base fee jumps a thousandfold, past the
    // cap: chunk 1 waits, retried each poll, and goes out once fees fall.
    // Chunk 0 is never posted again.
    let mut r = rig_chunked(Duration::from_millis(5), 3, 2);
    let spiky = Arc::new(Spiky {
        inner: r.l1.clone(),
        calm: AtomicBool::new(false),
    });
    let cap = L1Fees::offer(&r.l1.market()).cost(2) * 3;
    let (dir, db) = fresh_db();
    r.da = L1Da::new(spiky.clone(), db.clone(), cap, Duration::from_millis(5), 2).unwrap();
    (r.db, r._dir) = (db, dir);
    let job = r.job();
    let driver = async {
        wait_until("chunk 0 mined", async || r.l1.mined_posts() == 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(r.l1.sends().len(), 1, "chunk 1 sent above the cap");
        spiky.calm.store(true, Ordering::SeqCst);
    };
    let ((_, posting), ()) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job, std::future::ready(())),
        driver
    );
    assert_eq!(posting.posts.len(), 2, "{posting:?}");
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.iter().map(|x| x.nonce).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(r.l1.mined_posts(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_fee_spike_that_never_calms_stops_at_the_deadline() {
    // Chunk 0 mined, then fees stay above the cap: the retry loop must not
    // outlive the job's deadline (the flight would hold the process's single
    // slot until fees fall, however long after the grace end).
    let mut r = rig_chunked(Duration::from_millis(5), 3, 2);
    let spiky = Arc::new(Spiky {
        inner: r.l1.clone(),
        calm: AtomicBool::new(false), // never set
    });
    let cap = L1Fees::offer(&r.l1.market()).cost(2) * 3;
    let (dir, db) = fresh_db();
    r.da = L1Da::new(spiky.clone(), db.clone(), cap, Duration::from_millis(5), 2).unwrap();
    (r.db, r._dir) = (db, dir);
    let job = L1Job {
        deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        ..r.job()
    };
    let (_, posting) = with_post(&r.da, &r.s.chain, &job, std::future::ready(())).await;
    assert_eq!(posting.posts.len(), 1, "{posting:?}");
    assert!(
        matches!(posting.error, Some(PostError::Deadline)),
        "{posting:?}"
    );
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("deadline")),
        "{out:?}"
    );
    assert_eq!(r.l1.sends().len(), 1);
}

#[tokio::test]
async fn two_transitions_interleave_their_chunks() {
    // Two processes' transitions through one Ethereum account at once: the
    // posts go out one at a time at consecutive nonces, and each transition
    // keeps its own chunks, in order.
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    let other = wide_blobs(4);
    let job_a = r.job();
    let job_b = L1Job {
        pid: [9; 31],
        root_before: [9; 32],
        blobs: &other,
        ..r.job()
    };
    let ((_, pa), (_, pb)) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job_a, std::future::ready(())),
        with_post(&r.da, &r.s.chain, &job_b, std::future::ready(()))
    );
    let mut nonces: Vec<u64> = pa.posts.iter().chain(&pb.posts).map(|p| p.nonce).collect();
    nonces.sort();
    assert_eq!(nonces, [0, 1, 2, 3]);
    let origin = r.l1.origin();
    for (posting, blobs) in [(&pa, &r.blobs), (&pb, &other)] {
        assert_eq!(posting.posts.len(), 2);
        assert!(posting.posts[0].block < posting.posts[1].block);
        let txs: Vec<B256> = posting.posts.iter().map(|p| p.tx).collect();
        let proof = r.l1.build_proof(&txs, origin.number).await.unwrap();
        let (vhs, _) = verify_local(&proof, origin.hash).unwrap();
        let want: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        assert_eq!(vhs, want);
    }
    let out = settle(&r.da, &r.s.chain, &job_a, &r.snark, pa).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
}

#[tokio::test(start_paused = true)]
async fn a_single_chunk_repost_drops_the_recorded_origin() {
    // OP's origin reached the second chunk and was recorded; then a reorg
    // drops that chunk's block. The chunk goes out again alone, the stale
    // record anchors nothing, and the proof anchors at the current origin.
    let r = rig_chunked(Duration::from_secs(1), 3, 2);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    let p = posting.posts.clone();
    r.l1.pin_origin(Some(p[1].block));
    let rec = r.s.chain.record_l1_origin().await.unwrap();
    r.l1.pin_origin(None);
    r.l1.reorg(1);
    let posting = Posting {
        recorded: Some(rec),
        ..posting
    };
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 3, "one chunk again, not all: {s:?}");
    let b1 = r.l1.block_of(&s[2].hash).unwrap();
    assert!(b1 > rec.number);
    assert_eq!(r.s.chain.l1_records(), 1);
    assert_eq!(
        r.s.chain.l1_last().unwrap().blocks,
        vec![p[0].block, p[0].block, b1]
    );
}

/// An Ethereum whose fees are above any cap while `spike` is set. Counts
/// the fee reads and notes when the first blob transaction went out.
struct Capped {
    inner: FakeL1,
    spike: AtomicBool,
    reads: AtomicU64,
    sent_at: Mutex<Option<tokio::time::Instant>>,
}

impl Capped {
    fn new(inner: FakeL1) -> Arc<Self> {
        Arc::new(Capped {
            inner,
            spike: AtomicBool::new(true),
            reads: AtomicU64::new(0),
            sent_at: Mutex::new(None),
        })
    }
}

#[async_trait]
impl L1Chain for Capped {
    fn sender(&self) -> Option<Address> {
        self.inner.sender()
    }
    async fn head(&self) -> Result<u64, Web3Error> {
        self.inner.head().await
    }
    async fn nonce(&self) -> Result<u64, Web3Error> {
        self.inner.nonce().await
    }
    async fn fees(&self) -> Result<L1Market, Web3Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut m = self.inner.fees().await?;
        if self.spike.load(Ordering::SeqCst) {
            m.base_fee *= 1000;
        }
        Ok(m)
    }
    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx, Web3Error> {
        self.inner.sign_blob_tx(nonce, fees, blobs).await
    }
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> Result<(), L1SendError> {
        self.sent_at
            .lock()
            .unwrap()
            .get_or_insert_with(tokio::time::Instant::now);
        self.inner.send_blob_tx(tx).await
    }
    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>, Web3Error> {
        self.inner.receipt(tx).await
    }
    async fn tx_known(&self, tx: B256) -> Result<bool, Web3Error> {
        self.inner.tx_known(tx).await
    }
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        self.inner.build_proof(txs, top).await
    }
}

/// The events logged on this thread while the guard lives: (level, message).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<(tracing::Level, String)>>>);

impl Logs {
    fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt;
        let logs = Logs::default();
        let sub = tracing_subscriber::registry().with(logs.clone());
        (logs, tracing::subscriber::set_default(sub))
    }

    fn count(&self, level: tracing::Level, needle: &str) -> usize {
        let l = self.0.lock().unwrap();
        l.iter()
            .filter(|(lv, m)| *lv == level && m.contains(needle))
            .count()
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logs {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Msg(String);
        impl tracing::field::Visit for Msg {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    self.0 = format!("{v:?}");
                }
            }
        }
        let mut m = Msg(String::new());
        e.record(&mut m);
        self.0.lock().unwrap().push((*e.metadata().level(), m.0));
    }
}

// Chaos run C7: a post above the cap was retried every poll with a WARN
// each, 802 lines in 13.5 min. Fees above the cap for ten minutes: the
// retries back off to one a minute, the cap is logged at WARN when it
// starts and when it clears, and the post goes out within a minute of the
// fees falling. Both places that post: while proving and in the settle.
#[tokio::test(start_paused = true)]
async fn a_fee_capped_post_backs_off_and_warns_once() {
    for in_settle in [false, true] {
        let poll = Duration::from_secs(1);
        let mut r = rig_with(poll);
        let l1 = Capped::new(r.l1.clone());
        let cap = L1Fees::offer(&r.l1.market()).cost(r.blobs.blobs.len()) * 3;
        let (dir, db) = fresh_db();
        r.da = L1Da::new(l1.clone(), db.clone(), cap, poll, TX_BLOB_CAP).unwrap();
        (r.db, r._dir) = (db, dir);
        let job = L1Job {
            deadline: tokio::time::Instant::now() + Duration::from_secs(3600),
            ..r.job()
        };
        let (logs, _g) = Logs::capture();
        let t0 = tokio::time::Instant::now();
        let spike = Duration::from_secs(600);
        let lift = async {
            tokio::time::sleep(spike).await;
            l1.spike.store(false, Ordering::SeqCst);
            l1.reads.load(Ordering::SeqCst)
        };
        let run = async {
            let posting = match in_settle {
                true => Posting::default(),
                false => {
                    let (_, p) = with_post(&r.da, &r.s.chain, &job, std::future::ready(())).await;
                    assert_eq!(p.posts.len(), 1, "{p:?}");
                    p
                }
            };
            settle(&r.da, &r.s.chain, &job, &r.snark, posting).await
        };
        let (out, reads) = tokio::join!(run, lift);
        assert!(matches!(out, Settled::Landed(_)), "{out:?}");
        // 1, 2, 4 .. 32 s, then one a minute: 15 in ten minutes (not 600).
        assert!(
            reads <= 20,
            "in_settle {in_settle}: {reads} fee checks in {spike:?}"
        );
        let sent = l1.sent_at.lock().unwrap().expect("posted") - t0;
        assert!(
            sent >= spike && sent <= spike + Duration::from_secs(61),
            "in_settle {in_settle}: posted after {sent:?}"
        );
        assert_eq!(r.l1.sends().len(), 1);
        let warn = tracing::Level::WARN;
        assert_eq!(
            logs.count(warn, "L1 post failed"),
            1,
            "in_settle {in_settle}"
        );
        assert_eq!(
            logs.count(warn, "L1 post went through"),
            1,
            "in_settle {in_settle}"
        );
    }
}

// The backoff never waits past the job's deadline.
#[tokio::test(start_paused = true)]
async fn the_fee_cap_backoff_stops_at_the_deadline() {
    let poll = Duration::from_secs(1);
    let mut r = rig_with(poll);
    let l1 = Capped::new(r.l1.clone());
    let cap = L1Fees::offer(&r.l1.market()).cost(r.blobs.blobs.len()) * 3;
    let (dir, db) = fresh_db();
    r.da = L1Da::new(l1.clone(), db.clone(), cap, poll, TX_BLOB_CAP).unwrap();
    (r.db, r._dir) = (db, dir);
    let t0 = tokio::time::Instant::now();
    let job = L1Job {
        deadline: t0 + Duration::from_secs(100),
        ..r.job()
    };
    let (_, posting) = with_post(&r.da, &r.s.chain, &job, std::future::ready(())).await;
    assert!(
        matches!(posting.error, Some(PostError::Deadline)),
        "{posting:?}"
    );
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(100) && took <= Duration::from_secs(101),
        "{took:?}"
    );
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("deadline")),
        "{out:?}"
    );
    assert!(r.l1.sends().is_empty());
}

/// The registry chain's fee check, scripted: withheld while `over`.
#[derive(Default)]
struct OpGate {
    over: AtomicBool,
    checks: AtomicU64,
}

#[async_trait]
impl FeeGate for OpGate {
    async fn check(&self, gas: u64) -> Result<(), Web3Error> {
        assert_eq!(gas, MIN_SETTLE_GAS);
        self.checks.fetch_add(1, Ordering::SeqCst);
        match self.over.load(Ordering::SeqCst) {
            true => Err(Web3Error::FeeCap { want: 2, cap: 1 }),
            false => Ok(()),
        }
    }
}

// An OP fee spike would withhold the settlement after the blobs were paid
// for on Ethereum: once the proof is ready, the settle posts nothing while
// it lasts and the job ends transient, as a fee-capped send does. Under the
// cap the batch posts and lands.
#[tokio::test]
async fn no_l1_post_while_the_op_settlement_would_be_withheld() {
    let mut r = rig();
    let gate = Arc::new(OpGate::default());
    gate.over.store(true, Ordering::SeqCst);
    r.da.op_fees = Some(gate.clone());
    let job = r.job();
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, Posting::default()).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("--op-max-tx-fee")),
        "{out:?}"
    );
    assert!(r.l1.sends().is_empty(), "posted above the OP cap");
    assert!(!job.posted.load(Ordering::SeqCst));
    assert_eq!(r.s.chain.l1_simulates(), 0);

    gate.over.store(false, Ordering::SeqCst);
    let job = r.job();
    let posting = r.posting(&job).await;
    assert_eq!(posting.posts.len(), 1, "{posting:?}");
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.sends().len(), 1);
    // One before the settle's refused post, one before the post.
    assert_eq!(gate.checks.load(Ordering::SeqCst), 2);
}

// An OP fee spike at seal time: the post waits as a fee-capped one does and
// goes out once OP's fee falls, while the proof still runs, so the batch
// keeps the overlap and lands.
#[tokio::test(start_paused = true)]
async fn an_op_fee_spike_at_seal_delays_the_post_within_the_proof() {
    let poll = Duration::from_secs(1);
    let mut r = rig_with(poll);
    let l1 = Capped::new(r.l1.clone());
    l1.spike.store(false, Ordering::SeqCst);
    let (dir, db) = fresh_db();
    r.da = L1Da::new(l1.clone(), db.clone(), u128::MAX, poll, TX_BLOB_CAP).unwrap();
    (r.db, r._dir) = (db, dir);
    let gate = Arc::new(OpGate::default());
    gate.over.store(true, Ordering::SeqCst);
    r.da.op_fees = Some(gate.clone());
    let t0 = tokio::time::Instant::now();
    let job = L1Job {
        deadline: t0 + Duration::from_secs(600),
        ..r.job()
    };
    let lift = async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        gate.over.store(false, Ordering::SeqCst);
    };
    let prove = tokio::time::sleep(Duration::from_secs(300));
    let ((_, posting), ()) = tokio::join!(with_post(&r.da, &r.s.chain, &job, prove), lift);
    assert_eq!(posting.posts.len(), 1, "{posting:?}");
    let sent = l1.sent_at.lock().unwrap().expect("posted") - t0;
    assert!(
        sent >= Duration::from_secs(60) && sent <= Duration::from_secs(121),
        "posted after {sent:?}"
    );
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.sends().len(), 1);
}
