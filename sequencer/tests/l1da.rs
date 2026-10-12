//! The L1-blob poster and settle loop on the scripted Ethereum (`FakeL1`)
//! and registry (`FakeChain`).

mod common;

use common::*;
use davinci_sequencer::l1da::{
    GONE_POLLS, L1Da, L1Job, MAX_PROOF_RETRIES, MAX_REPOSTS, PostError, Poster, Posting, Settled,
    settle,
};
use davinci_sequencer::web3::L1Chain;
use davinci_sequencer::web3::L1Receipt;
use davinci_sequencer::web3::L1SendError;
use davinci_sequencer::web3::SignedBlobTx;
use davinci_sequencer::web3::l1proof::L1BlobProof;
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{TransitionData, build_blobs};
use davinci_zkvm_sdk::limits::VOTE_ID_MIN;

const PID: [u8; 31] = [7; 31];
const ROOT: [u8; 32] = [1; 32];

fn blobs(seed: u8) -> TransitionBlobs {
    let t = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + u64::from(seed)],
        updates: vec![],
        accumulator: Ballot::identity(),
        num_fields: 1,
    };
    build_blobs(&t, &Fr::from(u64::from(seed)), &[seed; 32]).unwrap()
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

/// FakeChain in L1 mode over a FakeL1, a poster on a fresh database, and a
/// one-vote batch on the chain's root with the snark FakeProver returns.
struct Rig {
    s: TestSetup,
    l1: FakeL1,
    da: L1Da,
    snark: PlonkSnark,
    blobs: TransitionBlobs,
    _dir: TempDir,
}

fn rig() -> Rig {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.attach_l1(&l1);
    let (dir, db) = fresh_db();
    let da = L1Da::new(
        Arc::new(l1.clone()),
        db,
        u128::MAX,
        Duration::from_millis(5),
    )
    .unwrap();
    let blobs = blobs(9);
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
        }
    }

    async fn posting(&self, job: &L1Job<'_>) -> Posting {
        Posting {
            post: self
                .da
                .poster
                .post(&job.pid, &job.root_before, job.blobs, job.deadline)
                .await,
            recorded: None,
        }
    }
}

#[tokio::test]
async fn settle_lands_with_the_l1_blocks() {
    let r = rig();
    let job = r.job();
    let posting = r.posting(&job).await;
    let posted = *posting.post.as_ref().unwrap();
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
    let n = posting.post.as_ref().unwrap().block;
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
    let n = posting.post.as_ref().unwrap().block;
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

/// An Ethereum whose receipt reads come back empty a few times first.
struct Lagging {
    inner: FakeL1,
    misses: Mutex<u32>,
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
        let miss = {
            let mut m = self.misses.lock().unwrap();
            let miss = *m > 0;
            *m = m.saturating_sub(1);
            miss
        };
        if miss {
            return Ok(None);
        }
        self.inner.receipt(tx).await
    }
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        self.inner.build_proof(txs, top).await
    }
}

#[tokio::test]
async fn a_lagging_receipt_does_not_repost() {
    let mut r = rig();
    // Poster and settle loop share the lagging view; two misses < GONE_POLLS.
    let lag = Arc::new(Lagging {
        inner: r.l1.clone(),
        misses: Mutex::new(0),
    });
    let (dir, db) = fresh_db();
    r.da = L1Da::new(lag.clone(), db, u128::MAX, Duration::from_millis(5)).unwrap();
    r._dir = dir;
    let job = r.job();
    let posting = r.posting(&job).await;
    *lag.misses.lock().unwrap() = GONE_POLLS - 1;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(
        r.l1.mined_posts(),
        1,
        "a receipt read as null twice is not gone"
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
