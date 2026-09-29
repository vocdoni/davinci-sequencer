//! HTTP API and the node entry point [`run`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;

use crate::actor::{Deps, ProverService, SystemClock};
use crate::config::Config;
use crate::monitor::{Node, spawn_node};
use crate::storage::Db;
use crate::web3::{Contracts, blob_source};

mod error;
mod info;
mod processes;
mod votes;

pub use error::ApiError;

/// Largest request body accepted (a 16-field vote is ~8 KiB).
const MAX_BODY: usize = 256 * 1024;

/// Hard per-request deadline; census rebuilds and proof checks fit well
/// inside it, a stuck handler gets a 408 instead of a leaked connection.
/// The handler keeps running detached, so a timed-out `POST /votes` may
/// still have admitted the vote — a retry then answers 409 duplicate.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Maps middleware errors (the timeout layer) onto the JSON error shape.
async fn layer_error(err: tower::BoxError) -> ApiError {
    if err.is::<tower::timeout::error::Elapsed>() {
        ApiError::Timeout
    } else {
        ApiError::Internal(err.to_string())
    }
}

/// One-minute token window for `POST /processes/keys`.
#[derive(Debug)]
struct KeyWindow {
    start: Instant,
    count: u32,
}

#[derive(Clone)]
pub struct AppState {
    node: Node,
    /// Per-client-IP key request windows.
    key_windows: Arc<Mutex<HashMap<IpAddr, KeyWindow>>>,
    /// Bounds concurrent Groth16/ECDSA vote validations.
    validate: Arc<Semaphore>,
}

impl AppState {
    /// Consumes one key request token of `ip` or fails with 429.
    fn take_key_token(&self, ip: IpAddr) -> Result<(), ApiError> {
        let mut m = self
            .key_windows
            .lock()
            .map_err(|_| ApiError::Internal("key window lock poisoned".into()))?;
        m.retain(|_, w| w.start.elapsed().as_secs() < 60);
        let w = m.entry(ip).or_insert(KeyWindow {
            start: Instant::now(),
            count: 0,
        });
        if w.count >= self.node.statics.keys_per_minute {
            return Err(ApiError::KeyRate);
        }
        w.count += 1;
        Ok(())
    }
}

/// A `0x` 20-byte hex address path segment.
fn parse_addr(s: &str) -> Result<[u8; 20], ApiError> {
    let h = s.strip_prefix("0x").unwrap_or(s);
    let mut out = [0u8; 20];
    hex::decode_to_slice(h, &mut out)
        .map_err(|_| ApiError::Invalid("want a 20-byte hex address".into()))?;
    Ok(out)
}

/// Concurrent vote validations: one per core, floor 2.
fn validate_permits() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2)
}

/// Every API route over a running [`Node`]. Serve it with
/// [`into_make_service_with_connect_info`](Router::into_make_service_with_connect_info)
/// so the per-IP rate limits see the peer address.
pub fn router(node: Node) -> Router {
    let state = AppState {
        node,
        key_windows: Arc::new(Mutex::new(HashMap::new())),
        validate: Arc::new(Semaphore::new(validate_permits())),
    };
    Router::new()
        .route("/ping", get(info::ping))
        .route("/info", get(info::info))
        .route("/processes", get(processes::list))
        .route("/processes/keys", post(processes::new_key))
        .route("/processes/:pid", get(processes::get))
        .route(
            "/processes/:pid/participants/:address",
            get(processes::participant),
        )
        .route("/processes/:pid/transitions", get(processes::transitions))
        .route(
            "/processes/:pid/transitions/:index/blobs",
            get(processes::blobs),
        )
        .route("/votes", post(votes::submit))
        .route("/votes/:pid/voteId/:vid", get(votes::status))
        .route("/votes/:pid/voteId/:vid/proof", get(votes::proof))
        .route("/votes/:pid/address/:address", get(votes::ballot))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        // tower's TimeoutLayer errors instead of responding; map it back
        // to the JSON error shape (408 40801, 500 50001 for the rest).
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(layer_error))
                .layer(TimeoutLayer::new(REQUEST_TIMEOUT)),
        )
        .with_state(state)
}

/// Builds the real dependencies, spawns the node and serves the API until
/// `shutdown` fires; then waits for the actor and monitor tasks.
pub async fn run(cfg: Config, shutdown: CancellationToken) -> anyhow::Result<()> {
    let tasks = tokio_util::task::TaskTracker::new();
    // Startup talks to the chain (release check, probes, respawns); a signal
    // during it stops the node too.
    let started = tokio::select! {
        _ = shutdown.cancelled() => None,
        r = start(cfg, tasks.clone(), shutdown.clone()) => Some(r?),
    };
    if let Some((listener, node)) = started {
        tracing::info!(addr = %listener.local_addr()?, "API listening");
        axum::serve(
            listener,
            router(node).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    }
    // Let the actors and the monitor finish before dropping the process:
    // they hold the database and may be mid-commit.
    tasks.close();
    tasks.wait().await;
    Ok(())
}

/// Connects, checks the chain, opens the deployment's database and spawns
/// the node; [`run`] serves it.
async fn start(
    cfg: Config,
    tasks: tokio_util::task::TaskTracker,
    shutdown: CancellationToken,
) -> anyhow::Result<(tokio::net::TcpListener, Node)> {
    cfg.check_chain_id(crate::web3::rpc_chain_id(&cfg.rpc_url).await?)?;
    let contracts = Arc::new(Contracts::connect(&cfg).await?);
    if contracts.signer().is_none() {
        tracing::info!("no signing key: running as an observer (never settles)");
    }
    let vk_hash = crate::monitor::ballot_verifier(&cfg)?.vk_hash();
    contracts.check_release(&vk_hash).await?;
    let grace = contracts.grace_params().await?;
    tracing::info!(?grace, "registry grace window");
    let registry = cfg.registry.into_array();
    let dir = crate::storage::deployment_dir(&cfg.datadir, contracts.chain_id(), &registry);
    tracing::info!(
        network = %cfg.network,
        chain_id = contracts.chain_id(),
        registry = %cfg.registry,
        start_block = ?cfg.start_block,
        datadir = %dir.display(),
        "deployment"
    );
    let db = Db::open_deployment(&cfg.datadir, contracts.chain_id(), &registry)?;
    let mut cfg = cfg;
    let cap = crate::web3::resolve_blob_cap(
        cfg.max_blobs_per_tx,
        contracts.chain_blob_cap(),
        contracts.chain_id(),
    );
    cfg.max_blobs_per_tx = Some(cap);
    tracing::info!(cap, "blob cap per transaction");
    let prover = Arc::new(ProverService::new(
        cfg.prover_url.as_str(),
        cfg.prover_poll,
        cfg.prover_timeout,
    ));
    let deps = Deps {
        db,
        contracts,
        prover,
        blobs: blob_source(&cfg),
        clock: Arc::new(SystemClock),
        grace,
        tasks,
    };
    let addr = format!("{}:{}", cfg.api_host, cfg.api_port);
    let node = spawn_node(cfg, deps, shutdown).await?;
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    Ok((listener, node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    // A signal during startup stops the node even when the RPC hangs.
    #[tokio::test]
    async fn shutdown_during_startup() {
        let app = Router::new().route(
            "/",
            axum::routing::post(|| async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                ""
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = format!("http://{}/", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = Config::parse_args([
            "davinci-sequencer",
            "--datadir",
            dir.path().to_str().unwrap(),
            "--network",
            "custom",
            "--rpc-url",
            &rpc,
            "--registry",
            "0x0000000000000000000000000000000000000001",
            "--blob-source",
            "anvil",
        ])
        .unwrap();
        let shutdown = CancellationToken::new();
        let node = tokio::spawn(run(cfg, shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!node.is_finished());
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), node)
            .await
            .expect("still starting 2s after shutdown")
            .unwrap()
            .unwrap();
    }

    // The timeout layer answers in the JSON error shape (408
    // 40801), not with tower's default empty body.
    #[tokio::test]
    async fn timeout_maps_to_json_408() {
        let app = Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            )
            .layer(
                ServiceBuilder::new()
                    .layer(HandleErrorLayer::new(layer_error))
                    .layer(TimeoutLayer::new(Duration::from_millis(50))),
            );
        let resp = app
            .oneshot(Request::get("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], 40801);
        assert!(v["error"].as_str().unwrap().contains("timed out"));
    }
}
