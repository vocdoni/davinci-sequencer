//! The money rules of the live benchmark: the fee gate, the spend budget,
//! seq-b key generation and capped funding. Keys are read only from files
//! and never printed or put in an error.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use alloy::eips::BlockNumberOrTag;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::utils::parse_ether;
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::chain;

/// Wei.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fees {
    pub base: u128,
    pub tip: u128,
    pub blob: u128,
}

/// Base fee plus tip, at most 0.5 gwei.
pub const MAX_EXEC: u128 = 500_000_000;
/// Blob base fee, at most 0.02 gwei.
pub const MAX_BLOB: u128 = 20_000_000;

pub fn gate(f: &Fees) -> Result<(), String> {
    let mut why = Vec::new();
    if f.base + f.tip > MAX_EXEC {
        why.push(format!(
            "base {} + tip {} wei is above {MAX_EXEC}",
            f.base, f.tip
        ));
    }
    if f.blob > MAX_BLOB {
        why.push(format!("blob base fee {} wei is above {MAX_BLOB}", f.blob));
    }
    if why.is_empty() {
        Ok(())
    } else {
        Err(why.join("; "))
    }
}

/// Ethereum's latest base fee, the suggested tip and the blob base fee.
pub async fn fees(l1: &DynProvider) -> Result<Fees> {
    let block = l1
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
        .context("latest Ethereum block")?
        .context("no latest Ethereum block")?;
    let base = u128::from(
        block
            .header
            .base_fee_per_gas
            .context("the latest block has no base fee")?,
    );
    let tip = l1
        .get_max_priority_fee_per_gas()
        .await
        .context("eth_maxPriorityFeePerGas")?;
    let blob = match l1.get_blob_base_fee().await {
        Ok(b) => b,
        Err(_) => l1
            .get_fee_history(1, BlockNumberOrTag::Latest, &[])
            .await
            .context("blob base fee (eth_blobBaseFee and fee history)")?
            .base_fee_per_blob_gas
            .last()
            .copied()
            .context("fee history has no blob base fee")?,
    };
    Ok(Fees { base, tip, blob })
}

/// What one key and the organizer may spend over the campaign.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    pub l1_per_key: U256,
    pub op_per_key: U256,
    pub organizer_op: U256,
}

/// Index 0 is seq-a, 1 is seq-b.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balances {
    pub l1: [U256; 2],
    pub op: [U256; 2],
    pub organizer_op: U256,
}

/// `<campaign>/budget.json`, written at the first go.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub start: Balances,
    /// Transfers `fund` made since the start.
    #[serde(default)]
    pub inflows: Balances,
    pub caps: Caps,
}

impl Budget {
    /// A budget needs funded sequencer accounts: below its cap, an account
    /// could never reach the cap by spending.
    pub fn new(start: Balances, caps: Caps) -> Result<Budget, String> {
        let mut low = Vec::new();
        for (n, b, cap) in [
            ("seq-a.l1", start.l1[0], caps.l1_per_key),
            ("seq-b.l1", start.l1[1], caps.l1_per_key),
            ("seq-a.op", start.op[0], caps.op_per_key),
            ("seq-b.op", start.op[1], caps.op_per_key),
        ] {
            if b < cap {
                low.push(format!("{n} holds {b} wei, below its cap {cap}"));
            }
        }
        if low.is_empty() {
            Ok(Budget {
                start,
                inflows: Balances::default(),
                caps,
            })
        } else {
            Err(format!("fund first: {}", low.join("; ")))
        }
    }

    /// Balances fell since the start plus the inflows.
    pub fn spent(&self, now: &Balances) -> Balances {
        let d = |s: U256, n: U256| s.saturating_sub(n);
        let s = &Balances {
            l1: [
                self.start.l1[0] + self.inflows.l1[0],
                self.start.l1[1] + self.inflows.l1[1],
            ],
            op: [
                self.start.op[0] + self.inflows.op[0],
                self.start.op[1] + self.inflows.op[1],
            ],
            organizer_op: self.start.organizer_op + self.inflows.organizer_op,
        };
        Balances {
            l1: [d(s.l1[0], now.l1[0]), d(s.l1[1], now.l1[1])],
            op: [d(s.op[0], now.op[0]), d(s.op[1], now.op[1])],
            organizer_op: d(s.organizer_op, now.organizer_op),
        }
    }

    /// Fails, naming the account, when one has spent its cap.
    pub fn check(&self, now: &Balances) -> Result<(), String> {
        let s = self.spent(now);
        let c = &self.caps;
        let rows = [
            ("seq-a.l1", s.l1[0], c.l1_per_key),
            ("seq-b.l1", s.l1[1], c.l1_per_key),
            ("seq-a.op", s.op[0], c.op_per_key),
            ("seq-b.op", s.op[1], c.op_per_key),
            ("organizer.op", s.organizer_op, c.organizer_op),
        ];
        let hit: Vec<String> = rows
            .iter()
            .filter(|(_, spent, cap)| spent >= cap)
            .map(|(n, spent, cap)| format!("{n} spent {spent} wei, cap {cap}"))
            .collect();
        if hit.is_empty() {
            Ok(())
        } else {
            Err(hit.join("; "))
        }
    }
}

fn create_key(path: &Path) -> Result<PrivateKeySigner> {
    let k = PrivateKeySigner::random();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| anyhow!("key file {}: {}", path.display(), e.kind()))?;
    f.write_all(format!("{}\n", hex::encode(k.to_bytes())).as_bytes())
        .map_err(|e| anyhow!("key file {}: {}", path.display(), e.kind()))?;
    Ok(k)
}

/// A fresh OP and a fresh Ethereum key as `<name>.op.key` and
/// `<name>.l1.key` (mode 0600, never overwritten). Returns `(op, l1)`.
pub fn keygen(dir: &Path, name: &str) -> Result<(Address, Address)> {
    let (op, l1) = (
        dir.join(format!("{name}.op.key")),
        dir.join(format!("{name}.l1.key")),
    );
    for p in [&op, &l1] {
        ensure!(!p.exists(), "key file {} exists", p.display());
    }
    let a = create_key(&op)?;
    let b = match create_key(&l1) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&op);
            return Err(e);
        }
    };
    Ok((a.address(), b.address()))
}

/// Hard ceilings of one `fund` call, in ETH.
pub const FUND_MAX_L1: &str = "0.01";
pub const FUND_MAX_OP: &str = "0.001";

pub fn check_fund(amount: U256, max: U256, to: Address, confirm: &[Address]) -> Result<()> {
    ensure!(
        amount <= max,
        "{amount} wei to {to} is above the ceiling {max} wei"
    );
    ensure!(confirm.contains(&to), "{to} is not in --confirm");
    Ok(())
}

async fn send(p: &DynProvider, to: Address, amount: U256) -> Result<String> {
    let tx = TransactionRequest::default().with_to(to).with_value(amount);
    let r = p
        .send_transaction(tx)
        .await
        .map_err(|e| anyhow!("send to {to}: {}", crate::redact_urls(&e.to_string())))?
        .get_receipt()
        .await
        .map_err(|e| {
            anyhow!(
                "receipt of the send to {to}: {}",
                crate::redact_urls(&e.to_string())
            )
        })?;
    ensure!(
        r.status(),
        "transfer to {to} reverted in {}",
        r.transaction_hash
    );
    Ok(r.transaction_hash.to_string())
}

/// Sends the owner's capped transfers and prints their hashes. The ceilings
/// hold here too, and the fee gate runs before the Ethereum send.
pub async fn fund(
    owner: &PrivateKeySigner,
    op_rpc: &str,
    l1_rpc: &str,
    to_op: Option<(Address, U256)>,
    to_l1: Option<(Address, U256)>,
) -> Result<()> {
    let (max_l1, max_op) = (parse_ether(FUND_MAX_L1)?, parse_ether(FUND_MAX_OP)?);
    if let Some((to, a)) = to_op {
        check_fund(a, max_op, to, &[to])?;
    }
    if let Some((to, a)) = to_l1 {
        check_fund(a, max_l1, to, &[to])?;
    }
    let wallet = EthereumWallet::from(owner.clone());
    let provider = |url: &str| -> Result<DynProvider> {
        Ok(ProviderBuilder::new()
            .wallet(wallet.clone())
            .connect_client(chain::rpc(url)?)
            .erased())
    };
    if let Some((to, a)) = to_op {
        println!("OP tx {}", send(&provider(op_rpc)?, to, a).await?);
    }
    if let Some((to, a)) = to_l1 {
        let l1 = provider(l1_rpc)?;
        let f = fees(&l1).await?;
        if let Err(why) = gate(&f) {
            bail!("fee gate: {why}");
        }
        println!("Ethereum tx {}", send(&l1, to, a).await?);
    }
    Ok(())
}

/// Lines of `pgrep -af davinci-sequencer` and `docker ps --format '{{.Names}} {{.Image}}'`
/// that are not the rig's own (`own` pids); prover containers (davinci-zkvm
/// images) are not sequencers.
pub fn foreign_sequencers(pgrep: &str, docker: &str, own: &[u32]) -> Vec<String> {
    let mut out = Vec::new();
    for l in pgrep.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (pid, cmd) = l.split_once(' ').unwrap_or((l, ""));
        let argv0 = cmd.split_whitespace().next().unwrap_or("");
        let base = argv0.rsplit('/').next().unwrap_or(argv0);
        if pid.parse::<u32>().is_ok_and(|p| own.contains(&p)) || base == "pgrep" {
            continue;
        }
        if base == "davinci-sequencer" {
            out.push(l.to_string());
        }
    }
    for l in docker.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if l.contains("davinci-sequencer") && !l.contains("davinci-zkvm") {
            out.push(l.to_string());
        }
    }
    out
}

/// Accounts whose pending nonce is above their latest one.
pub fn nonce_gaps(pending: &[(String, u64)], latest: &[(String, u64)]) -> Vec<String> {
    pending
        .iter()
        .filter(|(n, p)| latest.iter().any(|(m, l)| m == n && p > l))
        .map(|(n, _)| n.clone())
        .collect()
}

/// What `gate` found: `go` only when `problems` is empty.
#[derive(Debug, Default)]
pub struct GateReport {
    pub lines: Vec<String>,
    pub problems: Vec<String>,
}

impl GateReport {
    pub fn go(&self) -> bool {
        self.problems.is_empty()
    }
}

fn env_list(k: &str) -> Vec<String> {
    std::env::var(k)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn cap_env(k: &str, default: &str) -> Result<U256> {
    let v = std::env::var(k).ok().filter(|v| !v.is_empty());
    parse_ether(v.as_deref().unwrap_or(default)).with_context(|| k.to_string())
}

fn sha256_file(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let b = std::fs::read(p).map_err(|e| anyhow!("{}: {}", p.display(), e.kind()))?;
    Ok(hex::encode(Sha256::digest(b)))
}

/// `ok1`: exit code 1 is an answer (pgrep: no match), not a failure.
fn run(cmd: &str, args: &[&str], ok1: bool) -> Result<String> {
    let o = std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| anyhow!("{cmd}: {}", e.kind()))?;
    ensure!(
        o.status.success() || (ok1 && o.status.code() == Some(1)),
        "{cmd} failed ({})",
        o.status
    );
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

async fn balances(op: &DynProvider, l1: &DynProvider, a: &Accounts) -> Result<Balances> {
    Ok(Balances {
        l1: [
            l1.get_balance(a.l1[0]).await?,
            l1.get_balance(a.l1[1]).await?,
        ],
        op: [
            op.get_balance(a.op[0]).await?,
            op.get_balance(a.op[1]).await?,
        ],
        organizer_op: op.get_balance(a.organizer).await?,
    })
}

struct Accounts {
    l1: [Address; 2],
    op: [Address; 2],
    organizer: Address,
}

fn two_keys(var: &str) -> Result<[Address; 2]> {
    let v = env_list(var);
    ensure!(
        v.len() >= 2,
        "{var} must list seq-a's and seq-b's key files"
    );
    Ok([
        crate::net::read_key(Path::new(&v[0]))?.address(),
        crate::net::read_key(Path::new(&v[1]))?.address(),
    ])
}

/// Adds a transfer to `<campaign>/budget.json`'s inflows (a no-op while the
/// file does not exist: the first go then starts from the funded balances).
/// Call before sending, so a failed send only makes the budget stricter.
pub fn record_inflow(campaign: &Path, to: Address, l1: bool, amount: U256) -> Result<()> {
    record_inflow_with(campaign, to, l1, amount, |l1| {
        two_keys(if l1 {
            "DAVINCI_E2E_L1_KEYS"
        } else {
            "DAVINCI_E2E_SEQUENCER_KEYS"
        })
    })
}

/// [`record_inflow`] with the sequencer addresses (a side's A and B) from `keys`.
pub fn record_inflow_with(
    campaign: &Path,
    to: Address,
    l1: bool,
    amount: U256,
    keys: impl FnOnce(bool) -> Result<[Address; 2]>,
) -> Result<()> {
    let path = campaign.join("budget.json");
    let raw = match std::fs::read(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => bail!("{}: {}", path.display(), e.kind()),
    };
    let mut b: Budget =
        serde_json::from_slice(&raw).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let (keys, slots) = if l1 {
        (keys(true)?, &mut b.inflows.l1)
    } else {
        (keys(false)?, &mut b.inflows.op)
    };
    let i = keys
        .iter()
        .position(|a| *a == to)
        .with_context(|| format!("{to} is not seq-a or seq-b; cannot record the inflow"))?;
    slots[i] += amount;
    std::fs::write(&path, serde_json::to_vec_pretty(&b)?)
        .with_context(|| format!("write {}", path.display()))
}

/// The go/no-go gate: fees, balances, spend against the caps (when a
/// `campaign` dir is given; its `budget.json` is written at the first go),
/// both provers, foreign sequencers, nonce gaps and the beacon count.
/// `own` are the rig's own sequencer pids.
pub async fn gate_check(campaign: Option<&Path>, own: &[u32]) -> Result<GateReport> {
    let mut r = GateReport::default();
    let (op_rpc, ..) = crate::net::live_target()?;
    let l1_rpc = env_list("DAVINCI_E2E_L1_RPC")
        .into_iter()
        .next()
        .context("DAVINCI_E2E_L1_RPC")?;
    let op = ProviderBuilder::new()
        .connect_client(chain::rpc(&op_rpc)?)
        .erased();
    let l1 = ProviderBuilder::new()
        .connect_client(chain::rpc(&l1_rpc)?)
        .erased();
    let org = std::env::var("DAVINCI_E2E_ORGANIZER_KEY").context("DAVINCI_E2E_ORGANIZER_KEY")?;
    let acc = Accounts {
        l1: two_keys("DAVINCI_E2E_L1_KEYS")?,
        op: two_keys("DAVINCI_E2E_SEQUENCER_KEYS")?,
        organizer: crate::net::read_key(Path::new(&org))?.address(),
    };

    let f = fees(&l1).await?;
    r.lines.push(format!(
        "fees: base {} + tip {} wei, blob {} wei",
        f.base, f.tip, f.blob
    ));
    if let Err(why) = gate(&f) {
        r.problems.push(format!("fees: {why}"));
    }

    let now = balances(&op, &l1, &acc).await?;
    r.lines.push(format!(
        "balances (wei): L1 A {} B {} | OP A {} B {} | organizer {}",
        now.l1[0], now.l1[1], now.op[0], now.op[1], now.organizer_op
    ));
    if let Some(dir) = campaign {
        let caps = Caps {
            l1_per_key: cap_env("DAVINCI_BENCH_CAP_L1", "0.005")?,
            op_per_key: cap_env("DAVINCI_BENCH_CAP_OP", "0.0005")?,
            organizer_op: cap_env("DAVINCI_BENCH_CAP_ORG", "0.002")?,
        };
        let path = dir.join("budget.json");
        let budget = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice::<Budget>(&b)
                .map_err(|e| anyhow!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match Budget::new(now.clone(), caps.clone()) {
                    Ok(b) => {
                        if r.problems.is_empty() {
                            std::fs::write(&path, serde_json::to_vec_pretty(&b)?)
                                .with_context(|| format!("write {}", path.display()))?;
                        }
                        b
                    }
                    Err(why) => {
                        r.problems.push(format!("budget: {why}"));
                        Budget {
                            start: now.clone(),
                            inflows: Balances::default(),
                            caps,
                        }
                    }
                }
            }
            Err(e) => bail!("{}: {}", path.display(), e.kind()),
        };
        let s = budget.spent(&now);
        r.lines.push(format!(
            "spent (wei): L1 A {} B {} | OP A {} B {} | organizer {}",
            s.l1[0], s.l1[1], s.op[0], s.op[1], s.organizer_op
        ));
        r.lines.push(format!(
            "caps (wei): L1 {} | OP {} | organizer {}",
            budget.caps.l1_per_key, budget.caps.op_per_key, budget.caps.organizer_op
        ));
        if let Err(why) = budget.check(&now) {
            r.problems.push(format!("budget: {why}"));
        }
    }

    for url in env_list("DAVINCI_E2E_BENCH_PROVERS") {
        let url = url.trim_end_matches('/').to_string();
        match davinci_zkvm_sdk::client::ProverClient::new(&url)
            .health()
            .await
        {
            Ok(h) => {
                r.lines.push(format!(
                    "prover {url}: {} queue_len {}",
                    h.status, h.queue_len
                ));
                if h.status != "ok" || h.queue_len != 0 {
                    r.problems.push(format!(
                        "prover {url}: {} queue_len {}",
                        h.status, h.queue_len
                    ));
                }
            }
            Err(e) => r.problems.push(format!(
                "prover {url}: {}",
                crate::redact_urls(&e.to_string())
            )),
        }
    }
    if env_list("DAVINCI_E2E_BENCH_PROVERS").len() < 2 {
        r.problems
            .push("DAVINCI_E2E_BENCH_PROVERS must list both provers".into());
    }

    let beacons = match std::env::var("DAVINCI_E2E_BEACON") {
        Ok(_) => env_list("DAVINCI_E2E_BEACON").len(),
        Err(_) => crate::net::preset()?.beacon_urls().len(),
    };
    r.lines.push(format!("beacons: {beacons}"));
    if beacons < 2 {
        r.problems.push(format!(
            "DAVINCI_E2E_BEACON lists {beacons} beacons, C6 needs two"
        ));
    }

    let mut mine = own.to_vec();
    mine.push(std::process::id());
    match (
        run("pgrep", &["-af", "davinci-sequencer"], true),
        run(
            "docker",
            &["ps", "--format", "{{.Names}} {{.Image}}"],
            false,
        ),
    ) {
        (Ok(p), Ok(d)) => {
            for l in foreign_sequencers(&p, &d, &mine) {
                r.problems.push(format!("foreign sequencer: {l}"));
            }
        }
        (p, d) => r.problems.push(format!(
            "cannot list sequencers: {}",
            [p.err(), d.err()]
                .into_iter()
                .flatten()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        )),
    }

    let mut pending = Vec::new();
    let mut latest = Vec::new();
    for (i, n) in ["seq-a", "seq-b"].iter().enumerate() {
        for (suffix, p, a) in [("l1", &l1, acc.l1[i]), ("op", &op, acc.op[i])] {
            let name = format!("{n}.{suffix}");
            pending.push((name.clone(), p.get_transaction_count(a).pending().await?));
            latest.push((name, p.get_transaction_count(a).latest().await?));
        }
    }
    for n in nonce_gaps(&pending, &latest) {
        r.problems
            .push(format!("{n} has a pending nonce above its latest"));
    }

    // The rig refuses to start without it, so the hash is of what runs.
    match std::env::var_os("DAVINCI_SEQUENCER_BIN") {
        Some(b) => r
            .lines
            .push(format!("sequencer sha256 {}", sha256_file(Path::new(&b))?)),
        None => r
            .problems
            .push("DAVINCI_SEQUENCER_BIN is unset: no gated node binary".into()),
    }
    r.lines.push(format!(
        "davinci-bench sha256 {}",
        sha256_file(&std::env::current_exe()?)?
    ));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::net::read_key;

    #[test]
    fn gate_bounds() {
        let ok = Fees {
            base: 400_000_000,
            tip: 100_000_000,
            blob: 20_000_000,
        };
        assert!(gate(&ok).is_ok());
        assert!(
            gate(&Fees {
                tip: ok.tip + 1,
                ..ok
            })
            .is_err()
        );
        assert!(
            gate(&Fees {
                base: ok.base + 1,
                ..ok
            })
            .is_err()
        );
        assert!(
            gate(&Fees {
                blob: ok.blob + 1,
                ..ok
            })
            .is_err()
        );
    }

    #[test]
    fn budget_stops_at_a_cap() {
        let e = |x: u64| U256::from(x);
        let b = Budget {
            inflows: Balances::default(),
            start: Balances {
                l1: [e(1000), e(1000)],
                op: [e(100), e(100)],
                organizer_op: e(500),
            },
            caps: Caps {
                l1_per_key: e(300),
                op_per_key: e(50),
                organizer_op: e(200),
            },
        };
        let mut now = b.start.clone();
        now.l1[1] = e(701);
        now.op[0] = e(51);
        assert!(b.check(&now).is_ok());
        assert_eq!(b.spent(&now).l1[1], e(299));
        now.l1[1] = e(700);
        let err = b.check(&now).unwrap_err();
        assert!(err.contains("seq-b.l1") && !err.contains("seq-a"), "{err}");
        // A balance above the start is no spend.
        now.l1[1] = e(5000);
        assert!(b.check(&now).is_ok());
        // An inflow raises the baseline, not the cap.
        let mut f = b.clone();
        f.inflows.l1[1] = e(200);
        now.l1[1] = e(900);
        assert!(f.check(&now).is_err());
        // A budget cannot start below a cap (nothing would ever trip it).
        let low = Balances {
            l1: [e(1000), e(0)],
            op: [e(100), e(100)],
            organizer_op: e(500),
        };
        let err = Budget::new(low, b.caps.clone()).unwrap_err();
        assert!(
            err.contains("fund first") && err.contains("seq-b.l1"),
            "{err}"
        );
        assert!(Budget::new(b.start.clone(), b.caps.clone()).is_ok());
    }

    #[test]
    fn keygen_is_private() {
        let d = tempfile::tempdir().unwrap();
        let (op, l1) = keygen(d.path(), "seq-b").unwrap();
        for (f, a) in [("seq-b.op.key", op), ("seq-b.l1.key", l1)] {
            let p = d.path().join(f);
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(read_key(&p).unwrap().address(), a);
        }
        assert_ne!(op, l1);
        let key = std::fs::read_to_string(d.path().join("seq-b.op.key")).unwrap();
        let err = keygen(d.path(), "seq-b").unwrap_err().to_string();
        let hexish = |s: &str| s.chars().filter(char::is_ascii_hexdigit).count();
        assert!(
            !err.contains(key.trim()) && !err.contains(&hex::encode(op)),
            "{err}"
        );
        assert!(!err.contains("0x") && hexish(&err) < 40, "{err}");
        // A half-existing pair is refused, and nothing new is left behind.
        std::fs::remove_file(d.path().join("seq-b.op.key")).unwrap();
        assert!(keygen(d.path(), "seq-b").is_err());
        assert!(!d.path().join("seq-b.op.key").exists());
    }

    #[test]
    fn docker_failure_is_not_an_answer() {
        // `false` exits 1 with no output, like docker without its socket.
        assert!(run("false", &[], false).is_err());
        assert_eq!(run("false", &[], true).unwrap(), "");
    }

    #[test]
    fn fund_checks() {
        let (a, b) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let max = parse_ether(FUND_MAX_OP).unwrap();
        assert!(check_fund(max + U256::from(1), max, a, &[a]).is_err());
        assert!(check_fund(max, max, b, &[a]).is_err());
        assert!(check_fund(max, max, a, &[a, b]).is_ok());
        assert!(parse_ether(FUND_MAX_L1).unwrap() > max);
    }

    #[test]
    fn gate_finds_foreign_sequencers() {
        let pgrep = "101 /x/target/debug/davinci-sequencer --datadir a\n\
                     202 /usr/bin/davinci-sequencer --datadir prod\n\
                     303 pgrep -af davinci-sequencer\n";
        let docker = "davinci-sequencer ghcr.io/vocdoni/davinci-sequencer:v1\n\
                      prover ghcr.io/vocdoni/davinci-zkvm:1.3.2\n";
        let f = foreign_sequencers(pgrep, docker, &[101]);
        assert_eq!(f.len(), 2, "{f:?}");
        let bench = "404 /home/p4u/davinci-sequencer-bench/target/debug/deps/davinci_e2e-x bench\n\
                     405 /usr/local/bin/davinci-sequencer --datadir d\n";
        let f2 = foreign_sequencers(bench, "", &[]);
        assert_eq!(
            f2,
            vec!["405 /usr/local/bin/davinci-sequencer --datadir d".to_string()]
        );
        assert!(f[0].starts_with("202 ") && f[1].starts_with("davinci-sequencer ghcr"));
        let none = foreign_sequencers("", "prover ghcr.io/vocdoni/davinci-zkvm:x\n", &[]);
        assert!(none.is_empty());
        let s = |n: &str, v: u64| (n.to_string(), v);
        let gaps = nonce_gaps(
            &[s("seq-a.l1", 5), s("seq-b.l1", 3), s("seq-a.op", 2)],
            &[s("seq-a.l1", 4), s("seq-b.l1", 3), s("seq-a.op", 2)],
        );
        assert_eq!(gaps, vec!["seq-a.l1".to_string()]);
    }

    #[test]
    fn record_inflow_adds_once_to_the_matching_slot() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let one = U256::from(1u8);
        let bal = Balances {
            l1: [one; 2],
            op: [one; 2],
            organizer_op: one,
        };
        let caps = Caps {
            l1_per_key: one,
            op_per_key: one,
            organizer_op: one,
        };
        let budget = Budget::new(bal, caps).unwrap();
        std::fs::write(
            d.path().join("budget.json"),
            serde_json::to_vec(&budget).unwrap(),
        )
        .unwrap();
        let amount = U256::from(7u8);
        record_inflow_with(d.path(), b, true, amount, |_| Ok([a, b])).unwrap();
        let got: Budget =
            serde_json::from_slice(&std::fs::read(d.path().join("budget.json")).unwrap()).unwrap();
        assert_eq!(got.inflows.l1, [U256::ZERO, amount]);
        assert_eq!(got.inflows.op, [U256::ZERO; 2]);
    }
}
