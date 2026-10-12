# L1-blob DA, Plan 3: the e2e harness, the OP Mainnet deployment, the live run and the benchmarks

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove the L1-blob mode end to end: the e2e acceptance test in L1-blob mode on two anvils (with a node killed while its blob post is pending and L1 negative replays), the registry deployed on OP Mainnet with its preset filled in, a compact live run on OP Mainnet with the real prover, batch benchmarks (64/128/256 votes at 2 and 16 fields) on OP Mainnet, and a report, docs and a final Fable review of both branches.

**Architecture:** The e2e crate gains an L1-blob mode behind `DAVINCI_E2E_DA=l1-blobs`: `davinci_e2e::l1` starts a second, Ethereum-like anvil (chain id 1, Osaka, 2 s blocks) and a relayer that copies the L1 block `lag` blocks behind the head into the OP anvil's `MockL1Block`; `chain::deploy_l1` deploys `MockL1Block`, `L1BlobVerifier` and the registry in L1-blob mode; nodes get `DAVINCI_L1_*` and a dedicated Ethereum key each. Blob mode keeps its code path, deploy order and addresses. Live, the same harness reads the `optimism` preset and the `DAVINCI_E2E_L1_*` variables; a compact `op_live` test and a `bench_l1` benchmark run on OP Mainnet. The deployment uses davinci-contracts' `deploy_all.sh` with the existing `ZiskVerifier` and a fresh `L1BlobVerifier`.

**Tech Stack:** Rust 1.95 (edition 2024), alloy 2.5 (`sol!` from the vendored ABIs), tokio, anvil/forge/cast 1.8.3 (`~/.foundry/bin`), davinci-contracts `feat/l1-blob-da` (`DeployAll.s.sol`, `deploy_all.sh`, `verify_deployment.py`), python3 (stdlib only, run with `-I`), the davinci-zkvm GPU prover at `DAVINCI_ZKVM_URL` (shared with a production container).

**Spec:** `docs/superpowers/specs/2026-10-12-l1-blob-da-op-design.md` (§6 Tests: e2e, live, benchmarks; §7 Deployment; §8 phases 3 and 4).

**Inputs that override the spec where they differ:** `.superpowers/plan2-notes.md` (section "After the Fable gate": re-prove on restart plus a persisted L1 post record, the `record()` anchor policy, estimate + 20 % gas) and the "Handoff to Plan 3" section of `docs/superpowers/plans/2026-10-12-l1-blob-da-plan2-node.md` (node env, harness building blocks, library API, log lines, what to fill after deployment, restart behaviour). PoC numbers come from `docs/superpowers/reports/2026-10-12-l1-blob-op-poc.md` and `docs/superpowers/reports/2026-10-12-l1blob-verifier-op-check.md`.

**Precondition:** Plan 2 is committed on `feat/l1-blob-da` (Tasks 1 to 13, including `sequencer/abi/L1BlobVerifier.json`, the `optimism` preset with a zero registry, `RegistryDa` in `verify_registry`, the node's `DAVINCI_L1_*` options and the log lines `blobs posted on L1`, `L1 post of an earlier run on record; the next post replaces it if still pending`, `L1-blob transition sent`, `transition settled`). Check before Task 1:

```bash
cd /home/p4u/davinci-sequencer && git branch --show-current && git status --short | wc -l
grep -c "L1 post of an earlier run on record" sequencer/src/l1da/post.rs
ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil l1_blob_mode 2>&1 | grep -E "^test result|skipped"
```
Expected: `feat/l1-blob-da`, `0`, `1`, and `test result: ok. 3 passed` (no skip line). Stop if not.

## Global Constraints

- **Secrets.** Keys live only in files: the owner's funded key `~/optimism-privkey.txt` (address `0x470D1dDc7DdCeb0d818751D171d40366f010a3cc` on OP and Ethereum) and the new node keys under `~/.config/davinci-l1blob/` (directory 0700, files 0600). Never print, log, commit or pass a key value through an environment variable; pass key *files* (`DAVINCI_E2E_*_KEY*`, `DAVINCI_L1_PRIVKEY_FILE`). The OP RPC URL in `~/optimism-rpcs.txt` (first line) is secret: it is never printed in full, and every command that may echo it pipes through `redact` (Task 4 Step 1). Public endpoints (publicnode, dRPC, mainnet.optimism.io) may be printed. Never commit a `.env` file; deployments set their variables in a subshell.
- **Spending caps.** Out of the owner's Ethereum account at most **0.0025 ETH** for the whole plan (funding plus gas; balance at start about 0.0036 ETH). Out of the owner's OP account at most **0.0012 ETH** (balance about 0.0018 ETH). Every spending step has a go/no-go check (balance, current fees, a simulated call or `forge script` without `--broadcast`) and prints only addresses, transaction hashes, gas and cost.
- **Commits.** Titles `type(scope): lowercase title`; no AI attribution and no `Co-Authored-By` lines. Branch `feat/l1-blob-da` in both repositories; never push.
- **Build and run output** stays in the repository's `target/` or under `~/.cache` (`TMPDIR=$HOME/.cache/davinci-e2e`, logs in `~/.cache/davinci-e2e` and `~/.cache/davinci-bench`), never `/tmp`. `make e2e` gets `E2E_LOG=$HOME/.cache/davinci-e2e/<name>.log`.
- **Shared GPU prover.** The prover at `DAVINCI_ZKVM_URL` also serves a production node. Before every GPU run check `curl -s "$DAVINCI_ZKVM_URL/health"` shows `"queue_len":0`; run one GPU workload at a time; batch sizes stay bounded (e2e: 8 votes, 2 in the backlog phase; live: 4; benchmarks: at most 256 votes plus 256 refreshes); benchmark runs are 60 s apart and each waits for an empty queue. Never start a run while production jobs are queued.
- **Blob mode e2e unchanged.** Everything L1-specific sits behind `DAVINCI_E2E_DA=l1-blobs` or an L1-blob preset; blob mode keeps its deploy order (same anvil addresses), scenario and node environment.
- **Node behaviour belongs to Plan 2.** A node bug found here gets its own commit (`fix(l1da): ...`) with a test reproducing it, and a line in the report; the harness is not bent around it.
- Before every commit: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p davinci-e2e` (gated tests print skip lines and pass).

## Review Focus

- **The Ethereum send path refuses six-blob bodies or v1 sidecars** (publicnode answers 503 above about 1 MB; free tiers differ): posts must reach an endpoint that takes 1.5 MB, and the first post of a live run must land. Tests: Task 4 Step 2 (`probe_send.py` expects `eth.drpc.org 200 takes 1.5 MB`), Task 5 Step 8 (a `blobs posted on L1` line within 5 minutes, else stop), Task 7 Step 3 (every row `l1_posts == 1`, or explained).
- **A beacon API that does not keep full blobs** (PeerDAS; lodestar's public endpoint answers 403): an observer would stall on every transition. Tests: Task 4 Step 2 (`probe_beacons.py`, recent and day-old slots, `blobs 200, 1 blob(s)`), Task 5's `op_live` assertion that the observer synced every transition.
- **An Ethereum key that runs dry, or fees that spike mid-run:** nothing may be spent before balances are checked, a benchmark stops at its budget, and the error names addresses only. Tests: Task 5 Step 7 (`DAVINCI_E2E_L1_MIN_BALANCE=1000` fails before any transaction, with no key or URL in the output), Task 6 Step 4 (`DAVINCI_E2E_BENCH_L1_BUDGET=0` writes a report with the stop note and no row, no GPU job).
- **A restart while the node's blob transaction is pending:** the next post must replace it at the same nonce with every fee doubled (the real blob pool rejects less), and the settlement must carry the replacement's blobs. Tests: Task 3 `kill_after_post` (fees at least 2x on all three, receipt only for the replacement, event blobs from it); Task 5 `op_live` reports the mainnet outcome.
- **The private OP RPC URL or a key leaking into output, logs or committed files.** Tests: Task 4 Step 8 (secret scan of the broadcast and deployment records prints `no key no rpc`), Task 5 Step 9 and Task 7 Step 4 (`grep -rlF` of the URL in the run directories counts 0), Task 5 `l1::tests::live_keys_are_dedicated` (errors name the file, never its content).

---

## File Structure

davinci-sequencer (`/home/p4u/davinci-sequencer`):

- Create `e2e/src/l1.rs`: L1-blob mode for the harness: mode switch, Ethereum anvil timing, the origin relayer, anvil pool reads, `L1Net`, live Ethereum settings and keys, `posts` (which Ethereum transaction carried a settlement's blobs), `check_origin`, the `MockL1Block` and `L1BlobVerifier` bindings.
- Modify `e2e/src/lib.rs`: `pub mod l1;`.
- Modify `e2e/src/chain.rs`: `DEV_KEYS[9]`, `Anvil::start_with`, `deploy_l1`, `Deployment.{l1_blob_verifier, l1_block}`.
- Modify `e2e/src/node.rs`: `NodeConfig.{l1_rpc_url, l1_send_rpc_url, l1_privkey_file, l1_max_post_fee}` and their env.
- Modify `e2e/src/net.rs`: `Net.{l1, min_balance}`, the L1 anvil in `Net::anvil`, the registry's DA mode check in `Net::open`, L1 balances, node L1 config; live: `preset`, `l1_mode`, `MIN_BALANCE_L1`, the preset-driven `live_target`.
- Modify `e2e/src/negative.rs`: `revert_label`, the L1 transition checks (`l1_transition`, `l1_tampered`), blob checks moved into `blob_transition`, either entry point in `grace`.
- Modify `e2e/tests/e2e.rs`: L1 setup checks, the restart scenario call, the Ethereum lines of the gas bill, the `op_live` test.
- Create `e2e/tests/e2e/l1_restart.rs`: node B killed with its blob post pending.
- Create `e2e/tests/e2e/op_live.rs`: the compact live run on OP Mainnet.
- Modify `e2e/tests/e2e/grace.rs`: `create`, `ballots`, `send` become `pub(super)`.
- Create `e2e/tests/bench_l1.rs`: the L1-blob batch benchmark.
- Modify `.github/workflows/main.yml`: the e2e setup in L1-blob mode.
- Modify `client/src/networks.rs`: the deployed `OPTIMISM` registry and start block, a deployed-preset test.
- Modify `docs/testing.md`, `docs/deployments.md`, `docs/configuration.md`, `docs/benchmarks.md`, `README.md`.
- Create `docs/superpowers/reports/<run date>-l1-blob-da-op-live.md` and `docs/superpowers/reports/<run date>-l1-blob-da-op-bench.json`.

davinci-contracts (`/home/p4u/davinci-contracts`):

- Create (by `deploy_all.sh`) `broadcast/DeployAll.s.sol/10/run-latest.json`, `deployments/10.json`.
- Modify `README.md` (deployments table), `docs/deployment.md` (OP Mainnet section).

Outside the repositories (never committed): `~/.config/davinci-l1blob/{env.sh, seq-a.l1.key, seq-a.op.key}`, `~/.cache/davinci-l1blob/{probe/, bin/}`.

---

### Task 1: The e2e harness in L1-blob mode on two anvils

**Files:**
- Create: `e2e/src/l1.rs`
- Modify: `e2e/src/lib.rs`, `e2e/src/chain.rs` (`DEV_KEYS` L21-35, `Anvil` L70-130, `Deployment` L184-191, `deploy` L199-270), `e2e/src/node.rs` (`NodeConfig` L95-169, `Node::spawn` L205-260), `e2e/src/net.rs` (module doc, imports, `Net` L117-147, `Net::open` L153-191, `Net::live` L236-241, `Net::anvil` L243-284, `check_balances` L311-343, `node_config` L345-382), `e2e/tests/e2e.rs` (imports L55-75, `setup_checks` L401-402), `.github/workflows/main.yml` (after the "Run e2e setup" step), `docs/testing.md` (CI paragraph)

**Interfaces:**
- Consumes: Plan 2's ABI `sequencer/abi/L1BlobVerifier.json`; `davinci_client::organizer::{ProcessRegistry (with submitStateTransitionCall, ProcessStateTransitionedL1, l1BlobVerifier()), RegistryDa, verify_registry}`; `davinci_client::networks::OP_L1_BLOCK`; davinci-contracts artifacts `MockL1Block.sol/MockL1Block.json`, `L1BlobVerifier.sol/L1BlobVerifier.json`; the node's env `DAVINCI_L1_RPC_URL`, `DAVINCI_L1_SEND_RPC_URL`, `DAVINCI_L1_PRIVKEY_FILE`, `DAVINCI_L1_MAX_POST_FEE`.
- Produces:
  - `davinci_e2e::l1::{enabled() -> Result<bool>, anvil_timing() -> Result<(u64, u64)>, relay_target(head: u64, lag: u64) -> Option<u64>, Relay {start(l1: DynProvider, op: DynProvider, mock: Address, lag: u64) -> Relay, origin() -> u64, errors() -> u64}, PendingTx {hash: B256, max_fee: u128, tip: u128, blob_fee: u128}, pending_in(pool: &serde_json::Value, from: Address, nonce: u64) -> Result<Option<PendingTx>>, L1Anvil {relay, anvil, block_time, lag; pause_mining(), resume_mining(), pending(from, nonce)}, L1Net {rpc: String, send_rpc: Option<String>, keys: Vec<(PathBuf, Address)>, verifier: Address, l1_block: Address, max_post_fee: Option<String>, min_balance: U256, anvil: Option<L1Anvil>; read_rpc() -> &str, provider() -> Result<DynProvider>, check_balances(n)}, anvil_net(dir: &Path, op_url: &str, dep: &chain::Deployment) -> Result<L1Net>, L1Post {pid, op_tx, op_block, l1_blocks, vhs, l1_txs}, posts(op_rpc: &str, l1_rpc: &str, registry: Address, from: u64) -> Result<Vec<L1Post>>, check_origin(op_rpc: &str, l1: &L1Net) -> Result<u64>, MockL1Block, L1BlobVerifier, IL1BlobVerifier, RELAY_KEY = 9, L1_CHAIN_ID = 1, L1_KEYS = [5, 6, 7]}`
  - `davinci_e2e::chain::{Anvil::start_with(log: &Path, block_time: u64, chain_id: Option<u64>) -> Result<Anvil>, deploy_l1(url: &str, dir: &Path) -> Result<Deployment>, Deployment.l1_blob_verifier: Address, Deployment.l1_block: Address}`, `DEV_KEYS: [&str; 10]`
  - `davinci_e2e::node::NodeConfig.{l1_rpc_url: Option<String>, l1_send_rpc_url: Option<String>, l1_privkey_file: Option<PathBuf>, l1_max_post_fee: Option<String>}`
  - `davinci_e2e::net::Net.{l1: Option<L1Net>, min_balance: U256}`

- [ ] **Step 1: Write the failing unit tests**

Create `e2e/src/l1.rs` with only its test module for now:

```rust
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
    }
}
```

(The pool fixture is the shape anvil 1.8.3's `txpool_content` returns for a blob transaction; checked on a local anvil while writing this plan.) In `e2e/src/lib.rs` add `pub mod l1;` after `pub mod fixture;`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-e2e --lib l1 2>&1 | tail -5`
Expected: FAIL to compile (`relay_target`, `pending_in`, `parse_da`, `Address`, `B256` not found).

- [ ] **Step 3: Implement `e2e/src/l1.rs`**

Replace the file with (the test module from Step 1 stays at the end, unchanged):

```rust
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
```

- [ ] **Step 4: `e2e/src/chain.rs`: key 9, the anvil's block time and chain id, `deploy_l1`**

Replace the `DEV_KEYS` doc comment and declaration line:

```rust
/// anvil's default dev accounts 0..=9 (public test keys). Account 0 deploys
/// and organizes; 1..=3 sign the nodes' settlements; 4 owns the census
/// contract (its own nonces, apart from the organizer's cached ones); 5..=7
/// are the DKG operators and 8 deploys the DKG (`crate::dkg`). In L1-blob
/// mode 9 moves the OP anvil's `MockL1Block` and, on the Ethereum anvil,
/// 5..=7 are the sequencers' blob keys (`crate::l1`).
pub const DEV_KEYS: [&str; 10] = [
```

and add after the last entry (`"dbda1821…7b97",`):

```rust
    "2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
```

(anvil account 9, `0xa0Ee7A142d267C1f36714E4a8F75612F20a79720`; `Anvil::start` already checks every `DEV_KEYS` account is funded.)

Replace the `Anvil` doc comment with:

```rust
/// anvil with Osaka and 1 s blocks (or `block_time`); its output goes to
/// `log`. Killed on drop.
```

and the head of `impl Anvil` (from `pub async fn start(log: &Path) -> Result<Anvil> {` down to and including the `.args([... "127.0.0.1", ])` call) with:

```rust
    pub async fn start(log: &Path) -> Result<Anvil> {
        Anvil::start_with(log, 1, None).await
    }

    /// [`Anvil::start`] with a block every `block_time` seconds and, if
    /// given, chain id `chain_id` (anvil's default is 31337).
    pub async fn start_with(log: &Path, block_time: u64, chain_id: Option<u64>) -> Result<Anvil> {
        let port = free_port()?;
        let out = File::create(log)?;
        let mut cmd = Command::new(foundry_tool("anvil"));
        if let Some(id) = chain_id {
            cmd.args(["--chain-id", &id.to_string()]);
        }
        let child = cmd
            .args(["--hardfork", "osaka", "--block-time"])
            .arg(block_time.to_string())
            .args(["--host", "127.0.0.1"])
```

(the following `.args(["--port", &port.to_string()])` and the rest of the function stay). Add to `struct Deployment`, after `dkg_adapter`:

```rust
    /// L1-blob mode: the registry's `L1BlobVerifier` and the `MockL1Block`
    /// it reads OP's origin from; zero in blob mode.
    pub l1_blob_verifier: Address,
    pub l1_block: Address,
```

Replace the line `pub async fn deploy(url: &str, dir: &Path, dkg_manager: Address) -> Result<Deployment> {` with:

```rust
pub async fn deploy(url: &str, dir: &Path, dkg_manager: Address) -> Result<Deployment> {
    deploy_with(url, dir, dkg_manager, false).await
}

/// [`deploy`] in L1-blob mode, for an OP-like anvil: `MockL1Block`, then an
/// `L1BlobVerifier` that reads it, then the registry on that verifier. No
/// DKG: its key modes are not on OP Stack chains.
pub async fn deploy_l1(url: &str, dir: &Path) -> Result<Deployment> {
    deploy_with(url, dir, Address::ZERO, true).await
}

async fn deploy_with(url: &str, dir: &Path, dkg_manager: Address, l1: bool) -> Result<Deployment> {
```

In the body, insert right before `let vk_hash = B256::from(ballot_vk_hash()?);`:

```rust
    // Blob mode deploys nothing more, so its addresses stay what they were.
    let (l1_block, l1_blob_verifier) = if l1 {
        let mock = deploy_code(
            &provider,
            bytecode(dir, "MockL1Block.sol/MockL1Block.json")?,
        )
        .await
        .context("deploy MockL1Block")?;
        let mut code = bytecode(dir, "L1BlobVerifier.sol/L1BlobVerifier.json")?;
        code.extend_from_slice(&mock.abi_encode());
        let v = deploy_code(&provider, code)
            .await
            .context("deploy L1BlobVerifier")?;
        (mock, v)
    } else {
        (Address::ZERO, Address::ZERO)
    };

```

replace `code.extend_from_slice(&Address::ZERO.abi_encode()); // _l1BlobVerifier: blob mode` with

```rust
    code.extend_from_slice(&l1_blob_verifier.abi_encode()); // zero: blob mode
```

insert before `let dkg_adapter = r.dkgAdapter().call().await?;`:

```rust
    ensure!(r.l1BlobVerifier().call().await? == l1_blob_verifier);
```

and add `l1_blob_verifier,` and `l1_block,` after `dkg_adapter,` in the returned `Deployment`.

- [ ] **Step 5: `e2e/src/node.rs`: the node's L1 settings**

Add to `struct NodeConfig`, after `start_block`:

```rust
    /// L1-blob mode, `None` in blob mode: Ethereum RPCs (`DAVINCI_L1_RPC_URL`,
    /// a `,` list), send-only endpoints, the file with the node's dedicated
    /// Ethereum key (an observer has none) and the post fee cap.
    pub l1_rpc_url: Option<String>,
    pub l1_send_rpc_url: Option<String>,
    pub l1_privkey_file: Option<PathBuf>,
    pub l1_max_post_fee: Option<String>,
```

add `l1_rpc_url: None, l1_send_rpc_url: None, l1_privkey_file: None, l1_max_post_fee: None,` after `start_block: None,` in `NodeConfig::new`; in `Node::spawn`, after the `DAVINCI_PRIVKEY_FILE` block:

```rust
        if let Some(k) = &cfg.l1_privkey_file {
            cmd.env("DAVINCI_L1_PRIVKEY_FILE", k);
        }
```

and append to the `knobs` array, after the `DAVINCI_MAX_BLOBS_PER_TX` entry:

```rust
            ("DAVINCI_L1_RPC_URL", cfg.l1_rpc_url.clone()),
            ("DAVINCI_L1_SEND_RPC_URL", cfg.l1_send_rpc_url.clone()),
            ("DAVINCI_L1_MAX_POST_FEE", cfg.l1_max_post_fee.clone()),
```

(Only a key *file* path goes into the environment, as for the OP key.)

- [ ] **Step 6: `e2e/src/net.rs`: the mode, the second anvil, balances and node settings**

Append to the module doc (after the `DAVINCI_E2E_DKG_MANAGER` sentence):

```rust
//!
//! `DAVINCI_E2E_DA=l1-blobs` runs the registry in L1-blob mode, see
//! [`crate::l1`]: on anvil a second, Ethereum-like anvil and an origin relayer.
```

Replace the two client imports and add the `l1` import:

```rust
use davinci_client::networks::{GNOSIS, OP_L1_BLOCK};
use davinci_client::organizer::{Organizer, RegistryDa, verify_registry};
```

```rust
use crate::l1::{self, L1Net};
```

(the latter after `use crate::dkg::{self, DkgStack, DkgWiring};`). In `struct Net`, between `dkg_stack` and `anvil`:

```rust
    /// L1-blob mode: Ethereum, the sequencers' Ethereum keys and, on anvil,
    /// the Ethereum anvil and its relayer. Declared before `anvil`: the
    /// relayer writes to it.
    pub l1: Option<L1Net>,
    /// Least native balance of each funded account ([`Net::check_balances`]).
    pub min_balance: U256,
```

In `Net::open`, right after the `eprintln!("chain {} registry {} verifier {} (pins ok)", ...)`:

```rust
        match (net.l1.as_mut(), info.da) {
            (None, RegistryDa::Blobs) => {}
            (Some(l1), RegistryDa::L1Blobs { verifier, l1_block }) => {
                if l1.anvil.is_some() {
                    ensure!(
                        (verifier, l1_block) == (l1.verifier, l1.l1_block),
                        "registry verifier {verifier} on L1Block {l1_block}, deployed {} on {}",
                        l1.verifier,
                        l1.l1_block
                    );
                } else {
                    ensure!(
                        l1_block == OP_L1_BLOCK,
                        "the registry's verifier reads L1Block at {l1_block}, not OP's predeploy"
                    );
                }
                (l1.verifier, l1.l1_block) = (verifier, l1_block);
                eprintln!("L1-blob mode: L1BlobVerifier {verifier} on L1Block {l1_block}");
            }
            (None, da) => {
                bail!("the registry is in L1-blob mode ({da:?}): set DAVINCI_E2E_DA=l1-blobs")
            }
            (Some(_), RegistryDa::Blobs) => {
                bail!("L1-blob mode asked for, but the registry is in blob mode")
            }
        }
```

In `Net::live`'s returned struct add `l1: None, min_balance: U256::from(MIN_BALANCE),` before `anvil: None,` (Task 5 makes the live side real). In `Net::anvil`, insert at the top:

```rust
        let l1_mode = l1::enabled()?;
        ensure!(
            !(l1_mode && dkg::enabled()),
            "DAVINCI_E2E_DKG: the DKG key modes are not on OP Stack chains (L1-blob mode)"
        );
```

replace `let dep = chain::deploy(&anvil.url, &contracts, manager).await?;` with

```rust
        let (dep, l1) = if l1_mode {
            let dep = chain::deploy_l1(&anvil.url, &contracts).await?;
            let l1 = l1::anvil_net(dir, &anvil.url, &dep).await?;
            (dep, Some(l1))
        } else {
            (chain::deploy(&anvil.url, &contracts, manager).await?, None)
        };
```

and add `l1, min_balance: U256::from(MIN_BALANCE),` before `anvil: Some(anvil),`. In `check_balances`, change the doc comment's second line to `` /// `min_balance` (the first `n_seq` sequencers only). In L1-blob mode also `` and add `/// their Ethereum keys.`; replace `U256::from(MIN_BALANCE)` by `self.min_balance` in both places, and insert before the final `Ok(())`:

```rust
        if let Some(l1) = &self.l1 {
            l1.check_balances(n_seq).await?;
        }
```

In `node_config`, before `c.start_block = self.start_block;`:

```rust
        if let Some(l1) = &self.l1 {
            c.l1_rpc_url = Some(l1.rpc.clone());
            c.l1_send_rpc_url = l1.send_rpc.clone();
            c.l1_privkey_file = seq
                .map(|i| {
                    l1.keys
                        .get(i)
                        .map(|(p, _)| p.clone())
                        .context("no Ethereum key for this sequencer")
                })
                .transpose()?;
            c.l1_max_post_fee = l1.max_post_fee.clone();
            // A landing also waits for OP's origin to pass the blob block:
            // about lag x block time on anvil, 90-100 s on OP Mainnet.
            c.settle_margin = Some(if self.live { "120s" } else { "15s" }.into());
        }
```

- [ ] **Step 7: Run the unit tests**

Run: `cargo test -p davinci-e2e --lib 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: `test result: ok. 46 passed` (the three new `l1::tests` included).

- [ ] **Step 8: The setup phase checks the L1 plumbing (no prover)**

In `e2e/tests/e2e.rs`, add the imports (the first two before `use anyhow::...`, `ProcessRegistry` into the organizer list, `l1` after `fixture`):

```rust
use alloy::primitives::{Bytes, FixedBytes};
use alloy::sol_types::SolCall;
```

```rust
use davinci_client::organizer::{
    CreatedProcess, KeyMode, NewProcess, OnchainProcess, Organizer, OrganizerSecret,
    ProcessRegistry, RegistryReader, merkle_census,
};
```

```rust
use davinci_e2e::l1;
```

In `setup_checks`, insert after `bill.extend(c.receipts().iter().map(|(l, r)| TxCost::of(l.clone(), r)));`:

```rust

    // L1-blob mode: OP's origin follows Ethereum, and blob mode's entry
    // point is refused before anything else is read.
    if let Some(l1) = &net.l1 {
        let origin = l1::check_origin(&net.rpc, l1).await?;
        let blob_call = ProcessRegistry::submitStateTransitionCall {
            processId: FixedBytes(p1.id),
            publicValues: Bytes::new(),
            proofBytes: Bytes::new(),
            commitments: Vec::new(),
            ys: Vec::new(),
            kzgProofs: Vec::new(),
        };
        let got = negative::simulate(
            &net.rpc,
            org.address(),
            net.registry,
            blob_call.abi_encode(),
        )
        .await?;
        ensure!(
            got == negative::Outcome::Reverted("WrongDAMode".into()),
            "submitStateTransition on an L1-mode registry: {got}"
        );
        say!("L1-blob setup ok: OP origin at Ethereum block {origin}, blob entry point refused");
    }
```

- [ ] **Step 9: Run the setup phase in both modes**

```bash
cd /home/p4u/davinci-contracts && git branch --show-current && ~/.foundry/bin/forge build 2>&1 | tail -1
cd /home/p4u/davinci-sequencer && mkdir -p ~/.cache/davinci-e2e
DAVINCI_E2E_SETUP=1 DAVINCI_E2E_DA=l1-blobs TMPDIR=$HOME/.cache/davinci-e2e cargo test -p davinci-e2e --test e2e setup_without_nodes -- --nocapture 2>&1 | grep -E "L1-blob|pins ok|setup ok|^test |test result"
DAVINCI_E2E_SETUP=1 TMPDIR=$HOME/.cache/davinci-e2e cargo test -p davinci-e2e --test e2e setup_without_nodes -- --nocapture 2>&1 | grep -E "pins ok|setup ok|test result"
```
Expected, L1-blob mode (addresses are anvil's deterministic ones; checked while writing this plan):
```
L1-blob mode: Ethereum anvil http://127.0.0.1:<port> (chain 1, 2 s blocks), OP origin 6 blocks behind
chain 31337 registry 0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9 verifier 0x5FbDB2315678afecb367f032d93F642f64180aa3 (pins ok)
L1-blob mode: L1BlobVerifier 0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0 on L1Block 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512
[e2e ...] setup ok; expected tally [36, 24, 14, 50]
[e2e ...] dynamic census setup ok
[e2e ...] L1-blob setup ok: OP origin at Ethereum block <n>, blob entry point refused
test setup_without_nodes ... ok
```
and blob mode unchanged: `registry 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512 verifier 0x5FbDB2315678afecb367f032d93F642f64180aa3 (pins ok)`, `setup ok`, `test result: ok. 1 passed`.

- [ ] **Step 10: CI runs the L1 setup too**

In `.github/workflows/main.yml`, after the "Run e2e setup" step:

```yaml
      # The same in L1-blob mode: a second (Ethereum-like) anvil, the origin
      # relayer and the registry on an L1BlobVerifier.
      - name: Run e2e setup (L1-blob mode)
        working-directory: davinci-sequencer
        env:
          DAVINCI_E2E_SETUP: "1"
          DAVINCI_E2E_DA: l1-blobs
        run: cargo test -p davinci-e2e --test e2e --locked setup_without_nodes -- --nocapture
```

In `docs/testing.md` ("CI"), replace `e2e setup phase (`DAVINCI_E2E_SETUP=1`), the emulator` with `e2e setup phase (`DAVINCI_E2E_SETUP=1`) in blob mode and in L1-blob mode, the emulator`. (This step fails in CI until `CONTRACTS_REF` names a davinci-contracts commit with `L1BlobVerifier` and `MockL1Block`: see the open question in the report.)

- [ ] **Step 11: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p davinci-e2e 2>&1 | grep -E "^test result" | sort | uniq -c
git add e2e/src/l1.rs e2e/src/lib.rs e2e/src/chain.rs e2e/src/node.rs e2e/src/net.rs e2e/tests/e2e.rs .github/workflows/main.yml docs/testing.md
git commit -m "test(e2e): l1-blob mode on two anvils with an origin relayer"
```

---

### Task 2: L1 negative replays through `eth_call`

**Files:**
- Modify: `e2e/src/negative.rs` (imports L5-15, `Sim::call` L102-133, `run` L143-327, `grace` L363-448, tests L590-614)

**Interfaces:**
- Consumes: Task 1 `davinci_e2e::l1::L1BlobVerifier` (errors enum `L1BlobVerifierErrors`); `davinci_client::organizer::{ProcessRegistry::{submitStateTransitionL1Call, submitStateTransitionCall, WrongDAMode}, IL1BlobVerifier::{L1BlobProof, L1TxProof}, revert_name}`.
- Produces: `pub fn revert_label(data: &[u8]) -> String`; `pub fn l1_tampered(call: &PR::submitStateTransitionL1Call) -> Vec<(&'static str, Vec<u8>, &'static [&'static str])>`; `run()` and `grace()` accept a settlement through either entry point. Check names (Task 3 expects them): `L1 transition control at N-1`, `L1 replay at N`, `blob-mode entry point`, `L1 proof byte flipped at N-1`, `L1 header chain broken at N-1`, `L1 tx proof of another trie at N-1`, `L1 commitment swapped at N-1`, `L1 extra blob in the arrays at N-1`.

The expected reverts follow the contracts on `feat/l1-blob-da`: `submitStateTransition` reverts `WrongDAMode` first in L1 mode; `_settleL1` checks the array lengths (`BlobCountMismatch`), then the pair digest (`InvalidBlobsDigest`), then `L1BlobVerifier.verify` (a header whose keccak is not its successor's `parentHash` is `BrokenHeaderChain`; the vendored `MerkleTrie` reverts `Error("MerkleTrie: invalid root hash")` when the first proof node does not hash to `transactionsRoot`), then the PLONK proof (`InvalidProof`).

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `e2e/src/negative.rs`:

```rust
    #[test]
    fn revert_labels() {
        use alloy::sol_types::{Revert, SolError};
        let broken = L1BlobVerifier::BrokenHeaderChain {
            index: U256::from(1),
        };
        assert_eq!(revert_label(&broken.abi_encode()), "BrokenHeaderChain");
        let trie = Revert::from("MerkleTrie: invalid root hash");
        assert_eq!(
            revert_label(&trie.abi_encode()),
            "MerkleTrie: invalid root hash"
        );
        let mode = PR::WrongDAMode {};
        assert_eq!(revert_label(&mode.abi_encode()), "WrongDAMode");
        assert_eq!(revert_label(&[1, 2, 3, 4, 5]), "0x01020304");
    }

    #[test]
    fn l1_tampering() {
        use davinci_client::organizer::IL1BlobVerifier::{L1BlobProof, L1TxProof};
        let call = PR::submitStateTransitionL1Call {
            processId: FixedBytes([7; 31]),
            publicValues: vec![0u8; 512].into(),
            proofBytes: vec![9u8; 64].into(),
            commitments: vec![vec![0xaa; 48].into()],
            ys: vec![B256::repeat_byte(1)],
            kzgProofs: vec![vec![0xbb; 48].into()],
            l1: L1BlobProof {
                headers: vec![vec![0xf9, 1].into(), vec![0xf9, 2].into()],
                txs: vec![L1TxProof {
                    headerIndex: U256::from(1),
                    key: vec![0x80].into(),
                    proof: vec![vec![0xf8, 3].into(), vec![0xf8, 4].into()],
                }],
            },
        };
        let t = l1_tampered(&call);
        let names: Vec<_> = t.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(names.len(), 6, "{names:?}");
        let decode = |i: usize| PR::submitStateTransitionL1Call::abi_decode(&t[i].1).unwrap();
        assert!(t[0].1.starts_with(&PR::submitStateTransitionCall::SELECTOR));
        assert_ne!(decode(1).proofBytes, call.proofBytes);
        let h = decode(2).l1.headers;
        assert_eq!(
            (h.len(), &h[0], &h[1]),
            (3, &call.l1.headers[0], &call.l1.headers[0])
        );
        assert_eq!(decode(3).l1.txs[0].proof[0].as_ref(), &[0xc0]);
        assert_eq!(decode(3).l1.txs[0].proof[1], call.l1.txs[0].proof[1]);
        let c = decode(4).commitments;
        assert!(c.len() == 1 && c[0] != call.commitments[0] && c[0].len() == 48);
        let e = decode(5);
        assert_eq!(
            (e.commitments.len(), e.ys.len(), e.kzgProofs.len()),
            (2, 2, 2)
        );
        // Every tampered call keeps the pid and the publics.
        for i in 1..6 {
            let d = decode(i);
            assert_eq!(
                (d.processId, &d.publicValues),
                (call.processId, &call.publicValues)
            );
        }
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p davinci-e2e --lib negative 2>&1 | tail -5`
Expected: FAIL to compile (`revert_label`, `l1_tampered`, `L1BlobVerifier` not found).

- [ ] **Step 3: Decode the L1 verifier's errors and string reverts**

Add after `use crate::chain;`:

```rust
use crate::l1::L1BlobVerifier;
```

In `Sim::call`, replace `Some(d) => Outcome::Reverted(revert_name(&d)),` with `Some(d) => Outcome::Reverted(revert_label(&d)),`, and insert before the doc comment `` /// `input` from `from` to `to`, simulated at the latest block. ``:

```rust
/// A revert's name: the registry's and its adapters' errors (the client's
/// `revert_name`), else the `L1BlobVerifier`'s, else an `Error(string)`
/// reason (the vendored `MerkleTrie` reverts with strings).
pub fn revert_label(data: &[u8]) -> String {
    use alloy::sol_types::{Revert, SolError, SolInterface};
    let name = revert_name(data);
    if !name.starts_with("0x") {
        return name;
    }
    if let Ok(e) = L1BlobVerifier::L1BlobVerifierErrors::abi_decode(data) {
        let d = format!("{e:?}");
        return d
            .split(['(', ' ', '{'])
            .next()
            .unwrap_or_default()
            .to_string();
    }
    if let Ok(r) = Revert::abi_decode(data) {
        return r.reason;
    }
    name
}

```

(Registry names win first, so blob mode's verdicts read as before.)

- [ ] **Step 4: Split `run`'s transition checks by entry point**

In `run`, replace everything from the line `    let hashes = tx` (right after `let input = tx.inner.input().to_vec();`) up to, not including, the line `    // The results.` with:

```rust
    let (before, at) = (BlockId::number(tr.block - 1), BlockId::number(tr.block));
    if input.starts_with(&PR::submitStateTransitionL1Call::SELECTOR) {
        out.extend(l1_transition(&sim, from_tr, &input, (before, at)).await?);
    } else {
        let hashes = tx
            .inner
            .blob_versioned_hashes()
            .unwrap_or_default()
            .to_vec();
        let fee = tx.inner.max_fee_per_blob_gas().unwrap_or(1_000_000_000);
        out.extend(blob_transition(&sim, from_tr, &input, (hashes, fee), (before, at)).await?);
    }

```

and insert before the doc comment `/// Bytes of words 18 and 19 in the 512-byte publics` the blob-mode checks (the removed lines, unchanged but for `input` being a slice) and the L1 ones:

```rust
/// A blob-mode transition replayed and tampered with at the block before it
/// (`before`) and replayed at its own (`at`), with its blob hashes and fee
/// when the RPC simulates them.
async fn blob_transition<P: Provider>(
    sim: &Sim<P>,
    from_tr: Address,
    input: &[u8],
    (hashes, fee): (Vec<B256>, u128),
    (before, at): (BlockId, BlockId),
) -> Result<Vec<Check>> {
    let mut out = Vec::new();
    let mut blobs = Some((hashes.clone(), fee));
    let call = PR::submitStateTransitionCall::abi_decode(input).context("decode transition")?;

    // Whether this RPC simulates blob hashes. Refused fields: retry without
    // them. MissingBlob: it ignores them. Either way the rest runs without
    // blob fields (the flips reach the verifier before the openings) and the
    // extra-hash check is INCONCLUSIVE.
    let mut got = sim
        .call(from_tr, input.to_vec(), blobs.clone(), before)
        .await;
    if matches!(got, Outcome::Rpc(_)) {
        blobs = None;
        got = sim.call(from_tr, input.to_vec(), None, before).await;
    }
    let mut chk = control("transition control at N-1", got);
    if matches!(&chk.got, Outcome::Reverted(n) if n == "MissingBlob") {
        blobs = None;
        chk.inconclusive = true;
        chk.problem = Some("the RPC does not simulate blob hashes".into());
    }
    let blob_sim = chk.got == Outcome::Ok && blobs.is_some();
    out.push(chk);
    let replay = sim.call(from_tr, input.to_vec(), blobs.clone(), at).await;
    out.push(judge("replay at N", replay, &["InvalidStateRoot"]));

    let mut c = call.clone();
    let mut proof = c.proofBytes.to_vec();
    let mid = proof.len() / 2;
    proof[mid] ^= 1;
    c.proofBytes = proof.into();
    let got = sim
        .call(from_tr, c.abi_encode(), blobs.clone(), before)
        .await;
    out.push(judge("proof byte flipped at N-1", got, &["InvalidProof"]));

    let mut c = call.clone();
    let mut pubs = c.publicValues.to_vec();
    pubs[FLIP_AT] ^= 1;
    c.publicValues = pubs.into();
    let got = sim
        .call(from_tr, c.abi_encode(), blobs.clone(), before)
        .await;
    out.push(judge(
        "root-after byte flipped at N-1",
        got,
        &["InvalidProof"],
    ));

    let mut c = call.clone();
    c.commitments.push(c.commitments[0].clone());
    c.ys.push(c.ys[0]);
    c.kzgProofs.push(c.kzgProofs[0].clone());
    let got = sim
        .call(from_tr, c.abi_encode(), blobs.clone(), before)
        .await;
    out.push(judge(
        "extra blob in the arrays at N-1",
        got,
        &["BlobCountMismatch"],
    ));

    let mut more = hashes.clone();
    let mut extra = B256::with_last_byte(1);
    extra.0[0] = 1; // the KZG version byte
    more.push(extra);
    if blob_sim {
        let got = sim
            .call(from_tr, input.to_vec(), Some((more, fee)), before)
            .await;
        out.push(judge("extra blob hash at N-1", got, &["BlobCountMismatch"]));
    } else {
        out.push(Check {
            name: "extra blob hash at N-1",
            got: Outcome::Rpc("skipped".into()),
            problem: Some("the RPC does not simulate blob hashes".into()),
            inconclusive: true,
        });
    }
    Ok(out)
}

/// An L1-blob transition (`submitStateTransitionL1`) replayed at its own
/// block and, tampered with, at the block before it; no blob fields: its
/// blobs are on Ethereum.
async fn l1_transition<P: Provider>(
    sim: &Sim<P>,
    from_tr: Address,
    input: &[u8],
    (before, at): (BlockId, BlockId),
) -> Result<Vec<Check>> {
    let call =
        PR::submitStateTransitionL1Call::abi_decode(input).context("decode L1 transition")?;
    let mut out = Vec::new();
    let got = sim.call(from_tr, input.to_vec(), None, before).await;
    out.push(control("L1 transition control at N-1", got));
    let got = sim.call(from_tr, input.to_vec(), None, at).await;
    out.push(judge("L1 replay at N", got, &["InvalidStateRoot"]));
    for (name, data, want) in l1_tampered(&call) {
        let got = sim.call(from_tr, data, None, before).await;
        out.push(judge(name, got, want));
    }
    Ok(out)
}

/// The tampered L1-mode calls: check name, calldata and the reverts that
/// pass it.
pub fn l1_tampered(
    call: &PR::submitStateTransitionL1Call,
) -> Vec<(&'static str, Vec<u8>, &'static [&'static str])> {
    let mut out: Vec<(&'static str, Vec<u8>, &'static [&'static str])> = Vec::new();
    // Blob mode's entry point, on a registry in L1-blob mode.
    let blob = PR::submitStateTransitionCall {
        processId: call.processId,
        publicValues: call.publicValues.clone(),
        proofBytes: call.proofBytes.clone(),
        commitments: call.commitments.clone(),
        ys: call.ys.clone(),
        kzgProofs: call.kzgProofs.clone(),
    };
    out.push(("blob-mode entry point", blob.abi_encode(), &["WrongDAMode"]));
    let mut c = call.clone();
    let mut proof = c.proofBytes.to_vec();
    let mid = proof.len() / 2;
    proof[mid] ^= 1;
    c.proofBytes = proof.into();
    out.push((
        "L1 proof byte flipped at N-1",
        c.abi_encode(),
        &["InvalidProof"],
    ));
    // The newest header twice: the second is not the first one's parent.
    let mut c = call.clone();
    if let Some(h) = c.l1.headers.first().cloned() {
        c.l1.headers.insert(0, h);
    }
    out.push((
        "L1 header chain broken at N-1",
        c.abi_encode(),
        &["BrokenHeaderChain"],
    ));
    // A trie proof whose root node (an empty RLP list) is not the block's
    // transactions root.
    let mut c = call.clone();
    if let Some(n) = c.l1.txs.first_mut().and_then(|t| t.proof.first_mut()) {
        *n = Bytes::from_static(&[0xc0]);
    }
    out.push((
        "L1 tx proof of another trie at N-1",
        c.abi_encode(),
        &["MerkleTrie: invalid root hash"],
    ));
    // Commitments out of the order the guest's digest fixes; with one blob,
    // another commitment.
    let mut c = call.clone();
    if c.commitments.len() >= 2 {
        c.commitments.swap(0, 1);
        c.ys.swap(0, 1);
        c.kzgProofs.swap(0, 1);
    } else if let Some(cm) = c.commitments.first_mut() {
        let mut b = cm.to_vec();
        if let Some(x) = b.last_mut() {
            *x ^= 1;
        }
        *cm = b.into();
    }
    out.push((
        "L1 commitment swapped at N-1",
        c.abi_encode(),
        &["InvalidBlobsDigest"],
    ));
    // One (commitment, y, proof) more than the guest published.
    let mut c = call.clone();
    if let (Some(cm), Some(y), Some(k)) = (
        c.commitments.first().cloned(),
        c.ys.first().copied(),
        c.kzgProofs.first().cloned(),
    ) {
        c.commitments.push(cm);
        c.ys.push(y);
        c.kzgProofs.push(k);
    }
    out.push((
        "L1 extra blob in the arrays at N-1",
        c.abi_encode(),
        &["BlobCountMismatch"],
    ));
    out
}

```

- [ ] **Step 5: `grace` takes either entry point**

In `grace`, replace

```rust
    let mut c = PR::submitStateTransitionCall::abi_decode(&tin).context("decode transition")?;
    let mut pubs = c.publicValues.to_vec();
    pubs[COUNTS].fill(0);
    c.publicValues = pubs.into();
    let got = sim
        .call(tfrom, c.abi_encode(), None, BlockId::number(tr.block - 1))
        .await;
```

with

```rust
    let empty = if tin.starts_with(&PR::submitStateTransitionL1Call::SELECTOR) {
        let mut c =
            PR::submitStateTransitionL1Call::abi_decode(&tin).context("decode L1 transition")?;
        let mut pubs = c.publicValues.to_vec();
        pubs[COUNTS].fill(0);
        c.publicValues = pubs.into();
        c.abi_encode()
    } else {
        let mut c = PR::submitStateTransitionCall::abi_decode(&tin).context("decode transition")?;
        let mut pubs = c.publicValues.to_vec();
        pubs[COUNTS].fill(0);
        c.publicValues = pubs.into();
        c.abi_encode()
    };
    let got = sim
        .call(tfrom, empty, None, BlockId::number(tr.block - 1))
        .await;
```

(`EmptyTransition` is a shared prepare check, before either mode's blob checks.)

- [ ] **Step 6: Run the tests**

Run: `cargo test -p davinci-e2e --lib negative 2>&1 | grep -E "^test |test result"`
Expected: `verdicts`, `revert_labels`, `l1_tampering` ok; `test result: ok. 3 passed`.
Run: `DAVINCI_E2E_SETUP=1 TMPDIR=$HOME/.cache/davinci-e2e cargo test -p davinci-e2e --test e2e setup_without_nodes 2>&1 | grep "test result"`
Expected: `test result: ok. 1 passed` (blob mode unchanged).

- [ ] **Step 7: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
git add e2e/src/negative.rs
git commit -m "test(e2e): l1-blob negative replays through eth_call"
```

---

### Task 3: The scenario in L1-blob mode, and the `make e2e` gate

**Files:**
- Create: `e2e/tests/e2e/l1_restart.rs`
- Modify: `e2e/tests/e2e/grace.rs` (`create` L83, `ballots` L101, `send` L122), `e2e/tests/e2e.rs` (module list L144-145, `scenario` L877-879 and L950-957 and L1340-1345, `costs` L497-505), `docs/testing.md` (e2e section)

**Interfaces:**
- Consumes: Task 1 `Net.l1`, `L1Net.{keys, anvil, provider, read_rpc}`, `L1Anvil::{pause_mining, resume_mining, pending}`, `l1::posts`; Task 2 checks (run by `DAVINCI_E2E_NEGATIVE=1` and by `grace::closed_after_grace`); e2e helpers `wait_settled`, `check_settled`, `alive`, `settle_timeout`; node log line `L1 post of an earlier run on record; the next post replaces it if still pending` (Plan 2 Task 7).
- Produces: `l1_restart::kill_after_post(nodes, cfgs, bin, net, org, reader, (p1, tree), prover) -> Result<()>`; `grace::{create, ballots, send}` as `pub(super)`; the gas bill lists `L1 blobs <pid>` lines in L1-blob mode.

- [ ] **Step 1: Make three grace helpers visible to sibling modules**

In `e2e/tests/e2e/grace.rs` change `async fn create(` to `pub(super) async fn create(`, `fn ballots(` to `pub(super) fn ballots(` and `async fn send(` to `pub(super) async fn send(`.

- [ ] **Step 2: The restart with a pending post**

Create `e2e/tests/e2e/l1_restart.rs`:

```rust
//! L1-blob mode on anvil: node B killed while its blob transaction waits in
//! Ethereum's pool (the Ethereum anvil paused) and restarted on the same
//! datadir. Its batch secrets died with it, so it proves the batch again,
//! and its first post replaces the pending transaction at the same nonce with
//! every fee doubled instead of queueing behind it. The settled transition
//! carries the replacement's blobs.

use super::*;
use alloy::providers::Provider;

/// `fx::vote_k` tag of this process's ballots.
const TAG_RESTART: u64 = 14;
/// Its voters, all sent to node B.
const VOTERS: std::ops::Range<usize> = 0..4;

#[allow(clippy::too_many_arguments)]
pub(super) async fn kill_after_post(
    nodes: &mut [Node],
    cfgs: &[NodeConfig],
    bin: &Path,
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
) -> Result<()> {
    let l1 = net.l1.as_ref().context("L1-blob mode only")?;
    let eth = l1
        .anvil
        .as_ref()
        .context("anvil only: it pauses Ethereum")?;
    let (_, from) = l1.keys[B];
    let read = l1.provider()?;
    let p = grace::create(nodes, org, reader, p1, B).await?;
    let reqs = grace::ballots(prover, TAG_RESTART, &p, tree, VOTERS, &[])?;
    let nonce = read.get_transaction_count(from).await?;
    ensure!(
        read.get_transaction_count(from).pending().await? == nonce,
        "node-b has an Ethereum transaction pending already"
    );
    eth.pause_mining().await?;
    say!("Ethereum paused at L1 nonce {nonce} of node-b ({from})");
    let r: Result<Vec<Sent>> = async {
        let sent = grace::send(nodes, "restart", VOTERS, &reqs, |_| B).await?;
        let first = wait::until(
            "node-b's blob post in Ethereum's pool",
            settle_timeout(),
            Duration::from_millis(200),
            || async {
                alive(nodes)?;
                eth.pending(from, nonce).await
            },
        )
        .await?;
        nodes[B].kill()?;
        say!(
            "node-b killed with {} pending at L1 nonce {nonce}",
            first.hash
        );
        nodes[B] = Node::start(bin, &cfgs[B]).await.context("restart node-b")?;
        let second = wait::until(
            "node-b's replacement at the same L1 nonce",
            settle_timeout(),
            Duration::from_millis(200),
            || async {
                alive(nodes)?;
                Ok(eth
                    .pending(from, nonce)
                    .await?
                    .filter(|t| t.hash != first.hash))
            },
        )
        .await?;
        ensure!(
            second.max_fee >= 2 * first.max_fee
                && second.tip >= 2 * first.tip
                && second.blob_fee >= 2 * first.blob_fee,
            "replacement fees {second:?}, want each at least twice {first:?}"
        );
        ensure!(
            !nodes[B]
                .log_lines(&["L1 post of an earlier run on record"])
                .is_empty(),
            "node-b did not pick its post record up"
        );
        say!(
            "node-b replaced {} with {} at L1 nonce {nonce}, every fee doubled",
            first.hash,
            second.hash
        );
        eth.resume_mining().await?;
        wait_settled(nodes, &sent, "restart batch").await?;
        ensure!(
            read.get_transaction_receipt(first.hash).await?.is_none(),
            "the replaced post was mined"
        );
        read.get_transaction_receipt(second.hash)
            .await?
            .context("the replacement was not mined")?;
        let from_block = net.start_block.unwrap_or(net.from_block);
        let posts: Vec<_> = l1::posts(&net.rpc, l1.read_rpc(), net.registry, from_block)
            .await?
            .into_iter()
            .filter(|t| t.pid == p.id)
            .collect();
        ensure!(
            posts.len() == 1 && posts[0].l1_txs == [second.hash],
            "the settlement's blobs are not the replacement's: {posts:?}"
        );
        Ok(sent)
    }
    .await;
    // Ethereum runs again whatever happened.
    let resumed = eth.resume_mining().await;
    let sent = r?;
    resumed?;
    check_settled(nodes, reader, &p.id, &sent, (VOTERS.len() as u64, 0)).await?;
    org.cancel_process(&p.id).await.context("cancel")?;
    say!(
        "{}: restart with a pending post ok; OP origin at Ethereum block {}, {} failed relay steps",
        ProcessId(p.id),
        eth.relay.origin(),
        eth.relay.errors()
    );
    Ok(())
}
```

(Pausing Ethereum is what makes the pending state deterministic: with 2 s blocks the post would be mined before the kill. anvil accepts any higher-priced replacement, unlike the real blob pool, so the test itself asserts the 100 % bump on all three fees.)

- [ ] **Step 3: Wire it into the scenario and the gas bill**

In `e2e/tests/e2e.rs`, after `mod grace;`:

```rust
#[path = "e2e/l1_restart.rs"]
mod l1_restart;
```

In `scenario`, replace

```rust
    let net = Net::open(dir).await?;
    net.check_balances(OBS).await?;
```

with

```rust
    let net = Net::open(dir).await?;
    ensure!(
        !(net.live && net.l1.is_some()),
        "the full scenario does not run live in L1-blob mode: use DAVINCI_E2E_OP_LIVE=1"
    );
    net.check_balances(OBS).await?;
```

replace `Some(c) if net.live => ensure!(c == 2, "{}: blob cap {c}, want 2", n.name),` with

```rust
            Some(c) if net.live && net.l1.is_none() => {
                ensure!(c == 2, "{}: blob cap {c}, want 2", n.name)
            }
```

and insert before `        // Dynamic censuses on the same chain, prover and nodes.`:

```rust
        // L1-blob mode: a node killed with its blob post pending on Ethereum.
        if net.l1.is_some() {
            l1_restart::kill_after_post(
                &mut nodes,
                &seq_cfgs,
                &bin,
                &net,
                &org,
                &reader,
                (&p1, &tree),
                &prover,
            )
            .await
            .context("L1 restart with a pending post")?;
        }

```

In `costs`, replace

```rust
    Ok(cost::report(
        &format!("gas bill, chain {}", net.chain_id),
        &all,
    ))
```

with

```rust
    // L1-blob mode: the Ethereum blob transactions behind the settlements
    // (not the replaced or lost ones), paid in ETH on Ethereum.
    let mut title = format!("gas bill, chain {}", net.chain_id);
    if let Some(l1) = &net.l1 {
        title.push_str(" and Ethereum");
        for p in l1::posts(&net.rpc, l1.read_rpc(), net.registry, from)
            .await?
            .into_iter()
            .filter(|p| created.contains(&p.pid))
        {
            for tx in p.l1_txs {
                let label = format!("L1 blobs {}", ProcessId(p.pid));
                all.push(cost::fetch(l1.read_rpc(), label, tx).await?);
            }
        }
    }
    Ok(cost::report(&title, &all))
```

- [ ] **Step 4: Document the mode**

In `docs/testing.md`, insert after the bullet list of the e2e section (after "...with queued revotes and a node failover on the way."):

~~~markdown
With `DAVINCI_E2E_DA=l1-blobs` the same scenario runs in L1-blob mode. The registry settles on the
usual anvil (the OP side) through an `L1BlobVerifier` that reads a `MockL1Block`, and the blobs
go to a second anvil (the Ethereum side: chain id 1, Osaka, 2 s blocks). An origin relayer in the
harness copies the Ethereum block `DAVINCI_E2E_L1_LAG` (default 6) blocks behind the head into
`MockL1Block`, as OP's derivation would. Each sequencer posts from its own Ethereum key (accounts
5 to 7 of that anvil), every node reads blobs from the Ethereum anvil, and the scenario adds a
node killed while its blob transaction waits in the paused Ethereum pool: after the restart it
proves the batch again and replaces the pending transaction at its nonce with every fee doubled.
DKG processes are refused (their key modes are not on OP Stack chains). With
`DAVINCI_E2E_NEGATIVE=1` the L1 settlement is replayed tampered: blob mode's entry point
(`WrongDAMode`), a broken header chain (`BrokenHeaderChain`), a transaction proof of another trie
(`MerkleTrie: invalid root hash`), a swapped commitment (`InvalidBlobsDigest`), an extra blob
(`BlobCountMismatch`) and a flipped proof byte (`InvalidProof`).
~~~

replace the two `make e2e` lines of the code block below with:

```bash
make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e.log                              # blob mode
DAVINCI_E2E_DA=l1-blobs make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e-l1.log   # L1-blob mode
DAVINCI_SEQUENCER_BIN=/path/to/bin make e2e                                     # a prebuilt node binary
```

and add two rows to the variables table, after `DAVINCI_E2E_SETUP=1`:

```markdown
| `DAVINCI_E2E_DA=l1-blobs` | L1-blob mode, see above. With `DAVINCI_E2E_SETUP=1` it also checks that the relayer moves `MockL1Block` and that blob mode's entry point reverts `WrongDAMode`. |
| `DAVINCI_E2E_L1_BLOCK_TIME`, `DAVINCI_E2E_L1_LAG` | L1-blob mode on anvil: the Ethereum anvil's block time (default 2 s) and the relayer's lag (default 6 blocks, at most 48). |
```

- [ ] **Step 5: Build, lint and run the gated pieces without the prover**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test -p davinci-e2e 2>&1 | grep -E "^test result" | sort | uniq -c
DAVINCI_E2E_SETUP=1 DAVINCI_E2E_DA=l1-blobs TMPDIR=$HOME/.cache/davinci-e2e cargo test -p davinci-e2e --test e2e setup_without_nodes 2>&1 | grep "test result"
```
Expected: no warnings; every line `test result: ok`; the L1 setup `test result: ok. 1 passed`.

- [ ] **Step 6: Commit**

```bash
git add e2e/tests/e2e.rs e2e/tests/e2e/grace.rs e2e/tests/e2e/l1_restart.rs docs/testing.md
git commit -m "test(e2e): restart with a pending l1 blob post and the ethereum gas bill"
```

- [ ] **Step 7: Build the node once for the GPU runs**

```bash
cd /home/p4u/davinci-sequencer && cargo build --release -p davinci-sequencer --bin davinci-sequencer 2>&1 | tail -1
mkdir -p ~/.cache/davinci-l1blob/bin
cp target/release/davinci-sequencer ~/.cache/davinci-l1blob/bin/davinci-sequencer-$(git rev-parse --short HEAD)
ls ~/.cache/davinci-l1blob/bin/
```
Expected: `Finished release ...` and the copied binary named after the commit. Use that path as `DAVINCI_SEQUENCER_BIN` from now on, so a tree change during a run cannot swap the binary.

- [ ] **Step 8: Gate: the full e2e in L1-blob mode on the shared prover**

Go/no-go: `curl -s "${DAVINCI_ZKVM_URL:-http://127.0.0.1:8080}/health"` shows `"status":"ok"` and `"queue_len":0` (the production node is idle). The run sends batches of at most 8 votes (2 in the backlog phase) and takes about an hour; nothing else uses the prover meanwhile.

```bash
cd /home/p4u/davinci-sequencer
DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_NEGATIVE=1 \
  DAVINCI_SEQUENCER_BIN=$HOME/.cache/davinci-l1blob/bin/davinci-sequencer-$(git rev-parse --short HEAD) \
  make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e-l1.log
grep -E "L1-blob|node-b (killed|replaced)|restart with a pending post|negative checks|  L1 |blob-mode entry|PASSED|FAILED|test result" ~/.cache/davinci-e2e/e2e-l1.log
```
Expected (excerpt):
```
L1-blob mode: Ethereum anvil http://127.0.0.1:<port> (chain 1, 2 s blocks), OP origin 6 blocks behind
L1-blob mode: L1BlobVerifier 0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0 on L1Block 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512
[e2e ...] node-b killed with 0x... pending at L1 nonce <n>
[e2e ...] node-b replaced 0x... with 0x... at L1 nonce <n>, every fee doubled
[e2e ...] 0x...: restart with a pending post ok; OP origin at Ethereum block <m>, 0 failed relay steps
negative checks on 0x... (transition 0x... at <N>, results 0x... at <R>):
  L1 transition control at N-1       ok                           PASS
  L1 replay at N                     reverted InvalidStateRoot    PASS
  blob-mode entry point              reverted WrongDAMode         PASS
  L1 proof byte flipped at N-1       reverted InvalidProof        PASS
  L1 header chain broken at N-1      reverted BrokenHeaderChain   PASS
  L1 tx proof of another trie at N-1 reverted MerkleTrie: invalid root hash PASS
  L1 commitment swapped at N-1       reverted InvalidBlobsDigest  PASS
  L1 extra blob in the arrays at N-1 reverted BlobCountMismatch   PASS
[e2e ...] PASSED in <~60> min
test e2e ... ok
```
and the gas bill with `L1 blobs 0x...` lines. If it fails: keep the work directory it prints, read the node logs with superpowers:systematic-debugging, fix the harness here or the node in a separate `fix(l1da): ...` commit with a regression test (Global Constraints), rebuild the binary (Step 7) and re-run. A grace-phase failure that is pure timing (a landing after the window) is checked by re-running only that phase with a shorter origin lag, `DAVINCI_E2E_GRACE_ONLY=1 DAVINCI_E2E_L1_LAG=2`, and reported.

- [ ] **Step 9: Gate: blob mode unchanged**

Go/no-go: the prover queue is empty again (the same `curl` as Step 8).

```bash
DAVINCI_E2E_NEGATIVE=1 DAVINCI_SEQUENCER_BIN=$HOME/.cache/davinci-l1blob/bin/davinci-sequencer-$(git rev-parse --short HEAD) \
  make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e-blobs.log
grep -E "registry 0x|negative checks|PASSED|FAILED|test result" ~/.cache/davinci-e2e/e2e-blobs.log
```
Expected: `chain 31337 registry 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512 ...`, the blob-mode negative checks all PASS (`transition control at N-1`, `replay at N`, `extra blob hash at N-1`, ...), `PASSED`, `test e2e ... ok`. Record both run times for the report.

---

### Task 4: Deploy on OP Mainnet and fill in the `optimism` preset

**Files:**
- Create (outside the repos): `~/.config/davinci-l1blob/env.sh`, `~/.config/davinci-l1blob/seq-a.l1.key`, `~/.config/davinci-l1blob/seq-a.op.key`, `~/.cache/davinci-l1blob/probe/probe_beacons.py`, `~/.cache/davinci-l1blob/probe/probe_send.py`
- Create (davinci-contracts, by `deploy_all.sh`): `broadcast/DeployAll.s.sol/10/run-latest.json`, `deployments/10.json`
- Modify (davinci-contracts): `README.md` (Deployments), `docs/deployment.md` (new section after "L1-blob mode")
- Modify (davinci-sequencer): `client/src/networks.rs` (`OPTIMISM` L72-98, tests), `docs/deployments.md` ("OP Mainnet (pending)" section), `docs/configuration.md` (Networks, L23)

**Interfaces:**
- Consumes: davinci-contracts `feat/l1-blob-da` (`script/DeployAll.s.sol` with `ZISK_VERIFIER`/`L1_BLOCK`, `deploy_all.sh`, `script/verify_deployment.py --l1-block`); the `ZiskVerifier` on OP `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c`; release pins of davinci-zkvm `rust-sdk/src/release.rs`.
- Produces: the registry and `L1BlobVerifier` on OP (addresses and block in `deployments/10.json`); `OPTIMISM.registry` and `OPTIMISM.start_block`; the funded keys `seq-a.l1.key` (Ethereum blob poster of node A) and `seq-a.op.key` (node A's OP settlement key); `env.sh` with `redact`, `owner_key`, `key_of` for Tasks 5 and 7.

Spending in this task: Ethereum 0.0015 ETH to `seq-a.l1` plus one transfer's gas; OP 0.0002 ETH to `seq-a.op`, one transfer and the two deployments (simulated at well under 0.0001 ETH at OP's ~0.001 gwei).

- [ ] **Step 1: Operator environment (no secret inside) and preflight**

```bash
mkdir -p ~/.config/davinci-l1blob && chmod 700 ~/.config/davinci-l1blob
cat > ~/.config/davinci-l1blob/env.sh <<'EOF'
# Sourced by the Plan 3 steps. Holds no secret: the keys and the OP RPC are
# read from their files when used, and never echoed.
export OWNER=0x470D1dDc7DdCeb0d818751D171d40366f010a3cc
export L1_READ=https://ethereum-rpc.publicnode.com
export L1_SEND=https://eth.drpc.org
export OP_PUB=https://mainnet.optimism.io
export ETH_BEACON=https://ethereum-beacon-api.publicnode.com
export CAST=$HOME/.foundry/bin/cast FORGE=$HOME/.foundry/bin/forge
export KEYS=$HOME/.config/davinci-l1blob
export OP_RPC="$(head -n1 "$HOME/optimism-rpcs.txt" | tr -d '[:space:]')"
export ZISK=0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c
export L1BLOCK=0x4200000000000000000000000000000000000015
export BATCH_PROGRAM_VK=0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10
export RESULTS_PROGRAM_VK=0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794
export ROOT_C_VADCOP_FINAL=0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80
export BALLOT_VK_HASH=0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e
owner_key() { tr -d '[:space:]' < "$HOME/optimism-privkey.txt" | sed 's/^0x//; s/^/0x/'; }
key_of() { tr -d '[:space:]' < "$1" | sed 's/^0x//; s/^/0x/'; }
lc() { tr '[:upper:]' '[:lower:]'; }
# The private OP RPC never shows in full: the URL and its path are masked.
redact() {
  perl -pe 'BEGIN { $| = 1; $u = $ENV{OP_RPC} // ""; ($p) = $u =~ m{^\w+://[^/]+(/.{6,})$}; }
            if (length $u) { s/\Q$u\E/<op-rpc>/g; s/\Q$p\E/<op-rpc-path>/g if defined $p }'
}
EOF
chmod 600 ~/.config/davinci-l1blob/env.sh
. ~/.config/davinci-l1blob/env.sh
[ "$("$CAST" wallet address --private-key "$(owner_key)" 2>/dev/null | lc)" = "$(echo "$OWNER" | lc)" ] && echo "owner key ok" || echo "owner key MISMATCH"
echo "Ethereum $("$CAST" balance "$OWNER" --ether --rpc-url "$L1_READ") OP $("$CAST" balance "$OWNER" --ether --rpc-url "$OP_PUB")"
"$CAST" chain-id --rpc-url "$OP_RPC" 2>&1 | redact
```
Expected: `owner key ok`; `Ethereum 0.0036... OP 0.0018...`; `10`. No-go on a mismatch or on balances below 0.0030 (Ethereum) or 0.0015 (OP).

- [ ] **Step 2: Read-only probes of the preset's endpoints**

```bash
. ~/.config/davinci-l1blob/env.sh
mkdir -p ~/.cache/davinci-l1blob/probe && cd ~/.cache/davinci-l1blob/probe
cat > probe_beacons.py <<'EOF'
"""Read-only: do these Ethereum beacon APIs serve full blobs
(`/eth/v1/beacon/blobs/{slot}?versioned_hashes=`) for a recent and a day-old
slot? python3 -I probe_beacons.py <eth-rpc> <beacon> [<beacon>...]; prints hosts only."""
import json
import sys
import urllib.error
import urllib.parse
import urllib.request

UA = {"Content-Type": "application/json", "User-Agent": "davinci-probe"}


def host(u):
    return urllib.parse.urlsplit(u).netloc


def rpc(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    out = json.load(urllib.request.urlopen(urllib.request.Request(url, body, UA), timeout=30))
    if "error" in out:
        raise RuntimeError(f"{method}: {out['error'].get('message')}")
    return out["result"]


def get(url):
    req = urllib.request.Request(url, headers={"User-Agent": "davinci-probe", "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, b""
    except Exception as e:  # noqa: BLE001
        return f"error {type(e).__name__}", b""


def blob_block(eth, start):
    """Newest block at or below start with a type-3 transaction."""
    for n in range(start, start - 50, -1):
        b = rpc(eth, "eth_getBlockByNumber", [hex(n), True])
        for t in b["transactions"]:
            if t.get("type") == "0x3" and t.get("blobVersionedHashes"):
                return n, int(b["timestamp"], 16), t["blobVersionedHashes"][0]
    raise RuntimeError(f"no blob tx in the 50 blocks below {start}")


eth, beacons = sys.argv[1], sys.argv[2:]
head = int(rpc(eth, "eth_blockNumber", []), 16)
cases = [("recent", blob_block(eth, head - 3)), ("1 day old", blob_block(eth, head - 7200))]
for b in beacons:
    st, body = get(b + "/eth/v1/beacon/genesis")
    if st != 200:
        print(f"{host(b):45} genesis {st}")
        continue
    genesis = int(json.loads(body)["data"]["genesis_time"])
    for label, (n, ts, vh) in cases:
        slot = (ts - genesis) // 12
        st, body = get(f"{b}/eth/v1/beacon/blobs/{slot}?versioned_hashes={vh}")
        n_blobs = len(json.loads(body).get("data", [])) if st == 200 else 0
        print(f"{host(b):45} {label:9} block {n} slot {slot}: blobs {st}, {n_blobs} blob(s)")
EOF
cat > probe_send.py <<'EOF'
"""Read-only: does each Ethereum RPC take a 1.5 MB JSON-RPC body, the size of
a 6-blob send? An eth_call to 0x..01 with 750,000 bytes of calldata. Prints
hosts and verdicts only: python3 -I probe_send.py <rpc> [<rpc>...]"""
import json
import sys
import urllib.error
import urllib.parse
import urllib.request

body = json.dumps({
    "jsonrpc": "2.0", "id": 1, "method": "eth_call",
    "params": [{"to": "0x0000000000000000000000000000000000000001",
                "data": "0x" + "00" * 750_000}, "latest"],
}).encode()
for u in sys.argv[1:]:
    host = urllib.parse.urlsplit(u).netloc
    req = urllib.request.Request(
        u, body, {"Content-Type": "application/json", "User-Agent": "davinci-probe"})
    try:
        with urllib.request.urlopen(req, timeout=90) as r:
            out = json.load(r)
            verdict = "takes 1.5 MB" if "result" in out else f"error {str(out.get('error'))[:60]}"
            print(f"{host:32} {r.status} {verdict}")
    except urllib.error.HTTPError as e:
        print(f"{host:32} {e.code} refuses 1.5 MB")
    except Exception as e:  # noqa: BLE001
        print(f"{host:32} {type(e).__name__}")
EOF
python3 -I probe_beacons.py "$L1_READ" "$ETH_BEACON" https://lodestar-mainnet.chainsafe.io
python3 -I probe_send.py "$L1_READ" "$L1_SEND"
for u in "$L1_READ" "$L1_SEND"; do for m in eth_blobBaseFee eth_config; do
  printf '%-30s %-16s ' "${u#https://}" "$m"
  curl -s -m 20 -H 'Content-Type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$m\",\"params\":[]}" "$u" | head -c 70; echo
done; done
for u in https://optimism-rpc.publicnode.com "$OP_PUB"; do
  H=$("$CAST" block-number --rpc-url "$u")
  printf '%-34s ' "${u#https://}"
  curl -s -m 30 -H 'Content-Type: application/json' -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getLogs\",\"params\":[{\"address\":\"$L1BLOCK\",\"fromBlock\":\"$(printf 0x%x $((H-4999)))\",\"toBlock\":\"$(printf 0x%x "$H")\"}]}" "$u" | head -c 60; echo
done
echo "origin lag: Ethereum head $("$CAST" block-number --rpc-url "$L1_READ"), OP L1Block $("$CAST" call $L1BLOCK 'number()(uint64)' --rpc-url "$OP_PUB")"
```
Expected (as measured on 2026-10-12 while writing this plan):
```
ethereum-beacon-api.publicnode.com            recent    block ... slot ...: blobs 200, 1 blob(s)
ethereum-beacon-api.publicnode.com            1 day old block ... slot ...: blobs 200, 1 blob(s)
lodestar-mainnet.chainsafe.io                 recent    block ... slot ...: blobs 403, 0 blob(s)
lodestar-mainnet.chainsafe.io                 1 day old block ... slot ...: blobs 403, 0 blob(s)
ethereum-rpc.publicnode.com      503 refuses 1.5 MB
eth.drpc.org                     200 takes 1.5 MB
ethereum-rpc.publicnode.com    eth_blobBaseFee  {"jsonrpc":"2.0","id":1,"result":"0x..."}
ethereum-rpc.publicnode.com    eth_config       {"jsonrpc":"2.0","id":1,"result":{"current":{"activationT
eth.drpc.org                   eth_blobBaseFee  {"id":1,"jsonrpc":"2.0","error":{"message":"method is no
eth.drpc.org                   eth_config       {"id":1,"jsonrpc":"2.0","result":{"current":{"activationT
optimism-rpc.publicnode.com        {"jsonrpc":"2.0","id":1,"result":[]}
mainnet.optimism.io                {"jsonrpc":"2.0","id":1,"result":[]}
origin lag: Ethereum head <h>, OP L1Block <h - 5 to 10>
```
Go: publicnode's beacon serves both slots, dRPC takes 1.5 MB, both OP RPCs answer a 5,000-block `eth_getLogs`. The preset keeps `beacon:https://ethereum-beacon-api.publicnode.com` and the L1 list `publicnode, drpc` (the node's blob send goes to every L1 endpoint at once, so dRPC accepts what publicnode refuses; the harness also names dRPC as the send endpoint). No-go if publicnode's beacon fails either slot or dRPC refuses the body: stop and report; the preset must not ship without a working beacon and send endpoint. Record the outputs for the report.

- [ ] **Step 3: Generate node A's keys (printed: addresses only)**

```bash
. ~/.config/davinci-l1blob/env.sh
newkey() {  # a fresh key in file $1 (0600, never overwrites); prints its address only
  "$CAST" wallet new --json | python3 -I -c '
import json, os, sys
w = json.load(sys.stdin)["data"][0]
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
os.write(fd, (w["private_key"].removeprefix("0x") + "\n").encode())
os.close(fd)
print(w["address"])' "$1"
}
echo "seq-a Ethereum blob key: $(newkey "$KEYS/seq-a.l1.key")"
echo "seq-a OP settlement key:  $(newkey "$KEYS/seq-a.op.key")"
stat -c '%a %n' "$KEYS"/*.key
for f in seq-a.l1 seq-a.op; do echo "$f $("$CAST" wallet address --private-key "$(key_of "$KEYS/$f.key")")"; done
```
Expected: two addresses, `600 .../seq-a.l1.key`, `600 .../seq-a.op.key`, and the same two addresses read back. Keep them in the report as `SEQ_A_L1` and `SEQ_A_OP` (addresses only). One key per chain per node: the Ethereum account sends nothing but this node's blob posts (the blob pool forbids nonce gaps), the OP one nothing but its settlements, and neither is the organizer's (the owner's key).

- [ ] **Step 4: Fund the keys (go/no-go first)**

```bash
. ~/.config/davinci-l1blob/env.sh
SEQ_A_L1=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.l1.key")")
SEQ_A_OP=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.op.key")")
echo "owner Ethereum $("$CAST" balance "$OWNER" --ether --rpc-url "$L1_READ"), base fee $("$CAST" base-fee --rpc-url "$L1_READ") wei"
"$CAST" estimate --from "$OWNER" --value 0.0015ether "$SEQ_A_L1" --rpc-url "$L1_READ"
"$CAST" estimate --from "$OWNER" --value 0.0002ether "$SEQ_A_OP" --rpc-url "$OP_PUB"
```
Go: owner balance at least 0.0030 ETH on Ethereum, base fee at most 2000000000 wei (2 gwei), both estimates `21000`. Then:

```bash
receipt() { python3 -I -c 'import json,sys; r=json.load(sys.stdin); g=int(r["gasUsed"],16); p=int(r["effectiveGasPrice"],16); print(r["transactionHash"], "status", int(r["status"],16), "gas", g, "cost wei", g*p)'; }
"$CAST" send --rpc-url "$L1_READ" --private-key "$(owner_key)" --value 0.0015ether "$SEQ_A_L1" --json | receipt
"$CAST" send --rpc-url "$OP_RPC" --private-key "$(owner_key)" --value 0.0002ether "$SEQ_A_OP" --json 2>&1 | redact | receipt
echo "seq-a Ethereum $("$CAST" balance "$SEQ_A_L1" --ether --rpc-url "$L1_READ") OP $("$CAST" balance "$SEQ_A_OP" --ether --rpc-url "$OP_PUB")"
```
Expected: two lines `0x<hash> status 1 gas 21000 cost wei <n>` (Ethereum about 7.4e12 wei at 0.35 gwei), then `seq-a Ethereum 0.0015 OP 0.0002`. Running total out of the owner: Ethereum 0.0015 ETH plus gas (cap 0.0025), OP 0.0002 ETH plus gas (cap 0.0012).

- [ ] **Step 5: Contracts preflight (read-only)**

```bash
. ~/.config/davinci-l1blob/env.sh
cd /home/p4u/davinci-contracts && git branch --show-current && git status --short | wc -l
"$FORGE" build 2>&1 | tail -1
"$FORGE" test --match-contract "L1BlobVerifier|TransitionL1" 2>&1 | grep -E "Suite result|passed|failed" | tail -3
echo "ZiskVerifier codehash $("$CAST" keccak "$("$CAST" code $ZISK --rpc-url "$OP_PUB")")"
echo "ZiskVerifier rootC    $("$CAST" call $ZISK 'getRootCVadcopFinal()(bytes32)' --rpc-url "$OP_PUB")"
for f in batchProgramVK resultsProgramVK rootCVadcopFinal ballotVKHash; do
  echo "gnosis R2 $f $("$CAST" call 0x20b96e465CA7C3536B9C733571ec1eCf42b2eA21 "$f()(bytes32)" --rpc-url https://rpc.gnosischain.com)"
done
echo "owner OP nonce $("$CAST" nonce "$OWNER" --rpc-url "$OP_PUB"), balance $("$CAST" balance "$OWNER" --ether --rpc-url "$OP_PUB")"
```
Expected: `feat/l1-blob-da`, `0` (clean tree), a successful build, every L1 test passing; codehash `0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566` and rootC `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` (the release pins); the Gnosis R2 pins equal `BATCH_PROGRAM_VK`, `RESULTS_PROGRAM_VK`, `ROOT_C_VADCOP_FINAL`, `BALLOT_VK_HASH` of `env.sh`; the owner's OP balance at least 0.0015. No-go on any difference. (The test L1BlobVerifier `0xa6bE72b2e1AEF3aBD5DA031D4C893C572A01a67e` from before the `record()` fix is not reused: `L1_BLOCK` deploys a fresh one.)

- [ ] **Step 6: Simulate the deployment (no broadcast)**

```bash
. ~/.config/davinci-l1blob/env.sh
cd /home/p4u/davinci-contracts
( export PRIVATE_KEY="$(owner_key)" RPC_URL="$OP_RPC" CHAIN_ID=10 ZISK_VERIFIER=$ZISK L1_BLOCK=$L1BLOCK \
    BATCH_PROGRAM_VK RESULTS_PROGRAM_VK ROOT_C_VADCOP_FINAL BALLOT_VK_HASH
  "$FORGE" script script/DeployAll.s.sol:DeployAllScript --chain 10 --rpc-url "$RPC_URL" \
    --optimize --optimizer-runs 200 ) 2>&1 | redact | grep -E "Deployer|reused|code hash|deployed at|L1 b|Grace|Estimated|SIMULATION|Error|error"
```
Expected:
```
  Deployer address: 0x470D1dDc7DdCeb0d818751D171d40366f010a3cc
  ZiskVerifier reused at: 0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c
  ZiskVerifier code hash: 0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566
  L1BlobVerifier deployed at: 0x<A>
  ProcessRegistry deployed at: 0x<B>
  DavinciDKGAdapter deployed at: 0x0000000000000000000000000000000000000000
  CouncilAdapter deployed at: 0x0000000000000000000000000000000000000000
  L1 blob verifier: 0x<A>
  L1 block: 0x4200000000000000000000000000000000000015
  Grace (s): default 180, floor 150, ceil 600, max total 1800; notice min 60
Estimated total gas used for script: <about 8,000,000>
Estimated amount required: <well under 0.0001> ETH
SIMULATION COMPLETE. To broadcast these transactions, add --broadcast ...
```
Go: those lines, no error, estimated amount at most 0.0001 ETH. The grace window is the production default (DeployAll's), as on Gnosis.

- [ ] **Step 7: Deploy**

```bash
. ~/.config/davinci-l1blob/env.sh
cd /home/p4u/davinci-contracts
( export PRIVATE_KEY="$(owner_key)" RPC_URL="$OP_RPC" CHAIN_ID=10 VERIFY_MODE=false ZISK_VERIFIER=$ZISK L1_BLOCK=$L1BLOCK \
    BATCH_PROGRAM_VK RESULTS_PROGRAM_VK ROOT_C_VADCOP_FINAL BALLOT_VK_HASH
  ./deploy_all.sh ) 2>&1 | redact > ~/.cache/davinci-l1blob/deploy-op.log
grep -E "reused at|deployed at|^OK|^FAIL|l1 blob|record|Hash:|Block:|Paid:|Error|error" ~/.cache/davinci-l1blob/deploy-op.log
```
Expected: the same addresses as Step 6; forge's per-transaction `Hash: 0x...`, `Block: ...`, `Paid: ... ETH` lines for the two creations; every `verify_deployment.py` line `OK` (runtime code of `ProcessRegistry`, `ZiskVerifier` and `L1BlobVerifier` equal to the local build, the four pins, `registry.chainID == rpc chain id`, `l1BlobVerifier.l1Block == --l1-block`); `l1 blob  0x<A> (L1Block 0x4200000000000000000000000000000000000015)`; `adapter  none (DKG disabled)`; `record   deployments/10.json`. `VERIFY_MODE=false`: no explorer key was provided (source verification is an open question). The full redacted log stays in `~/.cache/davinci-l1blob/deploy-op.log`.

- [ ] **Step 8: Post-deployment checks and the secret scan**

```bash
. ~/.config/davinci-l1blob/env.sh
cd /home/p4u/davinci-contracts
REG=$(jq -r .processRegistry deployments/10.json); L1V=$(jq -r .l1BlobVerifier deployments/10.json); BLOCK=$(jq -r .block deployments/10.json)
echo "registry $REG verifier $L1V block $BLOCK"
"$CAST" call "$REG" 'l1BlobVerifier()(address)' --rpc-url "$OP_PUB"
"$CAST" call "$L1V" 'l1Block()(address)' --rpc-url "$OP_PUB"
"$CAST" call "$L1V" 'MAX_AGE()(uint256)' --rpc-url "$OP_PUB"
python3 script/verify_deployment.py --rpc "$OP_PUB" --chain-id 10 --registry "$REG" --verifier "$ZISK" \
  --batch-vk "$BATCH_PROGRAM_VK" --results-vk "$RESULTS_PROGRAM_VK" --root-c "$ROOT_C_VADCOP_FINAL" \
  --ballot-vk-hash "$BALLOT_VK_HASH" --dkg-manager 0x0 --council-manager 0x0 --l1-block "$L1BLOCK" | grep -c '^FAIL'
python3 -I - "$HOME/optimism-privkey.txt" "$HOME/optimism-rpcs.txt" broadcast/DeployAll.s.sol/10/run-latest.json deployments/10.json <<'EOF'
import sys
key = open(sys.argv[1]).read().strip().lower().removeprefix("0x")
rpc = open(sys.argv[2]).readline().strip()
for f in sys.argv[3:]:
    t = open(f).read()
    print(f, "key" if key in t.lower() else "no key", "rpc" if rpc and rpc in t else "no rpc")
EOF
git status --short
```
Expected: the registry's `l1BlobVerifier()` is `$L1V`, `l1Block()` is `0x4200000000000000000000000000000000000015`, `MAX_AGE()` is `7200`; `0` FAIL lines; both files `no key no rpc`; `git status` shows `broadcast/DeployAll.s.sol/10/run-latest.json`, `deployments/10.json` and possibly `golang-types/addresses.go` (`helpers/parse_deployall_runs.sh` skips chain 10; if that file changed, it changed for other chains' records: `git checkout golang-types/addresses.go`). Spend so far: compare the owner's OP balance with Step 5's and add it to the report.

- [ ] **Step 9: davinci-contracts records and docs, commit**

In davinci-contracts `README.md`, insert under `## Deployments`, before `### Gnosis Chain production beta` (fill the bracketed values from `deployments/10.json` and Step 7's log; dates in UTC):

~~~markdown
### OP Mainnet, L1-blob mode

OP Mainnet (chain id 10), deployed <date> from commit `<commit of deployments/10.json>` at block
<block>. The registry settles transitions on OP while their blobs live in Ethereum blob
transactions, checked by its `L1BlobVerifier` against OP's `L1Block` predeploy
([settlement.md](docs/settlement.md#l1-blob-mode)). [`deployments/10.json`](deployments/10.json)
holds the verified record. Not source-verified on an explorer yet.

| Contract | Address |
|---|---|
| `ProcessRegistry` | `<registry>` |
| `L1BlobVerifier` | `<verifier>` (reads `L1Block` `0x4200000000000000000000000000000000000015`) |
| `ZiskVerifier` | `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (deployed earlier; same code hash as Gnosis') |

Same pins as the Gnosis registry below; production grace defaults; no DKG or Council adapter.
~~~

In `docs/deployment.md`, add after the "L1-blob mode" section:

~~~markdown
## OP Mainnet

Deployed on <date> with `L1_BLOCK` (a fresh `L1BlobVerifier` on the OP predeploy), the
`ZiskVerifier` deployed earlier and the release pins, from the owner's account, with no `.env`
(variables set in a subshell; the RPC and the key never written to disk):

```bash
CHAIN_ID=10 VERIFY_MODE=false \
ZISK_VERIFIER=0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c \
L1_BLOCK=0x4200000000000000000000000000000000000015 \
BATCH_PROGRAM_VK=0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10 \
RESULTS_PROGRAM_VK=0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794 \
ROOT_C_VADCOP_FINAL=0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \
BALLOT_VK_HASH=0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e \
RPC_URL=... PRIVATE_KEY=... ./deploy_all.sh
```

The script ran `forge script` once without `--broadcast` first. Addresses are in the
[deployments table](../README.md#deployments).
~~~

```bash
cd /home/p4u/davinci-contracts
git add broadcast/DeployAll.s.sol/10/run-latest.json deployments/10.json README.md docs/deployment.md
git commit -m "chore(deploy): op mainnet registry in l1-blob mode"
```

- [ ] **Step 10: Write the failing preset test**

In `client/src/networks.rs`, append to `mod tests`:

```rust
    #[test]
    fn optimism_preset_is_deployed() {
        assert_ne!(OPTIMISM.registry, Address::ZERO, "optimism registry unset");
        assert_ne!(OPTIMISM.start_block, 0, "optimism start block unset");
    }
```

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-client --lib optimism_preset_is_deployed 2>&1 | grep -E "panicked|test result"`
Expected: FAIL with `optimism registry unset`.

- [ ] **Step 11: Fill in the preset**

```bash
. ~/.config/davinci-l1blob/env.sh
cd /home/p4u/davinci-sequencer
REG=$("$CAST" to-check-sum-address "$(jq -r .processRegistry /home/p4u/davinci-contracts/deployments/10.json)")
BLOCK=$(jq -r .block /home/p4u/davinci-contracts/deployments/10.json)
python3 -I - client/src/networks.rs "$REG" "$BLOCK" "$(date -u +%F)" <<'EOF'
import sys
path, reg, block, day = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
s = open(path).read()
old_doc = """/// OP Mainnet in L1-blob mode: transitions settle on OP, their blobs live on
/// Ethereum. Not deployed yet: the zero registry makes the node refuse this
/// preset (`--registry` overrides it) until a release fills in the registry
/// and its start block. The endpoint lists are provisional until then.
"""
new_doc = f"""/// OP Mainnet in L1-blob mode: transitions settle on OP, their blobs live on
/// Ethereum (deployed {day}, see docs/deployments.md). Blob sends need an
/// Ethereum RPC that takes 1.5 MB bodies: publicnode refuses them, dRPC takes
/// them, and the node sends to every L1 endpoint at once.
"""
assert old_doc in s, "OPTIMISM doc comment"
s = s.replace(old_doc, new_doc)
start = s.index("pub const OPTIMISM: Network = Network {")
end = s.index("};", start)
body = s[start:end]
assert "registry: Address::ZERO," in body and "start_block: 0," in body
body = body.replace("registry: Address::ZERO,", f'registry: address!("{reg[2:]}"),')
body = body.replace("start_block: 0,", f"start_block: {block:_},")
open(path, "w").write(s[:start] + body + s[end:])
print("OPTIMISM registry", reg, "start block", block)
EOF
cargo test -p davinci-client --lib networks 2>&1 | grep -E "^test |test result"
cargo test -p davinci-sequencer --lib config 2>&1 | grep -E "optimism|test result"
```
Expected: `OPTIMISM registry 0x<B> start block <block>`; `optimism_preset_is_deployed ... ok` and all networks tests pass; `optimism_preset_is_l1_mode_and_needs_its_registry ... ok`, config `test result: ok`.

- [ ] **Step 12: Docs**

In `docs/deployments.md`, replace the whole `## OP Mainnet (pending)` section (down to, not including, `## Gnosis Chain`) by running:

```bash
. ~/.config/davinci-l1blob/env.sh
cd /home/p4u/davinci-sequencer
DEP=/home/p4u/davinci-contracts/deployments/10.json
REG=$("$CAST" to-check-sum-address "$(jq -r .processRegistry $DEP)")
L1V=$("$CAST" to-check-sum-address "$(jq -r .l1BlobVerifier $DEP)")
python3 -I - docs/deployments.md "$DEP" "$(date -u +%F)" "$REG" "$L1V" <<'EOF'
import json, sys
path, rec, day = sys.argv[1], json.load(open(sys.argv[2])), sys.argv[3]
rec["processRegistry"], rec["l1BlobVerifier"] = sys.argv[4], sys.argv[5]
s = open(path).read()
a, b = s.index("## OP Mainnet (pending)"), s.index("## Gnosis Chain")
section = f"""## OP Mainnet

Chain id 10, preset `optimism`, L1-blob mode: transitions settle on OP Mainnet and their blobs
live in Ethereum blob transactions. Deployed on {day} from davinci-contracts commit
`{rec["commit"]}`; davinci-contracts' `deployments/10.json` holds the verified record. Not
source-verified on an explorer yet.

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (reused; code hash the pinned `0x82385a40…5566`) | |
| L1BlobVerifier | `{rec["l1BlobVerifier"]}` | {rec["block"]} |
| ProcessRegistry | `{rec["processRegistry"]}` | {rec["block"]} |

Pins are the release values listed under Gnosis Chain below. No DKG or Council adapter: those key
modes are not on OP Stack chains yet. The grace window is the production default (180 s, floor
150 s, ceiling 600 s, at most 1800 s past the end; 60 s shorten notice).

`L1BlobVerifier` reads OP's L1 origin from the `L1Block` predeploy
`0x4200000000000000000000000000000000000015`. Consumers of OP transitions (SDKs, explorers) read
`ProcessStateTransitionedL1` for the Ethereum blocks and versioned hashes; there is no `BLOBHASH`
on OP. The preset reads Ethereum from `https://ethereum-rpc.publicnode.com` and
`https://eth.drpc.org`: publicnode refuses request bodies above about 1 MB, dRPC takes the 1.5 MB
six-blob sends, and the node sends a blob transaction to every endpoint. Followers fetch blobs
from `https://ethereum-beacon-api.publicnode.com`, which served full blobs for recent and day-old
slots when the preset was filled in; production nodes should add a beacon node of their own that
keeps full blobs.

"""
open(path, "w").write(s[:a] + section + s[b:])
print("docs/deployments.md: OP Mainnet section written")
EOF
```

In `docs/configuration.md`, replace the sentence "The `optimism` preset is in L1-blob mode and is not deployed yet: its registry is the zero address, so `--network optimism` refuses to start without `--registry` until a release fills it in." with "The `optimism` preset is in L1-blob mode: its transitions settle on OP Mainnet and its blobs go to Ethereum, see [L1-blob mode](#l1-blob-mode) and [deployments.md](deployments.md#op-mainnet)."

- [ ] **Step 13: Smoke-test the preset with an observer (read-only, no key)**

```bash
cd /home/p4u/davinci-sequencer && cargo build --release -p davinci-sequencer --bin davinci-sequencer 2>&1 | tail -1
D=$(mktemp -d -p ~/.cache davinci-op-smoke-XXXX)
( env -i PATH="$PATH" HOME="$D" DAVINCI_NETWORK=optimism DAVINCI_DATADIR="$D/data" DAVINCI_API_HOST=127.0.0.1 \
    DAVINCI_API_PORT=19190 DAVINCI_PROVER_URL=http://127.0.0.1:1 DAVINCI_CENSUS_DIR="$D" DAVINCI_LOG_LEVEL=info \
    timeout 60 ./target/release/davinci-sequencer ) > "$D/node.log" 2>&1
grep -E "L1-blob mode|blob cap|observer|ERROR|refus" "$D/node.log" | head; rm -rf "$D"
```
Expected: `L1-blob mode: blobs go to Ethereum` with `l1_chain_id=1` and the `L1Block` predeploy, `blob cap per transaction` with `cap=6`, an observer-mode line, no `ERROR`. (The prover URL is unreachable on purpose: observers never prove.)

- [ ] **Step 14: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
git add client/src/networks.rs docs/deployments.md docs/configuration.md
git commit -m "feat(net): fill in the op mainnet registry"
```

---

### Task 5: The live harness and the live run on OP Mainnet

**Files:**
- Modify: `e2e/src/l1.rs` (imports, constants, `live_net`, `live_keys`, tests), `e2e/src/net.rs` (module doc, imports, `MIN_BALANCE_L1`, `preset`, `l1_mode`, `node_rpcs_of`, `live_target`, `Net::live`), `e2e/tests/e2e.rs` (module list, the `op_live` test), `docs/testing.md` ("Live runs")
- Create: `e2e/tests/e2e/op_live.rs`

**Interfaces:**
- Consumes: Task 1 `L1Net`, `Net.{l1, min_balance}`; Task 3 helpers in `e2e/tests/e2e.rs` (`create_processes`, `Elections`, `wait_listed`, `wait_settled`, `wait_roots`, `check_local_trees`, `all_metrics`, `show_metrics`, `wait_results`, `costs`, `cancel_open`, `submit`, `sent`, `alive`, `flag`) and `grace::closed_after_grace`; Task 2 `negative::run`; Task 4 `OPTIMISM` (filled), the keys and `env.sh`; the node's `blobs posted on L1` log line (`nonce=N`).
- Produces: `davinci_e2e::l1::{live_net(preset: &Network, organizer: Address) -> Result<L1Net>, live_keys(list: &str, organizer: Address) -> Result<Vec<(PathBuf, Address)>>, MIN_L1_BALANCE = "0.0002", LIVE_MAX_POST_FEE = "0.00015eth"}`; `davinci_e2e::net::{preset() -> Result<&'static Network>, l1_mode() -> Result<bool>, MIN_BALANCE_L1}`; env `DAVINCI_E2E_NETWORK`, `DAVINCI_E2E_MIN_BALANCE`, `DAVINCI_E2E_L1_{RPC, SEND_RPC, KEYS, MAX_POST_FEE, MIN_BALANCE}`, `DAVINCI_E2E_OP_LIVE`; the `op_live` test.

- [ ] **Step 1: Write the failing test for the live keys**

Append to `mod tests` in `e2e/src/l1.rs`:

```rust
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
```

and append `assert!(parse_ether(MIN_L1_BALANCE).unwrap() > U256::ZERO);` to `the_mode_is_named`.

Run: `cargo test -p davinci-e2e --lib l1 2>&1 | tail -3`
Expected: FAIL to compile (`live_keys`, `MIN_L1_BALANCE` not found).

- [ ] **Step 2: Live Ethereum settings in `e2e/src/l1.rs`**

Add the import `use davinci_client::networks::{Network, NetworkDa};` (before `use davinci_client::organizer::ProcessRegistry;`), the constants after `L1_KEYS`:

```rust
/// Least balance of a live sequencer's Ethereum key, unless
/// `DAVINCI_E2E_L1_MIN_BALANCE` says otherwise: about ten 6-blob posts at
/// 0.35 gwei.
pub const MIN_L1_BALANCE: &str = "0.0002";
/// The nodes' `DAVINCI_L1_MAX_POST_FEE` live, unless
/// `DAVINCI_E2E_L1_MAX_POST_FEE` says otherwise: a 6-blob post at twice
/// 0.35 gwei and twice 0.015 gwei per blob gas is about 0.00004 ETH, so this
/// leaves room for two doubled replacements.
pub const LIVE_MAX_POST_FEE: &str = "0.00015eth";
```

and, after `anvil_net`:

```rust
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
```

- [ ] **Step 3: Presets and L1 mode in `e2e/src/net.rs`**

Replace the module doc lines

```rust
//! Unset, they default to the Gnosis deployment of `davinci_client::networks`.
```

with

```rust
//! Unset, they default to the deployment of `davinci_client::networks` named
//! by `DAVINCI_E2E_NETWORK` (default `gnosis`). `DAVINCI_E2E_MIN_BALANCE`
//! (native token, decimal) overrides the least balance of each funded account.
```

and the L1 paragraph added in Task 1 with

```rust
//! `DAVINCI_E2E_DA=l1-blobs` (implied live by an L1-blob preset such as
//! `optimism`) runs the registry in L1-blob mode, see [`crate::l1`]: on anvil
//! a second, Ethereum-like anvil and an origin relayer; live, Ethereum from
//! `DAVINCI_E2E_L1_*`. Live, `DAVINCI_E2E_BEACON` then names Ethereum beacons.
```

Imports: `use alloy::primitives::{Address, U256, utils::format_ether, utils::parse_ether};` and `use davinci_client::networks::{self, GNOSIS, Network, NetworkDa, OP_L1_BLOCK};`. After `MIN_BALANCE`:

```rust
/// The same on an L1-blob preset, where OP gas is cheap: 0.00005 ETH.
pub const MIN_BALANCE_L1: u128 = 50_000_000_000_000;
```

Replace `GNOSIS_RPC` and `gnosis_node_rpcs` with:

```rust
/// Organizer and harness RPC on Gnosis: the network's last (publicnode, its
/// first, rate-limits organizer bursts).
#[cfg(test)]
const GNOSIS_RPC: &str = GNOSIS.rpc_urls[GNOSIS.rpc_urls.len() - 1];

/// Live node RPC lists (seq1, seq2, seq3, observer): the network's RPCs
/// rotated, so each node starts on another endpoint and fails over to the next.
fn node_rpcs_of(n: &Network) -> String {
    let u = n.rpc_urls;
    (0..4)
        .map(|i| format!("{},{}", u[i % u.len()], u[(i + 1) % u.len()]))
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
fn gnosis_node_rpcs() -> String {
    node_rpcs_of(&GNOSIS)
}

/// The live deployment's preset: `DAVINCI_E2E_NETWORK` (default `gnosis`).
pub fn preset() -> Result<&'static Network> {
    let name = env_or("DAVINCI_E2E_NETWORK", GNOSIS.name);
    networks::by_name(&name).with_context(|| format!("DAVINCI_E2E_NETWORK={name}: no such preset"))
}

/// Whether the run is in L1-blob mode: `DAVINCI_E2E_DA=l1-blobs`, or live on
/// an L1-blob preset.
pub fn l1_mode() -> Result<bool> {
    let preset_l1 = is_live() && matches!(preset()?.da, NetworkDa::L1Blobs { .. });
    Ok(l1::enabled()? || preset_l1)
}
```

Replace `live_target` with:

```rust
/// RPC, registry and first log block of the live chain, from the env.
pub fn live_target() -> Result<(String, Address, u64)> {
    let n = preset()?;
    let registry: Address = env_or("DAVINCI_E2E_REGISTRY", &n.registry.to_string())
        .parse()
        .context("DAVINCI_E2E_REGISTRY")?;
    ensure!(
        registry != Address::ZERO,
        "network {} has no registry yet: set DAVINCI_E2E_REGISTRY",
        n.name
    );
    let from_block = env_or("DAVINCI_E2E_FROM_BLOCK", &n.start_block.to_string())
        .parse()
        .context("DAVINCI_E2E_FROM_BLOCK")?;
    // The organizer's RPC: the network's last (publicnode, its first,
    // rate-limits organizer bursts).
    let rpc = n.rpc_urls[n.rpc_urls.len() - 1];
    Ok((env_or("DAVINCI_E2E_RPC", rpc), registry, from_block))
}
```

In `Net::live`: insert `let n = preset()?;` as its first line; replace `&gnosis_node_rpcs()` with `&node_rpcs_of(n)`; after `let chain_id = p.get_chain_id().await.context("RPC chain id")?;` insert

```rust
        ensure!(
            chain_id == n.chain_id || std::env::var("DAVINCI_E2E_REGISTRY").is_ok(),
            "the RPC is on chain {chain_id}, network {} is {}",
            n.name,
            n.chain_id
        );
```

after the `from_block <= head` check insert

```rust
        let l1 = if l1_mode()? {
            Some(l1::live_net(n, organizer.address())?)
        } else {
            None
        };
        let default_min = if l1.is_some() {
            MIN_BALANCE_L1
        } else {
            MIN_BALANCE
        };
        let min_balance = match std::env::var("DAVINCI_E2E_MIN_BALANCE") {
            Ok(v) if !v.is_empty() => parse_ether(&v).context("DAVINCI_E2E_MIN_BALANCE")?,
            _ => U256::from(default_min),
        };
```

and in the returned struct: `beacon: Some(node_lists(&env_or("DAVINCI_E2E_BEACON", &n.beacon_urls().join(","))).join(",")),`, `confirmations: env_or("DAVINCI_E2E_CONFIRMATIONS", &n.confirmations.to_string())` (Gnosis' preset value is the old default, 3), and `l1, min_balance,` instead of `l1: None, min_balance: U256::from(MIN_BALANCE),`.

- [ ] **Step 4: The live scenario**

Create `e2e/tests/e2e/op_live.rs`:

```rust
//! The live run on an L1-blob deployment (OP Mainnet), gated by
//! `DAVINCI_E2E_OP_LIVE=1` with `DAVINCI_E2E_LIVE=1`: one signing node and an
//! observer that only follows, a Merkle process (eight voters, three
//! revotes) and a CSP process (three voters), batches of four. Node A is
//! killed once its first blob transaction is in Ethereum's pool and
//! restarted. The observer rebuilds every transition from Ethereum beacon
//! blobs. Both processes end with their results; with
//! `DAVINCI_E2E_NEGATIVE=1` the L1 negatives run through `eth_call`. About
//! six blob posts, a few cents on Ethereum.

use super::*;
use alloy::providers::Provider;

/// Process 1 voters of the live run (round 1), all through node A.
const LIVE_VOTERS: std::ops::Range<usize> = 0..8;
/// Votes per batch: two round-1 batches, one CSP and one revote batch.
const LIVE_BATCH: usize = 4;
/// The node's batch timer: a partial batch (CSP, revotes) seals after it.
const LIVE_BATCH_TIME: &str = "20s";

pub(super) async fn run(dir: &Path) -> Result<()> {
    let prover_url =
        std::env::var("DAVINCI_ZKVM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let health = ProverClient::new(&prover_url)
        .health()
        .await
        .with_context(|| format!("prover health at {prover_url}"))?;
    ensure!(health.status == "ok", "prover health: {health:?}");
    say!("prover ok, queue_len {}", health.queue_len);
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir).await?;
    let l1 = net
        .l1
        .as_ref()
        .filter(|_| net.live)
        .context("DAVINCI_E2E_OP_LIVE needs DAVINCI_E2E_LIVE=1 on an L1-blob deployment")?;
    net.check_balances(1).await?;
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let census_dir = dir.join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;
    let cfg = |name: &str, seq: Option<usize>, rpc: usize| -> Result<NodeConfig> {
        let mut c = net.node_config(name, seq, rpc, &prover_url, &census_dir, dir)?;
        c.batch_max = LIVE_BATCH;
        c.batch_time = LIVE_BATCH_TIME.into();
        Ok(c)
    };
    let a_cfg = cfg("node-a", Some(0), 0)?;
    let mut nodes = vec![
        Node::start(&bin, &a_cfg).await?,
        Node::start(&bin, &cfg("node-obs", None, 1)?).await?,
    ];
    for n in &nodes {
        let info = n
            .api
            .info()
            .await
            .with_context(|| format!("{} /info", n.name))?;
        ensure!(
            info.chain_id == net.chain_id && info.process_registry == net.registry.0.0,
            "{}: chain {} or registry",
            n.name,
            info.chain_id
        );
        ensure!(
            info.observer == n.address.is_none(),
            "{}: observer flag",
            n.name
        );
        // Ethereum's blob limit, capped at six per transaction.
        if let Some(c) = n.logged_blob_cap() {
            ensure!(c == 6, "{}: blob cap {c}, want 6", n.name);
        }
        say!("{} at {} log {}", n.name, n.url, n.log.display());
    }

    let org = net.organizer()?;
    let reader = RegistryReader::connect(&net.rpc, net.registry)?;
    let scan_from = net.start_block.unwrap_or(net.from_block);
    let r: Result<()> = async {
        let Elections {
            pid1,
            pid2,
            p1,
            p2,
            tree,
            csp,
        } = create_processes(
            &org,
            &reader,
            &census_dir,
            async |pid| Ok(nodes[A].api.new_key(&pid).await?),
            async |pid| Ok(nodes[A].api.new_key(&pid).await?),
        )
        .await?;
        wait_listed(&nodes, &reader, &pid1).await?;
        wait_listed(&nodes, &reader, &pid2).await?;
        let prover = ballot_prover.await??;
        let b = fx::build_ballots(&prover, &p1, &tree, &p2, &csp)?;

        // Round 1 and the CSP votes, all to node A.
        let read = l1.provider()?;
        let (_, l1_addr) = l1.keys[0];
        let nonce = read.get_transaction_count(l1_addr).await?;
        let mut round1 = Vec::new();
        for v in LIVE_VOTERS {
            let label = format!("p1 voter {v} round 1");
            submit(&nodes[A], &b.round1[v], &label).await?;
            round1.push(sent(label, A, &b.round1[v]));
        }
        for (i, req) in b.csp.iter().enumerate() {
            let label = format!("p2 voter {i}");
            submit(&nodes[A], req, &label).await?;
            round1.push(sent(label, A, req));
        }
        say!("round 1 submitted: {} votes", round1.len());

        // Restart mid-flight: once node A's first post is in the pool.
        wait::until(
            "node-a's first blob post in Ethereum's pool",
            settle_timeout(),
            Duration::from_millis(500),
            || async {
                alive(&nodes)?;
                let pending = read.get_transaction_count(l1_addr).pending().await?;
                Ok((pending > nonce).then_some(()))
            },
        )
        .await?;
        nodes[A].kill()?;
        say!("node-a killed with an Ethereum blob post sent (nonce {nonce})");
        nodes[A] = Node::start(&bin, &a_cfg).await.context("restart node-a")?;
        wait_settled(&nodes, &round1, "round 1").await?;
        let posted = posted_nonces(&nodes[A]);
        let picked_up = !nodes[A]
            .log_lines(&["L1 post of an earlier run on record"])
            .is_empty();
        say!(
            "after the restart: post record picked up {picked_up}; mined posts at nonces {posted:?} \
             ({})",
            if posted.contains(&nonce) {
                "the pending post was replaced at its nonce, or mined"
            } else {
                "the orphan was mined before the restart's first post"
            }
        );

        // Round 2: the revotes of voters 0..8.
        let mut round2 = Vec::new();
        for (j, v) in fx::REVOTERS.into_iter().enumerate() {
            if LIVE_VOTERS.contains(&v) {
                let label = format!("p1 voter {v} round 2");
                submit(&nodes[A], &b.round2[j], &label).await?;
                round2.push(sent(label, A, &b.round2[j]));
            }
        }
        wait_settled(&nodes, &round2, "round 2").await?;

        // The observer rebuilt every transition from Ethereum beacon blobs.
        let root1 = wait_roots(&nodes, &reader, &pid1).await?;
        let root2 = wait_roots(&nodes, &reader, &pid2).await?;
        check_local_trees(&nodes, &pid1, b.round1[0].vote_id, root1).await?;
        check_local_trees(&nodes, &pid2, b.csp[0].vote_id, root2).await?;
        let events = chain::transitions(&net.rpc, net.registry, scan_from).await?;
        let ours = events
            .iter()
            .filter(|t| t.pid == pid1 || t.pid == pid2)
            .count() as u64;
        let ms = all_metrics(&nodes).await?;
        say!("transitions {ours}; {}", show_metrics(&nodes, &ms));
        ensure!(
            ms[1].settled_by_self == 0 && ms[1].synced_from_others == ours,
            "observer: {}",
            show_metrics(&nodes[1..], &ms[1..])
        );

        // Results of both processes, keyed by node A.
        let last1: Vec<_> = LIVE_VOTERS
            .map(|v| fx::choices(v, if fx::REVOTERS.contains(&v) { 2 } else { 1 }))
            .collect();
        let last2: Vec<_> = (0..fx::N_CSP).map(|i| fx::choices(i, 1)).collect();
        for (pid, want) in [
            (pid1, fx::expected_tally(&last1)),
            (pid2, fx::expected_tally(&last2)),
        ] {
            org.end_process(&pid).await.context("end process")?;
            wait_results(&nodes, &org, &pid, &want).await?;
            grace::closed_after_grace(&net, &reader, &pid).await?;
        }
        if flag("DAVINCI_E2E_NEGATIVE") {
            negative::run(&net.rpc, net.registry, scan_from, Some(pid1))
                .await
                .context("negative checks")?;
        }
        Ok(())
    }
    .await;
    let r = cancel_open(&org, r).await;
    let bill = costs(&net, &org, Vec::new()).await;
    say!("{}", bill.unwrap_or_else(|e| format!("gas bill: {e:#}")));
    drop(nodes);
    r
}

/// The nonces of the node's mined blob posts, from its log
/// (`blobs posted on L1` with `nonce=N`).
fn posted_nonces(n: &Node) -> Vec<u64> {
    n.log_lines(&["blobs posted on L1"])
        .iter()
        .filter_map(|l| {
            let v = l.split("nonce=").nth(1)?;
            v.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .collect()
}
```

(The pending count includes mined transactions, so the wait also ends if the post is mined before the poll sees it; that case is reported, not failed. One signing node keeps the live run free of lost races and their wasted posts; races are covered on anvil.)

In `e2e/tests/e2e.rs`, after the `l1_restart` module lines:

```rust
#[path = "e2e/op_live.rs"]
mod op_live;
```

and before the doc comment of `negative_checks`:

```rust
/// The compact live run on an L1-blob deployment (`e2e/op_live.rs`), gated
/// by `DAVINCI_E2E_OP_LIVE=1`; it needs `DAVINCI_E2E_LIVE=1` and the
/// `DAVINCI_E2E_L1_*` keys and endpoints.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn op_live() -> Result<()> {
    if !flag("DAVINCI_E2E_OP_LIVE") {
        eprintln!("DAVINCI_E2E_OP_LIVE not set, skipping");
        return Ok(());
    }
    t0();
    let mut art = WorkDir::new("davinci-op-live-")?;
    let dir = art.path().to_path_buf();
    say!("working directory {}", dir.display());
    let r = op_live::run(&dir).await;
    match &r {
        Ok(()) => say!("PASSED in {:.1} min", t0().elapsed().as_secs_f64() / 60.0),
        Err(e) => say!("FAILED: {e:#}"),
    }
    art.ok = r.is_ok();
    r
}

```

In `docs/testing.md`, append to "Live runs":

~~~markdown
The full scenario does not run live in L1-blob mode. With `DAVINCI_E2E_NETWORK=optimism`
(an L1-blob preset), `DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_LIVE=1` runs a compact one instead
(`e2e/tests/e2e/op_live.rs`): one signing node and an observer, a Merkle process with eight
voters and three revotes and a CSP process with three, batches of four, the signing node killed
once its first blob transaction is in Ethereum's pool and restarted, both results and, with
`DAVINCI_E2E_NEGATIVE=1`, the L1 negatives. Besides the organizer and sequencer key files it
needs `DAVINCI_E2E_L1_KEYS` (one dedicated Ethereum key file per sequencer, never the
organizer's account) and reads `DAVINCI_E2E_L1_RPC` (Ethereum RPCs, default the preset's),
`DAVINCI_E2E_L1_SEND_RPC` (endpoints that take 1.5 MB bodies), `DAVINCI_E2E_L1_MAX_POST_FEE`
(default `0.00015eth`), `DAVINCI_E2E_L1_MIN_BALANCE` (default 0.0002 ETH) and
`DAVINCI_E2E_MIN_BALANCE` (OP; default 0.00005 ETH on an L1-blob preset). `DAVINCI_E2E_BEACON`
then names Ethereum beacon APIs that keep full blobs. A run posts about six blob transactions.
~~~

- [ ] **Step 5: Build, lint, unit tests; commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test -p davinci-e2e 2>&1 | grep -E "^test result" | sort | uniq -c
DAVINCI_E2E_SETUP=1 TMPDIR=$HOME/.cache/davinci-e2e cargo test -p davinci-e2e --test e2e setup_without_nodes 2>&1 | grep "test result"
git add e2e/src/l1.rs e2e/src/net.rs e2e/tests/e2e.rs e2e/tests/e2e/op_live.rs docs/testing.md
git commit -m "test(e2e): compact live run on op mainnet"
```
Expected: no warnings; every line `test result: ok` (`live_keys_are_dedicated` among them, `op_live` skipped); blob-mode setup `1 passed`.

- [ ] **Step 6: Live environment (no secret in it)**

Every later live command starts with:

```bash
. ~/.config/davinci-l1blob/env.sh
cd /home/p4u/davinci-sequencer
export DAVINCI_E2E_LIVE=1 DAVINCI_E2E_NETWORK=optimism DAVINCI_E2E_POLL=2s \
  DAVINCI_E2E_RPC="$OP_RPC" \
  DAVINCI_E2E_NODE_RPCS="$OP_RPC,https://optimism-rpc.publicnode.com;$OP_RPC,https://mainnet.optimism.io" \
  DAVINCI_E2E_ORGANIZER_KEY=$HOME/optimism-privkey.txt \
  DAVINCI_E2E_SEQUENCER_KEYS=$KEYS/seq-a.op.key DAVINCI_E2E_L1_KEYS=$KEYS/seq-a.l1.key \
  DAVINCI_E2E_L1_SEND_RPC=$L1_SEND DAVINCI_E2E_BEACON=$ETH_BEACON \
  DAVINCI_SEQUENCER_BIN=$HOME/.cache/davinci-l1blob/bin/davinci-sequencer-$(git rev-parse --short HEAD) \
  TMPDIR=$HOME/.cache/davinci-e2e
cargo build --release -p davinci-sequencer --bin davinci-sequencer 2>&1 | tail -1 && cp target/release/davinci-sequencer "$DAVINCI_SEQUENCER_BIN"
```

(Key *files* only; the OP RPC reaches the harness and the nodes through the environment and is redacted from every output below.)

- [ ] **Step 7: Zero-cost dry check: balances are checked before anything is spent**

```bash
DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_L1_MIN_BALANCE=1000 cargo test -p davinci-e2e --test e2e op_live -- --nocapture 2>&1 | redact | tee ~/.cache/davinci-e2e/op-live-dry.log | grep -E "pins ok|L1-blob mode|balance|below|FAILED|test result"
grep -cF "$(key_of $KEYS/seq-a.l1.key | sed 's/^0x//')" ~/.cache/davinci-e2e/op-live-dry.log; grep -cF "$OP_RPC" ~/.cache/davinci-e2e/op-live-dry.log
```
Expected: `chain 10 registry 0x<B> verifier 0x6781... (pins ok)`, `L1-blob mode: L1BlobVerifier 0x<A> on L1Block 0x4200000000000000000000000000000000000015`, the organizer's and `sequencer 1`'s OP balances, `sequencer 1 L1 0x<SEQ_A_L1> balance 0.0015`, then `FAILED: below 1000 ETH on Ethereum: sequencer 1 0x<SEQ_A_L1>` and `test result: FAILED`; both counts `0`. No transaction was sent (the owner's nonces on both chains are unchanged: check with `"$CAST" nonce "$OWNER" --rpc-url "$OP_PUB"` before and after).

- [ ] **Step 8: The live run**

Go/no-go: prover idle (`curl -s "${DAVINCI_ZKVM_URL:-http://127.0.0.1:8080}/health"` shows `"queue_len":0`); Ethereum base fee at most 1 gwei and blob base fee at most 0.05 gwei (`"$CAST" base-fee --rpc-url "$L1_READ"`, `"$CAST" rpc eth_blobBaseFee --rpc-url "$L1_READ"`); `seq-a` balances at least 0.0005 ETH on Ethereum and 0.00005 on OP. Then:

```bash
DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_NEGATIVE=1 \
  systemd-run --user --scope -p MemoryMax=16G -- cargo test -p davinci-e2e --test e2e op_live -- --nocapture 2>&1 \
  | redact | tee ~/.cache/davinci-e2e/op-live.log \
  | grep -E "prover ok|pins ok|L1-blob|node-a (at|killed)|round [12]|after the restart|transitions|local tree|results on-chain|closed at|  [A-Za-z1]|gas bill|L1 blobs|PASSED|FAILED|test result"
```
Expected (about 30 to 45 minutes; OP's origin lag puts each landing about 90 to 110 s after its post):
```
[e2e ...] prover ok, queue_len 0
chain 10 registry 0x<B> verifier 0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c (pins ok)
L1-blob mode: L1BlobVerifier 0x<A> on L1Block 0x4200000000000000000000000000000000000015
[e2e ...] node-a at http://127.0.0.1:<port> log ...
[e2e ...] round 1 submitted: 11 votes
[e2e ...] node-a killed with an Ethereum blob post sent (nonce <n>)
[e2e ...] round 1: all 11 votes settled in <s> s
[e2e ...] after the restart: post record picked up true; mined posts at nonces [...] (...)
[e2e ...] round 2: all 3 votes settled in <s> s
[e2e ...] node-obs: local tree of 0x... at the chain root (and localStateRoot)
[e2e ...] transitions <4-6>; node-a: settled_by_self=<k> ...; node-obs: settled_by_self=0 synced_from_others=<k> ...
[e2e ...] 0x...: results on-chain [...] = expected tally
negative checks on 0x... : the L1 checks PASS or INCONCLUSIVE (an OP RPC without old state)
[e2e ...] gas bill, chain 10 and Ethereum: <n> txs, <total> total
  L1 blobs 0x...  block <L1 block> gas 21000 @ <wei> wei, blobs 1 @ <wei> wei, <ETH> 0x...
[e2e ...] PASSED in <m> min
test op_live ... ok
```
Go/no-go inside the run (Review Focus 1): the node's first post must land: `grep -c "blobs posted on L1" <node-a log path printed above>` is at least 1 within 5 minutes of `round 1 submitted`; if instead the log shows send refusals, stop the run (Ctrl-C), check `DAVINCI_E2E_L1_SEND_RPC` with `probe_send.py`, and report. A FAILED run keeps its work directory; cancel leftover processes with `DAVINCI_E2E_CANCEL=<pids> cargo test -p davinci-e2e --test e2e cancel_leftovers` (same environment).

- [ ] **Step 9: After the run: secrets and spend**

```bash
grep -rlF -- "$OP_RPC" ~/.cache/davinci-e2e 2>/dev/null | wc -l
grep -rlF -- "$(key_of $KEYS/seq-a.l1.key | sed 's/^0x//')" ~/.cache/davinci-e2e 2>/dev/null | wc -l
SEQ_A_L1=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.l1.key")"); SEQ_A_OP=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.op.key")")
echo "seq-a Ethereum $("$CAST" balance "$SEQ_A_L1" --ether --rpc-url "$L1_READ") (nonce $("$CAST" nonce "$SEQ_A_L1" --rpc-url "$L1_READ")) OP $("$CAST" balance "$SEQ_A_OP" --ether --rpc-url "$OP_PUB") owner OP $("$CAST" balance "$OWNER" --ether --rpc-url "$OP_PUB")"
```
Expected: `0`, `0`; balances consistent with the gas bill (Ethereum spend about 0.00004 to 0.0001 ETH). Keep `~/.cache/davinci-e2e/op-live.log` for the report (it is redacted; not committed).

---

### Task 6: The L1-blob benchmark tool

**Files:**
- Create: `e2e/tests/bench_l1.rs`
- Modify: `docs/testing.md` ("Benchmarks")

**Interfaces:**
- Consumes: Task 1 `davinci_e2e::l1::{L1Net, posts, IL1BlobVerifier, L1BlobVerifier}`, `Net.l1`; `davinci_e2e::{fixture, cost, proxy::{Proxy, Tap, Exchange}, wait, node, chain}`; `davinci_zkvm_sdk::{client::ProverClient (health, job, publics), publics::BatchPublics, types::{JobId, JobStatus}}`; `davinci_client::organizer::ProcessRegistry::{ProcessStateTransitioned, submitStateTransitionL1Call}`; vote status `processed` (set when the node has its proof).
- Produces: the `bench_l1` test (`DAVINCI_E2E_BENCH=l1`), a Markdown table at `DAVINCI_E2E_BENCH_OUT` and its rows as JSON next to it (`.json`), one row per settlement: `n, nf, run, phase, blobs, prove_s, wait_s, seal_to_l1_s, proof_to_settled_s, seal_to_settled_s, headers, calldata_bytes, verify_gas, op_gas, op_l2_wei, op_l1_fee_wei, records, record_wei, l1_gas, l1_blob_gas, l1_wei, l1_posts, l1_base_fee, l1_blob_base_fee, op_tx, l1_tx`.

Expected blob counts (`blob_count(vids, updates, nf) = ceil((2 + vids + updates·(1 + 2·nf) + 2·nf) / 4096)`; a steady batch has 2N updates): nf = 2: 1 blob for every size and phase; nf = 16, first: 1, 2, 3 blobs for N = 64, 128, 256; nf = 16, steady: 2, 3, 5 blobs (the 256 x 16 steady batch is the PoC's 5-blob, 530 KB case).

- [ ] **Step 1: Write the benchmark**

Create `e2e/tests/bench_l1.rs`:

```rust
//! L1-blob batch benchmark, gated by `DAVINCI_E2E_BENCH=l1`: live on an
//! L1-blob deployment (`DAVINCI_E2E_LIVE=1`, OP Mainnet) or, as a dry run of
//! the tool, on anvil with `DAVINCI_E2E_DA=l1-blobs`. Per case (N, nf) and
//! run: a fresh node (`--batch-max N`) whose prover calls go through a
//! recording proxy, an origin-1 process with a 2N-voter census, a first batch
//! (voters 0..N) and a steady batch (voters N..2N, which carries N silent
//! refreshes). Per settlement: the proof (prover job API), the Ethereum blob
//! post (execution plus blob gas), the OP settlement (L2 execution plus the
//! L1 data fee, plus any `record()` of the node), the L1 proof's header count,
//! the gas of `L1BlobVerifier.verify` alone (`eth_estimateGas` at the block
//! before), and the latencies from seal and from proof-ready to settled.
//! "Sealed" is the node's `/prove` call and "proof ready" the first vote seen
//! `processed`, both on the harness clock; "settled" is the OP block's
//! timestamp and "L1 included" the Ethereum block's.
//!
//! Env (defaults in brackets): `DAVINCI_E2E_BENCH_SIZES` [64,128,256],
//! `DAVINCI_E2E_BENCH_NFS` [2,16], `DAVINCI_E2E_BENCH_RUNS` [3],
//! `DAVINCI_E2E_BENCH_BATCH_TIME` [2m], `DAVINCI_E2E_BENCH_PAUSE` seconds
//! between runs [60], `DAVINCI_E2E_BENCH_L1_BUDGET` ETH the node's Ethereum
//! key may spend before no new run starts [0.0009], `DAVINCI_E2E_BENCH_OUT`
//! (the Markdown report; the JSON goes next to it with a `.json` extension).
//! The prover is shared: each run waits for an empty prover queue first.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::consensus::Transaction as _;
use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::utils::{format_ether, parse_ether};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use anyhow::{Context, Result, bail, ensure};
use davinci_client::api::{ProcessId, VoteRequest, VoteStatus};
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, ProcessRegistry};
use davinci_client::prover::BallotProver;
use davinci_e2e::chain;
use davinci_e2e::cost;
use davinci_e2e::fixture as fx;
use davinci_e2e::l1::{self, IL1BlobVerifier, L1BlobVerifier, L1Net};
use davinci_e2e::net::{self, Net};
use davinci_e2e::node::{self, Node, WorkDir};
use davinci_e2e::proxy::{Exchange, Proxy, Tap};
use davinci_e2e::wait;
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::{JobId, JobStatus};
use serde::Serialize;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.into())
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

struct Cfg {
    sizes: Vec<usize>,
    nfs: Vec<u8>,
    runs: usize,
    batch_time: String,
    pause: Duration,
    budget: U256,
}

impl Cfg {
    fn load() -> Result<Cfg> {
        let list = |k: &str, d: &str| -> Result<Vec<u64>> {
            env_or(k, d)
                .split(',')
                .map(|s| s.trim().parse::<u64>().with_context(|| format!("{k}: {s}")))
                .collect()
        };
        let sizes: Vec<usize> = list("DAVINCI_E2E_BENCH_SIZES", "64,128,256")?
            .into_iter()
            .map(|n| n as usize)
            .collect();
        let nfs: Vec<u8> = list("DAVINCI_E2E_BENCH_NFS", "2,16")?
            .into_iter()
            .map(u8::try_from)
            .collect::<Result<_, _>>()?;
        // The shared prover: 256 + 256 refreshes is the largest job we send.
        ensure!(
            sizes.iter().all(|n| (1..=256).contains(n)),
            "DAVINCI_E2E_BENCH_SIZES: 1 to 256"
        );
        ensure!(
            nfs.iter().all(|nf| (2..=16).contains(nf)),
            "DAVINCI_E2E_BENCH_NFS: 2 to 16"
        );
        let runs: usize = env_or("DAVINCI_E2E_BENCH_RUNS", "3").parse()?;
        ensure!((1..=5).contains(&runs), "DAVINCI_E2E_BENCH_RUNS: 1 to 5");
        Ok(Cfg {
            sizes,
            nfs,
            runs,
            batch_time: env_or("DAVINCI_E2E_BENCH_BATCH_TIME", "2m"),
            pause: Duration::from_secs(env_or("DAVINCI_E2E_BENCH_PAUSE", "60").parse()?),
            budget: parse_ether(&env_or("DAVINCI_E2E_BENCH_L1_BUDGET", "0.0009"))
                .context("DAVINCI_E2E_BENCH_L1_BUDGET")?,
        })
    }
}

/// One settlement of a benchmark batch.
#[derive(Clone, Debug, Serialize)]
struct Row {
    n: usize,
    nf: u8,
    run: usize,
    phase: &'static str,
    blobs: usize,
    /// Prover GPU time (`elapsed_ms`) and the rest of seal -> proof ready
    /// (queue, transfers), s.
    prove_s: Option<f64>,
    wait_s: Option<f64>,
    seal_to_l1_s: f64,
    proof_to_settled_s: Option<f64>,
    seal_to_settled_s: f64,
    headers: usize,
    calldata_bytes: usize,
    verify_gas: Option<u64>,
    op_gas: u64,
    op_l2_wei: u128,
    op_l1_fee_wei: u128,
    /// `record()` calls of the node during the batch and their cost.
    records: usize,
    record_wei: u128,
    l1_gas: u64,
    l1_blob_gas: u64,
    l1_wei: u128,
    /// Ethereum transactions the node mined during the batch (re-posts and
    /// replacements show as more than one).
    l1_posts: u64,
    l1_base_fee: u128,
    l1_blob_base_fee: u128,
    op_tx: B256,
    l1_tx: B256,
}

/// A `/prove` call of the node, as its proxy saw it.
struct ProveCall {
    at: f64,
    job: Option<String>,
}

fn prove_tap(log: Arc<Mutex<Vec<ProveCall>>>) -> Tap {
    Arc::new(move |x: &Exchange<'_>| {
        if x.method != "POST" || x.path != "/prove" {
            return;
        }
        let at = unix_now() - x.at.elapsed().as_secs_f64();
        let job = serde_json::from_slice::<serde_json::Value>(x.response)
            .ok()
            .and_then(|v| v["job_id"].as_str().map(String::from));
        if let Ok(mut l) = log.lock() {
            l.push(ProveCall { at, job });
        }
    })
}

/// What a case run shares.
struct Ctx<'a> {
    net: &'a Net,
    l1: &'a L1Net,
    org: &'a Organizer,
    prover_url: &'a str,
    op: DynProvider,
    eth: DynProvider,
    node_l1: Address,
    node_op: Address,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_l1() -> Result<()> {
    if std::env::var("DAVINCI_E2E_BENCH").as_deref() != Ok("l1") {
        eprintln!("DAVINCI_E2E_BENCH=l1 not set, skipping");
        return Ok(());
    }
    let cfg = Cfg::load()?;
    let prover_url = env_or("DAVINCI_ZKVM_URL", "http://127.0.0.1:8080");
    let mut dir = WorkDir::new("davinci-bench-l1-")?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir.path()).await?;
    let l1 = net
        .l1
        .as_ref()
        .context("DAVINCI_E2E_BENCH=l1 needs L1-blob mode")?;
    net.check_balances(1).await?;
    let org = net.organizer()?;
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let prover = ballot_prover.await??;
    let census_dir = dir.path().join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;
    let ctx = Ctx {
        net: &net,
        l1,
        org: &org,
        prover_url: &prover_url,
        op: ProviderBuilder::new()
            .connect_client(chain::rpc(&net.rpc)?)
            .erased(),
        eth: l1.provider()?,
        node_l1: l1.keys[0].1,
        node_op: net.sequencers[0].1,
    };
    let out_md = std::env::var_os("DAVINCI_E2E_BENCH_OUT").map(PathBuf::from);
    let start_balance = ctx.eth.get_balance(ctx.node_l1).await?;
    let mut rows: Vec<Row> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut cases = Vec::new();
    for run in 1..=cfg.runs {
        for &nf in &cfg.nfs {
            for &n in &cfg.sizes {
                cases.push((run, n, nf));
            }
        }
    }
    let r: Result<()> = async {
        for (i, &(run, n, nf)) in cases.iter().enumerate() {
            let spent = start_balance.saturating_sub(ctx.eth.get_balance(ctx.node_l1).await?);
            if spent >= cfg.budget {
                notes.push(format!(
                    "stopped before run {run} N={n} nf={nf}: {} ETH spent on Ethereum, budget {}",
                    format_ether(spent),
                    format_ether(cfg.budget)
                ));
                break;
            }
            if i > 0 {
                tokio::time::sleep(cfg.pause).await;
            }
            idle_prover(&prover_url).await?;
            let r = case(
                &ctx,
                &cfg,
                &bin,
                &prover,
                &census_dir,
                dir.path(),
                (run, n, nf),
            )
            .await
            .with_context(|| format!("run {run} N={n} nf={nf}"))?;
            for row in &r {
                eprintln!("{}", line(row));
            }
            rows.extend(r);
            write_report(&ctx, &cfg, out_md.as_deref(), &rows, &notes)?;
        }
        Ok(())
    }
    .await;
    let c = org.cancel_open().await;
    write_report(&ctx, &cfg, out_md.as_deref(), &rows, &notes)?;
    for n in &notes {
        eprintln!("{n}");
    }
    r?;
    c.context("cancel the open processes")?;
    dir.ok = true;
    Ok(())
}

/// Waits until the shared prover's queue is empty (at most 20 minutes).
async fn idle_prover(url: &str) -> Result<()> {
    let pc = ProverClient::new(url);
    wait::until(
        "an idle prover",
        Duration::from_secs(20 * 60),
        Duration::from_secs(15),
        || async {
            let h = pc.health().await?;
            ensure!(h.status == "ok", "prover health: {h:?}");
            Ok((h.queue_len == 0).then_some(()))
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn case(
    ctx: &Ctx<'_>,
    cfg: &Cfg,
    bin: &Path,
    prover: &BallotProver,
    census_dir: &Path,
    dir: &Path,
    (run, n, nf): (usize, usize, u8),
) -> Result<Vec<Row>> {
    let name = format!("l1bench-{n}-nf{nf}-r{run}");
    let log: Arc<Mutex<Vec<ProveCall>>> = Arc::new(Mutex::new(Vec::new()));
    let proxy = Proxy::tapped(ctx.prover_url, prove_tap(log.clone())).await?;
    let mut nc = ctx
        .net
        .node_config(&name, Some(0), 0, &proxy.url, census_dir, dir)?;
    nc.batch_max = n;
    nc.batch_time = cfg.batch_time.clone();
    let node = Node::start(bin, &nc).await?;

    let voters = fx::bench_voters(2 * n);
    let (census, tree) = fx::merkle_census_of(&voters)?;
    let uri = fx::write_census(census_dir, &format!("{name}.json"), &census)?;
    let (metadata, metadata_hash) =
        fx::write_metadata(census_dir, &format!("{name}-metadata.json"), &name)?;
    let next = ctx.org.next_process_id().await?;
    let pk = node.api.new_key(&next).await?;
    let pid = ctx
        .org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 4 * 3600,
            max_voters: 2 * n as u64,
            ballot_mode: fx::ballot_mode_nf(nf),
            census_origin: 1,
            census_root: tree.root(),
            census_contract: [0; 20],
            census_uri: uri,
            metadata,
            metadata_hash,
            key_mode: KeyMode::Sequencer(pk),
        })
        .await?
        .pid;
    let r = measure(ctx, &node, &log, prover, &tree, &voters, pid, (run, n, nf)).await;
    let c = ctx.org.cancel_process(&pid).await;
    let rows = r?;
    c.context("cancel")?;
    eprintln!("run {run} N={n} nf={nf}: process {}", ProcessId(pid));
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
async fn measure(
    ctx: &Ctx<'_>,
    node: &Node,
    log: &Arc<Mutex<Vec<ProveCall>>>,
    prover: &BallotProver,
    tree: &davinci_zkvm_sdk::census::LeanImt,
    voters: &[davinci_client::voter::Voter],
    pid: [u8; 31],
    (run, n, nf): (usize, usize, u8),
) -> Result<Vec<Row>> {
    let p = davinci_client::organizer::RegistryReader::connect(&ctx.net.rpc, ctx.net.registry)?
        .process(&pid)
        .await?;
    let mut jobs = Vec::new();
    for (v, voter) in voters.iter().enumerate() {
        jobs.push(fx::Job {
            label: format!("bench voter {v}"),
            key: voter.key.clone(),
            process: p.clone(),
            fields: fx::choices_nf(v, nf as usize),
            census: fx::merkle_witness(tree, v)?,
            weight: fx::WEIGHT,
            k: fx::vote_k(0x30 + run as u64, v, 1),
        });
    }
    let reqs = fx::prove_all(prover, &jobs)?;
    let last = voters.last().context("no voters")?.address();
    wait::until(
        "the node to serve the census",
        net::scaled(Duration::from_secs(180)),
        Duration::from_secs(1),
        || async {
            node.check_alive()?;
            Ok(node.api.participant(&pid, &last).await.ok().map(|_| ()))
        },
    )
    .await?;
    let mut rows = Vec::new();
    for (phase, part) in [("first", &reqs[..n]), ("steady", &reqs[n..])] {
        let (base_fee, blob_base_fee) = l1_market(&ctx.eth).await?;
        let nonce0 = ctx.eth.get_transaction_count(ctx.node_l1).await?;
        let op0 = ctx.op.get_block_number().await?;
        let calls0 = log.lock().map(|l| l.len()).unwrap_or(0);
        let proof_ready = batch(node, &pid, part).await?;
        let posts = ctx.eth.get_transaction_count(ctx.node_l1).await? - nonce0;
        let calls: Vec<(f64, Option<String>)> = log
            .lock()
            .map(|l| l[calls0..].iter().map(|c| (c.at, c.job.clone())).collect())
            .unwrap_or_default();
        let sealed = calls.first().map(|c| c.0).context("no /prove call seen")?;
        for s in settlements(ctx, pid, op0).await? {
            let mut row = settlement_row(ctx, &s, &calls, (sealed, proof_ready), op0).await?;
            (row.n, row.nf, row.run, row.phase) = (n, nf, run, phase);
            (row.l1_posts, row.l1_base_fee, row.l1_blob_base_fee) =
                (posts, base_fee, blob_base_fee);
            rows.push(row);
        }
    }
    Ok(rows)
}

/// The base fee of Ethereum's latest block and its blob base fee, in wei.
async fn l1_market(eth: &DynProvider) -> Result<(u128, u128)> {
    let b = eth
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
        .await?
        .context("no latest L1 block")?;
    let blob = match eth.get_blob_base_fee().await {
        Ok(f) => f,
        // Not every endpoint serves eth_blobBaseFee.
        Err(_) => eth
            .get_fee_history(1, alloy::eips::BlockNumberOrTag::Latest, &[])
            .await?
            .next_block_blob_base_fee()
            .unwrap_or(0),
    };
    Ok((u128::from(b.header.base_fee_per_gas.unwrap_or(0)), blob))
}

/// Submits `reqs` and waits until every one is settled; returns when the
/// node first had the batch's proof (a vote `processed`, else `settled`),
/// unix seconds on the harness clock.
async fn batch(node: &Node, pid: &[u8; 31], reqs: &[VoteRequest]) -> Result<f64> {
    for r in reqs {
        node.api
            .submit_vote(r)
            .await
            .with_context(|| format!("vote {}", r.vote_id))?;
    }
    let sentinel = reqs.first().context("empty batch")?.vote_id;
    let mut open: Vec<u64> = reqs.iter().map(|r| r.vote_id).collect();
    let mut proof_ready: Option<f64> = None;
    let limit = net::scaled(Duration::from_secs(30 * 60));
    let t = Instant::now();
    for tick in 0u64.. {
        node.check_alive()?;
        if proof_ready.is_none() {
            let s = node.api.vote_status_full(pid, sentinel).await?;
            match s.status {
                VoteStatus::Processed | VoteStatus::Settled => proof_ready = Some(unix_now()),
                VoteStatus::Error => bail!("vote {sentinel}: {}", s.error.unwrap_or_default()),
                _ => {}
            }
        }
        if tick % 10 == 0 {
            let mut still = Vec::new();
            for vid in open {
                let s = node.api.vote_status_full(pid, vid).await?;
                match s.status {
                    VoteStatus::Settled => {}
                    VoteStatus::Error => bail!("vote {vid}: {}", s.error.unwrap_or_default()),
                    _ => still.push(vid),
                }
            }
            open = still;
            if open.is_empty() {
                return proof_ready.context("settled before the first status read");
            }
        }
        ensure!(
            t.elapsed() < limit,
            "{} votes not settled after {limit:?}",
            open.len()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    unreachable!("the loop returns or fails")
}

/// A `ProcessStateTransitioned` of the batch's process.
struct Settlement {
    tx: B256,
    block: u64,
    new_root: [u8; 32],
}

/// The process's settlements from OP block `from`, in chain order.
async fn settlements(ctx: &Ctx<'_>, pid: [u8; 31], from: u64) -> Result<Vec<Settlement>> {
    use ProcessRegistry::ProcessStateTransitioned as Ev;
    let mut out = Vec::new();
    for l in chain::registry_logs(&ctx.net.rpc, ctx.net.registry, Ev::SIGNATURE_HASH, from).await? {
        let d = l.log_decode::<Ev>()?.inner.data;
        if d.processId.0 == pid {
            out.push(Settlement {
                tx: l.transaction_hash.context("log without tx hash")?,
                block: l.block_number.context("log without block")?,
                new_root: d.newStateRoot.0,
            });
        }
    }
    ensure!(!out.is_empty(), "no settlement of {}", ProcessId(pid));
    Ok(out)
}

/// The receipt's gas, L2 execution cost and OP's L1 data fee (`l1Fee`, zero
/// on anvil), and its sender.
async fn op_cost(op: &DynProvider, tx: B256) -> Result<(u64, u128, u128, Address)> {
    let r: serde_json::Value = op
        .raw_request("eth_getTransactionReceipt".into(), (tx,))
        .await?;
    let hex = |k: &str| -> Result<u128> {
        match r[k].as_str() {
            Some(v) => Ok(u128::from_str_radix(v.trim_start_matches("0x"), 16)?),
            None => Ok(0),
        }
    };
    let gas = u64::try_from(hex("gasUsed")?)?;
    let from: Address = r["from"]
        .as_str()
        .context("receipt without from")?
        .parse()?;
    Ok((
        gas,
        u128::from(gas) * hex("effectiveGasPrice")?,
        hex("l1Fee")?,
        from,
    ))
}

/// One settlement's row; `op0` is the OP head when its batch was submitted.
async fn settlement_row(
    ctx: &Ctx<'_>,
    s: &Settlement,
    calls: &[(f64, Option<String>)],
    (sealed, proof_ready): (f64, f64),
    op0: u64,
) -> Result<Row> {
    let tx = ctx
        .op
        .get_transaction_by_hash(s.tx)
        .await?
        .context("settlement tx not found")?;
    let input = tx.inner.input().to_vec();
    let call = ProcessRegistry::submitStateTransitionL1Call::abi_decode(&input)
        .context("not an L1-blob settlement")?;
    // The proof job whose root is this settlement's.
    let pc = ProverClient::new(ctx.prover_url);
    let mut prove_s = None;
    for (_, job) in calls {
        let Some(id) = job else { continue };
        let id = JobId::parse(id)?;
        let j = pc.job(&id).await?;
        if j.status != JobStatus::Done {
            continue;
        }
        let root = BatchPublics::parse(&pc.publics(&id).await?).map(|p| p.root_after);
        if root.is_ok_and(|r| r == s.new_root) {
            prove_s = j.elapsed_ms.map(|ms| ms as f64 / 1000.0);
        }
    }
    let post = l1::posts(&ctx.net.rpc, ctx.l1.read_rpc(), ctx.net.registry, s.block)
        .await?
        .into_iter()
        .find(|p| p.op_tx == s.tx)
        .context("no ProcessStateTransitionedL1 in the settlement")?;
    let l1_tx = *post.l1_txs.first().context("no Ethereum post")?;
    let l1c = cost::fetch(ctx.l1.read_rpc(), "L1 post", l1_tx).await?;
    let l1_ts = block_time(&ctx.eth, l1c.block).await?;
    let settled = block_time(&ctx.op, s.block).await? as f64;
    let (op_gas, op_l2_wei, op_l1_fee_wei, _) = op_cost(&ctx.op, s.tx).await?;
    // The verifier alone, where the settlement ran (the block before it).
    let p = IL1BlobVerifier::L1BlobProof::abi_decode(&call.l1.abi_encode())?;
    let verify = TransactionRequest::default()
        .with_to(ctx.l1.verifier)
        .with_input(L1BlobVerifier::verifyCall { p }.abi_encode());
    let verify_gas = ctx
        .op
        .estimate_gas(verify)
        .block(BlockId::number(s.block - 1))
        .await
        .ok();
    // record() calls of this node while the batch was in flight.
    let mut records = 0;
    let mut record_wei = 0u128;
    for l in chain::registry_logs(
        &ctx.net.rpc,
        ctx.l1.verifier,
        L1BlobVerifier::OriginRecorded::SIGNATURE_HASH,
        op0,
    )
    .await?
    {
        if l.block_number.is_some_and(|b| b > s.block) {
            continue;
        }
        let h = l.transaction_hash.context("log without tx hash")?;
        let (_, l2, l1f, from) = op_cost(&ctx.op, h).await?;
        if from == ctx.node_op {
            records += 1;
            record_wei += l2 + l1f;
        }
    }
    let prove_ready_s = proof_ready - sealed;
    Ok(Row {
        n: 0,
        nf: 0,
        run: 0,
        phase: "",
        blobs: call.commitments.len(),
        prove_s,
        wait_s: prove_s.map(|p| (prove_ready_s - p).max(0.0)),
        seal_to_l1_s: l1_ts as f64 - sealed,
        proof_to_settled_s: Some(settled - proof_ready),
        seal_to_settled_s: settled - sealed,
        headers: call.l1.headers.len(),
        calldata_bytes: input.len(),
        verify_gas,
        op_gas,
        op_l2_wei,
        op_l1_fee_wei,
        records,
        record_wei,
        l1_gas: l1c.gas,
        l1_blob_gas: l1c.blob_gas,
        l1_wei: u128::try_from(l1c.wei())?,
        l1_posts: 0,
        l1_base_fee: 0,
        l1_blob_base_fee: 0,
        op_tx: s.tx,
        l1_tx,
    })
}

async fn block_time(p: &DynProvider, n: u64) -> Result<u64> {
    Ok(p.get_block_by_number(n.into())
        .await?
        .with_context(|| format!("block {n}"))?
        .header
        .timestamp)
}

fn opt(v: Option<f64>) -> String {
    v.map_or("n/a".into(), |v| format!("{v:.1}"))
}

fn line(r: &Row) -> String {
    format!(
        "run {} N={} nf={} {}: {} blobs, prove {} s, seal->L1 {:.1} s, proof->settled {} s, \
         seal->settled {:.1} s, {} headers, verify gas {}, L1 {} ETH ({} posts), OP {} ETH",
        r.run,
        r.n,
        r.nf,
        r.phase,
        r.blobs,
        opt(r.prove_s),
        r.seal_to_l1_s,
        opt(r.proof_to_settled_s),
        r.seal_to_settled_s,
        r.headers,
        r.verify_gas.map_or("n/a".into(), |g| g.to_string()),
        format_ether(U256::from(r.l1_wei)),
        r.l1_posts,
        format_ether(U256::from(r.op_l2_wei + r.op_l1_fee_wei + r.record_wei)),
    )
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

/// Micro-ETH.
fn ueth(wei: u128) -> f64 {
    wei as f64 / 1e12
}

const HEADER: &str = "| N | nf | batch | runs | blobs | prove s | proof -> settled s | seal -> settled s | headers | verify gas | L1 µETH | OP µETH | L1 posts |\n|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|";

/// One Markdown line per (N, nf, phase): medians of the times, header counts
/// and verify gas, means of the costs.
fn table(rows: &[Row]) -> String {
    let mut by: BTreeMap<(u8, usize, &str), Vec<&Row>> = BTreeMap::new();
    for r in rows {
        by.entry((r.nf, r.n, r.phase)).or_default().push(r);
    }
    let mut out = vec![HEADER.to_string()];
    for ((nf, n, phase), rs) in by {
        let mean =
            |f: &dyn Fn(&Row) -> u128| rs.iter().map(|r| ueth(f(r))).sum::<f64>() / rs.len() as f64;
        out.push(format!(
            "| {n} | {nf} | {phase} | {} | {} | {} | {} | {} | {} | {} | {:.2} | {:.3} | {} |",
            rs.len(),
            rs[0].blobs,
            opt(median(rs.iter().filter_map(|r| r.prove_s).collect())),
            opt(median(
                rs.iter().filter_map(|r| r.proof_to_settled_s).collect()
            )),
            opt(median(rs.iter().map(|r| r.seal_to_settled_s).collect())),
            opt(median(rs.iter().map(|r| r.headers as f64).collect())),
            median(
                rs.iter()
                    .filter_map(|r| r.verify_gas.map(|g| g as f64))
                    .collect()
            )
            .map_or("n/a".into(), |g| format!("{g:.0}")),
            mean(&|r| r.l1_wei),
            mean(&|r| r.op_l2_wei + r.op_l1_fee_wei + r.record_wei),
            rs.iter().map(|r| r.l1_posts).max().unwrap_or(0),
        ));
    }
    out.join("\n")
}

/// The Markdown report at `md` and the JSON rows next to it.
fn write_report(
    ctx: &Ctx<'_>,
    cfg: &Cfg,
    md: Option<&Path>,
    rows: &[Row],
    notes: &[String],
) -> Result<()> {
    let Some(md) = md else { return Ok(()) };
    let date = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let prover = env_or("DAVINCI_E2E_BENCH_PROVER", "RTX 5090, single, shared");
    let body = format!(
        "# L1-blob batch benchmark, chain {}\n\n\
         Date {date}. Registry {}, L1BlobVerifier {}, prover {prover}. One node per run \
         (`--batch-max N`, `--batch-time {}`), {} runs per case; first batch: voters 0..N, steady: \
         voters N..2N plus N refreshes. Medians of the times, header counts and verify gas; means \
         of the costs. L1 is the blob post (execution plus blob gas), OP the settlement (L2 \
         execution plus L1 data fee) plus any `record()`.\n\n{}\n\n{}\n",
        ctx.net.chain_id,
        ctx.net.registry,
        ctx.l1.verifier,
        cfg.batch_time,
        cfg.runs,
        table(rows),
        notes
            .iter()
            .map(|n| format!("- {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    std::fs::write(md, body).with_context(|| format!("write {}", md.display()))?;
    let json = serde_json::json!({
        "date": date,
        "chainId": ctx.net.chain_id,
        "registry": ctx.net.registry,
        "l1BlobVerifier": ctx.l1.verifier,
        "prover": prover,
        "runs": cfg.runs,
        "rows": rows,
        "notes": notes,
    });
    let jp = md.with_extension("json");
    std::fs::write(&jp, serde_json::to_vec_pretty(&json)?)
        .with_context(|| format!("write {}", jp.display()))?;
    Ok(())
}
```

- [ ] **Step 2: Build and lint**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p davinci-e2e --test bench_l1 2>&1 | tail -2`
Expected: no warnings; `DAVINCI_E2E_BENCH=l1 not set, skipping` and `test result: ok. 1 passed`.

- [ ] **Step 3: Document it**

In `docs/testing.md` ("Benchmarks"), after the batch-size paragraph and its code block:

~~~markdown
**L1 blobs** (`e2e/tests/bench_l1.rs`, `DAVINCI_E2E_BENCH=l1`): the batch-size benchmark of the
L1-blob mode, live on OP Mainnet or, as a dry run, on anvil with `DAVINCI_E2E_DA=l1-blobs`. Per
case and run a fresh node proves a first and a steady batch; per settlement it records the proof
time, the latencies from seal and from proof-ready to settled, the Ethereum post (execution plus
blob gas), the OP settlement (L2 execution plus L1 data fee, plus any `record()`), the L1 proof's
header count and the gas of `L1BlobVerifier.verify` alone. Sizes come from
`DAVINCI_E2E_BENCH_SIZES` (default `64,128,256`, at most 256), fields from `DAVINCI_E2E_BENCH_NFS`
(`2,16`), runs from `DAVINCI_E2E_BENCH_RUNS` (3); runs are `DAVINCI_E2E_BENCH_PAUSE` (60) s apart
and each waits for an empty prover queue; no new run starts once the node's Ethereum key spent
`DAVINCI_E2E_BENCH_L1_BUDGET` (0.0009 ETH). It writes the Markdown table to
`DAVINCI_E2E_BENCH_OUT` and the rows as JSON next to it.
~~~

- [ ] **Step 4: Budget guard, without the GPU (Review Focus 3)**

```bash
mkdir -p ~/.cache/davinci-bench
DAVINCI_E2E_BENCH=l1 DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_BENCH_L1_BUDGET=0 DAVINCI_E2E_BENCH_SIZES=8 DAVINCI_E2E_BENCH_NFS=2 DAVINCI_E2E_BENCH_RUNS=1 \
  DAVINCI_E2E_BENCH_OUT=$HOME/.cache/davinci-bench/l1-budget0.md TMPDIR=$HOME/.cache/davinci-e2e \
  cargo test -p davinci-e2e --test bench_l1 -- --nocapture 2>&1 | grep -E "stopped|test result"
grep -c '^| [0-9]' ~/.cache/davinci-bench/l1-budget0.md; jq '.rows | length' ~/.cache/davinci-bench/l1-budget0.json
```
Expected: `stopped before run 1 N=8 nf=2: 0.000000000000000000 ETH spent on Ethereum, budget 0.000000000000000000`, `test result: ok. 1 passed`, `0`, `0` (no job reached the prover).

- [ ] **Step 5: Dry run on anvil with the real prover (two small proofs)**

Go/no-go: prover queue empty. Then:

```bash
DAVINCI_E2E_BENCH=l1 DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_BENCH_SIZES=8 DAVINCI_E2E_BENCH_NFS=2 DAVINCI_E2E_BENCH_RUNS=1 \
  DAVINCI_E2E_BENCH_PAUSE=0 DAVINCI_E2E_BENCH_BATCH_TIME=30s DAVINCI_E2E_BENCH_OUT=$HOME/.cache/davinci-bench/l1-anvil-dry.md \
  DAVINCI_SEQUENCER_BIN=$HOME/.cache/davinci-l1blob/bin/davinci-sequencer-$(git rev-parse --short HEAD) TMPDIR=$HOME/.cache/davinci-e2e \
  systemd-run --user --scope -p MemoryMax=16G -- cargo test -p davinci-e2e --test bench_l1 -- --nocapture 2>&1 | grep -E "^run 1|test result"
cat ~/.cache/davinci-bench/l1-anvil-dry.md | tail -4
jq '.rows[0] | {blobs, prove_s, headers, verify_gas, l1_posts, op_l1_fee_wei}' ~/.cache/davinci-bench/l1-anvil-dry.json
```
Expected: two lines `run 1 N=8 nf=2 first: 1 blobs, prove <s> s, seal->L1 <s> s, proof->settled <s> s, seal->settled <s> s, <h> headers, verify gas <g>, L1 <eth> ETH (1 posts), OP <eth> ETH` and `... steady: ...`; `test result: ok. 1 passed`; a two-row table; the JSON row with `blobs 1`, a numeric `prove_s`, `headers` at least 1, a numeric `verify_gas`, `l1_posts 1` and `op_l1_fee_wei 0` (anvil has no L1 data fee). A `null` `prove_s` means the job was not matched by root: fix before Task 7.

- [ ] **Step 6: Commit**

```bash
git add e2e/tests/bench_l1.rs docs/testing.md
git commit -m "test(e2e): l1-blob batch benchmark"
```

---

### Task 7: The benchmarks on OP Mainnet

**Files:**
- Modify: `docs/benchmarks.md` (new section after "Batch sizes on Gnosis")
- Create: `docs/superpowers/reports/<run date>-l1-blob-da-op-bench.json` (the run's JSON, copied)

**Interfaces:**
- Consumes: Task 6 `bench_l1`; Task 5 Step 6 environment; the funded `seq-a` keys.
- Produces: the Markdown table and the JSON rows (6 cases x 2 phases x up to 3 runs), the numbers the report uses.

Budget: at the 2026-10-12 prices (base fee 0.35 gwei, blob base fee 0.015 gwei) one run of the 6 cases is 12 posts and 22 blobs, about 0.00013 ETH on Ethereum; three runs about 0.0004 ETH. `DAVINCI_E2E_BENCH_L1_BUDGET=0.0009` stops starting runs past that; OP costs stay under 0.00005 ETH per run.

- [ ] **Step 1: Go/no-go**

```bash
# the Task 5 Step 6 environment, then:
SEQ_A_L1=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.l1.key")")
echo "seq-a Ethereum $("$CAST" balance "$SEQ_A_L1" --ether --rpc-url "$L1_READ"), base fee $("$CAST" base-fee --rpc-url "$L1_READ"), blob base fee $("$CAST" rpc eth_blobBaseFee --rpc-url "$L1_READ")"
curl -s "${DAVINCI_ZKVM_URL:-http://127.0.0.1:8080}/health"; echo
```
Go: `seq-a` holds at least 0.001 ETH on Ethereum (else top it up from the owner with Task 4 Step 4's commands, only while the owner's total stays within 0.0025 ETH), base fee at most 1 gwei (1000000000), blob base fee at most 0.05 gwei (`0x2faf080`), prover `"queue_len":0`. Otherwise wait for quieter fees or an idle prover; do not lower the guards.

- [ ] **Step 2: Run**

```bash
STAMP=$(date -u +%Y%m%d-%H%M)
DAVINCI_E2E_BENCH=l1 DAVINCI_E2E_BENCH_L1_BUDGET=0.0009 DAVINCI_E2E_BENCH_PROVER="RTX 5090, single, shared with a production node" \
  DAVINCI_E2E_BENCH_OUT=$HOME/.cache/davinci-bench/l1-op-$STAMP.md \
  systemd-run --user --scope -p MemoryMax=16G -- cargo test -p davinci-e2e --test bench_l1 -- --nocapture 2>&1 \
  | redact | tee ~/.cache/davinci-bench/l1-op-$STAMP.log | grep -E "^run [0-9]|stopped|test result"
```
Expected: about 2 to 2.5 hours; 36 lines `run r N=n nf=f first|steady: ...` (3 runs x 6 cases x 2 phases; nf = 16 steady at 256 shows `5 blobs`), `test result: ok. 1 passed`. Each settlement's `proof->settled` is roughly `max(0, ~100 s - proof time)`: the post at proving start hides OP's origin lag behind long proofs. A `stopped before run ...` line means the budget guard fired: keep the partial table and say so in the report.

- [ ] **Step 3: Check the numbers before trusting them**

```bash
J=$(ls -t ~/.cache/davinci-bench/l1-op-*.json | head -1)
jq -r '.rows[] | select(.l1_posts != 1 or .prove_s == null or .verify_gas == null) | "\(.run) \(.n) \(.nf) \(.phase): posts \(.l1_posts) prove \(.prove_s) verify \(.verify_gas)"' "$J"
jq -r '[.rows[] | select(.nf == 16 and .n == 256 and .phase == "steady") | .blobs] | unique' "$J"
jq -r '[.rows[] | .l1_wei] | add' "$J"
```
Expected: no line, or lines explained in the report (a re-post shows `posts 2`; an OP RPC without old state shows `verify null`); `[5]`; the total L1 wei within the budget.

- [ ] **Step 4: Secret scan, then record the results**

```bash
grep -rlF -- "$OP_RPC" ~/.cache/davinci-bench ~/.cache/davinci-e2e 2>/dev/null | wc -l
cp "$J" docs/superpowers/reports/$(date -u +%F)-l1-blob-da-op-bench.json
```
Expected: `0`. In `docs/benchmarks.md`, insert after the "Batch sizes on Gnosis" section (its last paragraph ends "...about 0.16 s each."):

~~~markdown
## L1-blob batch sizes on OP Mainnet

Measured on <date> on OP Mainnet in L1-blob mode (registry <registry>, see
[deployments.md](deployments.md#op-mainnet)) with one RTX 5090 prover shared with a production
node, `e2e/tests/bench_l1.rs`, 3 runs per case. Blobs go to Ethereum at proving start; the
settlement waits for OP's L1 origin to reach the blob block. "Prove" is the prover's GPU time,
"proof -> settled" and "seal -> settled" end at the OP block holding the settlement; L1 is the blob
transaction (21,000 gas plus 131,072 blob gas per blob), OP the settlement plus any `record()`.
Prices during the run: Ethereum base fee <..> gwei, blob base fee <..> gwei. Rows of every
settlement: [the JSON](superpowers/reports/<date>-l1-blob-da-op-bench.json).

<the table from the run's Markdown report, unchanged>

<two or three sentences: where proof -> settled reaches zero (proofs longer than OP's origin lag),
how verify gas grows with header count, and the L1 share of the cost>
~~~

(The bracketed values come from the run's Markdown header, `l1_base_fee`/`l1_blob_base_fee` of the JSON rows, and the table; write the sentences from the numbers.)

- [ ] **Step 5: Commit**

```bash
git add docs/benchmarks.md docs/superpowers/reports/*-l1-blob-da-op-bench.json
git commit -m "docs(bench): l1-blob batch sizes on op mainnet"
```

---

### Task 8: Report, docs, and the final Fable review

**Files:**
- Create: `docs/superpowers/reports/<run date>-l1-blob-da-op-live.md`
- Modify: `README.md` (after the `gnosis` preset paragraph), `docs/configuration.md` ("L1-blob mode" section)

**Interfaces:**
- Consumes: the outputs of Tasks 3 to 7 (`~/.cache/davinci-e2e/e2e-l1.log`, `e2e-blobs.log`, `op-live.log`, `~/.cache/davinci-l1blob/deploy-op.log`, `deployments/10.json`, the bench JSON and table); the PoC reports.
- Produces: the report; operator docs; a clean Fable review of both branches.

- [ ] **Step 1: Operator notes in the docs**

In `docs/configuration.md`, append to the "L1-blob mode" section:

~~~markdown
On OP Mainnet:

- A settlement lands only once OP's L1 origin reaches the blob's Ethereum block, about 90 to 110 s
  after the post (OP lags Ethereum by 6 to 8 blocks). The node posts when proving starts, so a
  proof longer than that hides the wait. Give the grace budget room for it:
  `--settle-margin 120s` instead of the 60 s default.
- `--l1-rpc-url` must include an endpoint that takes 1.5 MB request bodies (a six-blob send):
  publicnode refuses them, dRPC takes them, and the node sends a blob transaction to every listed
  endpoint, so the preset's pair works. With your own endpoint, set it as `--l1-send-rpc-url`.
- `--blob-source` must name beacon APIs that keep full blobs; after PeerDAS only supernodes and
  archives do. publicnode's served recent and day-old slots when the preset was filled in;
  lodestar's public endpoint refused blob requests (403).
- `--l1-max-post-fee` bounds what one post may cost at the fees it offers (twice the base fee and
  twice the blob base fee); a replacement doubles them. At 0.35 gwei and 0.015 gwei a six-blob post
  offers about 0.00004 ETH, so `0.00015eth` leaves room for two replacements.
~~~

In `README.md`, after the paragraph that starts "The `gnosis` network preset (the default)":

~~~markdown
The `optimism` preset runs a node on OP Mainnet in L1-blob mode: transitions settle on OP and
their blobs go to Ethereum. Such a node also needs a dedicated Ethereum key for its blob posts
(`DAVINCI_L1_PRIVKEY_FILE`, funded on Ethereum, used by nothing else) and Ethereum endpoints (the
preset lists public ones); see [configuration.md](docs/configuration.md#l1-blob-mode) and
[deployments.md](docs/deployments.md#op-mainnet).
~~~

- [ ] **Step 2: Write the report**

Create `docs/superpowers/reports/$(date -u +%F)-l1-blob-da-op-live.md` with these sections, each filled from the named source (numbers as measured, no estimates where a measurement exists):

~~~markdown
# L1-blob DA on OP Mainnet: deployment, live run and benchmarks

Date: <UTC dates of Tasks 4, 5 and 7>. Branches `feat/l1-blob-da`: davinci-sequencer `<git rev-parse --short HEAD>`,
davinci-contracts `<git -C ../davinci-contracts rev-parse --short HEAD>`. Spec:
`docs/superpowers/specs/2026-10-12-l1-blob-da-op-design.md`; plans 1 to 3.

## Verdict
<one paragraph: does the mode work on OP Mainnet end to end; cost and latency per transition at
the measured prices; what remains open>

## Deployment
<table: L1BlobVerifier and ProcessRegistry addresses, tx hashes, block, gas, cost: from
deployments/10.json and deploy-op.log's Hash/Block/Paid lines; the reused ZiskVerifier and its
codehash; verify_deployment.py: all OK; VERIFY_MODE=false (no explorer key)>

## e2e on anvil
<L1-blob run and blob-mode run: duration, transitions, the restart line (nonce, both hashes, fees
doubled), every L1 negative check and its revert, from e2e-l1.log and e2e-blobs.log>

## Live run on OP Mainnet
<from op-live.log: transitions, settle times, the restart outcome line, observer synced from the
beacon, results equal to the tallies, the negatives (PASS or INCONCLUSIVE and why), the gas bill
split into OP and Ethereum>

## Benchmarks
<the docs/benchmarks.md table; the fees during the run; proof -> settled versus proof time;
verify gas versus header count; records (`record()` calls) seen>

## Comparison with the PoC
| | PoC (beacon-root proof, 2026-10-12) | production (execution-layer proof) |
|---|---|---|
| 256 x 16 (5 blobs) OP gas | 774.0k | <measured> |
| verify part | (SSZ proof, in the 774k) | <verify gas, measured> |
| proof-ready -> settled, posted at proving start | 24.9 s (77 s proof) | <measured> |
| L1 cost per 5-blob post | 6.8-7.5 µETH (0.10 gwei base fee) | <measured, at <..> gwei> |
| total per 5-blob transition | 0.017-0.019 USD | <measured ETH; USD at the day's price> |
<two sentences on the differences: headers in calldata, the origin wait, fee levels>

## Deviations from the spec
- Restart: the batch is proved again (its secrets are never persisted) and the first post replaces a
  pending orphan at its nonce with doubled fees (persisted L1 post record), instead of resuming the
  stored sidecar (plan2-notes, "After the Fable gate").
- Anchor policy: `record()` once when OP's origin reaches the blob block before the proof is ready;
  16 / 64 header limits; OP gas is estimate + 20 %, not a fixed 1.2M (verify alone measured
  <..> gas).
- No OP Sepolia soak (no testnet funds); the live phase started with small elections on mainnet.
- One live signing node plus an observer (lost races are covered on anvil).

## Spend
<owner and seq-a balances before and after on both chains; totals against the caps 0.0025 ETH
(Ethereum) and 0.0012 ETH (OP); what stays on the seq-a keys>

## Operator notes
<send endpoint, beacon availability, settle margin, fee cap: as in docs/configuration.md, with
what the runs showed>

## Open items
<CI's CONTRACTS_REF; explorer source verification; whether the node should default its settle
margin in L1-blob mode; a beacon node of our own; anything the Fable review left for later>
~~~

- [ ] **Step 3: Commit the docs and the report**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked" | sort | uniq -c
git add README.md docs/configuration.md docs/superpowers/reports/*-l1-blob-da-op-live.md
git commit -m "docs(da): op mainnet live report, operator notes and readme"
```
Expected: every line `test result: ok`.

- [ ] **Step 4: Final Fable review of both branches**

Dispatch the `rust-reviewer` agent (Fable 5.1, read-only) with this prompt:

~~~text
Review the whole L1-blob DA work before it is declared done: davinci-sequencer
`git diff main...feat/l1-blob-da` (/home/p4u/davinci-sequencer) and davinci-contracts
`git diff main...feat/l1-blob-da` (/home/p4u/davinci-contracts). Spec:
docs/superpowers/specs/2026-10-12-l1-blob-da-op-design.md, overrides in .superpowers/plan2-notes.md
("After the Fable gate"), report docs/superpowers/reports/*-l1-blob-da-op-live.md. Focus:
(1) soundness of L1BlobVerifier and submitStateTransitionL1 (anchoring, header chain, trie proof,
versioned-hash binding, digest order, replay); (2) the node's poster and settle loop: nonce
handling, replacement pricing, fee cap, restart with a pending post, reorgs, origin lag;
(3) secrets: no key, seed or private RPC URL logged, persisted or committed (check
broadcast/DeployAll.s.sol/10/run-latest.json and deployments/10.json too); (4) the e2e harness:
does blob mode stay unchanged, do the L1 negatives assert the right reverts, can a test pass
without exercising what it names; (5) operator-facing failure modes on OP Mainnet. Build and run
tests to confirm findings (no GPU, no network transactions). Report findings as Critical /
Important / Minor with file:line and evidence.
~~~

Gate: every Critical and Important finding is fixed in its own commit (with a test where the finding is behavioural), and a second review of the fix diff (`git diff <commit before the fixes>..HEAD` in the affected repository) comes back without Critical or Important findings. Minor findings go to the report's "Open items" or are fixed. Then:

```bash
cd /home/p4u/davinci-sequencer && git status --short | wc -l && git log --oneline main..HEAD | head -40
cd /home/p4u/davinci-contracts && git status --short | wc -l && git log --oneline main..HEAD | head -20
```
Expected: `0` uncommitted files in both; the branch histories end with this plan's commits. Nothing is pushed.

---

## Self-review

- **Spec coverage.** §6 e2e: Task 1 (two anvils, `MockL1Block` relayer with a configurable lag, L1-mode deploy, per-node Ethereum keys, `DAVINCI_BLOB_SOURCE=anvil` on the L1 anvil, `DAVINCI_E2E_SETUP=1` in L1 mode and in CI), Task 2 (negatives: wrong header chain, wrong transaction proof, swapped commitment, wrong-mode entry point, plus replay, proof flip and extra blob), Task 3 (Merkle census with races, CSP, rejected packages, grace window, results and an observer through the unchanged scenario; the kill between posting and settling with re-prove and replacement at the nonce; the `make e2e` gate in both modes with bounded batches). §6 live: Task 5 (one node plus an observer, Merkle and CSP processes, several batches, results, follower sync from Ethereum beacon blobs, a restart mid-flight). §6 benchmarks: Tasks 6 and 7 (64/128/256 x 2/16, proof time, proof-ready -> settled and seal -> settled, L1 cost as blob gas plus execution, OP cost as L2 execution plus L1 data fee, header count and verify gas, runs bounded by a budget, JSON and Markdown, spaced runs). §7: Task 4 (dedicated keys in 0600 files with only addresses printed, funding within the caps, fresh `L1BlobVerifier` on `L1Block` 0x4200…0015, registry in L1-blob mode on the existing `ZiskVerifier` and release pins, no DKG/Council, `forge script` without `--broadcast` first, `verify_deployment.py --l1-block`, preset and `docs/deployments.md`, beacon and send-endpoint probes). §8 phases 3 and 4: Tasks 1 to 3 and 4 to 8; the report, docs and the Fable gate are Task 8.
- **Placeholder scan.** Every code step carries its code, compiled and linted in a scratch copy of this tree while writing (each task's state built on its own: Task 1; +2; +3; +5; +6), and the L1 and blob setup phases ran green there. The bracketed values in Task 4 Step 9, Task 7 Step 4 and Task 8 Step 2 are run outputs (addresses, dates, measured numbers) with their sources named; the scripts in Task 4 Steps 11 and 12 fill the preset and the deployments page from `deployments/10.json` directly.
- **Type consistency.** `L1Net {rpc, send_rpc, keys, verifier, l1_block, max_post_fee, min_balance, anvil}`, `L1Anvil {relay, anvil, block_time, lag}`, `PendingTx {hash, max_fee, tip, blob_fee}`, `L1Post {pid, op_tx, op_block, l1_blocks, vhs, l1_txs}`, `Deployment.{l1_blob_verifier, l1_block}`, `NodeConfig.{l1_rpc_url, l1_send_rpc_url, l1_privkey_file, l1_max_post_fee}`, `Anvil::start_with(log, block_time, chain_id)`, `l1::posts(op_rpc, l1_rpc, registry, from)`, `kill_after_post(nodes, cfgs, bin, net, org, reader, (p1, tree), prover)` and the negative check names are used with the same names and types in Tasks 1 to 8.
- **Review Focus.** Each of the five lines names its test in the owning task: Task 4 Step 2 probes and Task 5 Step 8 / Task 7 Step 3 first-post and posts-per-row checks; Task 4 Step 2 beacon probe and `op_live`'s observer assertion; Task 5 Step 7 and Task 6 Step 4 zero-cost guards; Task 3 `kill_after_post`; Task 4 Step 8, Task 5 Step 9, Task 7 Step 4 scans and `live_keys_are_dedicated`.
