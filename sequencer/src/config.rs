//! Node configuration: flags with `DAVINCI_*` environment fallbacks.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::Address;
use clap::Parser;
use davinci_zkvm_sdk::limits::{MAX_BATCH_SIZE, TX_BLOB_CAP};
use url::Url;
use zeroize::Zeroize;

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

#[derive(Clone, Parser)]
#[command(name = "davinci-sequencer", version, about = "DAVINCI sequencer node")]
pub struct Config {
    /// Data directory; the node keeps one redb file in it.
    #[arg(long, env = "DAVINCI_DATADIR", default_value = "~/.davinci-sequencer")]
    pub datadir: PathBuf,
    #[arg(long, env = "DAVINCI_API_HOST", default_value = "0.0.0.0")]
    pub api_host: String,
    #[arg(long, env = "DAVINCI_API_PORT", default_value_t = 9090)]
    pub api_port: u16,
    /// Execution-layer JSON-RPC endpoints, comma-separated, in order of
    /// preference; the node sticks to one and fails over on node-side errors.
    #[arg(long, env = "DAVINCI_RPC_URL", default_value = "http://127.0.0.1:8545",
        value_parser = parse_http_url, value_delimiter = ',')]
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
    /// ProcessRegistry address.
    #[arg(long, env = "DAVINCI_REGISTRY")]
    pub registry: Address,
    /// `beacon:<url>` or `anvil`.
    #[arg(long, env = "DAVINCI_BLOB_SOURCE")]
    pub blob_source: BlobSourceKind,
    /// davinci-zkvm prover service.
    #[arg(long, env = "DAVINCI_PROVER_URL", default_value = "http://127.0.0.1:8080", value_parser = parse_http_url)]
    pub prover_url: Url,
    /// Votes that seal a batch without waiting for `batch_time`.
    #[arg(long, env = "DAVINCI_BATCH_MAX", default_value = "1024", value_parser = parse_batch_max)]
    pub batch_max: usize,
    /// Most blobs one settlement transaction may carry. Unset: the chain's
    /// `eth_config` blob schedule, capped at the protocol's six.
    #[arg(long, env = "DAVINCI_MAX_BLOBS_PER_TX", value_parser = parse_blob_cap)]
    pub max_blobs_per_tx: Option<usize>,
    /// Longest a pending vote waits before its batch is sealed.
    #[arg(long, env = "DAVINCI_BATCH_TIME", default_value = "5m", value_parser = parse_duration)]
    pub batch_time: Duration,
    /// Never seal a batch when the election ends within this margin: a
    /// proof landing after the window closes would revert and lose its
    /// votes' place in line.
    #[arg(long, env = "DAVINCI_SETTLE_MARGIN", default_value = "120s", value_parser = parse_duration)]
    pub settle_margin: Duration,
    /// Blocks behind head the monitor treats as final (reorg margin).
    #[arg(long, env = "DAVINCI_CONFIRMATIONS", default_value_t = 2)]
    pub confirmations: u64,
    /// First block a fresh datadir scans for registry events (the registry's
    /// deployment block). Ignored once the datadir has scanned; unset scans
    /// from block 0.
    #[arg(long, env = "DAVINCI_START_BLOCK")]
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
            .field("settle_margin", &self.settle_margin)
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
    /// Parses flags and environment, expands a leading `~/` in paths and
    /// reads the signing key.
    pub fn load() -> Result<Self, ConfigError> {
        let mut c = Config::parse();
        c.datadir = expand_home(&c.datadir);
        c.census_dir = c.census_dir.as_deref().map(expand_home);
        c.privkey_file = c.privkey_file.as_deref().map(expand_home);
        c.resolve_privkey(std::env::var("DAVINCI_PRIVKEY").ok().map(SecretString::new))?;
        Ok(c)
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

    /// The node's redb file.
    pub fn db_path(&self) -> PathBuf {
        self.datadir.join("sequencer.redb")
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

    fn base() -> Vec<&'static str> {
        vec![
            "davinci-sequencer",
            "--registry",
            "0x5FbDB2315678afecb367f032d93F642f64180aa3",
            "--blob-source",
            "anvil",
        ]
    }

    #[test]
    fn defaults_and_overrides() {
        let c = Config::try_parse_from(base()).unwrap();
        assert_eq!(c.api_port, 9090);
        assert_eq!(c.batch_max, 1024);
        assert_eq!(c.max_blobs_per_tx, None);
        assert_eq!(c.batch_time, Duration::from_secs(300));
        assert_eq!(c.blob_source, BlobSourceKind::Anvil);
        assert!(c.privkey.is_none());

        assert_eq!(c.confirmations, 2);
        assert_eq!(c.rpc_url, [Url::parse("http://127.0.0.1:8545").unwrap()]);
        assert_eq!(c.start_block, None);
        assert!(c.census_dir.is_none() && !c.census_allow_private);
        let mut args = base();
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
        let c = Config::try_parse_from(args).unwrap();
        assert_eq!(c.start_block, Some(41_000_000));
        assert_eq!(c.batch_max, 8);
        assert_eq!(c.max_blobs_per_tx, Some(2));
        assert_eq!(c.poll_interval, Duration::from_millis(500));
    }

    #[test]
    fn rpc_url_list() {
        let mut args = base();
        args.extend(["--rpc-url", "http://a:8545,https://b/key"]);
        let c = Config::try_parse_from(args).unwrap();
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

        let mut c = Config::try_parse_from(base()).unwrap();
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
        let d = format!("{:?}", Config::try_parse_from(args).unwrap());
        for key in ["rpckey", "rpc2", "beaconkey", "proverkey"] {
            assert!(!d.contains(key), "{key} in {d}");
        }
        for host in ["rpc.example.org", "10.0.0.1:8545", "beacon.example.org"] {
            assert!(d.contains(host), "{host} missing in {d}");
        }

        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("key");
        std::fs::write(&f, "  c0ffee\n").unwrap();
        let mut args = base();
        let fs = f.display().to_string();
        args.extend(["--privkey-file", &fs]);
        let mut c = Config::try_parse_from(args).unwrap();
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
        ] {
            let mut args = base();
            args.extend([flag, v]);
            assert!(Config::try_parse_from(args).is_err(), "{flag} {v}");
        }
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
