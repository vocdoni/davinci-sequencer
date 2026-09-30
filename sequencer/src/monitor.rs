//! The chain monitor: one loop polling registry events up to the confirmed
//! head, spawning a process actor per `ProcessCreated` and routing every
//! later event to it. Also the node's assembly point (`spawn_node`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use davinci_state::{CensusOrigin, ProcessConfig, genesis_root};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::actor::{ActorError, ActorHandle, Deps, spawn_actor};
use crate::census::onchain::OnchainIndex;
use crate::census::{CensusError, CensusOptions, CensusStore};
use crate::config::Config;
use crate::keys::KeyStore;
use crate::metrics::Metrics;
use crate::storage::{Db, LocalStatus, ProcessRecord, StorageError};
use crate::web3::{EventKind, RegistryEvent};

const LAST_BLOCK_KEY: &str = "monitor_last_block";
const BOOT_RETRIES_KEY: &str = "monitor_boot_retries";
/// Bootstrap attempts per process before it is recorded ignored.
const BOOT_ATTEMPT_CAP: u32 = 10;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Census(#[from] CensusError),
    #[error(transparent)]
    Actor(#[from] ActorError),
    #[error("{0}")]
    Config(String),
}

/// Fixed facts and limits the API layer serves and enforces.
#[derive(Clone, Debug)]
pub struct NodeStatics {
    /// Settling account; `None` in observer mode.
    pub sequencer_address: Option<[u8; 20]>,
    pub chain_id: u64,
    pub registry: [u8; 20],
    /// `sha256` of the ballot VK wire bytes (the 0x07 leaf digest).
    pub vk_hash: [u8; 32],
    pub keys_per_minute: u32,
}

/// A running node: the actor map plus everything the API layer serves from.
#[derive(Clone)]
pub struct Node {
    pub processes: Arc<RwLock<HashMap<Fr, ActorHandle>>>,
    pub metrics: Arc<Metrics>,
    pub keys: KeyStore,
    pub census: Arc<CensusStore>,
    /// Census contracts of origin-3 processes.
    pub onchain: Arc<OnchainIndex>,
    pub db: Db,
    pub statics: NodeStatics,
    /// Ballot proof verifier for `POST /votes`.
    pub verifier: Arc<BallotVerifier>,
}

impl Node {
    /// The [`ProcessConfig`] `validate_vote` checks a vote against.
    pub fn process_config(&self, rec: &ProcessRecord) -> Result<ProcessConfig, NodeError> {
        process_config(self.statics.vk_hash, rec)
    }
}

/// Starts the monitor loop and respawns the actors of every process the
/// database holds as active. Returns once the node is running.
pub async fn spawn_node(
    cfg: Config,
    deps: Deps,
    shutdown: CancellationToken,
) -> Result<Node, NodeError> {
    let census = Arc::new(CensusStore::with_options(
        deps.db.clone(),
        CensusOptions::from_config(&cfg),
    )?);
    let verifier = Arc::new(ballot_verifier(&cfg)?);
    let vk_hash = verifier.vk_hash();
    let chain_id = deps.contracts.chain_id();
    let registry = cfg.registry.into_array();
    // Its own provider (connects lazily): census contracts are plain reads.
    let provider = crate::web3::rpc_provider(&cfg.rpc_url);
    let onchain = Arc::new(OnchainIndex::new(
        &deps.db,
        chain_id,
        provider,
        cfg.poll_interval,
    )?);
    let node = Node {
        processes: Arc::new(RwLock::new(HashMap::new())),
        metrics: Arc::new(Metrics::default()),
        keys: KeyStore::open(&deps.db, chain_id, registry)?,
        census,
        onchain,
        db: deps.db.clone(),
        statics: NodeStatics {
            sequencer_address: deps.contracts.signer().map(|a| a.into_array()),
            chain_id,
            registry,
            vk_hash,
            keys_per_minute: cfg.keys_per_minute,
        },
        verifier,
    };
    let ctx = Ctx {
        cfg,
        deps,
        node: node.clone(),
        vk_hash,
        updates: Arc::default(),
        prune: Arc::default(),
    };
    // Respawn what a previous run served. A head read seeds each actor's
    // chain time so it does not accept votes into an already-ended window;
    // 0 means "unknown" and the actor refuses votes until the first Head.
    let chain_time = ctx.deps.contracts.head().await.map(|(_, t)| t).unwrap_or(0);
    let mut map = node.processes.write().await;
    for rec in ctx.deps.db.processes()? {
        // Finalized processes respawn too: their actor is read-only
        // (never accepts, seals or finalizes) but keeps serving tracker
        // proofs and stored ballots. Only Ignored records stay down.
        if rec.local == LocalStatus::Ignored {
            continue;
        }
        resume_census(&ctx, &rec).await;
        match respawn(&ctx, &rec, chain_time, &shutdown) {
            Ok(h) => {
                map.insert(rec.pid, h);
            }
            Err(e) => error!(pid = %pid_hex(&rec.pid), %e, "respawn failed"),
        }
    }
    drop(map);
    let tracker = ctx.deps.tasks.clone();
    tracker.spawn(monitor_loop(ctx, shutdown));
    Ok(node)
}

/// Origin-2 census updates: pid -> (uri, newest root, state). A newer
/// update replaces uri and root; a running fetch finishes first, so each
/// process has at most one fetch in flight and one pending.
type Updates = Arc<Mutex<HashMap<Fr, (String, Fr, Job)>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
    Queued,
    Running,
    Done,
}

struct Ctx {
    cfg: Config,
    deps: Deps,
    node: Node,
    vk_hash: [u8; 32],
    updates: Updates,
    /// Set when an update loaded: the next tick drops unused censuses.
    prune: Arc<AtomicBool>,
}

/// A respawned dynamic census: re-register an origin-3 contract, queue an
/// origin-2 root that never loaded.
async fn resume_census(ctx: &Ctx, rec: &ProcessRecord) {
    let c = &rec.onchain.census;
    match c.origin {
        2 => {
            if let Ok(root) = fr_from_be(&c.root)
                && !ctx.node.census.has(&root).unwrap_or(false)
                && ctx.node.census.bad_reason(&rec.pid, &root).is_none()
            {
                queue_census(ctx, rec.pid, c.uri.clone(), root);
            }
        }
        3 => {
            // A failure here is transient: the actor re-registers on every
            // tick while its contract is not indexed.
            if let Err(e) = ctx.node.onchain.register(c.contract_address).await {
                error!(pid = %pid_hex(&rec.pid), ?e, "census contract register; actor will retry");
            }
        }
        _ => {}
    }
}

fn queue_census(ctx: &Ctx, pid: Fr, uri: String, root: Fr) {
    if let Err(e) = ctx.node.census.clear_bad(&pid) {
        error!(pid = %pid_hex(&pid), %e, "clear census failure");
    }
    if let Ok(mut q) = ctx.updates.lock() {
        let e = q.entry(pid).or_insert((String::new(), root, Job::Queued));
        (e.0, e.1) = (uri, root);
        if e.2 == Job::Done {
            e.2 = Job::Queued;
        }
    }
}

/// Drops stored censuses no process uses any more: not the root of an
/// origin-1/2 record nor the newest update of one. Runs on the monitor loop,
/// so it never races a bootstrap that stored a census before its record.
/// Roots of finished processes are kept, so their proofs stay servable.
async fn prune_censuses(ctx: &Ctx) {
    let mut keep: HashSet<[u8; 32]> = match ctx.updates.lock() {
        Ok(q) => q.values().map(|e| fr_to_be(&e.1)).collect(),
        Err(_) => return,
    };
    match ctx.deps.db.processes() {
        Ok(recs) => keep.extend(
            recs.iter()
                .filter(|r| matches!(r.onchain.census.origin, 1 | 2))
                .map(|r| r.onchain.census.root),
        ),
        Err(e) => return error!(%e, "census prune: processes"),
    }
    match ctx.node.census.prune(keep).await {
        Ok(0) => {}
        Ok(n) => info!(dropped = n, "unused censuses pruned"),
        Err(e) => error!(%e, "census prune"),
    }
}

/// Starts a fetch for every queued origin-2 census not already in flight.
/// Off the monitor loop: `CensusStore::fetch` applies the usual SSRF, size
/// and backoff rules, and a transient failure stays queued for the next tick.
fn fetch_queued_censuses(ctx: &Ctx, shutdown: &CancellationToken) {
    let jobs: Vec<(Fr, String, Fr)> = match ctx.updates.lock() {
        Ok(mut q) => q
            .iter_mut()
            .filter(|(_, e)| e.2 == Job::Queued)
            .map(|(pid, e)| {
                e.2 = Job::Running;
                (*pid, e.0.clone(), e.1)
            })
            .collect(),
        Err(_) => return,
    };
    for (pid, uri, root) in jobs {
        let (census, q, prune, sd) = (
            ctx.node.census.clone(),
            ctx.updates.clone(),
            ctx.prune.clone(),
            shutdown.clone(),
        );
        ctx.deps.tasks.spawn(async move {
            let res = tokio::select! {
                _ = sd.cancelled() => return,
                r = census.fetch(&uri, &root) => r,
            };
            let done = match res {
                Ok(()) => {
                    prune.store(true, Ordering::Relaxed);
                    true
                }
                Err(CensusError::Backoff(_)) => false,
                Err(
                    e @ (CensusError::Format(_)
                    | CensusError::RootMismatch { .. }
                    | CensusError::Config(_)
                    | CensusError::Refused(_)),
                ) => {
                    error!(pid = %pid_hex(&pid), ?e, "census update can never load; votes are refused until the next one");
                    if let Err(e) = census.mark_bad(&pid, &root, &e.to_string()) {
                        error!(pid = %pid_hex(&pid), %e, "persist census failure");
                    }
                    true
                }
                Err(e) => {
                    warn!(pid = %pid_hex(&pid), ?e, "census update fetch; retrying");
                    false
                }
            };
            // A newer root queued meanwhile runs on the next tick.
            if let Ok(mut q) = q.lock()
                && let Some(e) = q.get_mut(&pid)
            {
                e.2 = if done && e.1 == root {
                    Job::Done
                } else {
                    Job::Queued
                };
            }
        });
    }
}

fn pid_hex(pid: &Fr) -> String {
    hex::encode(davinci_zkvm_sdk::crypto::field::fr_to_be(pid))
}

/// Verifier of the guest-pinned ballot vk (0x07 leaf): a `--ballot-vk` file
/// or the SDK's embedded protocol key.
pub(crate) fn ballot_verifier(cfg: &Config) -> Result<BallotVerifier, NodeError> {
    let json = match &cfg.ballot_vk {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| NodeError::Config(format!("{}: {e}", p.display())))?,
        None => release::ballot_vk_json().to_string(),
    };
    BallotVerifier::from_snarkjs_json(&json)
        .map_err(|e| NodeError::Config(format!("ballot vk: {e}")))
}

/// The census lookups an actor needs at seal time.
fn census_access(ctx: &Ctx) -> crate::actor::CensusAccess {
    crate::actor::CensusAccess {
        store: ctx.node.census.clone(),
        onchain: ctx.node.onchain.clone(),
    }
}

fn respawn(
    ctx: &Ctx,
    rec: &ProcessRecord,
    chain_time: u64,
    shutdown: &CancellationToken,
) -> Result<ActorHandle, NodeError> {
    let pcfg = process_config(ctx.vk_hash, rec)?;
    Ok(spawn_actor(
        &ctx.cfg,
        &ctx.deps,
        census_access(ctx),
        ctx.node.keys.clone(),
        ctx.node.metrics.clone(),
        rec.clone(),
        pcfg,
        chain_time,
        shutdown.clone(),
    )?)
}

/// Rebuilds the guest's `ProcessConfig` from the on-chain process record.
fn process_config(vk_hash: [u8; 32], rec: &ProcessRecord) -> Result<ProcessConfig, NodeError> {
    let bad = |m: &str| NodeError::Config(m.to_string());
    let p = &rec.onchain;
    if p.enc_key == Point::IDENTITY || !p.enc_key.in_subgroup() {
        return Err(bad("encryption key is not a valid subgroup point"));
    }
    let census_origin = match p.census.origin {
        1 => CensusOrigin::MerkleStatic,
        2 => CensusOrigin::MerkleOffchainDynamic,
        3 => CensusOrigin::MerkleOnchainDynamic,
        4 => CensusOrigin::Csp,
        o => return Err(NodeError::Config(format!("unsupported census origin {o}"))),
    };
    let census_root =
        fr_from_be(&p.census.root).map_err(|e| NodeError::Config(format!("census root: {e}")))?;
    Ok(ProcessConfig {
        process_id: rec.pid,
        ballot_mode: p.ballot_mode,
        enc_key: p.enc_key,
        census_origin,
        census_root,
        ballot_vk_hash: vk_hash,
    })
}

/// A process whose bootstrap failed transiently: retried with backoff up to
/// [`BOOT_ATTEMPT_CAP`], then recorded ignored. Attempts survive restarts.
struct BootRetry {
    pid: Fr,
    pid31: [u8; 31],
    attempts: u32,
    due: tokio::time::Instant,
}

fn boot_backoff(poll: std::time::Duration, attempts: u32) -> std::time::Duration {
    // 1..32 poll intervals; ~13 min of patience at the 5 s default.
    poll * 2u32.pow(attempts.saturating_sub(1).min(5))
}

fn load_retries(db: &Db) -> Vec<BootRetry> {
    let bytes = match db.meta_bytes(BOOT_RETRIES_KEY) {
        Ok(Some(b)) => b,
        Ok(None) => return Vec::new(),
        Err(e) => {
            error!(%e, "bootstrap retry list unreadable; starting empty");
            return Vec::new();
        }
    };
    let list: Vec<(String, u32)> = match serde_json::from_slice(&bytes) {
        Ok(l) => l,
        Err(e) => {
            error!(%e, "bootstrap retry list corrupt; starting empty");
            return Vec::new();
        }
    };
    let now = tokio::time::Instant::now();
    list.iter()
        .filter_map(|(h, attempts)| {
            let mut pid31 = [0u8; 31];
            let pid = hex::decode_to_slice(h, &mut pid31)
                .ok()
                .and_then(|()| {
                    let mut be = [0u8; 32];
                    be[1..].copy_from_slice(&pid31);
                    fr_from_be(&be).ok()
                })
                .or_else(|| {
                    error!(entry = %h, "dropping undecodable bootstrap retry entry");
                    None
                })?;
            Some(BootRetry {
                pid,
                pid31,
                attempts: *attempts,
                due: now,
            })
        })
        .collect()
}

fn save_retries(db: &Db, retries: &[BootRetry]) {
    let list: Vec<(String, u32)> = retries
        .iter()
        .map(|r| (hex::encode(r.pid31), r.attempts))
        .collect();
    match serde_json::to_vec(&list) {
        Ok(b) => {
            if let Err(e) = db.set_meta_bytes(BOOT_RETRIES_KEY, &b) {
                error!(%e, "persist bootstrap retries");
            }
        }
        Err(e) => error!(%e, "encode bootstrap retries"),
    }
}

async fn monitor_loop(ctx: Ctx, shutdown: CancellationToken) {
    // The last scanned block; the start block only seeds a fresh database.
    let mut last = match ctx.deps.db.meta_u64(LAST_BLOCK_KEY).ok().flatten() {
        Some(b) => b,
        None => match ctx.cfg.start_block {
            Some(n) => n.saturating_sub(1),
            None => {
                warn!(
                    "fresh deployment without a start block: scanning registry events from \
                     block 0; set --start-block to the registry deployment block"
                );
                0
            }
        },
    };
    let mut retries = load_retries(&ctx.deps.db);
    // Independent cadences: a due tick is never lost to the other branch.
    let mut poll = tokio::time::interval(ctx.cfg.poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.reset(); // skip the immediate first tick
    let mut beat = tokio::time::interval(ctx.cfg.heartbeat());
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.reset();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = beat.tick() => {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = heartbeat(&ctx) => {}
                }
                continue;
            }
            _ = poll.tick() => {}
        }
        let head = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = ctx.deps.contracts.confirmed_head(ctx.cfg.confirmations) => r,
        };
        let confirmed = match head {
            Ok(v) => v,
            Err(e) => {
                warn!(%e, "confirmed_head");
                continue;
            }
        };
        // One eth_getLogs page at a time, each persisted: a failed page
        // resumes where it stopped, and shutdown cuts in between and during.
        let mut failed = false;
        while last < confirmed {
            let to = confirmed.min(last.saturating_add(crate::web3::MAX_LOG_RANGE));
            let page = tokio::select! {
                _ = shutdown.cancelled() => return,
                r = ctx.deps.contracts.events(last + 1, to) => r,
            };
            let events = match page {
                Ok(events) => events,
                Err(e) => {
                    warn!(?e, from = last + 1, to, "event fetch");
                    failed = true;
                    break;
                }
            };
            for ev in events {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = route(&ctx, ev, &mut retries, &shutdown) => {}
                }
            }
            // last always advances: one unservable process must not stall
            // event routing for every other election. Failed bootstraps
            // live in the retry list instead.
            last = to;
            if let Err(e) = ctx.deps.db.set_meta_u64(LAST_BLOCK_KEY, last) {
                error!(%e, "persist last block");
            }
        }
        if failed {
            continue;
        }
        // Census contracts and origin-2 updates sync in the background.
        let (ix, sd) = (ctx.node.onchain.clone(), shutdown.clone());
        ctx.deps.tasks.spawn(async move {
            tokio::select! {
                _ = sd.cancelled() => {}
                _ = ix.sync_all(confirmed) => {}
            }
        });
        fetch_queued_censuses(&ctx, &shutdown);
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = async {
                if ctx.prune.swap(false, Ordering::Relaxed) {
                    prune_censuses(&ctx).await;
                }
                run_boot_retries(&ctx, &mut retries, &shutdown).await;
            } => {}
        }
    }
}

/// The heartbeat: actors seal, close out, resync and finalize on Head.
/// The block is the confirmed head, so gap replays never apply a
/// transition still shallow enough to reorg out.
async fn heartbeat(ctx: &Ctx) {
    if let Ok((block, time)) = ctx.deps.contracts.head().await {
        let confirmed_block = block.saturating_sub(ctx.cfg.confirmations);
        for h in ctx.node.processes.read().await.values() {
            h.head(confirmed_block, time).await;
        }
    }
}

/// Routes one event. A failed bootstrap goes onto the retry list; routing
/// itself never fails, so the monitor always advances past the block.
async fn route(
    ctx: &Ctx,
    ev: RegistryEvent,
    retries: &mut Vec<BootRetry>,
    shutdown: &CancellationToken,
) {
    let pid31 = *ev.kind.pid();
    let mut be = [0u8; 32];
    be[1..].copy_from_slice(&pid31);
    let Ok(pid) = fr_from_be(&be) else {
        warn!("event with an out-of-field pid");
        return;
    };
    if let EventKind::ProcessCreated { .. } = ev.kind {
        if retries.iter().any(|r| r.pid == pid) {
            return; // already queued for retry
        }
        if let Err(e) = bootstrap(ctx, pid, &pid31, shutdown).await {
            warn!(pid = %hex::encode(pid31), %e, "bootstrap failed; queued for retry");
            // A wait (a census still syncing) is not a failed attempt.
            let attempts = u32::from(!matches!(e, NodeError::Census(CensusError::Backoff(_))));
            retries.push(BootRetry {
                pid,
                pid31,
                attempts,
                due: tokio::time::Instant::now() + boot_backoff(ctx.cfg.poll_interval, attempts),
            });
            save_retries(&ctx.deps.db, retries);
        }
        return;
    }
    if let Some(h) = ctx.node.processes.read().await.get(&pid) {
        // Only origin 2 has a document to fetch; origin 3 follows its contract.
        if let EventKind::CensusUpdated { root, uri, .. } = &ev.kind
            && let Ok(Some(rec)) = ctx.deps.db.process(&pid)
            && rec.onchain.census.origin == 2
        {
            match fr_from_be(root) {
                Ok(r) => queue_census(ctx, pid, uri.clone(), r),
                Err(e) => {
                    warn!(pid = %hex::encode(pid31), %e, "census update with an out-of-field root")
                }
            }
        }
        h.event(ev).await;
    }
}

/// Re-attempts due bootstraps; at the attempt cap the process is recorded
/// ignored so it stops consuming retries (and is never re-bootstrapped).
async fn run_boot_retries(ctx: &Ctx, retries: &mut Vec<BootRetry>, shutdown: &CancellationToken) {
    if retries.is_empty() {
        return;
    }
    let now = tokio::time::Instant::now();
    let mut changed = false;
    let mut i = 0;
    while i < retries.len() {
        if retries[i].due > now {
            i += 1;
            continue;
        }
        let (pid, pid31) = (retries[i].pid, retries[i].pid31);
        match bootstrap(ctx, pid, &pid31, shutdown).await {
            Ok(()) => {
                retries.remove(i);
                changed = true;
            }
            // The census store is still backing this (uri, root) off: not a
            // fresh attempt (no I/O happened), so it does not consume one.
            // Re-check at our own cadence too — the census may land in the
            // store out-of-band and `fetch` then succeeds from `has(root)`.
            Err(NodeError::Census(CensusError::Backoff(d))) => {
                retries[i].due =
                    now + d.min(boot_backoff(ctx.cfg.poll_interval, retries[i].attempts));
                i += 1;
            }
            Err(e) => {
                changed = true;
                retries[i].attempts += 1;
                if retries[i].attempts >= BOOT_ATTEMPT_CAP {
                    ignore_unbootstrapped(ctx, pid, &pid31, &e.to_string()).await;
                    retries.remove(i);
                } else {
                    warn!(
                        pid = %hex::encode(pid31),
                        %e,
                        attempts = retries[i].attempts,
                        "bootstrap retry failed"
                    );
                    retries[i].due = now + boot_backoff(ctx.cfg.poll_interval, retries[i].attempts);
                    i += 1;
                }
            }
        }
    }
    if changed {
        save_retries(&ctx.deps.db, retries);
    }
}

/// Records a process that never bootstrapped as ignored, with whatever
/// on-chain data is reachable (a placeholder if the read still fails).
async fn ignore_unbootstrapped(ctx: &Ctx, pid: Fr, pid31: &[u8; 31], note: &str) {
    let onchain = match ctx.deps.contracts.process(pid31).await {
        Ok(p) => p,
        Err(_) => placeholder_process(),
    };
    let rec = ProcessRecord {
        pid,
        onchain,
        local: LocalStatus::Active,
        note: None,
    };
    let note = format!("bootstrap gave up after {BOOT_ATTEMPT_CAP} attempts: {note}");
    if let Err(e) = ignore(ctx, rec, &note) {
        error!(pid = %hex::encode(pid31), %e, "record ignored process");
    }
}

/// Stand-in record for a process whose on-chain read never succeeded.
fn placeholder_process() -> crate::web3::OnchainProcess {
    crate::web3::OnchainProcess {
        status: crate::web3::ProcessStatus::Unknown,
        organizer: [0u8; 20],
        enc_key: Point::IDENTITY,
        state_root: [0u8; 32],
        results: Vec::new(),
        start_time: 0,
        duration: 0,
        max_voters: 0,
        voters_count: 0,
        overwritten_count: 0,
        creation_block: 0,
        batch_number: 0,
        metadata_uri: String::new(),
        metadata_hash: [0u8; 32],
        ballot_mode: davinci_zkvm_sdk::ballot::BallotMode {
            num_fields: 0,
            group_size: 0,
            unique_values: false,
            cost_exponent: 0,
            max_value: 0,
            min_value: 0,
            max_value_sum: 0,
            min_value_sum: 0,
        },
        census: crate::web3::OnchainCensus {
            origin: 0,
            root: [0u8; 32],
            uri: String::new(),
            contract_address: [0u8; 20],
        },
        key_mode: crate::web3::KeyMode::Sequencer,
        dkg: crate::web3::DkgState::default(),
        grace: 0,
        last_vote_at: 0,
    }
}

/// Handles a `ProcessCreated`: fetch the process, validate it, fetch its
/// census, and spawn its actor. A process this node cannot serve honestly
/// is recorded `ignored` with the reason.
async fn bootstrap(
    ctx: &Ctx,
    pid: Fr,
    pid31: &[u8; 31],
    shutdown: &CancellationToken,
) -> Result<(), NodeError> {
    if ctx.deps.db.process(&pid)?.is_some() || ctx.node.processes.read().await.contains_key(&pid) {
        return Ok(()); // seen before (respawned or ignored)
    }
    // A failed fetch is transient: the caller queues the pid for retry.
    let onchain = match ctx.deps.contracts.process(pid31).await {
        Ok(p) => p,
        Err(e) => return Err(NodeError::Config(format!("process fetch: {e}"))),
    };
    let rec = ProcessRecord {
        pid,
        onchain,
        local: LocalStatus::Active,
        note: None,
    };
    let pcfg = match process_config(ctx.vk_hash, &rec) {
        Ok(c) => c,
        Err(e) => return ignore(ctx, rec, &e.to_string()),
    };
    // The election must have started at the genesis root this config
    // derives, or the node would prove transitions of a different election.
    // A late bootstrap sees the CURRENT root, so past genesis the check
    // moves to the first transition's old_root; the actor then catches up
    // by replaying every transition from its DA blobs (which the blob
    // archive must still serve — a known production retention limit).
    match genesis_root(&pcfg) {
        Ok(g) if g == rec.onchain.state_root => {}
        Ok(g) => {
            if !first_transition_from(ctx, pid31, rec.onchain.creation_block, &g).await? {
                return ignore(ctx, rec, "on-chain root is not the genesis of this config");
            }
        }
        Err(e) => return ignore(ctx, rec, &format!("genesis root: {e}")),
    }
    // Origin 3: the contract's first sync runs on the monitor tick; until it
    // lands the process waits on the retry list. Only a sync that failed
    // since the last check uses an attempt, not the wait in its backoff.
    if pcfg.census_origin == CensusOrigin::MerkleOnchainDynamic {
        let c = rec.onchain.census.contract_address;
        let ix = &ctx.node.onchain;
        ix.register(c).await?;
        // NotIndexed cannot happen right after register; only a terminal
        // verdict ignores the process, anything else waits below.
        if let Err(e @ crate::census::onchain::UsableError::Unusable(_)) = ix.usable(&c) {
            return ignore(ctx, rec, &e.to_string());
        }
        if ix.latest(&c).is_none() {
            return Err(NodeError::Census(match ix.take_failure(&c) {
                Some(f) => CensusError::Fetch(f),
                None => CensusError::Backoff(ix.retry_in(&c).unwrap_or(ctx.cfg.poll_interval)),
            }));
        }
    }
    if matches!(
        pcfg.census_origin,
        CensusOrigin::MerkleStatic | CensusOrigin::MerkleOffchainDynamic
    ) && let Err(e) = ctx
        .node
        .census
        .fetch(&rec.onchain.census.uri, &pcfg.census_root)
        .await
    {
        return match e {
            // A broken document or a policy refusal can never validate:
            // ignore for good.
            CensusError::Format(_)
            | CensusError::RootMismatch { .. }
            | CensusError::Config(_)
            | CensusError::Refused(_) => ignore(ctx, rec, &format!("census: {e}")),
            // Network trouble (or the store's own backoff): the caller puts
            // the process on the bootstrap retry list.
            e => Err(NodeError::Census(e)),
        };
    }
    ctx.deps.db.put_process(&rec)?;
    let chain_time = ctx.deps.contracts.head().await.map(|(_, t)| t).unwrap_or(0);
    let h = spawn_actor(
        &ctx.cfg,
        &ctx.deps,
        census_access(ctx),
        ctx.node.keys.clone(),
        ctx.node.metrics.clone(),
        rec.clone(),
        pcfg,
        chain_time,
        shutdown.clone(),
    )?;
    ctx.node.processes.write().await.insert(pid, h);
    info!(pid = %hex::encode(pid31), "process actor started");
    Ok(())
}

/// Whether this process's first on-chain transition departed from
/// `genesis` (validates a bootstrap arriving after transitions settled).
/// Fetch failures — and a moved root with no transition visible yet
/// (confirmation lag) — are transient: the caller retries the bootstrap.
async fn first_transition_from(
    ctx: &Ctx,
    pid31: &[u8; 31],
    creation_block: u64,
    genesis: &[u8; 32],
) -> Result<bool, NodeError> {
    let terr = |e: &dyn std::fmt::Display| NodeError::Config(format!("transition scan: {e}"));
    let head = ctx
        .deps
        .contracts
        .confirmed_head(ctx.cfg.confirmations)
        .await
        .map_err(|e| terr(&e))?;
    let events = ctx
        .deps
        .contracts
        .events(creation_block, head)
        .await
        .map_err(|e| terr(&e))?;
    for ev in events {
        if ev.kind.pid() != pid31 {
            continue;
        }
        if let EventKind::StateTransitioned { old_root, .. } = ev.kind {
            return Ok(old_root == *genesis);
        }
    }
    Err(terr(
        &"root moved past genesis but no transition event is visible",
    ))
}

fn ignore(ctx: &Ctx, mut rec: ProcessRecord, note: &str) -> Result<(), NodeError> {
    warn!(pid = %pid_hex(&rec.pid), note, "ignoring process");
    rec.local = LocalStatus::Ignored;
    rec.note = Some(note.to_string());
    ctx.deps.db.put_process(&rec)?;
    Ok(())
}
