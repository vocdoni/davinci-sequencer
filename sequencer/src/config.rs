//! Node configuration: flags with `DAVINCI_*` environment fallbacks.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::Address;
use clap::Parser;
use davinci_client::networks::{self, Network};
use davinci_zkvm_sdk::limits::{MAX_BATCH_SIZE, TX_BLOB_CAP};
use url::Url;
use zeroize::Zeroize;

/// `--confirmations` when neither the flag nor a network sets it.
pub const DEFAULT_CONFIRMATIONS: u64 = 2;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid {0}: {1}")]
    Invalid(&'static str, String),
}

/// A string that never prints (Debug and Display are redacted) and is
/// zeroed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        SecretString(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// `--network`: a known deployment, or `custom` for explicit settings only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkChoice {
    Known(&'static Network),
    Custom,
}

impl NetworkChoice {
    pub fn preset(&self) -> Option<&'static Network> {
        match self {
            NetworkChoice::Known(n) => Some(n),
            NetworkChoice::Custom => None,
        }
    }
}

impl FromStr for NetworkChoice {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().eq_ignore_ascii_case("custom") {
            return Ok(NetworkChoice::Custom);
        }
        networks::by_name(s)
            .map(NetworkChoice::Known)
            .ok_or_else(|| {
                let known: Vec<&str> = networks::NETWORKS.iter().map(|n| n.name).collect();
                ConfigError::Invalid(
                    "network",
                    format!("{s}: want {} or custom", known.join(", ")),
                )
            })
    }
}

impl fmt::Display for NetworkChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkChoice::Known(n) => f.write_str(n.name),
            NetworkChoice::Custom => f.write_str("custom"),
        }
    }
}

/// Where the node fetches blobs of transitions it did not send.
#[derive(Clone, PartialEq, Eq)]
pub enum BlobSourceKind {
    /// Consensus-layer beacon APIs, in order of preference.
    Beacon(Vec<Url>),
    /// anvil's `anvil_getBlobsByTransactionHash` on the RPC endpoint.
    Anvil,
}

impl FromStr for BlobSourceKind {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "anvil" {
            return Ok(BlobSourceKind::Anvil);
        }
        let urls = s.strip_prefix("beacon:").ok_or_else(|| {
            ConfigError::Invalid(
                "blob source",
                "want beacon:<url>[,<url>...] or anvil".into(),
            )
        })?;
        let urls = urls
            .split(',')
            .map(|u| {
                let u = Url::parse(u.trim())
                    .map_err(|e| ConfigError::Invalid("beacon url", e.to_string()))?;
                if !matches!(u.scheme(), "http" | "https") {
                    return Err(ConfigError::Invalid("beacon url", "not http(s)".into()));
                }
                Ok(u)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BlobSourceKind::Beacon(urls))
    }
}

impl fmt::Display for BlobSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlobSourceKind::Beacon(us) => {
                let us: Vec<&str> = us.iter().map(Url::as_str).collect();
                write!(f, "beacon:{}", us.join(","))
            }
            BlobSourceKind::Anvil => f.write_str("anvil"),
        }
    }
}

// URLs may carry API keys in the path or query: Debug shows hosts only.
struct Hosts<'a>(&'a [Url]);

impl fmt::Debug for Hosts<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(crate::web3::host))
            .finish()
    }
}

impl fmt::Debug for BlobSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlobSourceKind::Beacon(us) => f.debug_tuple("Beacon").field(&Hosts(us)).finish(),
            BlobSourceKind::Anvil => f.write_str("Anvil"),
        }
    }
}

/// `90`, `90s`, `500ms`, `5m` or `2h`.
pub fn parse_duration(s: &str) -> Result<Duration, ConfigError> {
    let bad = || ConfigError::Invalid("duration", s.to_string());
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| bad())?;
    let d = match unit {
        "" | "s" => Duration::from_secs(n),
        "ms" => Duration::from_millis(n),
        "m" => Duration::from_secs(n.checked_mul(60).ok_or_else(bad)?),
        "h" => Duration::from_secs(n.checked_mul(3600).ok_or_else(bad)?),
        _ => return Err(bad()),
    };
    Ok(d)
}

fn parse_http_url(s: &str) -> Result<Url, ConfigError> {
    let u = Url::parse(s).map_err(|e| ConfigError::Invalid("url", e.to_string()))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(ConfigError::Invalid("url", format!("{s}: not http(s)")));
    }
    Ok(u)
}

fn parse_batch_max(s: &str) -> Result<usize, ConfigError> {
    let n: usize = s
        .parse()
        .map_err(|_| ConfigError::Invalid("batch max", s.to_string()))?;
    if n == 0 || n > MAX_BATCH_SIZE {
        return Err(ConfigError::Invalid(
            "batch max",
            format!("{n} not in 1..={MAX_BATCH_SIZE}"),
        ));
    }
    Ok(n)
}

fn parse_blob_cap(s: &str) -> Result<usize, ConfigError> {
    let n: usize = s
        .parse()
        .map_err(|_| ConfigError::Invalid("max blobs per tx", s.to_string()))?;
    if n == 0 || n > TX_BLOB_CAP {
        return Err(ConfigError::Invalid(
            "max blobs per tx",
            format!("{n} not in 1..={TX_BLOB_CAP}"),
        ));
    }
    Ok(n)
}

/// The node's settings. Parse with [`Config::load`] or [`Config::parse_args`]:
/// both resolve `--network` into `registry`, `rpc_url`, `blob_source`,
/// `confirmations` and `start_block`, which a bare `try_parse_from` leaves
/// unset.
#[derive(Clone, Parser)]
#[command(name = "davinci-sequencer", version, about = "DAVINCI sequencer node")]
pub struct Config {
    /// Known deployment that supplies every chain setting not given
    /// explicitly (registry, start block, RPCs, blob source, confirmations),
    /// or `custom` to use only explicit settings.
    #[arg(long, env = "DAVINCI_NETWORK", default_value = networks::DEFAULT.name)]
    pub network: NetworkChoice,
    /// Data directory; each deployment keeps its redb file in a
    /// `<chain id>-<registry>` subdirectory.
    #[arg(long, env = "DAVINCI_DATADIR", default_value = "~/.davinci-sequencer")]
    pub datadir: PathBuf,
    #[arg(long, env = "DAVINCI_API_HOST", default_value = "0.0.0.0")]
    pub api_host: String,
    #[arg(long, env = "DAVINCI_API_PORT", default_value_t = 9090)]
    pub api_port: u16,
    /// Execution-layer JSON-RPC endpoints, comma-separated, in order of
    /// preference; the node sticks to one and fails over on node-side errors.
    /// Default: the network's.
    #[arg(long = "rpc-url", env = "DAVINCI_RPC_URL", value_name = "URL",
        value_parser = parse_http_url, value_delimiter = ',')]
    rpc_url_arg: Vec<Url>,
    /// Resolved RPC endpoints.
    #[arg(skip)]
    pub rpc_url: Vec<Url>,
    /// Hex secp256k1 key that signs settlement transactions, from the
    /// `DAVINCI_PRIVKEY` environment variable or `--privkey-file` (never a
    /// command-line value, which other users can read). Without it the node
    /// runs as an observer.
    #[arg(skip)]
    pub privkey: Option<SecretString>,
    /// File holding the hex signing key; the `DAVINCI_PRIVKEY` environment
    /// variable is the alternative. Unset: observer mode.
    #[arg(long, env = "DAVINCI_PRIVKEY_FILE")]
    pub privkey_file: Option<PathBuf>,
    /// ProcessRegistry address. Default: the network's.
    #[arg(long = "registry", env = "DAVINCI_REGISTRY", value_name = "ADDRESS")]
    registry_arg: Option<Address>,
    /// Resolved registry.
    #[arg(skip = Address::ZERO)]
    pub registry: Address,
    /// `beacon:<url>[,<url>...]` or `anvil`. Default: the network's.
    #[arg(
        long = "blob-source",
        env = "DAVINCI_BLOB_SOURCE",
        value_name = "SOURCE"
    )]
    blob_source_arg: Option<BlobSourceKind>,
    /// Resolved blob source.
    #[arg(skip = BlobSourceKind::Anvil)]
    pub blob_source: BlobSourceKind,
    /// davinci-zkvm prover service.
    #[arg(long, env = "DAVINCI_PROVER_URL", default_value = "http://127.0.0.1:8080", value_parser = parse_http_url)]
    pub prover_url: Url,
    /// Hard cap on a batch; pending votes past it seal one without waiting.
    #[arg(long, env = "DAVINCI_BATCH_MAX", default_value = "1024", value_parser = parse_batch_max)]
    pub batch_max: usize,
    /// Most blobs one settlement transaction may carry. Unset: the chain's
    /// `eth_config` blob schedule, capped at the protocol's six.
    #[arg(long, env = "DAVINCI_MAX_BLOBS_PER_TX", value_parser = parse_blob_cap)]
    pub max_blobs_per_tx: Option<usize>,
    /// Timer: the oldest pending vote's wait before a batch of at least
    /// `min_mix` votes seals (jittered by 10%).
    #[arg(long, env = "DAVINCI_BATCH_TIME", default_value = "15m", value_parser = parse_duration)]
    pub batch_time: Duration,
    /// Fewest distinct slots the timer seals; smaller batches wait `solo_wait`.
    #[arg(long, env = "DAVINCI_MIN_MIX", default_value = "2", value_parser = clap::value_parser!(u32).range(1..))]
    pub min_mix: u32,
    /// Longest wait for a batch below `min_mix` (jittered by 10%). Default:
    /// three times `batch_time`; never below it.
    #[arg(long = "solo-wait", env = "DAVINCI_SOLO_WAIT", value_name = "DURATION", value_parser = parse_duration)]
    solo_wait_arg: Option<Duration>,
    /// Resolved solo wait.
    #[arg(skip)]
    pub solo_wait: Duration,
    /// Seal whatever is pending, any size, from this long before the end.
    #[arg(long, env = "DAVINCI_FLUSH_HORIZON", default_value = "3m", value_parser = parse_duration)]
    pub flush_horizon: Duration,
    /// Landing margin reserved after proving (submit, inclusion,
    /// confirmations): a batch is sized so its estimated proving time plus
    /// this margin ends before the window closes.
    #[arg(long, env = "DAVINCI_SETTLE_MARGIN", default_value = "60s", value_parser = parse_duration)]
    pub settle_margin: Duration,
    /// Queued votes one ballot slot may hold at once (settled in order).
    #[arg(long, env = "DAVINCI_SLOT_DEPTH", default_value = "3", value_parser = clap::value_parser!(u32).range(1..))]
    pub slot_depth: u32,
    /// Fixed term of the proving-time estimate.
    #[arg(long, env = "DAVINCI_PROVE_BASE", default_value = "30s", value_parser = parse_duration)]
    pub prove_base: Duration,
    /// Blocks behind head the monitor treats as final (reorg margin).
    /// Default: the network's, else 2.
    #[arg(
        long = "confirmations",
        env = "DAVINCI_CONFIRMATIONS",
        value_name = "N"
    )]
    confirmations_arg: Option<u64>,
    /// Resolved confirmations.
    #[arg(skip = DEFAULT_CONFIRMATIONS)]
    pub confirmations: u64,
    /// First block a fresh deployment directory scans for registry events
    /// (the registry's deployment block). Ignored once it has scanned.
    /// Default: the network's when its registry is in use, else block 0.
    #[arg(
        long = "start-block",
        env = "DAVINCI_START_BLOCK",
        value_name = "BLOCK"
    )]
    start_block_arg: Option<u64>,
    /// Resolved start block.
    #[arg(skip)]
    pub start_block: Option<u64>,
    /// Directory `file://` census URIs may read from. Unset: `file://` is refused.
    #[arg(long, env = "DAVINCI_CENSUS_DIR")]
    pub census_dir: Option<PathBuf>,
    /// Allow census downloads from loopback and private addresses (local dev).
    #[arg(long, env = "DAVINCI_CENSUS_ALLOW_PRIVATE")]
    pub census_allow_private: bool,
    /// Largest census accepted, in participants.
    #[arg(long, env = "DAVINCI_CENSUS_MAX_PARTICIPANTS", default_value_t = 1 << 22)]
    pub census_max_participants: usize,
    /// Chain polling interval.
    #[arg(long, env = "DAVINCI_POLL_INTERVAL", default_value = "5s", value_parser = parse_duration)]
    pub poll_interval: Duration,
    /// Actor heartbeat interval; the poll interval when unset.
    #[arg(long, env = "DAVINCI_HEARTBEAT", value_parser = parse_duration)]
    pub heartbeat: Option<Duration>,
    /// Prover job status polling interval.
    #[arg(long, env = "DAVINCI_PROVER_POLL", default_value = "2s", value_parser = parse_duration)]
    pub prover_poll: Duration,
    /// Longest wait for one proving job.
    #[arg(long, env = "DAVINCI_PROVER_TIMEOUT", default_value = "30m", value_parser = parse_duration)]
    pub prover_timeout: Duration,
    /// `POST /processes/keys` rate limit (keys per minute).
    #[arg(long, env = "DAVINCI_KEYS_PER_MINUTE", default_value_t = 10)]
    pub keys_per_minute: u32,
    /// Ballot proof verification key JSON; the SDK's embedded key by default.
    #[arg(long, env = "DAVINCI_BALLOT_VK")]
    pub ballot_vk: Option<PathBuf>,
    #[arg(long, env = "DAVINCI_LOG_LEVEL", default_value = "info")]
    pub log_level: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("network", &self.network.to_string())
            .field("datadir", &self.datadir)
            .field("api_host", &self.api_host)
            .field("api_port", &self.api_port)
            .field("rpc_url", &Hosts(&self.rpc_url))
            .field("privkey", &self.privkey)
            .field("privkey_file", &self.privkey_file)
            .field("registry", &self.registry)
            .field("blob_source", &self.blob_source)
            .field("prover_url", &Hosts(std::slice::from_ref(&self.prover_url)))
            .field("batch_max", &self.batch_max)
            .field("max_blobs_per_tx", &self.max_blobs_per_tx)
            .field("batch_time", &self.batch_time)
            .field("min_mix", &self.min_mix)
            .field("solo_wait", &self.solo_wait)
            .field("flush_horizon", &self.flush_horizon)
            .field("settle_margin", &self.settle_margin)
            .field("slot_depth", &self.slot_depth)
            .field("prove_base", &self.prove_base)
            .field("confirmations", &self.confirmations)
            .field("start_block", &self.start_block)
            .field("census_dir", &self.census_dir)
            .field("census_allow_private", &self.census_allow_private)
            .field("census_max_participants", &self.census_max_participants)
            .field("poll_interval", &self.poll_interval)
            .field("heartbeat", &self.heartbeat)
            .field("prover_poll", &self.prover_poll)
            .field("prover_timeout", &self.prover_timeout)
            .field("keys_per_minute", &self.keys_per_minute)
            .field("ballot_vk", &self.ballot_vk)
            .field("log_level", &self.log_level)
            .finish()
    }
}

impl Config {
    /// Parses flags and environment, resolves the network, expands a leading
    /// `~/` in paths and reads the signing key.
    pub fn load() -> Result<Self, ConfigError> {
        let mut c = Config::parse();
        c.resolve_network()?;
        c.resolve_batching()?;
        c.datadir = expand_home(&c.datadir);
        c.census_dir = c.census_dir.as_deref().map(expand_home);
        c.privkey_file = c.privkey_file.as_deref().map(expand_home);
        c.resolve_privkey(std::env::var("DAVINCI_PRIVKEY").ok().map(SecretString::new))?;
        Ok(c)
    }

    /// Parses `args` (and the environment) and resolves the network; the
    /// signing key is left to [`Config::resolve_privkey`].
    pub fn parse_args<I, T>(args: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut c = Config::try_parse_from(args)
            .map_err(|e| ConfigError::Invalid("arguments", e.to_string()))?;
        c.resolve_network()?;
        c.resolve_batching()?;
        Ok(c)
    }

    // Solo wait defaults to three batch times and never undercuts the timer.
    fn resolve_batching(&mut self) -> Result<(), ConfigError> {
        self.solo_wait = match self.solo_wait_arg {
            Some(d) => d,
            None => self.batch_time.saturating_mul(3),
        };
        if self.solo_wait < self.batch_time {
            return Err(ConfigError::Invalid(
                "solo wait",
                format!(
                    "{:?} below batch time {:?}",
                    self.solo_wait, self.batch_time
                ),
            ));
        }
        Ok(())
    }

    /// Fills the chain settings: an explicit value wins, then the network's.
    /// `custom` needs the registry, the RPC and the blob source. The
    /// network's start block applies only to its own registry.
    pub fn resolve_network(&mut self) -> Result<(), ConfigError> {
        let preset = self.network.preset();
        let need = |what: &'static str, flag: &str| {
            ConfigError::Invalid(what, format!("--network custom needs {flag}"))
        };
        self.registry = match (self.registry_arg, preset) {
            (Some(a), _) => a,
            (None, Some(n)) => n.registry,
            (None, None) => return Err(need("registry", "--registry")),
        };
        self.rpc_url = match (self.rpc_url_arg.is_empty(), preset) {
            (false, _) => self.rpc_url_arg.clone(),
            (true, Some(n)) => n
                .rpc_urls
                .iter()
                .map(|u| parse_http_url(u))
                .collect::<Result<_, _>>()?,
            (true, None) => return Err(need("rpc url", "--rpc-url")),
        };
        self.blob_source = match (&self.blob_source_arg, preset) {
            (Some(b), _) => b.clone(),
            (None, Some(n)) => n.blob_source.parse()?,
            (None, None) => return Err(need("blob source", "--blob-source")),
        };
        self.confirmations = self
            .confirmations_arg
            .or(preset.map(|n| n.confirmations))
            .unwrap_or(DEFAULT_CONFIRMATIONS);
        self.start_block = self.start_block_arg.or(preset
            .filter(|n| n.registry == self.registry)
            .map(|n| n.start_block));
        Ok(())
    }

    /// Whether `--registry` was given (not taken from the network).
    pub fn registry_explicit(&self) -> bool {
        self.registry_arg.is_some()
    }

    /// Checks the RPC's chain id against the network's. A mismatch is fatal
    /// unless the registry was given explicitly, then only a warning.
    pub fn check_chain_id(&self, chain_id: u64) -> Result<(), ConfigError> {
        let Some(n) = self.network.preset() else {
            return Ok(());
        };
        if n.chain_id == chain_id {
            return Ok(());
        }
        if self.registry_explicit() {
            tracing::warn!(
                network = n.name,
                expected = n.chain_id,
                chain_id,
                "the RPC is not on the network's chain; following the explicit --registry"
            );
            return Ok(());
        }
        Err(ConfigError::Invalid(
            "chain",
            format!(
                "the RPC is on chain {chain_id}, network {} is chain {}; fix --rpc-url, or \
                 use --network custom for another chain",
                n.name, n.chain_id
            ),
        ))
    }

    /// Sets `privkey` from the environment value or `privkey_file`; both
    /// set is an error.
    pub fn resolve_privkey(&mut self, env: Option<SecretString>) -> Result<(), ConfigError> {
        let file = match &self.privkey_file {
            Some(p) => {
                let mut raw = std::fs::read_to_string(p).map_err(|e| {
                    ConfigError::Invalid("privkey file", format!("{}: {e}", p.display()))
                })?;
                let key = SecretString::new(raw.trim());
                raw.zeroize();
                Some(key)
            }
            None => None,
        };
        self.privkey = match (env, file) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::Invalid(
                    "privkey",
                    "set DAVINCI_PRIVKEY or --privkey-file, not both".into(),
                ));
            }
            (k, None) | (None, k) => k.filter(|k| !k.expose().trim().is_empty()),
        };
        Ok(())
    }

    /// Heartbeat cadence: `--heartbeat` or the poll interval.
    pub fn heartbeat(&self) -> Duration {
        self.heartbeat.unwrap_or(self.poll_interval)
    }
}

fn expand_home(p: &std::path::Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &str = "0x5FbDB2315678afecb367f032d93F642f64180aa3";

    fn base() -> Vec<&'static str> {
        vec![
            "davinci-sequencer",
            "--network",
            "custom",
            "--registry",
            REG,
            "--blob-source",
            "anvil",
        ]
    }

    fn with_rpc() -> Vec<&'static str> {
        let mut args = base();
        args.extend(["--rpc-url", "http://127.0.0.1:8545"]);
        args
    }

    #[test]
    fn defaults_and_overrides() {
        let c = Config::parse_args(with_rpc()).unwrap();
        assert_eq!(c.network, NetworkChoice::Custom);
        assert_eq!(c.api_port, 9090);
        assert_eq!(c.batch_max, 1024);
        assert_eq!(c.max_blobs_per_tx, None);
        assert_eq!(c.batch_time, Duration::from_secs(900));
        assert_eq!(c.min_mix, 2);
        assert_eq!(c.solo_wait, Duration::from_secs(2700));
        assert_eq!(c.flush_horizon, Duration::from_secs(180));
        assert_eq!(c.settle_margin, Duration::from_secs(60));
        assert_eq!(c.slot_depth, 3);
        assert_eq!(c.prove_base, Duration::from_secs(30));
        assert_eq!(c.blob_source, BlobSourceKind::Anvil);
        assert!(c.privkey.is_none());

        assert_eq!(c.confirmations, 2);
        assert_eq!(c.rpc_url, [Url::parse("http://127.0.0.1:8545").unwrap()]);
        assert_eq!(c.start_block, None);
        assert!(c.census_dir.is_none() && !c.census_allow_private);
        let mut args = with_rpc();
        args.extend([
            "--batch-max",
            "8",
            "--batch-time",
            "3s",
            "--poll-interval",
            "500ms",
            "--max-blobs-per-tx",
            "2",
            "--start-block",
            "41000000",
        ]);
        let c = Config::parse_args(args).unwrap();
        assert_eq!(c.start_block, Some(41_000_000));
        assert_eq!(c.batch_max, 8);
        assert_eq!(c.max_blobs_per_tx, Some(2));
        assert_eq!(c.poll_interval, Duration::from_millis(500));
        assert_eq!(c.solo_wait, Duration::from_secs(9));
        let mut args = with_rpc();
        args.extend([
            "--batch-time",
            "3s",
            "--solo-wait",
            "3s",
            "--min-mix",
            "1",
            "--slot-depth",
            "1",
        ]);
        let c = Config::parse_args(args).unwrap();
        assert_eq!(
            (c.solo_wait, c.min_mix, c.slot_depth),
            (Duration::from_secs(3), 1, 1)
        );
    }

    #[test]
    fn rpc_url_list() {
        let mut args = base();
        args.extend(["--rpc-url", "http://a:8545,https://b/key"]);
        let c = Config::parse_args(args).unwrap();
        assert_eq!(c.rpc_url.len(), 2);
        assert_eq!(c.rpc_url[1].as_str(), "https://b/key");
        let mut args = base();
        args.extend(["--rpc-url", "http://a:8545,ftp://b"]);
        assert!(Config::try_parse_from(args).is_err());
    }

    #[test]
    fn key_only_from_env_or_file() {
        // No command-line value.
        let mut args = base();
        args.extend(["--privkey", "deadbeef"]);
        assert!(Config::try_parse_from(args).is_err());

        let mut c = Config::parse_args(with_rpc()).unwrap();
        c.resolve_privkey(Some(SecretString::new("deadbeef")))
            .unwrap();
        assert_eq!(c.privkey.as_ref().map(|s| s.expose()), Some("deadbeef"));
        // The key never shows up in Debug output, nor URL paths or queries.
        assert!(!format!("{c:?}").contains("deadbeef"));
        let args = [
            "davinci-sequencer",
            "--registry",
            "0x5FbDB2315678afecb367f032d93F642f64180aa3",
            "--rpc-url",
            "https://rpc.example.org/v3/rpckey,http://10.0.0.1:8545/?k=rpc2",
            "--blob-source",
            "beacon:https://beacon.example.org/beaconkey",
            "--prover-url",
            "https://prover.example.org/proverkey",
        ];
        let d = format!("{:?}", Config::parse_args(args).unwrap());
        for key in ["rpckey", "rpc2", "beaconkey", "proverkey"] {
            assert!(!d.contains(key), "{key} in {d}");
        }
        for host in ["rpc.example.org", "10.0.0.1:8545", "beacon.example.org"] {
            assert!(d.contains(host), "{host} missing in {d}");
        }

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("key");
        std::fs::write(&f, "  c0ffee\n").unwrap();
        let mut args = with_rpc();
        let fs = f.display().to_string();
        args.extend(["--privkey-file", &fs]);
        let mut c = Config::parse_args(args).unwrap();
        c.resolve_privkey(None).unwrap();
        assert_eq!(c.privkey.as_ref().map(|s| s.expose()), Some("c0ffee"));
        assert!(c.resolve_privkey(Some(SecretString::new("x"))).is_err());
        c.privkey_file = None;
        c.resolve_privkey(None).unwrap();
        assert!(c.privkey.is_none());
    }

    #[test]
    fn rejects_bad_values() {
        for (flag, v) in [
            ("--batch-max", "0"),
            ("--batch-max", "1025"),
            ("--max-blobs-per-tx", "0"),
            ("--max-blobs-per-tx", "7"),
            ("--batch-time", "5 minutes"),
            ("--rpc-url", "ftp://x"),
            ("--blob-source", "beacon:not a url"),
            ("--blob-source", "ipfs"),
            ("--network", "mainnet"),
            ("--min-mix", "0"),
            ("--slot-depth", "0"),
            ("--prove-base", "-1s"),
        ] {
            let mut args = base();
            args.extend([flag, v]);
            assert!(Config::try_parse_from(args).is_err(), "{flag} {v}");
        }
        // The solo wait never undercuts the timer.
        let mut args = with_rpc();
        args.extend(["--batch-time", "10s", "--solo-wait", "9s"]);
        assert!(Config::parse_args(args).is_err());
    }

    #[test]
    fn gnosis_is_the_default_network() {
        let g = &networks::GNOSIS;
        let c = Config::parse_args(["davinci-sequencer"]).unwrap();
        assert_eq!(c.network, NetworkChoice::Known(g));
        assert_eq!(c.registry, g.registry);
        assert!(!c.registry_explicit());
        assert_eq!(c.start_block, Some(g.start_block));
        assert_eq!(c.confirmations, 3);
        let rpcs: Vec<&str> = c.rpc_url.iter().map(|u| u.as_str()).collect();
        assert_eq!(
            rpcs,
            [
                "https://gnosis-rpc.publicnode.com/",
                "https://gnosis-rpc.blockreq.com/v1/rpc/public",
                "https://rpc.gnosischain.com/",
            ]
        );
        assert_eq!(c.blob_source, g.blob_source.parse().unwrap());
        assert_eq!(
            Config::parse_args(["davinci-sequencer", "--network", "GNOSIS"])
                .unwrap()
                .network,
            NetworkChoice::Known(g)
        );
    }

    // Each explicit setting replaces the network's; the network's start
    // block goes with its registry only.
    #[test]
    fn explicit_settings_override_the_network() {
        let c = Config::parse_args(["davinci-sequencer", "--registry", REG]).unwrap();
        assert!(c.registry_explicit());
        assert_eq!(c.registry, REG.parse::<Address>().unwrap());
        assert_eq!(c.start_block, None);
        assert_eq!(c.rpc_url.len(), networks::GNOSIS.rpc_urls.len());
        assert_eq!(c.confirmations, 3);
        let c = Config::parse_args([
            "davinci-sequencer",
            "--start-block",
            "7",
            "--confirmations",
            "0",
            "--rpc-url",
            "http://a:8545",
            "--blob-source",
            "anvil",
        ])
        .unwrap();
        assert_eq!(c.registry, networks::GNOSIS.registry);
        assert_eq!(c.start_block, Some(7));
        assert_eq!(c.confirmations, 0);
        assert_eq!(c.rpc_url, [Url::parse("http://a:8545").unwrap()]);
        assert_eq!(c.blob_source, BlobSourceKind::Anvil);
    }

    #[test]
    fn custom_needs_explicit_settings() {
        let custom = ["davinci-sequencer", "--network", "custom"];
        for extra in [
            &[][..],
            &["--registry", REG][..],
            &["--registry", REG, "--rpc-url", "http://a"][..],
            &["--rpc-url", "http://a", "--blob-source", "anvil"][..],
        ] {
            let args = custom.iter().chain(extra);
            assert!(Config::parse_args(args).is_err(), "{extra:?}");
        }
        let c = Config::parse_args(with_rpc()).unwrap();
        assert_eq!(c.confirmations, DEFAULT_CONFIRMATIONS);
        assert_eq!(c.start_block, None);
    }

    #[test]
    fn chain_id_must_match_the_network() {
        let c = Config::parse_args(["davinci-sequencer"]).unwrap();
        assert!(c.check_chain_id(100).is_ok());
        let e = c.check_chain_id(31337).unwrap_err().to_string();
        assert!(e.contains("31337") && e.contains("gnosis"), "{e}");
        // An explicit registry may live on another chain.
        let c = Config::parse_args(["davinci-sequencer", "--registry", REG]).unwrap();
        assert!(c.check_chain_id(31337).is_ok());
        let c = Config::parse_args(with_rpc()).unwrap();
        assert!(c.check_chain_id(31337).is_ok());
    }

    #[test]
    fn blob_source_forms() {
        assert_eq!(
            "beacon:http://localhost:5052"
                .parse::<BlobSourceKind>()
                .unwrap(),
            BlobSourceKind::Beacon(vec![Url::parse("http://localhost:5052").unwrap()])
        );
        let two = "beacon:http://a:5052/,https://b/x?k=1"
            .parse::<BlobSourceKind>()
            .unwrap();
        assert!(matches!(&two, BlobSourceKind::Beacon(us) if us.len() == 2));
        assert_eq!(two.to_string().parse::<BlobSourceKind>().unwrap(), two);
        assert!("beacon:http://a,ftp://b".parse::<BlobSourceKind>().is_err());
        assert!(parse_duration("2h").unwrap() == Duration::from_secs(7200));
    }
}
