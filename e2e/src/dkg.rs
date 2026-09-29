//! A davinci-dkg committee on the run's anvil (`DAVINCI_E2E_DKG=1`): the DKG
//! contracts, three `davinci-dkg-node` operators and one Live epoch. Live,
//! the committee is external: the manager of the registry's adapter, or
//! `DAVINCI_E2E_DKG_MANAGER`, and [`check_wiring`] checks it serves the
//! registry.
//!
//! Env: `DAVINCI_DKG_DIR` (the davinci-dkg checkout, default
//! `../davinci-dkg`), `DAVINCI_E2E_DKG_NODE_BIN` (a prebuilt node; else `go
//! build` into `~/.cache/davinci-e2e/dkg`), `DAVINCI_E2E_DKG_FORGE_DIR`
//! (forge `out/` and `cache/`, kept out of the checkout; default
//! `~/.cache/dkg-forge`), `DAVINCI_E2E_DKG_ARTIFACTS` (the nodes' shared
//! circuit artifacts, default `~/.cache/davinci-dkg-artifacts`).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolEvent, SolValue};
use anyhow::{Context, Result, bail, ensure};

use crate::chain::{self, DEV_KEYS};
use crate::net;
use crate::wait;

sol! {
    #[sol(rpc)]
    interface IDKGRegistry {
        function setManager(address manager) external;
        function isActive(address operator) external view returns (bool);
        function activeCount() external view returns (uint64);
    }

    #[sol(rpc)]
    #[allow(clippy::too_many_arguments)]
    interface IDKGManager {
        struct EpochPolicy {
            uint16 threshold;
            uint16 committeeSize;
            uint16 minValidContributions;
            uint16 lotteryAlphaBps;
            uint64 committeeSelectionDeadlineBlock;
            uint64 keyAssemblyDeadlineBlock;
            uint64 liveNotBeforeBlock;
        }
        struct Epoch {
            address organizer;
            EpochPolicy policy;
            uint8 status;
            uint64 nonce;
            uint64 startBlock;
            uint64 seedBlock;
            bytes32 seed;
            uint256 lotteryThreshold;
            uint16 claimedCount;
            uint16 contributionCount;
            uint16 partialDecryptionCount;
            uint16 ciphertextCount;
        }
        event EpochCreated(bytes12 indexed epochId, address indexed organizer, uint64 startBlock, uint64 seedBlock, uint256 lotteryThreshold);
        event SlotClaimed(bytes12 indexed epochId, address indexed claimer, uint16 slot);
        event ContributionSubmitted(bytes12 indexed epochId, address indexed contributor, uint16 contributorIndex, bytes32 commitmentsHash, bytes32 encryptedSharesHash);
        event CiphertextSubmitted(bytes12 indexed epochId, bytes32 indexed aid, uint16 indexed ciphertextIndex, address submitter, uint256 c1x, uint256 c1y, uint256 c2x, uint256 c2y);
        event PartialDecryptionSubmitted(bytes12 indexed epochId, bytes32 indexed aid, address indexed participant, uint16 participantIndex, uint16 ciphertextIndex, uint256 deltaX, uint256 deltaY);
        function setAppManager(address a) external;
        function appManager() external view returns (address);
        function createEpoch(uint16 threshold, uint16 committeeSize, uint16 minValidContributions, uint16 lotteryAlphaBps) external returns (bytes12);
        function getEpoch(bytes12 epochId) external view returns (Epoch memory);
    }

    #[sol(rpc)]
    #[allow(clippy::too_many_arguments)]
    interface IDKGAppManager {
        struct AppPolicy {
            uint8 mode;
            bool openSubmission;
            address[] submitters;
            uint16 maxCiphertexts;
            uint64 notBeforeBlock;
            uint64 notAfterBlock;
            uint64 decryptNotBefore;
            uint64 decryptNotAfter;
        }
        function registerApplication(bytes12 epochId, bytes32 aid, AppPolicy calldata policy, uint256 pkOrgX, uint256 pkOrgY, uint256 schnorrAx, uint256 schnorrAy, uint256 schnorrZ) external;
        function getApplicationKey(bytes12 epochId, bytes32 aid) external view returns (uint256 x, uint256 y);
        function getOrganizerPK(bytes12 epochId, bytes32 aid) external view returns (uint256, uint256);
    }

    #[sol(rpc)]
    interface IDavinciDKGAdapter {
        function registry() external view returns (address);
        function manager() external view returns (address);
        function appManager() external view returns (address);
        function registrationEpoch() external view returns (bytes12);
    }
}

/// Dev accounts of the three operators and of the DKG deployer.
pub const OPERATORS: [usize; 3] = [5, 6, 7];
pub const DEPLOYER: usize = 8;

/// The fast test config on 1 s blocks.
pub const EPOCH_DURATION_BLOCKS: u64 = 17_280;
pub const COMMITTEE_SELECTION_BLOCKS: u64 = 8;
pub const KEY_ASSEMBLY_BLOCKS: u64 = 12;
pub const FINALIZE_GAP_BLOCKS: u64 = 1;
pub const INACTIVITY_WINDOW: u64 = 50_400;
pub const MIN_THRESHOLD: u16 = 2;
pub const MIN_COMMITTEE_SIZE: u16 = 3;
pub const MAX_LOTTERY_ALPHA_BPS: u16 = 20_000;

/// `createEpoch(2, 3, 2, 10000)`: with all 3 operators active every one of
/// them wins the lottery.
pub const THRESHOLD: u16 = 2;
pub const COMMITTEE_SIZE: u16 = 3;
pub const MIN_VALID_CONTRIBUTIONS: u16 = 2;
pub const LOTTERY_ALPHA_BPS: u16 = 10_000;

/// `DKGTypes.EpochPhase`.
const PHASE_SELECTION: u8 = 1;
const PHASE_ASSEMBLY: u8 = 2;
const PHASE_LIVE: u8 = 3;
const PHASE_ABORTED: u8 = 4;

pub type EpochId = FixedBytes<12>;

pub fn enabled() -> bool {
    std::env::var("DAVINCI_E2E_DKG").as_deref() == Ok("1")
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME")?))
}

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The davinci-dkg checkout: `DAVINCI_DKG_DIR`, default `../davinci-dkg`
/// next to the workspace.
pub fn dkg_dir() -> PathBuf {
    env_path("DAVINCI_DKG_DIR")
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-dkg"))
}

fn forge_dir() -> Result<PathBuf> {
    match env_path("DAVINCI_E2E_DKG_FORGE_DIR") {
        Some(p) => Ok(p),
        None => Ok(home()?.join(".cache/dkg-forge")),
    }
}

fn artifacts_dir() -> Result<PathBuf> {
    match env_path("DAVINCI_E2E_DKG_ARTIFACTS") {
        Some(p) => Ok(p),
        None => Ok(home()?.join(".cache/davinci-dkg-artifacts")),
    }
}

/// `forge build` of the DKG contracts with its output in the forge dir, so
/// the checkout stays clean. A no-op when nothing changed.
pub fn forge_build() -> Result<PathBuf> {
    let src = dkg_dir().join("solidity");
    let out = forge_dir()?;
    let o = Command::new(chain::foundry_tool("forge"))
        .args(["build", "--skip", "test", "--skip", "script"])
        .current_dir(&src)
        .env("FOUNDRY_OUT", out.join("out"))
        .env("FOUNDRY_CACHE_PATH", out.join("cache"))
        .env("FOUNDRY_LINT_LINT_ON_BUILD", "false")
        .output()
        .context("run forge build")?;
    ensure!(
        o.status.success(),
        "forge build failed in {}:\n{}",
        src.display(),
        String::from_utf8_lossy(&o.stderr)
    );
    Ok(out)
}

/// The node binary: `DAVINCI_E2E_DKG_NODE_BIN`, or `go build` of the
/// checkout into `~/.cache/davinci-e2e/dkg` (cached by go).
pub fn node_bin() -> Result<PathBuf> {
    if let Some(p) = env_path("DAVINCI_E2E_DKG_NODE_BIN") {
        ensure!(
            p.is_file(),
            "DAVINCI_E2E_DKG_NODE_BIN {} is not a file",
            p.display()
        );
        return Ok(p);
    }
    let out = home()?.join(".cache/davinci-e2e/dkg");
    std::fs::create_dir_all(&out)?;
    let bin = out.join("davinci-dkg-node");
    let o = Command::new("go")
        .arg("build")
        .arg("-o")
        .arg(&bin)
        .arg("./cmd/davinci-dkg-node")
        .current_dir(dkg_dir())
        .output()
        .context("run go build (is go on PATH?)")?;
    ensure!(
        o.status.success(),
        "go build davinci-dkg-node failed in {}:\n{}",
        dkg_dir().display(),
        String::from_utf8_lossy(&o.stderr)
    );
    Ok(bin)
}

#[derive(Clone, Debug)]
pub struct DkgDeployment {
    pub manager: Address,
    pub app_manager: Address,
    pub registry: Address,
}

/// Deploys the DKG like `DeployAll.s.sol` from `provider`'s account, from the
/// forge artifacts in `forge_dir`, and checks the links.
pub async fn deploy(provider: &DynProvider, forge_dir: &Path) -> Result<DkgDeployment> {
    let chain_id = u32::try_from(provider.get_chain_id().await?)?;
    let code = |name: &str| chain::bytecode(forge_dir, &format!("{name}.sol/{name}.json"));
    let mut verifiers = Vec::new();
    for v in [
        "ContributionVerifier",
        "PartialDecryptVerifier",
        "FinalizeVerifier",
        "DecryptCombineVerifier",
    ] {
        let a = chain::deploy_code(provider, code(v)?)
            .await
            .with_context(|| format!("deploy {v}"))?;
        verifiers.push(a);
    }
    let mut c = code("DKGRegistry")?;
    c.extend((INACTIVITY_WINDOW,).abi_encode_params());
    let registry = chain::deploy_code(provider, c)
        .await
        .context("deploy DKGRegistry")?;
    let args = (
        chain_id,
        registry,
        verifiers[0],
        verifiers[1],
        verifiers[2],
        verifiers[3],
        U256::from(EPOCH_DURATION_BLOCKS),
        U256::from(COMMITTEE_SELECTION_BLOCKS),
        U256::from(KEY_ASSEMBLY_BLOCKS),
        U256::from(FINALIZE_GAP_BLOCKS),
        MIN_THRESHOLD,
        MIN_COMMITTEE_SIZE,
        MAX_LOTTERY_ALPHA_BPS,
    )
        .abi_encode_params();
    let mut c = code("DKGManager")?;
    c.extend(args);
    let manager = chain::deploy_code(provider, c)
        .await
        .context("deploy DKGManager")?;
    let r = IDKGRegistry::new(registry, provider)
        .setManager(manager)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "DKGRegistry.setManager reverted");
    let mut c = code("DKGAppManager")?;
    c.extend((manager,).abi_encode_params());
    let app_manager = chain::deploy_code(provider, c)
        .await
        .context("deploy DKGAppManager")?;
    let m = IDKGManager::new(manager, provider);
    let r = m
        .setAppManager(app_manager)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "DKGManager.setAppManager reverted");
    ensure!(m.appManager().call().await? == app_manager);
    Ok(DkgDeployment {
        manager,
        app_manager,
        registry,
    })
}

/// The node's command line: every setting as a flag, the key only in the
/// environment (never argv).
fn node_command(
    bin: &Path,
    key: &str,
    rpc: &str,
    manager: Address,
    datadir: &Path,
    artifacts: &Path,
    home: &Path,
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("DAVINCI_DKG_PRIVKEY", key)
        .env("DAVINCI_DKG_ARTIFACTS_DIR", artifacts)
        .arg(format!("--web3.rpc={rpc}"))
        .arg(format!("--manager={manager}"))
        .arg(format!("--datadir={}", datadir.display()))
        .args(["--poll-interval=1s", "--auto-create-epochs=false"])
        .arg("--log.level=info");
    cmd
}

/// A running `davinci-dkg-node`; killed on drop.
pub struct DkgNode {
    pub name: String,
    pub address: Address,
    pub log: PathBuf,
    child: Mutex<Child>,
}

impl Drop for DkgNode {
    fn drop(&mut self) {
        if let Ok(c) = self.child.get_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl DkgNode {
    /// Operator `i` (dev account `OPERATORS[i]`), datadir and log in `dir`.
    fn spawn(bin: &Path, i: usize, rpc: &str, manager: Address, dir: &Path) -> Result<DkgNode> {
        let name = format!("dkg{}", i + 1);
        let key = DEV_KEYS[OPERATORS[i]];
        let address = chain::signer(OPERATORS[i])?.address();
        let datadir = dir.join(format!("{name}-data"));
        std::fs::create_dir_all(&datadir)?;
        let log = dir.join(format!("{name}.log"));
        let out = File::options().create(true).append(true).open(&log)?;
        let child = node_command(bin, key, rpc, manager, &datadir, &artifacts_dir()?, dir)
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;
        Ok(DkgNode {
            name,
            address,
            log,
            child: Mutex::new(child),
        })
    }

    pub fn check_alive(&self) -> Result<()> {
        let mut c = self.child.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        if let Some(st) = c.try_wait()? {
            bail!(
                "DKG node {} exited with {st}; log {}",
                self.name,
                self.log.display()
            );
        }
        Ok(())
    }
}

/// The DKG contracts and three registered operators on anvil. Dropping it
/// kills the nodes; their logs stay in the run's work dir.
pub struct DkgStack {
    pub deployment: DkgDeployment,
    pub nodes: Vec<DkgNode>,
    /// The deployer's provider (dev account `DEPLOYER`).
    pub provider: DynProvider,
}

impl DkgStack {
    /// Builds the node and the contracts, deploys, starts the operators and
    /// waits until all three are registered. Node 1 starts alone first, so
    /// a cold artifacts cache is downloaded once.
    pub async fn start(rpc: &str, dir: &Path) -> Result<DkgStack> {
        let t = Instant::now();
        let bin = tokio::task::spawn_blocking(node_bin).await??;
        let forge = tokio::task::spawn_blocking(forge_build).await??;
        let signer: PrivateKeySigner = chain::signer(DEPLOYER)?;
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_client(chain::rpc(rpc)?)
            .erased();
        let deployment = deploy(&provider, &forge).await?;
        eprintln!(
            "dkg: manager {} app manager {} registry {} ({:.1} s)",
            deployment.manager,
            deployment.app_manager,
            deployment.registry,
            t.elapsed().as_secs_f64()
        );
        let mut stack = DkgStack {
            deployment,
            nodes: Vec::new(),
            provider,
        };
        let t = Instant::now();
        for i in 0..OPERATORS.len() {
            stack
                .nodes
                .push(DkgNode::spawn(&bin, i, rpc, stack.deployment.manager, dir)?);
            if i == 0 {
                // Artifacts load (or download) before the node registers.
                stack
                    .wait_registered(1, net::scaled(Duration::from_secs(10 * 60)))
                    .await?;
            }
        }
        stack
            .wait_registered(OPERATORS.len(), net::scaled(Duration::from_secs(3 * 60)))
            .await?;
        eprintln!(
            "dkg: {} operators registered in {:.1} s",
            stack.nodes.len(),
            t.elapsed().as_secs_f64()
        );
        Ok(stack)
    }

    pub fn check_alive(&self) -> Result<()> {
        self.nodes.iter().try_for_each(DkgNode::check_alive)
    }

    /// Waits until the first `n` nodes are active operators.
    async fn wait_registered(&self, n: usize, timeout: Duration) -> Result<()> {
        let reg = IDKGRegistry::new(self.deployment.registry, &self.provider);
        wait::until(
            &format!("{n} DKG operators"),
            timeout,
            Duration::from_secs(1),
            || async {
                self.check_alive()?;
                for node in &self.nodes[..n] {
                    if !reg.isActive(node.address).call().await? {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            },
        )
        .await?;
        let active = reg.activeCount().call().await?;
        ensure!(
            active == n as u64,
            "{active} active DKG operators, expected {n}"
        );
        Ok(())
    }

    /// `createEpoch(2, 3, 2, 10000)` from the deployer.
    pub async fn create_epoch(&self) -> Result<EpochId> {
        let m = IDKGManager::new(self.deployment.manager, &self.provider);
        let r = m
            .createEpoch(
                THRESHOLD,
                COMMITTEE_SIZE,
                MIN_VALID_CONTRIBUTIONS,
                LOTTERY_ALPHA_BPS,
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        ensure!(r.status(), "createEpoch reverted");
        let ev = r
            .decoded_log::<IDKGManager::EpochCreated>()
            .context("no EpochCreated in the createEpoch receipt")?;
        Ok(ev.data.epochId)
    }

    /// Waits for `epoch` to go Live. Fails early when a node dies, the
    /// epoch aborts, or a phase deadline passes short of what it needs.
    pub async fn wait_live(&self, epoch: EpochId) -> Result<()> {
        let m = IDKGManager::new(self.deployment.manager, &self.provider);
        let t = Instant::now();
        wait::until(
            &format!("DKG epoch {epoch} Live"),
            net::scaled(Duration::from_secs(5 * 60)),
            Duration::from_secs(1),
            || async {
                self.check_alive()?;
                let e = m.getEpoch(epoch).call().await?;
                let head = self.provider.get_block_number().await?;
                let p = &e.policy;
                match e.status {
                    PHASE_LIVE => Ok(Some(())),
                    PHASE_ABORTED => bail!("DKG epoch {epoch} aborted"),
                    PHASE_SELECTION if head > p.committeeSelectionDeadlineBlock => bail!(
                        "DKG epoch {epoch}: {} of {} slots claimed by block {}",
                        e.claimedCount,
                        p.committeeSize,
                        p.committeeSelectionDeadlineBlock
                    ),
                    PHASE_ASSEMBLY
                        if head > p.keyAssemblyDeadlineBlock
                            && e.contributionCount < p.minValidContributions =>
                    {
                        bail!(
                            "DKG epoch {epoch}: {} of {} contributions by block {}",
                            e.contributionCount,
                            p.minValidContributions,
                            p.keyAssemblyDeadlineBlock
                        )
                    }
                    _ => Ok(None),
                }
            },
        )
        .await?;
        let e = m.getEpoch(epoch).call().await?;
        let p = &e.policy;
        let claims = self
            .last_event::<IDKGManager::SlotClaimed>(e.startBlock, |l| l.epochId == epoch)
            .await?;
        let contribs = self
            .last_event::<IDKGManager::ContributionSubmitted>(e.startBlock, |l| l.epochId == epoch)
            .await?;
        eprintln!(
            "dkg: epoch {epoch} Live in {:.1} s: start block {}, last claim {claims} \
             (deadline {}), last contribution {contribs} (deadline {}), live not before {}, \
             head {}",
            t.elapsed().as_secs_f64(),
            e.startBlock,
            p.committeeSelectionDeadlineBlock,
            p.keyAssemblyDeadlineBlock,
            p.liveNotBeforeBlock,
            self.provider.get_block_number().await?
        );
        Ok(())
    }

    /// Block of the last manager event `E` since `from` that `keep` accepts
    /// (0 if none): shows how close a phase came to its deadline.
    async fn last_event<E: SolEvent>(&self, from: u64, keep: impl Fn(&E) -> bool) -> Result<u64> {
        let f = Filter::new()
            .address(self.deployment.manager)
            .event_signature(E::SIGNATURE_HASH)
            .from_block(from);
        let mut last = 0;
        for l in self.provider.get_logs(&f).await? {
            if keep(&l.log_decode::<E>()?.inner.data) {
                last = last.max(l.block_number.unwrap_or(0));
            }
        }
        Ok(last)
    }
}

/// How the registry reaches the committee, checked by [`check_wiring`].
#[derive(Clone, Copy, Debug)]
pub struct DkgWiring {
    pub manager: Address,
    pub app_manager: Address,
    pub adapter: Address,
    /// `adapter.registrationEpoch()` when checked: Live, with a free pool key.
    pub epoch: EpochId,
}

/// The registry's DKG adapter; an error if it has none.
async fn registry_adapter<P: Provider>(p: &P, registry: Address) -> Result<Address> {
    let adapter = davinci_client::organizer::ProcessRegistry::new(registry, p)
        .dkgAdapter()
        .call()
        .await?;
    ensure!(
        adapter != Address::ZERO,
        "registry {registry} has no DKG adapter (deployed without dkgManager)"
    );
    Ok(adapter)
}

/// The DKGManager the registry's adapter registers on.
pub async fn registry_manager(rpc: &str, registry: Address) -> Result<Address> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    let adapter = registry_adapter(&p, registry).await?;
    Ok(IDavinciDKGAdapter::new(adapter, &p)
        .manager()
        .call()
        .await?)
}

/// The Live epoch with a free pool key a new application would take now.
pub async fn registration_epoch(rpc: &str, adapter: Address) -> Result<EpochId> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    Ok(IDavinciDKGAdapter::new(adapter, &p)
        .registrationEpoch()
        .call()
        .await?)
}

/// Longest wait for the committee's next epoch once its pool is spent: its
/// nodes open one on their own, Live about two minutes later on Gnosis.
const EPOCH_WAIT: Duration = Duration::from_secs(5 * 60);

/// Waits (timeouts scaled) until `adapter` has a Live epoch with a free pool
/// key and returns it.
pub async fn wait_registration_epoch(rpc: &str, adapter: Address) -> Result<EpochId> {
    wait::until(
        "a Live DKG epoch with a free pool key",
        net::scaled(EPOCH_WAIT),
        Duration::from_secs(5),
        || async { Ok(registration_epoch(rpc, adapter).await.ok()) },
    )
    .await
}

/// A DKG-mode `newProcess` refused only because no Live epoch has a free
/// pool key: the pool is spent and the next epoch is not Live yet.
/// `PoolExhausted` is the last key taken under a create, after the client's
/// own retry.
pub fn no_free_pool_key(e: &davinci_client::Error) -> bool {
    matches!(e, davinci_client::Error::Reverted(n) if n == "NoLiveEpoch" || n == "PoolExhausted")
}

/// Fails unless `registry` has an adapter on `manager` wired to its app
/// manager, and the adapter has a Live epoch to register in.
pub async fn check_wiring(rpc: &str, registry: Address, manager: Address) -> Result<DkgWiring> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    let adapter = registry_adapter(&p, registry).await?;
    let a = IDavinciDKGAdapter::new(adapter, &p);
    ensure!(a.registry().call().await? == registry, "adapter.registry");
    ensure!(
        a.manager().call().await? == manager,
        "adapter {adapter} is on another DKG manager than {manager}"
    );
    let app_manager = IDKGManager::new(manager, &p).appManager().call().await?;
    ensure!(
        a.appManager().call().await? == app_manager,
        "adapter.appManager"
    );
    let epoch = a
        .registrationEpoch()
        .call()
        .await
        .context("adapter.registrationEpoch (no Live epoch with a free pool key?)")?;
    ensure!(epoch != EpochId::ZERO, "adapter.registrationEpoch is zero");
    let e = IDKGManager::new(manager, &p).getEpoch(epoch).call().await?;
    ensure!(e.status == PHASE_LIVE, "epoch {epoch} is not Live");
    Ok(DkgWiring {
        manager,
        app_manager,
        adapter,
        epoch,
    })
}

/// The on-chain key of a DKG process must be the committee's application
/// key, and a locked one's organizer key must be `secret`'s.
pub async fn check_process_key(
    rpc: &str,
    w: &DkgWiring,
    p: &davinci_client::organizer::OnchainProcess,
    secret: Option<&davinci_client::organizer::OrganizerSecret>,
) -> Result<()> {
    use davinci_zkvm_sdk::dkg::{point_from_rte, point_to_rte};
    let d = p.dkg.context("not a DKG process")?;
    ensure!(d.locked == secret.is_some(), "locked flag {}", d.locked);
    let prov = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    let am = IDKGAppManager::new(w.app_manager, &prov);
    let (eid, aid) = (FixedBytes(d.epoch_id), FixedBytes(d.aid));
    let k = am.getApplicationKey(eid, aid).call().await?;
    let key = point_from_rte(&k.x.to_be_bytes(), &k.y.to_be_bytes())?;
    ensure!(
        key == p.encryption_key,
        "encryption key is not the DKG application key"
    );
    if let Some(sk) = secret {
        let o = am.getOrganizerPK(eid, aid).call().await?;
        let want = point_to_rte(&sk.public_key());
        ensure!(
            (o._0.to_be_bytes(), o._1.to_be_bytes()) == want,
            "organizer key on the DKG is not sk·G"
        );
    }
    Ok(())
}

/// `registerApplication` calldata for an automatic-mode application `aid`
/// (nonzero, below the BN254 scalar field) in `epoch`.
pub fn register_app_call(epoch: EpochId, aid: FixedBytes<32>) -> Vec<u8> {
    use alloy::sol_types::SolCall;
    IDKGAppManager::registerApplicationCall {
        epochId: epoch,
        aid,
        policy: IDKGAppManager::AppPolicy {
            mode: 1,
            openSubmission: true,
            submitters: Vec::new(),
            maxCiphertexts: 1,
            notBeforeBlock: 0,
            notAfterBlock: 0,
            decryptNotBefore: 0,
            decryptNotAfter: 0,
        },
        pkOrgX: U256::ZERO,
        pkOrgY: U256::ZERO,
        schnorrAx: U256::ZERO,
        schnorrAy: U256::ZERO,
        schnorrZ: U256::ZERO,
    }
    .abi_encode()
}

/// Blocks of the manager's `PartialDecryptionSubmitted` (`partials`) or
/// `CiphertextSubmitted` logs for application `aid` from block `from`.
pub async fn aid_log_blocks(
    rpc: &str,
    manager: Address,
    aid: [u8; 32],
    partials: bool,
    from: u64,
) -> Result<Vec<u64>> {
    let sig = if partials {
        IDKGManager::PartialDecryptionSubmitted::SIGNATURE_HASH
    } else {
        IDKGManager::CiphertextSubmitted::SIGNATURE_HASH
    };
    Ok(chain::registry_logs(rpc, manager, sig, from)
        .await?
        .iter()
        .filter(|l| l.topics().get(2).is_some_and(|t| t.0 == aid))
        .filter_map(|l| l.block_number)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_stays_out_of_argv() {
        let key = DEV_KEYS[OPERATORS[0]];
        let d = Path::new("/nonexistent");
        let cmd = node_command(
            Path::new("davinci-dkg-node"),
            key,
            "http://127.0.0.1:1",
            Address::ZERO,
            d,
            d,
            d,
        );
        assert!(cmd.get_args().all(|a| !a.to_string_lossy().contains(key)));
        let env: Vec<_> = cmd.get_envs().collect();
        assert!(
            env.iter()
                .any(|(k, v)| { *k == "DAVINCI_DKG_PRIVKEY" && v.is_some_and(|v| v == key) })
        );
    }

    #[test]
    fn only_a_spent_pool_waits() {
        use davinci_client::Error;
        let r = |n: &str| Error::Reverted(n.into());
        assert!(no_free_pool_key(&r("NoLiveEpoch")));
        assert!(no_free_pool_key(&r("PoolExhausted")));
        for e in [
            r("InvalidEpoch"),
            r("InvalidSchnorrProof"),
            Error::Chain("reverted: NoLiveEpoch".into()),
            Error::DkgDisabled,
        ] {
            assert!(!no_free_pool_key(&e), "{e}");
        }
    }

    #[test]
    fn dkg_accounts_are_their_own() {
        // Not the organizer, a sequencer or the census owner (0..=4).
        assert!(OPERATORS.iter().chain([&DEPLOYER]).all(|&i| i > 4));
        let mut all = OPERATORS.to_vec();
        all.push(DEPLOYER);
        all.sort();
        all.dedup();
        assert_eq!(all.len(), OPERATORS.len() + 1);
        assert!(all.iter().all(|&i| i < DEV_KEYS.len()));
    }
}
