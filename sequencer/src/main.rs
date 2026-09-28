#![forbid(unsafe_code)]

//! The `davinci-sequencer` binary: parse config, init tracing, run the node
//! until SIGINT or SIGTERM.

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    let cfg = davinci_sequencer::config::Config::load()?;
    // RUST_LOG wins; --log-level / DAVINCI_LOG_LEVEL is the fallback.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(cfg.log_level.clone()));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let r = rt.block_on(run(cfg));
    // Detached blocking work (a census build) must not hold the exit.
    rt.shutdown_timeout(Duration::from_secs(1));
    r
}

/// Longest wait for the node to stop after a signal before exiting anyway.
/// Every phase honours the token; this bounds whatever still blocks (a
/// CPU-bound seal, an in-flight HTTP request). The database is crash-safe.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

async fn run(cfg: davinci_sequencer::config::Config) -> anyhow::Result<()> {
    let shutdown = CancellationToken::new();
    tokio::spawn(watch_signals(shutdown.clone()));
    let grace = shutdown.clone();
    tokio::select! {
        r = davinci_sequencer::api::run(cfg, shutdown) => r,
        _ = async {
            grace.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => {
            tracing::warn!("node still busy {SHUTDOWN_GRACE:?} after the signal: exiting");
            Ok(())
        }
    }
}

async fn watch_signals(shutdown: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT: shutting down"),
                _ = term.recv() => tracing::info!("SIGTERM: shutting down"),
            }
        }
        // No SIGTERM handler is no reason to ignore SIGINT too.
        Err(e) => {
            tracing::error!(%e, "SIGTERM handler");
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("SIGINT: shutting down");
        }
    }
    shutdown.cancel();
}
