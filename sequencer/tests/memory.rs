//! Memory and disk bounds: no arbo read snapshot outlives an actor message
//! (an idle election's snapshot would pin every later commit's pages), and a
//! gated soak (`MEMSOAK=1`) of the node pipeline on the shipped allocator.

mod common;

use arbo::{Sha256, Tree};
use common::*;

// The node binary's allocator, so the soak measures what ships.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// Bytes the allocator holds for live Rust objects.
fn live_mb() -> u64 {
    tikv_jemalloc_ctl::epoch::advance().unwrap();
    tikv_jemalloc_ctl::stats::allocated::read().unwrap() as u64 >> 20
}

fn rss_mb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|r| r.trim().trim_end_matches(" kB").trim().parse::<u64>().ok())
        .unwrap()
        >> 10
}

/// Serializes the request like `reqwest`'s `.json()` before proving, so
/// the 13-17 MB prove body is part of every batch.
struct JsonProver(Arc<FakeProver>);

#[async_trait]
impl Prover for JsonProver {
    async fn prove_batch(
        &self,
        request: &ProveRequest,
        expected: &BatchPublics,
    ) -> Result<(BatchPublics, PlonkSnark, Option<f64>), ProverError> {
        let body = serde_json::to_vec(request).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(body);
        self.0.prove_batch(request, expected).await
    }

    async fn prove_results(
        &self,
        request: &ResultsRequest,
    ) -> Result<(ResultsPublics, PlonkSnark), ProverError> {
        self.0.prove_results(request).await
    }
}

/// L1-blob pipeline on fakes, `MEMSOAK_BATCHES` (40) batches of 512 votes
/// at `MEMSOAK_NF` (16) fields, votes cast from many tasks, status and
/// proof pollers alongside. RSS stays near the live heap, the live heap
/// near the page cache, and the file near its data.
#[tokio::test(flavor = "multi_thread")]
async fn memsoak() {
    if std::env::var("MEMSOAK").is_err() {
        eprintln!("skipped: set MEMSOAK=1 (optional MEMSOAK_BATCHES, MEMSOAK_NF, MEMSOAK_DIR)");
        return;
    }
    let nf: u8 = env_or("MEMSOAK_NF", 16);
    let batches: usize = env_or("MEMSOAK_BATCHES", 40);
    let scratch: PathBuf = env_or("MEMSOAK_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")));
    let bsize = 512usize;
    let s = setup(nf, bsize * batches, None);
    s.chain.set_max_voters(10_000_000);
    let dir = TempDir::new_in(&scratch).unwrap();
    let shutdown = CancellationToken::new();
    let cfg = test_config(dir.path(), &s.census_dir, bsize, "0s");
    let cache_mb = cfg.db_cache_mb as u64;
    let db = Db::open_with(
        &dir.path().join(davinci_sequencer::storage::DB_FILE),
        cfg.db_cache_mb as usize,
    )
    .unwrap();
    let node = start_l1_node_cfg(
        db,
        s.chain.clone(),
        FakeL1::new(),
        Arc::new(JsonProver(FakeProver::open())),
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    // The bench polls every vote's status; proof lookups read the tree.
    let polled: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..4 {
        let (h, polled, stop) = (h.clone(), polled.clone(), shutdown.clone());
        tokio::spawn(async move {
            let mut i = 0usize;
            while !stop.is_cancelled() {
                let vid = {
                    let p = polled.lock().unwrap();
                    (!p.is_empty()).then(|| p[i % p.len()])
                };
                if let Some(vid) = vid {
                    let _ = h.status(vid).await;
                    let _ = h.vote_id_proof(vid).await;
                }
                i += 7;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });
    }
    println!("start: live {} MB rss {} MB", live_mb(), rss_mb());
    let env = Arc::new(Env {
        cfg: s.env.cfg.clone(),
        imt: s.env.imt.clone(),
    });
    let t0 = std::time::Instant::now();
    let (mut peak_rss, mut peak_live) = (0u64, 0u64);
    for b in 0..batches {
        // A fast prover estimate: the time budget fits a full batch.
        for _ in 0..40 {
            node.metrics.prove.observe(nf, bsize, 30.0, 30.0);
        }
        let mut tasks = Vec::new();
        for c in 0..16usize {
            let (env, h) = (env.clone(), h.clone());
            tasks.push(tokio::spawn(async move {
                let mut vids = Vec::new();
                for i in (c..bsize).step_by(16) {
                    let idx = b * bsize + i;
                    let env = env.clone();
                    let v = tokio::task::spawn_blocking(move || {
                        fake_vote(&env, idx, &[1, 2], 1_000_000 + idx as u64)
                    })
                    .await
                    .unwrap();
                    vids.push(v.pkg.vote_id);
                    h.submit(v).await.unwrap();
                }
                vids
            }));
        }
        let mut vids = Vec::new();
        for t in tasks {
            vids.extend(t.await.unwrap());
        }
        polled.lock().unwrap().extend(vids.iter().copied());
        wait_until_long(&h, &vids).await;
        let (live, rss) = (live_mb(), rss_mb());
        (peak_rss, peak_live) = (peak_rss.max(rss), peak_live.max(live));
        println!(
            "batch {:>2} t={:>4}s live {live:>5} MB rss {rss:>5} MB file {:>6} MB",
            b + 1,
            t0.elapsed().as_secs(),
            file_len(dir.path()) >> 20
        );
    }
    shutdown.cancel();
    let file = file_len(dir.path()) >> 20;
    println!(
        "end: peak live {peak_live} MB, peak rss {peak_rss} MB, file {file} MB, cache cap {cache_mb} MB"
    );
    assert!(
        peak_rss <= peak_live + 600,
        "rss {peak_rss} MB, live {peak_live} MB"
    );
    assert!(
        peak_live <= cache_mb + 300,
        "live {peak_live} MB, cache cap {cache_mb} MB"
    );
    // About 15 MB of votes, blobs and tree a batch, and redb doubles the
    // file as it grows; a pinned snapshot made it about 300 MB a batch.
    let bound = 48 * batches as u64;
    assert!(file <= bound, "file {file} MB, bound {bound} MB");
}

// Until every vote of the batch settled (no 15 s cap: a 512-vote batch at
// 16 fields takes longer on a busy host).
async fn wait_until_long(h: &ActorHandle, vids: &[u64]) {
    let last = *vids.last().unwrap();
    for _ in 0..60_000 {
        if vote_status(h, last).await == Some(VoteStatus::Settled) && all_settled(h, vids).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("batch never settled");
}
