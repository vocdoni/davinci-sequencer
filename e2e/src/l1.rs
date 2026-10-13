//! L1-blob mode (`DAVINCI_E2E_DA=l1-blobs`): the registry settles on an
//! OP-like chain while the blobs go to Ethereum. On anvil that is a second,
//! Ethereum-like anvil (Osaka, blobs) and a relayer that copies the L1 block
//! `lag` blocks behind the head into the OP anvil's `MockL1Block`, the way
//! OP's derivation moves `L1Block`; live it is Ethereum's RPCs and beacons.
//! Each sequencer gets a dedicated Ethereum key, never shared. Also the
//! lookup the gas bill and the benchmark need: which Ethereum transaction
//! carried a settlement's blobs. Key files are read in-process; only
//! addresses are printed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use alloy::consensus::Transaction as _;
use alloy::network::EthereumWallet;
use alloy::primitives::utils::{format_ether, parse_ether};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::sol_types::SolEvent;
use anyhow::{Context, Result, bail, ensure};
use davinci_client::networks::{Network, NetworkDa};
use davinci_client::organizer::ProcessRegistry;

use crate::chain::{self, Anvil};

alloy::sol! {
    /// The test stand-in for OP's `L1Block` predeploy (davinci-contracts
    /// `test/mocks/MockL1Block.sol`).
    #[sol(rpc)]
    interface MockL1Block {
        function set(uint64 n, bytes32 h) external;
        function number() external view returns (uint64);
        function hash() external view returns (bytes32);
    }
}

alloy::sol!(
    #[sol(rpc, all_derives)]
    #[allow(missing_docs, clippy::too_many_arguments)]
    L1BlobVerifier,
    "../sequencer/abi/L1BlobVerifier.json"
);

/// OP anvil account that moves `MockL1Block` (nonces of its own).
pub const RELAY_KEY: usize = 9;
/// The Ethereum anvil's chain id: Ethereum's, not the OP anvil's 31337, so a
/// blob post signed for the wrong chain is refused.
pub const L1_CHAIN_ID: u64 = 1;
/// Ethereum anvil accounts of sequencers A, B and C. Not their OP keys
/// (1..=3): a node that signed a blob post with its OP key would show.
pub const L1_KEYS: [usize; 3] = [5, 6, 7];
/// Least balance of a live sequencer's Ethereum key, unless
/// `DAVINCI_E2E_L1_MIN_BALANCE` says otherwise: about ten 6-blob posts at
/// 0.35 gwei.
pub const MIN_L1_BALANCE: &str = "0.0002";
/// The nodes' `DAVINCI_L1_MAX_POST_FEE` live, unless
/// `DAVINCI_E2E_L1_MAX_POST_FEE` says otherwise: a 6-blob post at twice
/// 0.35 gwei and twice 0.015 gwei per blob gas is about 0.00004 ETH, so this
/// leaves room for two doubled replacements.
pub const LIVE_MAX_POST_FEE: &str = "0.00015eth";

/// `DAVINCI_E2E_DA`: `blobs` (the default) or `l1-blobs`.
pub fn enabled() -> Result<bool> {
    parse_da(std::env::var("DAVINCI_E2E_DA").ok().as_deref())
}

fn parse_da(v: Option<&str>) -> Result<bool> {
    match v.map(str::trim) {
        None | Some("") | Some("blobs") => Ok(false),
        Some("l1-blobs") => Ok(true),
        Some(v) => bail!("DAVINCI_E2E_DA={v}: blobs or l1-blobs"),
    }
}

fn env_num(k: &str, default: u64) -> Result<u64> {
    match std::env::var(k).ok().filter(|v| !v.is_empty()) {
        Some(v) => v.trim().parse().with_context(|| format!("{k}={v}")),
        None => Ok(default),
    }
}

/// The L1 anvil's block time in seconds (`DAVINCI_E2E_L1_BLOCK_TIME`,
/// default 2) and the relayer's lag in L1 blocks (`DAVINCI_E2E_L1_LAG`,
/// default 6, about OP Mainnet's).
pub fn anvil_timing() -> Result<(u64, u64)> {
    let block_time = env_num("DAVINCI_E2E_L1_BLOCK_TIME", 2)?;
    let lag = env_num("DAVINCI_E2E_L1_LAG", 6)?;
    ensure!(
        block_time >= 1,
        "DAVINCI_E2E_L1_BLOCK_TIME must be at least 1"
    );
    // The node re-posts past 64 headers without a recorded origin.
    ensure!(lag <= 48, "DAVINCI_E2E_L1_LAG {lag}: at most 48");
    Ok((block_time, lag))
}

/// The origin a relayer sets at L1 head `head`: `lag` blocks behind it, and
/// never block 0, which `MockL1Block` already reports with a zero hash.
pub fn relay_target(head: u64, lag: u64) -> Option<u64> {
    head.checked_sub(lag).filter(|n| *n >= 1)
}

/// Copies the Ethereum block `lag` behind the L1 head into `MockL1Block`
/// every 250 ms. Stops on drop.
pub struct Relay {
    task: tokio::task::JoinHandle<()>,
    origin: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Relay {
    /// `l1` reads Ethereum; `op` signs the `set` calls with its own key.
    pub fn start(l1: DynProvider, op: DynProvider, mock: Address, lag: u64) -> Relay {
        let origin = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let (o, e) = (origin.clone(), errors.clone());
        let task = tokio::spawn(async move {
            let m = MockL1Block::new(mock, &op);
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let step = async {
                    let head = l1.get_block_number().await?;
                    let Some(n) = relay_target(head, lag) else {
                        return anyhow::Ok(());
                    };
                    if n <= o.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    let b = l1
                        .get_block_by_number(n.into())
                        .await?
                        .with_context(|| format!("L1 block {n}"))?;
                    let r = m.set(n, b.header.hash).send().await?.get_receipt().await?;
                    ensure!(r.status(), "MockL1Block.set reverted");
                    o.store(n, Ordering::SeqCst);
                    Ok(())
                };
                if let Err(err) = step.await {
                    // A run of failures is one line per 20.
                    if e.fetch_add(1, Ordering::SeqCst) % 20 == 0 {
                        eprintln!("L1 relayer: {err:#}");
                    }
                }
            }
        });
        Relay {
            task,
            origin,
            errors,
        }
    }

    /// The last origin it set (0 before the first).
    pub fn origin(&self) -> u64 {
        self.origin.load(Ordering::SeqCst)
    }

    /// Failed relay steps so far.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::SeqCst)
    }
}

/// A pending transaction as anvil's pool shows it, fees in wei.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTx {
    pub hash: B256,
    pub max_fee: u128,
    pub tip: u128,
    pub blob_fee: u128,
}

/// The transaction of `from` at `nonce` in a `txpool_content` answer.
pub fn pending_in(
    pool: &serde_json::Value,
    from: Address,
    nonce: u64,
) -> Result<Option<PendingTx>> {
    let Some(by_sender) = pool["pending"].as_object() else {
        return Ok(None);
    };
    let want = from.to_string().to_ascii_lowercase();
    let Some(txs) = by_sender
        .iter()
        .find(|(a, _)| a.to_ascii_lowercase() == want)
        .map(|(_, v)| v)
    else {
        return Ok(None);
    };
    let Some(t) = txs.get(nonce.to_string()) else {
        return Ok(None);
    };
    let fee = |k: &str| -> Result<u128> {
        let v = t[k]
            .as_str()
            .with_context(|| format!("pending tx without {k}"))?;
        Ok(u128::from_str_radix(v.trim_start_matches("0x"), 16)?)
    };
    Ok(Some(PendingTx {
        hash: t["hash"]
            .as_str()
            .context("pending tx without hash")?
            .parse()?,
        max_fee: fee("maxFeePerGas")?,
        tip: fee("maxPriorityFeePerGas")?,
        blob_fee: fee("maxFeePerBlobGas")?,
    }))
}

/// The Ethereum anvil of an L1-blob run and its relayer.
pub struct L1Anvil {
    /// Declared first, so the relayer stops before the chain goes.
    pub relay: Relay,
    pub anvil: Anvil,
    pub block_time: u64,
    pub lag: u64,
}

impl L1Anvil {
    fn provider(&self) -> Result<DynProvider> {
        Ok(ProviderBuilder::new()
            .connect_client(chain::rpc(&self.anvil.url)?)
            .erased())
    }

    /// Stops block production; sent transactions stay in the pool.
    pub async fn pause_mining(&self) -> Result<()> {
        let _: serde_json::Value = self
            .provider()?
            .raw_request("anvil_setIntervalMining".into(), (0u64,))
            .await?;
        Ok(())
    }

    /// A block every `block_time` seconds again.
    pub async fn resume_mining(&self) -> Result<()> {
        let _: serde_json::Value = self
            .provider()?
            .raw_request("anvil_setIntervalMining".into(), (self.block_time,))
            .await?;
        Ok(())
    }

    /// The pending transaction of `from` at `nonce`, if any.
    pub async fn pending(&self, from: Address, nonce: u64) -> Result<Option<PendingTx>> {
        let pool: serde_json::Value = self
            .provider()?
            .raw_request("txpool_content".into(), ())
            .await?;
        pending_in(&pool, from, nonce)
    }
}

/// Ethereum as an L1-blob run sees it.
pub struct L1Net {
    /// The nodes' `DAVINCI_L1_RPC_URL`, a `,` failover list; the harness
    /// reads from its first entry.
    pub rpc: String,
    /// The nodes' `DAVINCI_L1_SEND_RPC_URL`, if set.
    pub send_rpc: Option<String>,
    /// Dedicated Ethereum key file and address of each sequencer, parallel
    /// to `Net::sequencers`.
    pub keys: Vec<(PathBuf, Address)>,
    /// The registry's `L1BlobVerifier` and the `L1Block` it reads; live,
    /// filled in by `Net::open` from the registry.
    pub verifier: Address,
    pub l1_block: Address,
    /// The nodes' `DAVINCI_L1_MAX_POST_FEE`; `None` keeps the node's default.
    pub max_post_fee: Option<String>,
    /// Least Ethereum balance of each sequencer key.
    pub min_balance: U256,
    /// On anvil: the Ethereum anvil and its relayer.
    pub anvil: Option<L1Anvil>,
}

impl L1Net {
    /// The harness's Ethereum RPC: the first of `rpc`.
    pub fn read_rpc(&self) -> &str {
        self.rpc.split(',').next().unwrap_or(&self.rpc).trim()
    }

    pub fn provider(&self) -> Result<DynProvider> {
        Ok(ProviderBuilder::new()
            .connect_client(chain::rpc(self.read_rpc())?)
            .erased())
    }

    /// Prints the first `n` sequencer keys' Ethereum balances; fails below
    /// `min_balance`.
    pub async fn check_balances(&self, n: usize) -> Result<()> {
        ensure!(
            self.keys.len() >= n,
            "{n} Ethereum keys needed, {} given",
            self.keys.len()
        );
        let p = self.provider()?;
        let mut low = Vec::new();
        for (i, (_, a)) in self.keys[..n].iter().enumerate() {
            let b = p.get_balance(*a).await?;
            eprintln!("sequencer {} L1 {a} balance {}", i + 1, format_ether(b));
            if b < self.min_balance {
                low.push(format!("sequencer {} {a}", i + 1));
            }
        }
        if !low.is_empty() {
            bail!(
                "below {} ETH on Ethereum: {}",
                format_ether(self.min_balance),
                low.join(", ")
            );
        }
        Ok(())
    }
}

/// On anvil: the Ethereum anvil in `dir`, its relayer moving the OP anvil's
/// `MockL1Block` of `dep`, and the sequencers' Ethereum key files.
pub async fn anvil_net(dir: &Path, op_url: &str, dep: &chain::Deployment) -> Result<L1Net> {
    let (block_time, lag) = anvil_timing()?;
    let anvil = Anvil::start_with(&dir.join("anvil-l1.log"), block_time, Some(L1_CHAIN_ID)).await?;
    let l1 = ProviderBuilder::new()
        .connect_client(chain::rpc(&anvil.url)?)
        .erased();
    let op = ProviderBuilder::new()
        .wallet(EthereumWallet::from(chain::signer(RELAY_KEY)?))
        .connect_client(chain::rpc(op_url)?)
        .erased();
    let relay = Relay::start(l1, op, dep.l1_block, lag);
    let keys_dir = dir.join("keys");
    std::fs::create_dir_all(&keys_dir)?;
    let mut keys = Vec::new();
    for (i, k) in L1_KEYS.into_iter().enumerate() {
        let p = keys_dir.join(format!("seq{}.l1.key", i + 1));
        std::fs::write(&p, chain::DEV_KEYS[k])?;
        keys.push((p, chain::signer(k)?.address()));
    }
    eprintln!(
        "L1-blob mode: Ethereum anvil {} (chain {L1_CHAIN_ID}, {block_time} s blocks), OP origin {lag} blocks behind",
        anvil.url
    );
    Ok(L1Net {
        rpc: anvil.url.clone(),
        send_rpc: None,
        keys,
        verifier: dep.l1_blob_verifier,
        l1_block: dep.l1_block,
        max_post_fee: None,
        min_balance: parse_ether("1")?,
        anvil: Some(L1Anvil {
            relay,
            anvil,
            block_time,
            lag,
        }),
    })
}

fn env_or(k: &str, default: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Live: Ethereum from the env, defaults from `preset`.
/// `DAVINCI_E2E_L1_RPC` (a `,` list), `DAVINCI_E2E_L1_SEND_RPC`,
/// `DAVINCI_E2E_L1_KEYS` (key files, one per sequencer, in the order of
/// `DAVINCI_E2E_SEQUENCER_KEYS`), `DAVINCI_E2E_L1_MAX_POST_FEE`,
/// `DAVINCI_E2E_L1_MIN_BALANCE` (ETH).
pub fn live_net(preset: &Network, organizer: Address) -> Result<L1Net> {
    let default_rpcs = match preset.da {
        NetworkDa::L1Blobs { l1_rpc_urls, .. } => l1_rpc_urls.join(","),
        NetworkDa::Blobs => String::new(),
    };
    let rpc = env_or("DAVINCI_E2E_L1_RPC", &default_rpcs);
    ensure!(
        !rpc.is_empty(),
        "DAVINCI_E2E_L1_RPC (Ethereum RPCs) is required live in L1-blob mode"
    );
    let send_rpc = std::env::var("DAVINCI_E2E_L1_SEND_RPC")
        .ok()
        .filter(|v| !v.is_empty());
    let list = std::env::var("DAVINCI_E2E_L1_KEYS")
        .context("DAVINCI_E2E_L1_KEYS (Ethereum key files) is required live in L1-blob mode")?;
    let min_balance = parse_ether(&env_or("DAVINCI_E2E_L1_MIN_BALANCE", MIN_L1_BALANCE))
        .context("DAVINCI_E2E_L1_MIN_BALANCE")?;
    Ok(L1Net {
        rpc,
        send_rpc,
        keys: live_keys(&list, organizer)?,
        verifier: Address::ZERO,
        l1_block: Address::ZERO,
        max_post_fee: Some(env_or("DAVINCI_E2E_L1_MAX_POST_FEE", LIVE_MAX_POST_FEE)),
        min_balance,
        anvil: None,
    })
}

/// The Ethereum key files of a `,` list and their addresses. None may be
/// the organizer's account, whose Ethereum nonces belong to its owner, and
/// none may repeat: the blob pool wants one sender per node.
pub fn live_keys(list: &str, organizer: Address) -> Result<Vec<(PathBuf, Address)>> {
    let mut keys: Vec<(PathBuf, Address)> = Vec::new();
    for p in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        let a = crate::net::read_key(&p)?.address();
        ensure!(a != organizer, "{}: the organizer's account", p.display());
        ensure!(
            keys.iter().all(|(_, b)| *b != a),
            "{}: an Ethereum key given twice",
            p.display()
        );
        keys.push((p, a));
    }
    ensure!(!keys.is_empty(), "DAVINCI_E2E_L1_KEYS lists no key file");
    Ok(keys)
}

/// One L1-mode settlement and the Ethereum blob transactions behind it.
#[derive(Clone, Debug)]
pub struct L1Post {
    pub pid: [u8; 31],
    pub op_tx: B256,
    pub op_block: u64,
    /// One entry per blob, as `ProcessStateTransitionedL1` lists them.
    pub l1_blocks: Vec<u64>,
    pub vhs: Vec<B256>,
    /// The Ethereum transactions that carry `vhs`, in block order.
    pub l1_txs: Vec<B256>,
}

/// Every `ProcessStateTransitionedL1` of `registry` from OP block `from`,
/// with the Ethereum transactions (read from `l1_rpc`) that carried each
/// settlement's blobs.
pub async fn posts(
    op_rpc: &str,
    l1_rpc: &str,
    registry: Address,
    from: u64,
) -> Result<Vec<L1Post>> {
    use ProcessRegistry::ProcessStateTransitionedL1 as Ev;
    let l1 = ProviderBuilder::new().connect_client(chain::rpc(l1_rpc)?);
    let mut out = Vec::new();
    for l in chain::registry_logs(op_rpc, registry, Ev::SIGNATURE_HASH, from).await? {
        let d = l.log_decode::<Ev>()?.inner.data;
        let l1_blocks = d
            .l1BlockNumbers
            .iter()
            .map(|n| u64::try_from(*n))
            .collect::<std::result::Result<Vec<u64>, _>>()?;
        let vhs = d.versionedHashes.clone();
        let mut blocks = l1_blocks.clone();
        blocks.dedup();
        let mut l1_txs = Vec::new();
        for b in blocks {
            let blk = l1
                .get_block_by_number(b.into())
                .full()
                .await?
                .with_context(|| format!("L1 block {b}"))?;
            for t in blk.transactions.txns() {
                let carries = t
                    .inner
                    .blob_versioned_hashes()
                    .is_some_and(|hs| hs.iter().any(|h| vhs.contains(h)));
                let h = *t.inner.tx_hash();
                if carries && !l1_txs.contains(&h) {
                    l1_txs.push(h);
                }
            }
        }
        let op_tx = l.transaction_hash.context("log without tx hash")?;
        ensure!(
            !l1_txs.is_empty(),
            "no Ethereum transaction carries the blobs of {op_tx}"
        );
        out.push(L1Post {
            pid: d.processId.0,
            op_tx,
            op_block: l.block_number.context("log without block")?,
            l1_blocks,
            vhs,
            l1_txs,
        });
    }
    Ok(out)
}

/// Waits until the relayer has moved `MockL1Block` and its hash is the
/// Ethereum block's: the mode's plumbing, before any node runs.
pub async fn check_origin(op_rpc: &str, l1: &L1Net) -> Result<u64> {
    let op = ProviderBuilder::new()
        .connect_client(chain::rpc(op_rpc)?)
        .erased();
    let m = MockL1Block::new(l1.l1_block, &op);
    let eth = l1.provider()?;
    crate::wait::until(
        "the relayer to move OP's origin",
        Duration::from_secs(60),
        Duration::from_millis(500),
        || async {
            let n = m.number().call().await?;
            if n == 0 {
                return Ok(None);
            }
            let h = m.hash().call().await?;
            let b = eth
                .get_block_by_number(n.into())
                .await?
                .with_context(|| format!("L1 block {n}"))?;
            ensure!(
                b.header.hash == h,
                "MockL1Block holds {h} for block {n}, Ethereum {}",
                b.header.hash
            );
            Ok(Some(n))
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_lags_the_head() {
        assert_eq!(relay_target(10, 6), Some(4));
        assert_eq!(relay_target(6, 6), None);
        assert_eq!(relay_target(5, 6), None);
        assert_eq!(relay_target(1, 0), Some(1));
        assert_eq!(relay_target(0, 0), None);
    }

    #[test]
    fn pending_tx_from_the_pool() {
        let from: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
            .parse()
            .unwrap();
        let h = "0x804c76e506967ae08690a10fa66b065af539bb01b1ec063f2f4db10eca896884";
        let pool = serde_json::json!({
            "pending": { "0x70997970C51812dc3A010C7d01b50e0d17dc79C8": { "3": {
                "hash": h,
                "maxFeePerGas": "0x12a05f200",
                "maxPriorityFeePerGas": "0x9502f900",
                "maxFeePerBlobGas": "0x9502f900"
            }}},
            "queued": {}
        });
        let t = pending_in(&pool, from, 3).unwrap().unwrap();
        assert_eq!(t.hash, h.parse::<B256>().unwrap());
        assert_eq!(
            (t.max_fee, t.tip, t.blob_fee),
            (5_000_000_000, 2_500_000_000, 2_500_000_000)
        );
        assert_eq!(pending_in(&pool, from, 4).unwrap(), None);
        assert_eq!(pending_in(&pool, Address::ZERO, 3).unwrap(), None);
        assert_eq!(pending_in(&serde_json::json!({}), from, 3).unwrap(), None);
    }

    #[test]
    fn the_mode_is_named() {
        for (v, want) in [(None, false), (Some(""), false), (Some("blobs"), false)] {
            assert_eq!(parse_da(v).unwrap(), want, "{v:?}");
        }
        assert!(parse_da(Some(" l1-blobs ")).unwrap());
        assert!(parse_da(Some("l1")).is_err());
        assert!(parse_ether(MIN_L1_BALANCE).unwrap() > U256::ZERO);
    }

    #[test]
    fn live_keys_are_dedicated() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, i: usize| {
            let p = dir.path().join(name);
            std::fs::write(&p, chain::DEV_KEYS[i]).unwrap();
            p.display().to_string()
        };
        let (a, b, org) = (file("a", 5), file("b", 6), file("org", 0));
        let organizer = chain::signer(0).unwrap().address();
        let keys = live_keys(&format!("{a}, {b}"), organizer).unwrap();
        let want = [
            chain::signer(5).unwrap().address(),
            chain::signer(6).unwrap().address(),
        ];
        assert_eq!(keys.iter().map(|(_, x)| *x).collect::<Vec<_>>(), want);
        let e = live_keys(&format!("{a},{org}"), organizer).unwrap_err();
        assert!(e.to_string().contains("organizer"), "{e}");
        assert!(live_keys(&format!("{a},{a}"), organizer).is_err());
        assert!(live_keys(" , ", organizer).is_err());
        // Errors name the file, never its content.
        assert!(!e.to_string().contains(chain::DEV_KEYS[0]));
    }
}
