//! Origin-2 census updates racing their own downloads: the organizer moves
//! the URI while the previous one is still being fetched.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use common::*;
use davinci_client::SequencerClient;
use davinci_sequencer::api::router;
use davinci_sequencer::storage::Db;
use davinci_state::CensusOrigin;
use davinci_zkvm_sdk::census::{LeanImt, census_leaf};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// A census server that takes the request, waits for `release` and then
/// answers 200 with `body`. Counts the requests it took.
async fn gated(body: &'static [u8], release: Arc<Notify>) -> (String, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/census.json", l.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let (h, release) = (h.clone(), release.clone());
            tokio::spawn(async move {
                let mut req = [0u8; 4096];
                let _ = s.read(&mut req).await;
                h.fetch_add(1, SeqCst);
                release.notified().await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (url, hits)
}

fn root_of(voters: std::ops::Range<usize>) -> [u8; 32] {
    LeanImt::from_leaves(
        voters
            .map(|i| census_leaf(&voter_address(i), 1).unwrap())
            .collect(),
    )
    .root()
}

// The organizer points the update at URI A, then corrects it to URI B with
// the same root while A's download is still running. A then answers with a
// broken document: that verdict belongs to A only. B must be fetched, the
// root must load, and no "can never load" mark may stay behind (it would
// refuse every vote with 41201, across restarts too).
#[tokio::test]
async fn same_root_uri_correction_outlives_the_stale_download() {
    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::MerkleOffchainDynamic;
    let census_dir = TempDir::new().unwrap();
    let uri0 = write_census(census_dir.path(), 4);
    let chain = FakeChain::new(&e, uri0);
    let s = TestSetup {
        env: e,
        chain,
        census_dir: census_dir.path().to_path_buf(),
        _census_dir: census_dir,
    };
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let cfg = test_config_with(
        dir.path(),
        &s.census_dir,
        1,
        "0s",
        &["--census-allow-private"],
    );
    let node = start_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        FakeProver::open(),
        cfg,
        shutdown.clone(),
    )
    .await;
    handle(&node).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(node.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { sd.cancelled().await })
            .await
            .unwrap();
    });
    let client = SequencerClient::new(&base);

    // Five voters now; the corrected document is B.
    let root = root_of(0..5);
    let good = s.census_dir.join("good");
    std::fs::create_dir(&good).unwrap();
    let uri_b = write_census_of(&good, 0..5);
    let release = Arc::new(Notify::new());
    let (uri_a, hits) = gated(br#"{"participants": "not a list"}"#, release.clone()).await;

    s.chain.set_census(root, uri_a);
    wait_until("A requested", async || hits.load(SeqCst) == 1).await;
    s.chain.set_census(root, uri_b);
    // Let the monitor route the correction while A is still held.
    tokio::time::sleep(Duration::from_millis(400)).await;
    release.notify_waiters();

    // BUG: A's failure marks the root bad and the queued B is never fetched
    wait_until("the corrected census loaded", async || {
        node.census.has(&root).unwrap()
    })
    .await;
    assert!(node.census.bad_reason(&pid_fr(), &root).is_none());
    // Votes are admitted again: voter 4, new in this census, gets a proof.
    wait_until("voter 4 served at the new root", async || {
        client
            .participant(&pid31(), &voter_address(4))
            .await
            .is_ok_and(|(p, _)| p.root == root)
    })
    .await;
    assert_eq!(hits.load(SeqCst), 1, "A was fetched again");
    shutdown.cancel();
}

// A correction to a different root, same race: the stale failure does not
// mark the new root either (the control for the generation check).
#[tokio::test]
async fn new_root_correction_outlives_the_stale_download() {
    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::MerkleOffchainDynamic;
    let census_dir = TempDir::new().unwrap();
    let uri0 = write_census(census_dir.path(), 4);
    let chain = FakeChain::new(&e, uri0);
    let s = TestSetup {
        env: e,
        chain,
        census_dir: census_dir.path().to_path_buf(),
        _census_dir: census_dir,
    };
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let cfg = test_config_with(
        dir.path(),
        &s.census_dir,
        1,
        "0s",
        &["--census-allow-private"],
    );
    let node = start_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        FakeProver::open(),
        cfg,
        shutdown.clone(),
    )
    .await;
    handle(&node).await;
    let release = Arc::new(Notify::new());
    let (uri_a, hits) = gated(b"[", release.clone()).await;
    s.chain.set_census(root_of(0..3), uri_a);
    wait_until("A requested", async || hits.load(SeqCst) == 1).await;
    let root = root_of(0..5);
    let good = s.census_dir.join("good");
    std::fs::create_dir(&good).unwrap();
    s.chain.set_census(root, write_census_of(&good, 0..5));
    tokio::time::sleep(Duration::from_millis(400)).await;
    release.notify_waiters();
    wait_until("the new census loaded", async || {
        node.census.has(&root).unwrap()
    })
    .await;
    assert!(node.census.bad_reason(&pid_fr(), &root).is_none());
    shutdown.cancel();
}
