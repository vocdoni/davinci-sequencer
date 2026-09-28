//! Sequencer nodes as subprocesses of the real `davinci-sequencer` binary,
//! each configured through `DAVINCI_*` environment variables.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use anyhow::{Context, Result, bail, ensure};
use davinci_client::SequencerClient;

/// A run's work directory under `TMPDIR`, else `~/.cache` (never `/tmp`):
/// removed on success, kept with its log paths printed unless `ok` is set.
pub struct WorkDir {
    dir: Option<tempfile::TempDir>,
    pub ok: bool,
}

impl WorkDir {
    pub fn new(prefix: &str) -> Result<WorkDir> {
        let base = match std::env::var_os("TMPDIR") {
            Some(t) if !t.is_empty() => PathBuf::from(t),
            _ => PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".cache"),
        };
        std::fs::create_dir_all(&base)?;
        let dir = tempfile::Builder::new().prefix(prefix).tempdir_in(&base)?;
        Ok(WorkDir {
            dir: Some(dir),
            ok: false,
        })
    }

    pub fn path(&self) -> &Path {
        self.dir
            .as_ref()
            .map(|d| d.path())
            .expect("live until drop")
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let Some(d) = self.dir.take() else { return };
        if self.ok {
            return;
        }
        let p = d.keep();
        eprintln!("FAILED: logs and datadirs kept in {}", p.display());
        if let Ok(rd) = std::fs::read_dir(&p) {
            let mut logs: Vec<_> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "log"))
                .collect();
            logs.sort();
            for l in logs {
                eprintln!("  log: {}", l.display());
            }
        }
    }
}

/// The node binary: `DAVINCI_SEQUENCER_BIN`, or a release build of the
/// workspace's `davinci-sequencer`.
pub fn sequencer_bin() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("DAVINCI_SEQUENCER_BIN") {
        let p = PathBuf::from(p);
        ensure!(
            p.is_file(),
            "DAVINCI_SEQUENCER_BIN {} is not a file",
            p.display()
        );
        return Ok(p);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let st = Command::new(cargo)
        .args(["build", "--release", "-p", "davinci-sequencer"])
        .args(["--bin", "davinci-sequencer"])
        .current_dir(&root)
        .status()
        .context("run cargo build")?;
    ensure!(st.success(), "cargo build -p davinci-sequencer failed");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let p = target.join("release/davinci-sequencer");
    ensure!(p.is_file(), "{} not built", p.display());
    Ok(p)
}

/// What differs between nodes, plus what they share.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub name: String,
    /// File with the hex signing key, passed as `DAVINCI_PRIVKEY_FILE` (the
    /// key never goes into the environment); `None` runs an observer.
    pub privkey_file: Option<PathBuf>,
    pub address: Option<Address>,
    pub rpc_url: String,
    pub registry: Address,
    pub prover_url: String,
    pub census_dir: PathBuf,
    /// Parent of the node's datadir and log.
    pub dir: PathBuf,
    /// `anvil` or `beacon:<url>`.
    pub blob_source: String,
    pub confirmations: u64,
    /// Chain poll interval, e.g. `1s`.
    pub poll: String,
    pub batch_max: usize,
    /// e.g. `3s`.
    pub batch_time: String,
    /// First block the monitor scans on a fresh datadir
    /// (`DAVINCI_START_BLOCK`; ignored by nodes that lack it).
    pub start_block: Option<u64>,
}

impl NodeConfig {
    /// The anvil defaults: 8 votes or 3 s per batch, no confirmations, 1 s polls.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: &str,
        privkey_file: Option<PathBuf>,
        address: Option<Address>,
        rpc_url: &str,
        registry: Address,
        prover_url: &str,
        census_dir: &Path,
        dir: &Path,
    ) -> NodeConfig {
        NodeConfig {
            name: name.into(),
            privkey_file,
            address,
            rpc_url: rpc_url.into(),
            registry,
            prover_url: prover_url.into(),
            census_dir: census_dir.into(),
            dir: dir.into(),
            blob_source: "anvil".into(),
            confirmations: 0,
            poll: "1s".into(),
            batch_max: 8,
            batch_time: "3s".into(),
            start_block: None,
        }
    }
}

/// Counters from `GET /info`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub settled_by_self: u64,
    pub synced_from_others: u64,
    /// `None` when the node does not publish it.
    pub lost_races: Option<u64>,
}

/// A running node; killed on drop.
pub struct Node {
    pub name: String,
    pub url: String,
    pub api: SequencerClient,
    /// Settling account; `None` for an observer.
    pub address: Option<Address>,
    pub log: PathBuf,
    child: Mutex<Child>,
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Ok(c) = self.child.get_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Node {
    /// Starts the node and waits for `GET /ping`.
    pub async fn spawn(bin: &Path, cfg: &NodeConfig, port: u16) -> Result<Node> {
        let datadir = cfg.dir.join(format!("{}-data", cfg.name));
        std::fs::create_dir_all(&datadir)?;
        let log = cfg.dir.join(format!("{}.log", cfg.name));
        // Appends, so a restarted node keeps its first run's log.
        let out = File::options().create(true).append(true).open(&log)?;
        let level = std::env::var("DAVINCI_E2E_NODE_LOG").unwrap_or_else(|_| "info".into());
        let mut cmd = Command::new(bin);
        cmd.env_clear();
        if let Some(k) = &cfg.privkey_file {
            cmd.env("DAVINCI_PRIVKEY_FILE", k);
        }
        if let Some(b) = cfg.start_block {
            cmd.env("DAVINCI_START_BLOCK", b.to_string());
        }
        let child = cmd
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &cfg.dir)
            .env("RUST_BACKTRACE", "1")
            .env("NO_COLOR", "1")
            .env("DAVINCI_DATADIR", &datadir)
            .env("DAVINCI_API_HOST", "127.0.0.1")
            .env("DAVINCI_API_PORT", port.to_string())
            .env("DAVINCI_RPC_URL", &cfg.rpc_url)
            .env("DAVINCI_REGISTRY", cfg.registry.to_string())
            .env("DAVINCI_BLOB_SOURCE", &cfg.blob_source)
            .env("DAVINCI_PROVER_URL", &cfg.prover_url)
            .env("DAVINCI_BATCH_MAX", cfg.batch_max.to_string())
            .env("DAVINCI_BATCH_TIME", &cfg.batch_time)
            .env("DAVINCI_SETTLE_MARGIN", "20s")
            .env("DAVINCI_CONFIRMATIONS", cfg.confirmations.to_string())
            .env("DAVINCI_CENSUS_DIR", &cfg.census_dir)
            .env("DAVINCI_POLL_INTERVAL", &cfg.poll)
            .env("DAVINCI_LOG_LEVEL", level)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;
        let url = format!("http://127.0.0.1:{port}");
        let node = Node {
            name: cfg.name.clone(),
            api: SequencerClient::new(&url),
            url,
            address: cfg.address,
            log,
            child: Mutex::new(child),
        };
        let start = Instant::now();
        loop {
            if node.api.ping().await.is_ok() {
                // Something answers; it must be our child, not a stranger
                // that won the port (the caller also checks /info).
                node.check_alive()?;
                return Ok(node);
            }
            node.check_alive()?;
            if start.elapsed() > Duration::from_secs(60) {
                bail!(
                    "node {} not answering after 60 s; log {}",
                    node.name,
                    node.log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Starts the node on a free port, retrying twice with another port if
    /// it dies before answering (the port may be taken between the probe
    /// and the node's bind).
    pub async fn start(bin: &Path, cfg: &NodeConfig) -> Result<Node> {
        let mut last = None;
        for _ in 0..3 {
            match Node::spawn(bin, cfg, crate::chain::free_port()?).await {
                Ok(n) => return Ok(n),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("node {} did not start", cfg.name)))
    }

    /// `GET /processes/{pid}` as raw JSON, for fields the client types may
    /// not carry yet.
    pub async fn process_json(&self, pid: &[u8; 31]) -> Result<serde_json::Value> {
        Ok(
            reqwest::get(format!("{}/processes/0x{}", self.url, hex::encode(pid)))
                .await?
                .error_for_status()?
                .json()
                .await?,
        )
    }

    /// Kills the process and waits for it (frees the datadir lock for a
    /// restart on the same config).
    pub fn kill(&self) -> Result<()> {
        let mut c = self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("node handle poisoned"))?;
        c.kill()?;
        c.wait()?;
        Ok(())
    }

    /// Fails if the process has exited.
    pub fn check_alive(&self) -> Result<()> {
        let mut c = self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("node handle poisoned"))?;
        if let Some(st) = c.try_wait()? {
            bail!(
                "node {} exited with {st}; log {}",
                self.name,
                self.log.display()
            );
        }
        Ok(())
    }

    /// The per-transaction blob cap the node logged at startup
    /// (`blob cap per transaction` with `cap=N`), if it did.
    /// Log lines containing any of `needles`, colours stripped.
    pub fn log_lines(&self, needles: &[&str]) -> Vec<String> {
        let log = strip_ansi(&std::fs::read_to_string(&self.log).unwrap_or_default());
        log.lines()
            .filter(|l| needles.iter().any(|n| l.contains(n)))
            .map(String::from)
            .collect()
    }

    pub fn logged_blob_cap(&self) -> Option<usize> {
        let log = strip_ansi(&std::fs::read_to_string(&self.log).ok()?);
        log.lines()
            .rev()
            .filter(|l| l.contains("blob cap per transaction"))
            .find_map(|l| {
                let v = l.split("cap=").nth(1)?;
                let d: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
                d.parse().ok()
            })
    }

    /// The counters, read from the raw `/info` JSON so an extra counter
    /// (`lostRaces`) is picked up when present.
    pub async fn metrics(&self) -> Result<Metrics> {
        let v: serde_json::Value = reqwest::get(format!("{}/info", self.url))
            .await?
            .error_for_status()?
            .json()
            .await?;
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64());
        Ok(Metrics {
            settled_by_self: n("settledBySelf").context("/info without settledBySelf")?,
            synced_from_others: n("syncedFromOthers").context("/info without syncedFromOthers")?,
            lost_races: n("lostRaces"),
        })
    }
}

// Drops `ESC [ ... m` colour sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            for c in it.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_colour() {
        let l = "INFO \x1b[3mcap\x1b[0m\x1b[2m=\x1b[0m2 blob cap per transaction";
        assert_eq!(super::strip_ansi(l), "INFO cap=2 blob cap per transaction");
    }
}
