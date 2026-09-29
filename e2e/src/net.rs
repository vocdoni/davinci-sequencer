//! The chain a run uses: a fresh anvil with the contracts, or with
//! `DAVINCI_E2E_LIVE=1` an existing deployment (Gnosis by default). Key
//! files are read in-process; only addresses are ever printed.
//!
//! Live env: `DAVINCI_E2E_RPC` (organizer and harness reads),
//! `DAVINCI_E2E_NODE_RPCS` (nodes split by `;`, each a `,` failover list
//! handed to the node as is; seq1, seq2, seq3, observer), `DAVINCI_E2E_BEACON`
//! (a `,` list), `DAVINCI_E2E_REGISTRY`, `DAVINCI_E2E_FROM_BLOCK`,
//! `DAVINCI_E2E_CONFIRMATIONS`, `DAVINCI_E2E_POLL`, `DAVINCI_E2E_TIMEOUT_SCALE`.
//! Unset, they default to the Gnosis deployment of `davinci_client::networks`.
//! With `DAVINCI_E2E_DKG=1`, `DAVINCI_E2E_DKG_MANAGER` names the external
//! committee's DKGManager. Unset, it is the manager of the registry's adapter.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256, utils::format_ether};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, bail, ensure};
use davinci_client::networks::GNOSIS;
use davinci_client::organizer::{Organizer, verify_registry};

use crate::chain::{self, Anvil, DEV_KEYS};
use crate::dkg::{self, DkgStack, DkgWiring};
use crate::node::NodeConfig;

/// Least balance each funded account needs for a live run (0.05 xDAI).
pub const MIN_BALANCE: u128 = 50_000_000_000_000_000;

/// Organizer and harness RPC: the network's last (publicnode, its first,
/// rate-limits organizer bursts).
const GNOSIS_RPC: &str = GNOSIS.rpc_urls[GNOSIS.rpc_urls.len() - 1];

/// Live node RPC lists (seq1, seq2, seq3, observer): the network's RPCs
/// rotated, so each node starts on another endpoint and fails over to the next.
fn gnosis_node_rpcs() -> String {
    let u = GNOSIS.rpc_urls;
    (0..4)
        .map(|i| format!("{},{}", u[i % u.len()], u[(i + 1) % u.len()]))
        .collect::<Vec<_>>()
        .join(";")
}

pub fn is_live() -> bool {
    std::env::var("DAVINCI_E2E_LIVE").as_deref() == Ok("1")
}

fn env_or(k: &str, default: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Multiplier for every settle and sync timeout: `DAVINCI_E2E_TIMEOUT_SCALE`
/// (>= 1), default 3 live (5 s blocks plus confirmations) and 1 on anvil.
/// [`Net::open`] rejects a bad value.
pub fn timeout_scale() -> u32 {
    scale_env()
        .ok()
        .flatten()
        .unwrap_or(if is_live() { 3 } else { 1 })
}

fn scale_env() -> Result<Option<u32>> {
    let Ok(v) = std::env::var("DAVINCI_E2E_TIMEOUT_SCALE") else {
        return Ok(None);
    };
    let s: u32 = v.parse().context("DAVINCI_E2E_TIMEOUT_SCALE")?;
    ensure!(s >= 1, "DAVINCI_E2E_TIMEOUT_SCALE must be at least 1");
    Ok(Some(s))
}

/// Node RPC lists: nodes split by `;`, each a `,` failover list passed to
/// the node as is.
fn node_lists(v: &str) -> Vec<String> {
    v.split(';')
        .map(|n| {
            n.split(',')
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|n| !n.is_empty())
        .collect()
}

pub fn scaled(d: Duration) -> Duration {
    d * timeout_scale()
}

/// A hex signing key from `path` (64 hex digits, `0x` optional). The error
/// names the path only, never the content.
pub fn read_key(path: &Path) -> Result<PrivateKeySigner> {
    let bad = || anyhow::anyhow!("key file {}: not a hex secp256k1 key", path.display());
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("key file {}: {}", path.display(), e.kind()))?;
    let hex = raw.trim().trim_start_matches("0x");
    if hex.len() != 64 {
        return Err(bad());
    }
    hex.parse().map_err(|_| bad())
}

/// RPC, registry and first log block of the live chain, from the env.
pub fn live_target() -> Result<(String, Address, u64)> {
    let registry = env_or("DAVINCI_E2E_REGISTRY", &GNOSIS.registry.to_string())
        .parse()
        .context("DAVINCI_E2E_REGISTRY")?;
    let from_block = env_or("DAVINCI_E2E_FROM_BLOCK", &GNOSIS.start_block.to_string())
        .parse()
        .context("DAVINCI_E2E_FROM_BLOCK")?;
    Ok((env_or("DAVINCI_E2E_RPC", GNOSIS_RPC), registry, from_block))
}

pub struct Net {
    pub live: bool,
    pub rpc: String,
    /// Per-node RPC lists (`DAVINCI_E2E_NODE_RPCS`), else `rpc` for all.
    node_rpcs: Vec<String>,
    /// Live beacon APIs, a `,` failover list; `None` on anvil (the nodes
    /// read blobs from anvil).
    pub beacon: Option<String>,
    pub registry: Address,
    pub chain_id: u64,
    /// First block of every log scan.
    pub from_block: u64,
    /// Where fresh nodes start scanning: the head when the run began (live).
    pub start_block: Option<u64>,
    pub organizer: PrivateKeySigner,
    /// The census contract owner on anvil; live it is the organizer.
    census_owner: Option<PrivateKeySigner>,
    /// Key file and address of each sequencer.
    pub sequencers: Vec<(PathBuf, Address)>,
    pub confirmations: u64,
    pub poll: String,
    /// `DAVINCI_E2E_DKG=1`: the registry's committee, checked by
    /// [`dkg::check_wiring`].
    pub dkg: Option<DkgWiring>,
    /// The committee's nodes on anvil. Declared before `anvil` so they die
    /// first.
    pub dkg_stack: Option<DkgStack>,
    pub anvil: Option<Anvil>,
}

impl Net {
    /// Live: reads the env and checks the registry pins. Anvil: starts
    /// anvil in `dir`, deploys, and writes the public dev keys of the three
    /// sequencers to key files there.
    pub async fn open(dir: &Path) -> Result<Net> {
        scale_env()?;
        let mut net = if is_live() {
            Net::live().await?
        } else {
            Net::anvil(dir).await?
        };
        let info = verify_registry(&net.rpc, net.registry)
            .await
            .context("registry pins")?;
        ensure!(info.chain_id == net.chain_id, "chain id changed");
        eprintln!(
            "chain {} registry {} verifier {} (pins ok)",
            net.chain_id, net.registry, info.verifier
        );
        if dkg::enabled() {
            let manager = match (&net.dkg_stack, std::env::var("DAVINCI_E2E_DKG_MANAGER")) {
                (Some(s), _) => s.deployment.manager,
                (None, Ok(m)) if !m.is_empty() => m.parse().context("DAVINCI_E2E_DKG_MANAGER")?,
                (None, _) => dkg::registry_manager(&net.rpc, net.registry)
                    .await
                    .context("the registry adapter's DKG manager")?,
            };
            // A spent pool leaves nothing to register in until the
            // committee's next epoch goes Live.
            if let Some(a) = info.dkg_adapter {
                dkg::wait_registration_epoch(&net.rpc, a).await?;
            }
            let w = dkg::check_wiring(&net.rpc, net.registry, manager)
                .await
                .context("DKG wiring")?;
            ensure!(info.dkg_adapter == Some(w.adapter), "registry DKG adapter");
            eprintln!(
                "DKG manager {} app manager {} adapter {} registration epoch {} (wiring ok)",
                w.manager, w.app_manager, w.adapter, w.epoch
            );
            net.dkg = Some(w);
        }
        Ok(net)
    }

    async fn live() -> Result<Net> {
        let (rpc, registry, from_block) = live_target()?;
        let node_rpcs = node_lists(&env_or("DAVINCI_E2E_NODE_RPCS", &gnosis_node_rpcs()));
        let org_path = std::env::var_os("DAVINCI_E2E_ORGANIZER_KEY")
            .context("DAVINCI_E2E_ORGANIZER_KEY (a key file) is required live")?;
        let organizer = read_key(Path::new(&org_path))?;
        let seq = std::env::var("DAVINCI_E2E_SEQUENCER_KEYS")
            .context("DAVINCI_E2E_SEQUENCER_KEYS (key files) is required live")?;
        let mut sequencers = Vec::new();
        for p in seq.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let p = PathBuf::from(p);
            let a = read_key(&p)?.address();
            sequencers.push((p, a));
        }
        let p = ProviderBuilder::new().connect_client(chain::rpc(&rpc)?);
        let chain_id = p.get_chain_id().await.context("RPC chain id")?;
        let head = p.get_block_number().await?;
        ensure!(
            from_block <= head,
            "DAVINCI_E2E_FROM_BLOCK {from_block} is past the head {head}"
        );
        Ok(Net {
            live: true,
            rpc,
            node_rpcs,
            beacon: Some(
                node_lists(&env_or(
                    "DAVINCI_E2E_BEACON",
                    &GNOSIS.beacon_urls().join(","),
                ))
                .join(","),
            ),
            registry,
            chain_id,
            from_block,
            start_block: Some(head),
            organizer,
            census_owner: None,
            sequencers,
            confirmations: env_or("DAVINCI_E2E_CONFIRMATIONS", "3")
                .parse()
                .context("DAVINCI_E2E_CONFIRMATIONS")?,
            poll: env_or("DAVINCI_E2E_POLL", "5s"),
            dkg: None,
            dkg_stack: None,
            anvil: None,
        })
    }

    async fn anvil(dir: &Path) -> Result<Net> {
        let contracts = chain::contracts_dir();
        chain::forge_build(&contracts)?;
        let anvil = Anvil::start(&dir.join("anvil.log")).await?;
        let dkg_stack = if dkg::enabled() {
            let d = DkgStack::start(&anvil.url, dir).await?;
            d.wait_live(d.create_epoch().await?).await?;
            Some(d)
        } else {
            None
        };
        let manager = dkg_stack
            .as_ref()
            .map_or(Address::ZERO, |d| d.deployment.manager);
        let dep = chain::deploy(&anvil.url, &contracts, manager).await?;
        let keys = dir.join("keys");
        std::fs::create_dir_all(&keys)?;
        let mut sequencers = Vec::new();
        for (i, k) in DEV_KEYS.iter().enumerate().take(4).skip(1) {
            let p = keys.join(format!("seq{i}.key"));
            std::fs::write(&p, k)?;
            sequencers.push((p, chain::signer(i)?.address()));
        }
        Ok(Net {
            live: false,
            rpc: anvil.url.clone(),
            node_rpcs: Vec::new(),
            beacon: None,
            registry: dep.registry,
            chain_id: dep.chain_id,
            from_block: 0,
            start_block: None,
            organizer: chain::signer(0)?,
            census_owner: Some(chain::signer(4)?),
            sequencers,
            confirmations: 0,
            poll: "1s".into(),
            dkg: None,
            dkg_stack,
            anvil: Some(anvil),
        })
    }

    /// RPC list of node `i`: its entry of `DAVINCI_E2E_NODE_RPCS`, else the
    /// main RPC.
    pub fn node_rpc(&self, i: usize) -> String {
        self.node_rpcs.get(i).unwrap_or(&self.rpc).clone()
    }

    pub fn organizer(&self) -> Result<Organizer> {
        Ok(
            Organizer::connect(&self.rpc, self.organizer.clone(), self.registry)?
                .with_receipt_timeout(scaled(Duration::from_secs(180))),
        )
    }

    /// The account that deploys and grows `OwnedCensus`: dev account 4 on
    /// anvil, the organizer's own provider live (one nonce cache per key).
    pub fn census_provider(&self, org: &Organizer) -> Result<DynProvider> {
        Ok(match &self.census_owner {
            Some(s) => ProviderBuilder::new()
                .wallet(EthereumWallet::from(s.clone()))
                .connect_client(chain::rpc(&self.rpc)?)
                .erased(),
            None => org.provider(),
        })
    }

    /// Prints each funded account's address and balance; fails below
    /// [`MIN_BALANCE`] (the first `n_seq` sequencers only).
    pub async fn check_balances(&self, n_seq: usize) -> Result<()> {
        ensure!(
            self.sequencers.len() >= n_seq,
            "{n_seq} sequencer keys needed, {} given",
            self.sequencers.len()
        );
        let p = ProviderBuilder::new().connect_client(chain::rpc(&self.rpc)?);
        let accounts = std::iter::once(("organizer".to_string(), self.organizer.address())).chain(
            self.sequencers[..n_seq]
                .iter()
                .enumerate()
                .map(|(i, (_, a))| (format!("sequencer {}", i + 1), *a)),
        );
        let mut low = Vec::new();
        for (who, a) in accounts {
            let b = p.get_balance(a).await?;
            eprintln!("{who:<12} {a} balance {}", format_ether(b));
            if b < U256::from(MIN_BALANCE) {
                low.push(format!("{who} {a}"));
            }
        }
        if !low.is_empty() {
            bail!(
                "below {} native: {}",
                format_ether(U256::from(MIN_BALANCE)),
                low.join(", ")
            );
        }
        Ok(())
    }

    /// A node on this chain: sequencer `seq` (index into `sequencers`) or,
    /// with `None`, an observer, on RPC `node_rpc(rpc_index)`.
    pub fn node_config(
        &self,
        name: &str,
        seq: Option<usize>,
        rpc_index: usize,
        prover_url: &str,
        census_dir: &Path,
        dir: &Path,
    ) -> Result<NodeConfig> {
        let key = seq
            .map(|i| self.sequencers.get(i).context("no such sequencer key"))
            .transpose()?;
        let mut c = NodeConfig::new(
            name,
            key.map(|(p, _)| p.clone()),
            key.map(|(_, a)| *a),
            &self.node_rpc(rpc_index),
            self.registry,
            prover_url,
            census_dir,
            dir,
        );
        if let Some(b) = &self.beacon {
            c.blob_source = format!("beacon:{b}");
        }
        c.confirmations = self.confirmations;
        c.poll = self.poll.clone();
        if self.live {
            // The registry's production grace: the node's own budget
            // defaults, with room for 5 s blocks.
            c.flush_horizon = None;
            c.prove_base = None;
            c.settle_margin = Some("20s".into());
        }
        c.start_block = self.start_block;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_lists_split() {
        assert_eq!(
            node_lists(" a, b ;c;; d,"),
            ["a,b", "c", "d"].map(String::from)
        );
        let live = node_lists(&gnosis_node_rpcs());
        assert_eq!(live.len(), 4);
        assert!(
            live.iter()
                .all(|l| l.split(',').count() == 2 && !l.contains(' '))
        );
        // The three signing sequencers start on different endpoints.
        let first: std::collections::HashSet<_> =
            live[..3].iter().map(|l| l.split(',').next()).collect();
        assert_eq!(first.len(), 3);
        assert!(GNOSIS.rpc_urls.contains(&GNOSIS_RPC));
    }

    #[test]
    fn key_errors_do_not_leak() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        let secret = "zz".repeat(32);
        std::fs::write(&p, &secret).unwrap();
        let e = read_key(&p).unwrap_err().to_string();
        assert!(!e.contains("zz"), "{e}");
        std::fs::write(&p, format!("0x{}\n", DEV_KEYS[1])).unwrap();
        assert_eq!(
            read_key(&p).unwrap().address(),
            chain::signer(1).unwrap().address()
        );
        assert!(read_key(&dir.path().join("none")).is_err());
    }
}
