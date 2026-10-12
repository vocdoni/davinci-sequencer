//! The L1-blob poster and settle loop on the scripted Ethereum (`FakeL1`)
//! and registry (`FakeChain`).

mod common;

use common::*;
use davinci_sequencer::l1da::{PostError, Poster};
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
