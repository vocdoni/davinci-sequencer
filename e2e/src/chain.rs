//! anvil and the contract deployment.

use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, Bytes};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log, TransactionReceipt, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolEvent, SolValue};
use anyhow::{Context, Result, bail, ensure};
use davinci_client::organizer::ProcessRegistry;
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;

/// anvil's default dev accounts 0..=9 (public test keys). Account 0 deploys
/// and organizes; 1..=3 sign the nodes' settlements; 4 owns the census
/// contract (its own nonces, apart from the organizer's cached ones); 5..=7
/// are the DKG operators and 8 deploys the DKG (`crate::dkg`). In L1-blob
/// mode 9 moves the OP anvil's `MockL1Block` and, on the Ethereum anvil,
/// 5..=7 are the sequencers' blob keys (`crate::l1`).
pub const DEV_KEYS: [&str; 10] = [
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    "47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    "8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba",
    "92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e",
    "4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356",
    "dbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97",
    "2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
];

sol! {
    #[sol(rpc)]
    interface IRootC {
        function getRootCVadcopFinal() external pure returns (bytes32);
    }
}

/// A foundry binary: `~/.foundry/bin/<name>` if present, else from `PATH`.
pub fn foundry_tool(name: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".foundry/bin").join(name);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(name)
}

/// A loopback port nobody listens on right now.
pub fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub fn signer(i: usize) -> Result<PrivateKeySigner> {
    let k = DEV_KEYS.get(i).context("no such dev key")?;
    Ok(k.parse()?)
}

/// The ballot VK hash the registry pins (0x07 leaf).
pub fn ballot_vk_hash() -> Result<[u8; 32]> {
    Ok(BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?.vk_hash())
}

/// anvil with Osaka and 1 s blocks (or `block_time`); its output goes to
/// `log`. Killed on drop.
pub struct Anvil {
    child: Child,
    pub url: String,
    pub log: PathBuf,
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Anvil {
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
            .args(["--port", &port.to_string()])
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .context("spawn anvil")?;
        let mut anvil = Anvil {
            child,
            url: format!("http://127.0.0.1:{port}"),
            log: log.to_path_buf(),
        };
        let provider = ProviderBuilder::new().connect_client(rpc(&anvil.url)?);
        let start = Instant::now();
        loop {
            if provider.get_chain_id().await.is_ok() {
                break;
            }
            if let Some(st) = anvil.child.try_wait()? {
                bail!("anvil exited with {st}; log {}", log.display());
            }
            if start.elapsed() > Duration::from_secs(30) {
                bail!("anvil not up after 30 s; log {}", log.display());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // The dev keys must be the funded accounts of this anvil.
        for i in 0..DEV_KEYS.len() {
            let a = signer(i)?.address();
            let bal = provider.get_balance(a).await?;
            ensure!(!bal.is_zero(), "dev account {i} ({a}) has no balance");
        }
        Ok(anvil)
    }
}

/// Where the forge project lives: `DAVINCI_CONTRACTS_DIR`, default
/// `../davinci-contracts` next to the workspace.
pub fn contracts_dir() -> PathBuf {
    std::env::var_os("DAVINCI_CONTRACTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-contracts")
        })
}

pub fn forge_build(dir: &Path) -> Result<()> {
    let out = Command::new(foundry_tool("forge"))
        .arg("build")
        .current_dir(dir)
        .output()
        .context("run forge build")?;
    ensure!(
        out.status.success(),
        "forge build failed in {}:\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

pub(crate) fn bytecode(dir: &Path, artifact: &str) -> Result<Vec<u8>> {
    let p = dir.join("out").join(artifact);
    let j: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&p).with_context(|| p.display().to_string())?)?;
    let hex = j["bytecode"]["object"]
        .as_str()
        .with_context(|| format!("{}: no bytecode", p.display()))?;
    Ok(hex::decode(hex.trim_start_matches("0x"))?)
}

pub(crate) async fn deploy_code(provider: &DynProvider, code: Vec<u8>) -> Result<Address> {
    let r = deploy_receipt(provider, code).await?;
    r.contract_address.context("no contract address")
}

pub(crate) async fn deploy_receipt(
    provider: &DynProvider,
    code: Vec<u8>,
) -> Result<TransactionReceipt> {
    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(code));
    let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
    ensure!(receipt.status(), "deployment reverted");
    Ok(receipt)
}

#[derive(Clone, Debug)]
pub struct Deployment {
    pub chain_id: u64,
    pub verifier: Address,
    pub registry: Address,
    /// The registry's `DavinciDKGAdapter`; zero without a `dkg_manager`.
    pub dkg_adapter: Address,
    /// L1-blob mode: the registry's `L1BlobVerifier` and the `MockL1Block`
    /// it reads OP's origin from; zero in blob mode.
    pub l1_blob_verifier: Address,
    pub l1_block: Address,
}

/// Grace immutables of the anvil registry, in seconds: `defaultGrace,
/// graceFloor, graceCeil, graceMaxTotal, noticeMin`. The floor bounds every
/// node's round budget, so it fits one small batch's proof and settlement. The
/// default covers the first landing after an END that three racing nodes flush
/// through one GPU: the losers' proofs still queue ahead, up to five jobs.
pub const GRACE_ARGS: (u32, u32, u32, u32, u32) = (120, 40, 180, 240, 5);

/// The anvil registry's grace args: `DAVINCI_E2E_GRACE`
/// (`default,floor,ceil,max_total,notice`, seconds; `300,240,600,1800,60` is
/// the OP Mainnet registry's), else [`GRACE_ARGS`].
pub fn grace_args() -> Result<(u32, u32, u32, u32, u32)> {
    let Ok(v) = std::env::var("DAVINCI_E2E_GRACE") else {
        return Ok(GRACE_ARGS);
    };
    let n: Vec<u32> = v
        .split(',')
        .map(|x| x.trim().parse())
        .collect::<std::result::Result<_, _>>()
        .context("DAVINCI_E2E_GRACE")?;
    let [default, floor, ceil, max_total, notice] = n[..] else {
        bail!("DAVINCI_E2E_GRACE: default,floor,ceil,max_total,notice");
    };
    Ok((default, floor, ceil, max_total, notice))
}

/// Deploys `ZiskVerifier` (a `PlonkVerifier`) and `ProcessRegistry` pinned to
/// the release vks, the ballot VK hash and [`grace_args`], with `dkg_manager`
/// (zero disables the DKG key modes), then reads the pins back.
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
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer(0)?))
        .connect_client(rpc(url)?)
        .erased();
    let chain_id = provider.get_chain_id().await?;

    let verifier = deploy_code(
        &provider,
        bytecode(dir, "ZiskVerifier.sol/ZiskVerifier.json")?,
    )
    .await
    .context("deploy ZiskVerifier")?;
    let root_c = IRootC::new(verifier, &provider)
        .getRootCVadcopFinal()
        .call()
        .await?;
    ensure!(
        root_c == B256::from(release::ROOT_C_VADCOP_FINAL),
        "the verifier's rootCVadcopFinal {root_c} is not the release pin"
    );

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

    let vk_hash = B256::from(ballot_vk_hash()?);
    let grace = grace_args()?;
    let args = (
        u32::try_from(chain_id)?,
        verifier,
        B256::from(release::BATCH_PROGRAM_VK),
        B256::from(release::RESULTS_PROGRAM_VK),
        B256::from(release::ROOT_C_VADCOP_FINAL),
        vk_hash,
        dkg_manager,
        Address::ZERO, // _councilManager: no Council mode
    )
        .abi_encode_params();
    let mut code = bytecode(dir, "ProcessRegistry.sol/ProcessRegistry.json")?;
    code.extend_from_slice(&args);
    code.extend_from_slice(&grace.abi_encode_params());
    code.extend_from_slice(&l1_blob_verifier.abi_encode()); // zero: blob mode
    let registry = deploy_code(&provider, code)
        .await
        .context("deploy ProcessRegistry")?;

    let r = ProcessRegistry::new(registry, &provider);
    ensure!(r.batchProgramVK().call().await? == B256::from(release::BATCH_PROGRAM_VK));
    ensure!(r.resultsProgramVK().call().await? == B256::from(release::RESULTS_PROGRAM_VK));
    ensure!(r.rootCVadcopFinal().call().await? == B256::from(release::ROOT_C_VADCOP_FINAL));
    ensure!(r.ballotVKHash().call().await? == vk_hash);
    ensure!(r.ziskVerifier().call().await? == verifier);
    ensure!(
        (
            r.defaultGrace().call().await?,
            r.graceFloor().call().await?,
            r.graceCeil().call().await?,
            r.graceMaxTotal().call().await?,
            r.noticeMin().call().await?,
        ) == grace
    );
    ensure!(r.l1BlobVerifier().call().await? == l1_blob_verifier);
    let dkg_adapter = r.dkgAdapter().call().await?;
    ensure!(
        (dkg_adapter == Address::ZERO) == (dkg_manager == Address::ZERO),
        "dkgAdapter {dkg_adapter} for dkgManager {dkg_manager}"
    );
    Ok(Deployment {
        chain_id,
        verifier,
        registry,
        dkg_adapter,
        l1_blob_verifier,
        l1_block,
    })
}

/// `User-Agent` of the harness's RPC requests (publicnode answers 403
/// without one).
pub const USER_AGENT: &str = concat!("davinci-e2e/", env!("CARGO_PKG_VERSION"));

/// The RPC client under every harness provider: [`USER_AGENT`] and
/// rate-limit backoff.
pub fn rpc(url: &str) -> Result<alloy::rpc::client::RpcClient> {
    Ok(davinci_client::organizer::rpc_client(url, USER_AGENT)?)
}

/// Blocks per `eth_getLogs` page: public RPCs refuse wide ranges.
pub const LOG_PAGE: u64 = 5_000;

/// Registry logs with topic0 `sig` in `[from, latest]`, paged.
pub async fn registry_logs(url: &str, registry: Address, sig: B256, from: u64) -> Result<Vec<Log>> {
    let provider = ProviderBuilder::new().connect_client(rpc(url)?);
    let head = provider.get_block_number().await?;
    let mut out = Vec::new();
    let mut start = from;
    while start <= head {
        let end = (start + LOG_PAGE - 1).min(head);
        let filter = Filter::new()
            .address(registry)
            .event_signature(sig)
            .from_block(start)
            .to_block(end);
        out.extend(
            provider
                .get_logs(&filter)
                .await
                .with_context(|| format!("eth_getLogs {start}..{end}"))?,
        );
        start = end + 1;
    }
    Ok(out)
}

/// A settled registry transaction: a state transition or the results.
#[derive(Clone, Debug)]
pub struct RegistryTx {
    pub pid: [u8; 31],
    /// From the event; zero for a DKG request, whose event has none.
    pub sender: Address,
    pub tx: B256,
    pub block: u64,
    /// Blobs of a transition (0 for results).
    pub n_blobs: u64,
}

fn registry_tx(l: &Log, pid: [u8; 31], sender: Address, n_blobs: u64) -> Result<RegistryTx> {
    Ok(RegistryTx {
        pid,
        sender,
        tx: l.transaction_hash.context("log without tx hash")?,
        block: l.block_number.context("log without block")?,
        n_blobs,
    })
}

/// Every `ProcessStateTransitioned` from block `from`, in chain order.
pub async fn transitions(url: &str, registry: Address, from: u64) -> Result<Vec<RegistryTx>> {
    use ProcessRegistry::ProcessStateTransitioned as Ev;
    registry_logs(url, registry, Ev::SIGNATURE_HASH, from)
        .await?
        .iter()
        .map(|l| {
            let d = l.log_decode::<Ev>()?.inner.data;
            registry_tx(l, d.processId.0, d.sender, u64::try_from(d.nBlobs)?)
        })
        .collect()
}

/// Every `ResultsDecryptionRequested` from block `from`, in chain order.
pub async fn dkg_requests(url: &str, registry: Address, from: u64) -> Result<Vec<RegistryTx>> {
    use ProcessRegistry::ResultsDecryptionRequested as Ev;
    registry_logs(url, registry, Ev::SIGNATURE_HASH, from)
        .await?
        .iter()
        .map(|l| {
            let d = l.log_decode::<Ev>()?.inner.data;
            registry_tx(l, d.processId.0, Address::ZERO, 0)
        })
        .collect()
}

/// Every `ProcessResultsSet` from block `from`, in chain order.
pub async fn results_txs(url: &str, registry: Address, from: u64) -> Result<Vec<RegistryTx>> {
    use ProcessRegistry::ProcessResultsSet as Ev;
    registry_logs(url, registry, Ev::SIGNATURE_HASH, from)
        .await?
        .iter()
        .map(|l| {
            let d = l.log_decode::<Ev>()?.inner.data;
            registry_tx(l, d.processId.0, d.sender, 0)
        })
        .collect()
}
