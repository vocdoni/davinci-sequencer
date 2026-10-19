//! The chain monitor: one loop polling registry events up to the confirmed
//! head, spawning a process actor per `ProcessCreated` and routing every
//! later event to it. Also the node's assembly point (`spawn_node`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use davinci_state::{CensusOrigin, ProcessConfig, genesis_root};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::actor::{ActorError, ActorHandle, Deps, pid31_of, spawn_actor};
use crate::census::onchain::OnchainIndex;
use crate::census::{CensusError, CensusOptions, CensusStore};
use crate::config::Config;
use crate::keys::KeyStore;
use crate::metrics::Metrics;
use crate::storage::{Db, LocalStatus, ProcessRecord, StorageError};
use crate::web3::{EventKind, RegistryEvent};

/// The monitor's cursor: every event up to this block was routed.
pub(crate) const LAST_BLOCK_KEY: &str = "monitor_last_block";
/// Hash of the cursor block, written with it: a change means a reorg.
pub(crate) const LAST_HASH_KEY: &str = "monitor_last_hash";
/// Rewind on a reorg at the cursor, in blocks. Not floored at the safe
/// head: on OP the derivation behind an unsafe reorg makes the new branch
/// safe at once, above the fork.
const REORG_REWIND: u64 = 128;
/// Consecutive failed block-hash reads before routing goes on unchecked: a
/// rate-limited block read must not stall every election.
const HASH_TRIES: u32 = 5;
const BOOT_RETRIES_KEY: &str = "monitor_boot_retries";

/// A block the monitor routed every event up to, with its hash: the one
/// view of the chain actors make state decisions from (reads at the hash,
/// EIP-1898). It only exists with a hash; a reorg is a new, lower anchor.
/// `time` is the block's timestamp (0 when unknown: loaded at boot);
/// `reorgs` counts the reorgs the monitor saw this run, so an actor learns
/// of one even when the next anchor it sees is higher than the last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub block: u64,
    pub hash: [u8; 32],
    pub time: u64,
    pub reorgs: u64,
}

impl Anchor {
    /// The same block on the same branch, whatever time was read with it.
    pub fn same(&self, o: &Anchor) -> bool {
        self.block == o.block && self.hash == o.hash
    }
}

/// The current anchor, shared with the heartbeat that hands it to actors.
type AnchorCell = Arc<Mutex<Option<Anchor>>>;

/// The anchor stored with the cursor, if the cursor has its hash.
fn stored_anchor(db: &Db) -> Option<Anchor> {
    let block = db.meta_u64(LAST_BLOCK_KEY).ok()??;
    let hash = db.meta_bytes(LAST_HASH_KEY).ok()??.try_into().ok()?;
    Some(Anchor {
        block,
        hash,
        time: 0,
        reorgs: 0,
    })
}

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
    let onchain = Arc::new(
        OnchainIndex::new(
            &deps.db,
            chain_id,
            provider,
            cfg.poll_interval,
            cfg.census_sync_every,
            cfg.confirmations,
        )?
        .with_max_members(cfg.census_max_participants as u64),
    );
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
    let anchor: AnchorCell = Arc::new(Mutex::new(stored_anchor(&deps.db)));
    let ctx = Ctx {
        cfg,
        deps,
        node: node.clone(),
        anchor: anchor.clone(),
        vk_hash,
        queue: CensusQueue {
            store: node.census.clone(),
            updates: Arc::default(),
        },
        prune: Arc::default(),
    };
    // Before anything spawns: an unreadable list fails startup.
    let mut retries = load_retries(&ctx.deps.db)?;
    // Respawn what a previous run served. A head read seeds each actor's
    // chain time so it does not accept votes into an already-ended window;
    // 0 means "unknown" and the actor refuses votes until the first Head.
    // Events lost in mailboxes at a crash are not routed again: each
    // actor's first tick re-reads its process and applies what moved.
    let (_, chain_time) = ctx.deps.contracts.head().await.unwrap_or((0, 0));
    if chain_time != 0 && ctx.deps.clock.now().abs_diff(chain_time) > 60 {
        warn!(
            local = ctx.deps.clock.now(),
            head = chain_time,
            "local clock differs from the chain's head time by over 60 s; admission closes at the later of the two, so a clock running ahead refuses votes early"
        );
    }
    let mut map = node.processes.write().await;
    let mut failed = Vec::new();
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
            Err(e) => {
                error!(pid = %pid_hex(&rec.pid), %e, "respawn failed; retrying");
                failed.push(rec.pid);
            }
        }
    }
    drop(map);
    // The monitor retries them (bootstrap starts a stored record's actor).
    // The list goes to the monitor in memory: a failed persist (the same
    // outage) must not lose a pid whose creation is behind the cursor.
    if !failed.is_empty() {
        for pid in failed {
            if !retries.iter().any(|r| r.pid == pid) {
                retries.push(BootRetry {
                    pid,
                    pid31: pid31_of(&pid),
                    attempts: 1,
                    due: tokio::time::Instant::now(),
                });
            }
        }
        save_retries(&ctx.deps.db, &retries);
    }
    let tracker = ctx.deps.tasks.clone();
    tracker.spawn(heartbeat_loop(
        ctx.deps.contracts.clone(),
        node.processes.clone(),
        anchor,
        ctx.cfg.confirmations,
        ctx.cfg.heartbeat(),
        shutdown.clone(),
    ));
    tracker.spawn(monitor_loop(ctx, shutdown, retries));
    Ok(node)
}

/// Census downloads by pid: origin-2 updates and bootstrap censuses. A newer
/// root replaces uri and root; a running fetch finishes first, so each
/// process has at most one fetch in flight and one pending.
/// Failed fetches of one process before its next ones start waiting.
const FREE_CENSUS_FAILURES: u32 = 3;

type Updates = Arc<Mutex<HashMap<Fr, Update>>>;

struct Update {
    uri: String,
    /// A Keccak census root, big-endian.
    root: [u8; 32],
    job: Job,
    /// The last transient failure, until a bootstrap counts it.
    failed: Option<String>,
    /// Consecutive fetches of this process that failed, whatever their root:
    /// an organizer re-rooting with `setProcessCensus` cannot dodge the
    /// per-(uri, root) backoff of the store by changing the root.
    fails: u32,
    /// No fetch starts before this.
    not_before: Option<std::time::Instant>,
}

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
    anchor: AnchorCell,
    vk_hash: [u8; 32],
    queue: CensusQueue,
    /// Set when an update loaded: the next tick drops unused censuses.
    prune: Arc<AtomicBool>,
}

/// A respawned dynamic census: re-register an origin-3 contract, queue an
/// origin-2 root that never loaded.
async fn resume_census(ctx: &Ctx, rec: &ProcessRecord) {
    let c = &rec.onchain.census;
    match c.origin {
        2 if !ctx.node.census.has(&c.root).unwrap_or(false)
            && ctx.node.census.bad_reason(&rec.pid, &c.root).is_none() =>
        {
            queue_census(ctx, rec.pid, c.uri.clone(), c.root);
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

/// The origin-2 census downloads the monitor runs off its loop. Actors
/// queue one too: a chain read can move a root whose event a crash lost.
#[derive(Clone)]
pub struct CensusQueue {
    store: Arc<CensusStore>,
    updates: Updates,
}

impl CensusQueue {
    /// Queues `root` of `pid` for download; a newer root replaces it.
    pub(crate) fn queue(&self, pid: Fr, uri: String, root: [u8; 32]) {
        if let Err(e) = self.store.clear_bad(&pid) {
            error!(pid = %pid_hex(&pid), %e, "clear census failure");
        }
        if let Ok(mut q) = self.updates.lock() {
            let e = q.entry(pid).or_insert(Update {
                uri: String::new(),
                root,
                job: Job::Queued,
                failed: None,
                fails: 0,
                not_before: None,
            });
            if e.root != root {
                e.failed = None;
            }
            (e.uri, e.root) = (uri, root);
            if e.job == Job::Done {
                e.job = Job::Queued;
            }
        }
    }

    /// Whether `pid`'s download is queued or running, with no failure to
    /// report and its census not stored yet.
    fn waiting(&self, pid: &Fr) -> bool {
        let root = match self.updates.lock() {
            Ok(q) => match q.get(pid) {
                Some(e) if e.job != Job::Done && e.failed.is_none() => e.root,
                _ => return false,
            },
            Err(_) => return false,
        };
        !self.store.has(&root).unwrap_or(true)
    }

    /// The transient failure of `pid`'s download since the last call.
    fn take_failure(&self, pid: &Fr) -> Option<String> {
        self.updates.lock().ok()?.get_mut(pid)?.failed.take()
    }

    /// Drops `pid`'s entry: its bootstrap ended.
    fn forget(&self, pid: &Fr) {
        if let Ok(mut q) = self.updates.lock() {
            q.remove(pid);
        }
    }
}

fn queue_census(ctx: &Ctx, pid: Fr, uri: String, root: [u8; 32]) {
    ctx.queue.queue(pid, uri, root);
}

/// Drops stored censuses no process uses any more: not the root of an
/// origin-1/2 record nor the newest update of one. Runs on the monitor loop,
/// so it never races a bootstrap that stored a census before its record.
/// Roots of finished processes are kept, so their proofs stay servable.
async fn prune_censuses(ctx: &Ctx) {
    let mut keep: HashSet<[u8; 32]> = match ctx.queue.updates.lock() {
        Ok(q) => q.values().map(|e| e.root).collect(),
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

/// Starts a fetch for every queued census not already in flight. Off the
/// monitor loop: `CensusStore::fetch` applies the usual SSRF, size and
/// backoff rules, and a transient failure stays queued for the next tick.
fn fetch_queued_censuses(ctx: &Ctx, shutdown: &CancellationToken) {
    let jobs: Vec<(Fr, String, [u8; 32])> = match ctx.queue.updates.lock() {
        Ok(mut q) => q
            .iter_mut()
            .filter(|(_, e)| {
                e.job == Job::Queued && e.not_before.is_none_or(|t| std::time::Instant::now() >= t)
            })
            .map(|(pid, e)| {
                e.job = Job::Running;
                (*pid, e.uri.clone(), e.root)
            })
            .collect(),
        Err(_) => return,
    };
    for (pid, uri, root) in jobs {
        let (census, q, prune, sd) = (
            ctx.node.census.clone(),
            ctx.queue.updates.clone(),
            ctx.prune.clone(),
            shutdown.clone(),
        );
        ctx.deps.tasks.spawn(async move {
            let res = tokio::select! {
                _ = sd.cancelled() => return,
                r = census.fetch(&uri, &root) => r,
            };
            let mut failed = None;
            // Charged to the process: everything but success and the store's
            // own "not yet".
            let charged = !matches!(res, Ok(()) | Err(CensusError::Backoff(_)));
            let ok = res.is_ok();
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
                    // Another process stored the root meanwhile: it is
                    // servable, and its process may already run on it.
                    if census.has(&root).unwrap_or(false) {
                        warn!(pid = %pid_hex(&pid), ?e, "census document unusable; its root is stored");
                    } else {
                        error!(pid = %pid_hex(&pid), ?e, "census update can never load; votes are refused until the next one");
                        if let Err(e) = census.mark_bad(&pid, &root, &e.to_string()) {
                            error!(pid = %pid_hex(&pid), %e, "persist census failure");
                        }
                    }
                    true
                }
                Err(e) => {
                    warn!(pid = %pid_hex(&pid), ?e, "census update fetch; retrying");
                    failed = Some(e.to_string());
                    false
                }
            };
            // A newer root queued meanwhile runs on the next tick.
            if let Ok(mut q) = q.lock()
                && let Some(e) = q.get_mut(&pid)
            {
                if e.root == root && failed.is_some() {
                    e.failed = failed;
                }
                if ok {
                    (e.fails, e.not_before) = (0, None);
                } else if charged {
                    // The first few failures cost nothing, so an honest
                    // organizer correcting a typo or two is never held back.
                    e.fails = e.fails.saturating_add(1);
                    if e.fails > FREE_CENSUS_FAILURES {
                        let n = e.fails - FREE_CENSUS_FAILURES;
                        e.not_before =
                            Some(std::time::Instant::now() + crate::census::backoff(n));
                    }
                }
                e.job = if done && e.root == root {
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
        queue: ctx.queue.clone(),
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
    Ok(ProcessConfig {
        process_id: rec.pid,
        ballot_mode: p.ballot_mode,
        enc_key: p.enc_key,
        census_origin,
        // Any 256-bit value: a Keccak root is not a field element.
        census_root: p.census.root,
        ballot_vk_hash: vk_hash,
    })
}

/// A process whose bootstrap failed transiently: retried with backoff up to
/// [`BOOT_ATTEMPT_CAP`], then recorded ignored, unless it has a stored
/// record (then retried at the longest backoff). Attempts survive restarts.
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

/// The persisted retry list. A storage error fails startup (a restart
/// retries): starting empty would drop pids whose creation is behind the
/// cursor for good. A corrupt list is logged and dropped, since failing on
/// it would wedge the node.
fn load_retries(db: &Db) -> Result<Vec<BootRetry>, NodeError> {
    let bytes = match db.meta_bytes(BOOT_RETRIES_KEY) {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(Vec::new()),
        Err(e) => {
            error!(%e, "bootstrap retry list unreadable; refusing to start");
            return Err(e.into());
        }
    };
    let list: Vec<(String, u32)> = match serde_json::from_slice(&bytes) {
        Ok(l) => l,
        Err(e) => {
            error!(%e, "bootstrap retry list corrupt; starting empty");
            return Ok(Vec::new());
        }
    };
    let now = tokio::time::Instant::now();
    Ok(list
        .iter()
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
        .collect())
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

async fn monitor_loop(ctx: Ctx, shutdown: CancellationToken, mut retries: Vec<BootRetry>) {
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
    let mut last_hash = match ctx.deps.db.meta_bytes(LAST_HASH_KEY) {
        Ok(b) => b.and_then(|b| <[u8; 32]>::try_from(b).ok()),
        Err(e) => {
            error!(%e, "cursor hash unreadable; reorg check resumes at the next page");
            None
        }
    };
    let mut hash_errors = 0u32;
    // Actor task deaths per pid this run: each one doubles the respawn wait.
    let mut deaths: HashMap<Fr, u32> = HashMap::new();
    let mut poll = tokio::time::interval(ctx.cfg.poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.reset(); // skip the immediate first tick
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = poll.tick() => {}
        }
        if shutdown.is_cancelled() {
            return; // actors end at shutdown: nothing to reap
        }
        reap_dead_actors(&ctx, &mut retries, &mut deaths).await;
        // A reorg below the cursor may have replaced routed events: route
        // the range again (actors are replay-tolerant). A cursor without a
        // hash (a fail-open page, an older database) is armed here, so an
        // idle chain is checked too.
        let got = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = ctx.deps.contracts.block_ref(last) => r,
        };
        let got = known(got, last);
        if got.is_ok() {
            hash_errors = 0;
        }
        match (got, last_hash) {
            (Ok(r), None) => {
                last_hash = Some(r.0);
                save_cursor(&ctx, last, Some(r));
            }
            (Ok((h, _)), Some(want)) if h != want => {
                // The rewound cursor is saved with its hash in one write, so
                // the anchor never goes without one: a failed read retries
                // the whole check at the next poll from the old cursor.
                let to = last.saturating_sub(REORG_REWIND);
                let got = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    r = ctx.deps.contracts.block_ref(to) => r,
                };
                let r = match known(got, to) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(%e, to, "block hash read at the reorg rewind; retrying");
                        continue;
                    }
                };
                error!(
                    depth = last - to,
                    cursor = last,
                    to,
                    "registry reorg below the event cursor; routing the range again"
                );
                ctx.node.metrics.reorgs.fetch_add(1, Ordering::Relaxed);
                // Actors see the lower anchor at the next Head and re-read
                // their process at it.
                (last, last_hash) = (to, Some(r.0));
                save_cursor(&ctx, last, Some(r));
            }
            // The same block: the anchor again (a boot one learns its time).
            (Ok(r), Some(_)) => publish(&ctx, last, r),
            // Nothing to check yet: route on.
            (Err(e), None) => {
                hash_failed(&mut hash_errors, &e, last);
            }
            (Err(e), Some(_)) => {
                if !hash_failed(&mut hash_errors, &e, last) {
                    continue;
                }
            }
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
            // Read before the page: a reorg in between leaves the old hash,
            // which the next poll catches.
            let hash = tokio::select! {
                _ = shutdown.cancelled() => return,
                r = ctx.deps.contracts.block_ref(to) => r,
            };
            let hash = match known(hash, to) {
                Ok(r) => {
                    hash_errors = 0;
                    Some(r)
                }
                Err(e) if hash_failed(&mut hash_errors, &e, to) => {
                    // Routed, but the anchor stays where it was.
                    ctx.node
                        .metrics
                        .anchor_stalls
                        .fetch_add(1, Ordering::Relaxed);
                    None
                }
                Err(_) => {
                    failed = true;
                    break;
                }
            };
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
            let queued = retries.len();
            for ev in events {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = route(&ctx, ev, &mut retries, &shutdown) => {}
                }
            }
            // Once per page, before the cursor passes it: a save per queued
            // bootstrap rewrites the whole list, quadratic in a flood.
            if retries.len() != queued {
                save_retries(&ctx.deps.db, &retries);
            }
            // last always advances: one unservable process must not stall
            // event routing for every other election. Failed bootstraps
            // live in the retry list instead.
            (last, last_hash) = (to, hash.map(|r| r.0));
            save_cursor(&ctx, last, hash);
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

/// Drops the handles of actors whose task ended (the release build unwinds,
/// so a panic ends only its task) and queues their respawn on the retry
/// list, where a stored record without an actor is respawned. Each death of
/// a pid doubles its wait: a panic at every start respawns at the longest
/// backoff, not every poll.
async fn reap_dead_actors(ctx: &Ctx, retries: &mut Vec<BootRetry>, deaths: &mut HashMap<Fr, u32>) {
    let dead: Vec<Fr> = ctx
        .node
        .processes
        .read()
        .await
        .iter()
        .filter(|(_, h)| h.is_closed())
        .map(|(pid, _)| *pid)
        .collect();
    if dead.is_empty() {
        return;
    }
    let mut map = ctx.node.processes.write().await;
    let now = tokio::time::Instant::now();
    for pid in dead {
        if !map.get(&pid).is_some_and(ActorHandle::is_closed) {
            continue;
        }
        map.remove(&pid);
        let n = deaths.entry(pid).or_default();
        *n = n.saturating_add(1);
        error!(pid = %pid_hex(&pid), deaths = *n, "actor task ended; respawning");
        if !retries.iter().any(|r| r.pid == pid) {
            retries.push(BootRetry {
                pid,
                pid31: pid31_of(&pid),
                attempts: 0,
                due: now + boot_backoff(ctx.cfg.poll_interval, *n),
            });
        }
    }
    drop(map);
    save_retries(&ctx.deps.db, retries);
}

/// Persists the cursor and its block hash in one write; with a hash, the
/// cursor is the new anchor. Without one (fail-open) the anchor stays.
fn save_cursor(ctx: &Ctx, last: u64, block: Option<([u8; 32], u64)>) {
    let bytes = block.as_ref().map(|(h, _)| &h[..]);
    if let Err(e) = ctx
        .deps
        .db
        .set_meta_u64_and_bytes(LAST_BLOCK_KEY, last, LAST_HASH_KEY, bytes)
    {
        error!(%e, "persist last block");
    }
    if let Some(r) = block {
        publish(ctx, last, r);
    }
}

fn publish(ctx: &Ctx, block: u64, (hash, time): ([u8; 32], u64)) {
    let reorgs = ctx.node.metrics.reorgs.load(Ordering::Relaxed);
    *ctx.anchor.lock().unwrap_or_else(|e| e.into_inner()) = Some(Anchor {
        block,
        hash,
        time,
        reorgs,
    });
}

/// A block the endpoint does not know (a backend behind the confirmed head)
/// is a failed read: one such answer must not drop the cursor's hash.
fn known(
    r: Result<Option<([u8; 32], u64)>, crate::web3::Web3Error>,
    block: u64,
) -> Result<([u8; 32], u64), crate::web3::Web3Error> {
    r?.ok_or_else(|| crate::web3::Web3Error::Rpc(format!("block {block} unknown to the endpoint")))
}

/// Counts a failed block-hash read; true once [`HASH_TRIES`] failed in a
/// row: go on without the reorg check rather than stall routing.
fn hash_failed(errors: &mut u32, e: &crate::web3::Web3Error, block: u64) -> bool {
    *errors = errors.saturating_add(1);
    match (*errors).cmp(&HASH_TRIES) {
        std::cmp::Ordering::Less => warn!(%e, block, "block hash read"),
        std::cmp::Ordering::Equal => {
            warn!(%e, block, "block hash reads keep failing; routing without the reorg check")
        }
        std::cmp::Ordering::Greater => {}
    }
    *errors >= HASH_TRIES
}

/// The heartbeat: actors seal, close out, resync and finalize on Head.
/// Its own task, so a slow page or bootstrap never delays it. The block is
/// the confirmed head, so gap replays never apply a transition still
/// shallow enough to reorg out.
async fn heartbeat_loop(
    contracts: Arc<dyn crate::actor::Chain>,
    processes: Arc<RwLock<HashMap<Fr, ActorHandle>>>,
    anchor: AnchorCell,
    confirmations: u64,
    every: std::time::Duration,
    shutdown: CancellationToken,
) {
    let mut beat = tokio::time::interval(every);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.reset();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = beat.tick() => {}
        }
        let head = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = contracts.head() => r,
        };
        let Ok((block, time)) = head else { continue };
        let confirmed_block = block.saturating_sub(confirmations);
        let anchor = *anchor.lock().unwrap_or_else(|e| e.into_inner());
        // Not under the map lock: a full mailbox must not hold up a spawn.
        let hs: Vec<ActorHandle> = processes.read().await.values().cloned().collect();
        // Never waits: a full mailbox drops this Head, not the others'.
        for h in hs {
            h.head(confirmed_block, time, anchor).await;
        }
    }
}

/// Routes one event. A failed bootstrap goes onto the retry list, which the
/// caller persists; routing itself never fails, so the monitor always
/// advances past the block.
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
            // A wait (a census still loading) is not a failed attempt.
            let wait = matches!(e, NodeError::Census(CensusError::Backoff(_)));
            if wait {
                info!(pid = %hex::encode(pid31), "bootstrap waits for its census");
            } else {
                warn!(pid = %hex::encode(pid31), %e, "bootstrap failed; queued for retry");
            }
            let attempts = u32::from(!wait);
            retries.push(BootRetry {
                pid,
                pid31,
                attempts,
                due: tokio::time::Instant::now() + boot_backoff(ctx.cfg.poll_interval, attempts),
            });
        }
        return;
    }
    if let Some(h) = ctx.node.processes.read().await.get(&pid) {
        // Only origin 2 has a document to fetch; origin 3 follows its contract.
        if let EventKind::CensusUpdated { root, uri, .. } = &ev.kind
            && let Ok(Some(rec)) = ctx.deps.db.process(&pid)
            && rec.onchain.census.origin == 2
        {
            queue_census(ctx, pid, uri.clone(), *root);
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
                // Fail closed: a read error may hide a live record.
                let stored = !matches!(ctx.deps.db.process(&pid), Ok(None));
                if retries[i].attempts >= BOOT_ATTEMPT_CAP && stored {
                    // A process with state is never given up: retry at the
                    // longest backoff and keep saying so.
                    error!(pid = %hex::encode(pid31), %e, "actor still fails to start; retrying");
                    retries[i].attempts = BOOT_ATTEMPT_CAP;
                    retries[i].due = now + boot_backoff(ctx.cfg.poll_interval, BOOT_ATTEMPT_CAP);
                    i += 1;
                } else if retries[i].attempts >= BOOT_ATTEMPT_CAP {
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
    ctx.queue.forget(&pid);
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
    if ctx.node.processes.read().await.contains_key(&pid) {
        return Ok(()); // running
    }
    if let Some(rec) = ctx.deps.db.process(&pid)? {
        // Ignored stays down. A stored record without an actor (its spawn
        // failed here or at boot) starts now; an error keeps the retry.
        if rec.local == LocalStatus::Ignored {
            return Ok(());
        }
        let chain_time = ctx.deps.contracts.head().await.map(|(_, t)| t).unwrap_or(0);
        let h = respawn(ctx, &rec, chain_time, shutdown)?;
        ctx.node.processes.write().await.insert(pid, h);
        info!(pid = %hex::encode(pid31), "process actor started on retry");
        return Ok(());
    }
    // Its census is still downloading: nothing to re-read yet.
    if ctx.queue.waiting(&pid) {
        return Err(NodeError::Census(CensusError::Backoff(
            ctx.cfg.poll_interval,
        )));
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
    // Origins 1 and 2 download off the loop (the URI is the creator's: a
    // slow server must not stall routing). The process waits on the retry
    // list until the census is stored; a failed download costs an attempt.
    if matches!(
        pcfg.census_origin,
        CensusOrigin::MerkleStatic | CensusOrigin::MerkleOffchainDynamic
    ) {
        // A broken document or a policy refusal can never validate: ignore
        // for good, even if another process stored the root meanwhile.
        if let Some(why) = ctx.node.census.bad_reason(&pid, &pcfg.census_root) {
            ctx.queue.forget(&pid);
            if let Err(e) = ctx.node.census.clear_bad(&pid) {
                error!(pid = %pid_hex(&pid), %e, "clear census failure");
            }
            return ignore(ctx, rec, &format!("census: {why}"));
        }
        if !ctx.node.census.has(&pcfg.census_root)? {
            if let Some(why) = ctx.queue.take_failure(&pid) {
                return Err(NodeError::Census(CensusError::Fetch(why)));
            }
            queue_census(ctx, pid, rec.onchain.census.uri.clone(), pcfg.census_root);
            return Err(NodeError::Census(CensusError::Backoff(
                ctx.cfg.poll_interval,
            )));
        }
    }
    ctx.deps.db.put_process(&rec)?;
    ctx.queue.forget(&pid);
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
    warn!(pid = %pid_hex(&rec.pid), note = %note, "ignoring process");
    rec.local = LocalStatus::Ignored;
    rec.note = Some(note.to_string());
    ctx.deps.db.put_process(&rec)?;
    Ok(())
}
