//! The HTTP prover client cancels the job of an aborted flight: a lost race
//! sends one `DELETE /jobs/{id}` for the in-flight job, a normal landing
//! none, and a prover without the route (405) changes nothing.

mod common;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use common::*;
use davinci_sequencer::actor::ProverService;

/// A davinci-zkvm stand-in on an ephemeral port. Jobs numbered up to `hold`
/// stay `running`, later ones are `done` at once; every DELETE is recorded.
struct FakeService {
    next: AtomicU64,
    hold: u64,
    /// Answer to DELETE (405: a prover without the route).
    delete_status: StatusCode,
    deletes: Mutex<Vec<String>>,
    /// The publics each job proves, set by [`Tap`] before the submit.
    expected: Mutex<Option<BatchPublics>>,
    jobs: Mutex<HashMap<String, BatchPublics>>,
}

impl FakeService {
    fn job_no(id: &str) -> u64 {
        id.trim_start_matches("job-").parse().unwrap_or(0)
    }

    fn publics(&self, id: &str) -> Option<BatchPublics> {
        self.jobs.lock().unwrap().get(id).copied()
    }

    fn deletes(&self) -> Vec<String> {
        self.deletes.lock().unwrap().clone()
    }
}

async fn serve(hold: u64, delete_status: StatusCode) -> (String, Arc<FakeService>) {
    use axum::routing::{get, post};
    let svc = Arc::new(FakeService {
        next: AtomicU64::new(0),
        hold,
        delete_status,
        deletes: Mutex::new(Vec::new()),
        expected: Mutex::new(None),
        jobs: Mutex::new(HashMap::new()),
    });
    let app = axum::Router::new()
        .route("/prove", post(prove))
        .route("/jobs/:id", get(job).delete(cancel))
        .route("/jobs/:id/publics", get(publics))
        .route("/jobs/:id/snark", get(snark))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(svc.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, svc)
}

async fn prove(
    State(s): State<Arc<FakeService>>,
    _body: axum::body::Bytes,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    let id = format!("job-{}", s.next.fetch_add(1, Ordering::SeqCst) + 1);
    let p = s.expected.lock().unwrap().expect("tap sets the publics");
    s.jobs.lock().unwrap().insert(id.clone(), p);
    (
        StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({"job_id": id, "status": "queued"})),
    )
}

async fn job(
    State(s): State<Arc<FakeService>>,
    UrlPath(id): UrlPath<String>,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    if s.publics(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "job not found"})),
        );
    }
    let status = if FakeService::job_no(&id) <= s.hold {
        "running"
    } else {
        "done"
    };
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"job_id": id, "status": status, "elapsed_ms": 1000})),
    )
}

async fn cancel(State(s): State<Arc<FakeService>>, UrlPath(id): UrlPath<String>) -> StatusCode {
    s.deletes.lock().unwrap().push(id);
    s.delete_status
}

// The 64 registers as u32 LE, the service's `publics.bin`.
async fn publics(
    State(s): State<Arc<FakeService>>,
    UrlPath(id): UrlPath<String>,
) -> Result<Vec<u8>, StatusCode> {
    let p = s.publics(&id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(encode_batch(&p)
        .chunks_exact(8)
        .flat_map(|w| w[..4].to_vec())
        .collect())
}

async fn snark(
    State(s): State<Arc<FakeService>>,
    UrlPath(id): UrlPath<String>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let p = s.publics(&id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(axum::Json(serde_json::json!({
        "program_vk": format!("0x{}", hex::encode(release::BATCH_PROGRAM_VK)),
        "root_c_vadcop_final": format!("0x{}", hex::encode(release::ROOT_C_VADCOP_FINAL)),
        "public_values": format!("0x{}", hex::encode(encode_batch(&p))),
        "proof_bytes": format!("0x{}", hex::encode([0u8; 768])),
    })))
}

/// The real HTTP client, telling the fake service which publics the next
/// job proves (the service ignores `expected`).
struct Tap {
    inner: ProverService,
    svc: Arc<FakeService>,
}

#[async_trait]
impl Prover for Tap {
    async fn prove_batch(
        &self,
        request: &ProveRequest,
        expected: &BatchPublics,
    ) -> Result<(BatchPublics, PlonkSnark, Option<f64>), ProverError> {
        *self.svc.expected.lock().unwrap() = Some(*expected);
        self.inner.prove_batch(request, expected).await
    }

    async fn prove_results(
        &self,
        request: &ResultsRequest,
    ) -> Result<(ResultsPublics, PlonkSnark), ProverError> {
        self.inner.prove_results(request).await
    }
}

async fn node_on(
    s: &TestSetup,
    dir: &TempDir,
    hold: u64,
    delete_status: StatusCode,
    shutdown: &CancellationToken,
) -> (Node, Arc<FakeService>) {
    node_waiting(
        s,
        dir,
        hold,
        delete_status,
        Duration::from_secs(30),
        shutdown,
    )
    .await
}

async fn node_waiting(
    s: &TestSetup,
    dir: &TempDir,
    hold: u64,
    delete_status: StatusCode,
    window: Duration,
    shutdown: &CancellationToken,
) -> (Node, Arc<FakeService>) {
    let (url, svc) = serve(hold, delete_status).await;
    let prover = Arc::new(Tap {
        inner: ProverService::new(&url, Duration::from_millis(10), window),
        svc: svc.clone(),
    });
    let cfg = test_config(dir.path(), &s.census_dir, 1, "0s");
    let db = Db::open_in(dir.path()).unwrap();
    let node = start_node_cfg(db, s.chain.clone(), prover, cfg, shutdown.clone()).await;
    (node, svc)
}

/// Our vote is proving as job-1 (held) when a peer's transition lands: the
/// flight aborts, job-1 is cancelled once, and the vote settles on top as
/// job-2, which is never cancelled.
async fn lost_race_cancels_the_stale_job(delete_status: StatusCode) {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, svc) = node_on(&s, &dir, 1, delete_status, &shutdown).await;
    let h = handle(&node).await;
    let ours = fake_vote(&s.env, 0, &[1, 2], 700);
    let vid = ours.pkg.vote_id;
    h.submit(ours).await.unwrap();
    wait_until("job-1 proving", async || svc.publics("job-1").is_some()).await;

    let peer = fake_vote(&s.env, 1, &[2, 1], 701);
    s.chain
        .settle_externally(&foreign_batch_of(&s, std::slice::from_ref(&peer)));
    wait_until("ours settled on top", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    wait_until("stale job cancelled", async || !svc.deletes().is_empty()).await;
    assert_eq!(node.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(s.chain.voters(), 2);
    // Late duplicates would show up by now.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(svc.deletes(), vec!["job-1".to_string()]);
    assert!(svc.publics("job-2").is_some());
    shutdown.cancel();
}

#[tokio::test]
async fn lost_race_cancels_the_in_flight_job() {
    lost_race_cancels_the_stale_job(StatusCode::OK).await;
}

// A prover without the route answers 405: the node carries on.
#[tokio::test]
async fn cancel_refused_by_an_old_prover_is_harmless() {
    lost_race_cancels_the_stale_job(StatusCode::METHOD_NOT_ALLOWED).await;
}

#[tokio::test]
async fn landed_job_is_not_cancelled() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, svc) = node_on(&s, &dir, 0, StatusCode::OK, &shutdown).await;
    let h = handle(&node).await;
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 710 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in votes {
        h.submit(v).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 2);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(svc.deletes().is_empty(), "{:?}", svc.deletes());
    shutdown.cancel();
}

// A job that outlives every wait window is abandoned and the batch proved
// again: the abandoned job is cancelled, not left ahead of its successor.
#[tokio::test]
async fn abandoned_wait_cancels_the_job() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let window = Duration::from_millis(50);
    let (node, svc) = node_waiting(&s, &dir, 1, StatusCode::OK, window, &shutdown).await;
    let h = handle(&node).await;
    let ours = fake_vote(&s.env, 0, &[1, 2], 720);
    let vid = ours.pkg.vote_id;
    h.submit(ours).await.unwrap();
    wait_until("settled after a new job", async || {
        all_settled(&h, &[vid]).await
    })
    .await;
    assert_eq!(svc.deletes(), vec!["job-1".to_string()]);
    shutdown.cancel();
}
