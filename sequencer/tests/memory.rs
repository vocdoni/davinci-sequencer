//! Memory and disk bounds: no arbo read snapshot outlives an actor message
//! (an idle election's snapshot would pin every later commit's pages).

mod common;

use arbo::{Sha256, Tree};
use common::*;

// Commits on another election in the same file.
const COMMITS: u64 = 1000;

fn file_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join(davinci_sequencer::storage::DB_FILE))
        .unwrap()
        .len()
}

// Another election's tree, one commit per leaf.
fn churn(db: &Db) {
    let other = db.arbo_storage(&Fr::from(424_242u64)).unwrap();
    let mut t = Tree::new(other, 64, Sha256).unwrap();
    for i in 0..COMMITS {
        t.add(&i.to_le_bytes(), &[i as u8; 32]).unwrap();
    }
}

#[tokio::test]
async fn idle_actor_snapshot_does_not_pin_the_file() {
    let tmp = env!("CARGO_TARGET_TMPDIR");
    // Baseline: the same commits with no reader at all.
    let base_dir = TempDir::new_in(tmp).unwrap();
    churn(&Db::open_in(base_dir.path()).unwrap());
    let baseline = file_len(base_dir.path());

    let s = setup(2, 8, None);
    let dir = TempDir::new_in(tmp).unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        db.clone(),
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
    // A proof lookup walks the idle election's tree.
    assert!(matches!(
        h.vote_id_proof(42).await,
        Err(ActorError::NotFound)
    ));
    let mut open = usize::MAX;
    for _ in 0..200 {
        open = db.arbo_snapshots_open();
        if open == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    churn(&db);
    let pinned = file_len(dir.path());
    shutdown.cancel();
    println!("file {pinned} B after the idle read, {baseline} B with no reader");
    assert!(
        pinned <= 2 * baseline,
        "file {pinned} B after the idle read, {baseline} B with no reader"
    );
    assert_eq!(open, 0, "a read snapshot outlived the actor message");
}
