//! One scenario from the fee gate to its audited report (spec §8, §11 and
//! §12), and the audit of a run that crashed (`audit --run`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use anyhow::{Context, Result, anyhow, ensure};
use davinci_client::SequencerClient;
use davinci_client::api::{ProcessId, parse_vote_id, vote_id_hex};
use davinci_client::organizer::Organizer;
use davinci_zkvm_sdk::client::ProverClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;

use super::audit::{self, AuditResult, Published};
use super::ballots::{BallotRecord, sha256_file};
use super::chain_view::{self, JobRow, TransitionRow};
use super::chaos::{self, Controller, Observed};
use super::ledger::{Entry, Ledger};
use super::merge::ElectionIndex;
use super::metrics::{self, Kind as LogKind, LogTail};
use super::money::{self, Balances, Budget, Fees};
use super::plan::{self, Election, ElectionSpec, NodeId, Plan, Seed};
use super::race;
use super::report::{self, Finding, Progress, ScenarioReport, Throughput};
use super::rig::{Rig, Role};
use super::scenarios::{
    Cell, EST_BLOB_WEI, EST_EXEC_WEI, End, Kind, Lane, Scenario, on_anvil, post_bound, scaled,
    scenarios,
};
use super::schedule;
use super::submit::{Route, SubmitStats, Submitter};
use crate::net::Net;
use crate::{chain, cost, l1};

/// What every step of a run reads.
pub struct Ctx {
    pub net: Net,
    pub plan: Plan,
    pub campaign: PathBuf,
    pub seed: Seed,
    /// Live: `budget.json`, for the stop rule. `None` on anvil.
    pub budget: Option<Budget>,
}

/// A chaos controller and its scenario's start. The caller holds it, so a
/// signal that drops the scenario can still undo what it injected.
pub type ChaosSlot = Option<(Controller, Instant)>;

/// A no-go at the start: how long the run waits before it skips.
const GATE_WAIT: Duration = Duration::from_secs(30 * 60);
/// The longest a fee breach pauses sends before the scenario stops.
const FEE_PAUSE: Duration = Duration::from_secs(30 * 60);
const IDLE_WAIT: Duration = Duration::from_secs(30 * 60);
/// Progress line, fee gate and budget.
const TICK: Duration = Duration::from_secs(30);
const LOG_POLL: Duration = Duration::from_secs(1);
/// Results are due by `grace_end` plus this.
const RESULTS_LATE: u64 = 15 * 60;
/// The results wait never outlasts this: the window closes at most
/// `graceMaxTotal` (30 min on OP) after the end.
const RESULTS_MAX: Duration = Duration::from_secs(60 * 60);
/// The audit's wait for the observer and the queues (spec §11).
const QUIET_MAX: Duration = Duration::from_secs(15 * 60);
/// `AfterSettled`: the longest wait, and how long nothing pending and no
/// new landing ends it early (a vote that errored never settles).
const SETTLED_MAX: Duration = Duration::from_secs(45 * 60);
const SETTLED_STABLE: Duration = Duration::from_secs(300);
/// A ceiling burst waits at most this for the previous one to seal.
const BURST_MAX: Duration = Duration::from_secs(20 * 60);
/// A shortened end lands a few blocks after it is computed: the registry
/// wants at least the notice from the transaction's block.
const SHORTEN_MARGIN: u64 = 15;
/// After a stop, sends already past their permit get this long to answer
/// (they give up their retries at once) before the lanes are dropped.
const STOP_DRAIN: Duration = Duration::from_secs(120);

pub(super) fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

pub(super) fn role_of(n: NodeId) -> Role {
    match n {
        NodeId::A => Role::A,
        NodeId::B => Role::B,
    }
}

fn node_of(r: Role) -> Option<NodeId> {
    match r {
        Role::A => Some(NodeId::A),
        Role::B => Some(NodeId::B),
        Role::O => None,
    }
}

/// `runs/w<N>-<scenario>`.
pub fn run_dir(campaign: &Path, wave: u32, name: &str) -> PathBuf {
    campaign.join("runs").join(format!("w{wave}-{name}"))
}

/// A pid as the node logs it.
fn pid_hex(pid: &ProcessId) -> String {
    hex::encode(pid.0)
}

pub(super) fn op_provider(net: &Net) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .connect_client(chain::rpc(&net.rpc)?)
        .erased())
}

/// The scenarios of `all` named in `names` (`all` takes every one), in
/// run order.
pub fn select(all: Vec<Scenario>, names: &[String]) -> Result<Vec<Scenario>> {
    if names.iter().any(|n| n == "all") {
        ensure!(names.len() == 1, "`all` goes alone");
        return Ok(all);
    }
    for n in names {
        ensure!(
            all.iter().any(|s| &s.name == n),
            "no scenario {n:?} in this wave"
        );
    }
    Ok(all
        .into_iter()
        .filter(|s| names.contains(&s.name))
        .collect())
}

/// After a stop: keeps in `planned` only the votes whose send started (the
/// rest never left the harness, so they are not missing); returns how many
/// it dropped.
pub fn keep_started(
    planned: &mut BTreeMap<String, BTreeSet<u64>>,
    started: &BTreeSet<u64>,
) -> usize {
    let mut dropped = 0;
    for v in planned.values_mut() {
        let before = v.len();
        v.retain(|x| started.contains(x));
        dropped += before - v.len();
    }
    dropped
}

/// A lane's schedule seed: Poisson gaps and overwrite delays repeat run to
/// run.
fn lane_seed(tag: &str) -> u64 {
    let h = Sha256::new()
        .chain_update(b"davinci-bench/v1/schedule")
        .chain_update([0])
        .chain_update(tag)
        .finalize();
    u64::from_be_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]])
}

/// Every ballot of a `Plain` lane with its send time from the scenario's
/// start: round 1 on the lane's schedule, later rounds after their voter's
/// previous one, the i-th revoter (voter order) by `gaps[i % len]`.
pub fn lane_plan(
    lane: &Lane,
    recs: Vec<BallotRecord>,
    seed: u64,
) -> Result<Vec<(Duration, BallotRecord)>> {
    let mut first: Vec<u32> = recs
        .iter()
        .filter(|r| r.round == 1)
        .map(|r| r.voter)
        .collect();
    first.sort_unstable();
    let offsets = lane.schedule.offsets(first.len(), seed);
    let mut later: Vec<(u32, u8)> = recs
        .iter()
        .filter(|r| r.round > 1)
        .map(|r| (r.voter, r.round))
        .collect();
    later.sort_unstable();
    ensure!(
        later.is_empty() || !lane.gaps.is_empty(),
        "{}: later rounds but no gap",
        lane.tag
    );
    let mut revoters: Vec<u32> = later.iter().map(|(v, _)| *v).collect();
    revoters.dedup();
    let mut times: Vec<(Duration, u32, u8)> = first
        .iter()
        .zip(&offsets)
        .map(|(v, t)| (*t, *v, 1))
        .collect();
    for (g, gap) in lane.gaps.iter().enumerate() {
        let n = lane.gaps.len();
        let mine: Vec<(u32, u8)> = later
            .iter()
            .filter(|(v, _)| revoters.binary_search(v).is_ok_and(|i| i % n == g))
            .copied()
            .collect();
        if mine.is_empty() {
            continue;
        }
        let o = schedule::order(&first, &mine, &offsets, gap, seed ^ (g as u64 + 1));
        times.extend(o.into_iter().filter(|(_, _, r)| *r > 1));
    }
    times.sort_by_key(|(t, _, _)| *t);
    let mut by: HashMap<(u32, u8), BallotRecord> =
        recs.into_iter().map(|r| ((r.voter, r.round), r)).collect();
    let plan = times
        .into_iter()
        .map(|(t, v, r)| {
            let rec = by
                .remove(&(v, r))
                .with_context(|| format!("{}: voter {v} round {r} twice", lane.tag))?;
            Ok((lane.start + t, rec))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        by.is_empty(),
        "{}: {} ballots never scheduled (a later round without its earlier one)",
        lane.tag,
        by.len()
    );
    Ok(plan)
}

/// A `Ceiling` lane's bursts in ascending B: `(B, ballots)`, each of
/// ⌈scale·B⌉ ballots in voter order. The ballots must fill them exactly.
pub fn bursts(
    cells: &[Cell],
    scale: f64,
    mut recs: Vec<BallotRecord>,
) -> Result<Vec<(usize, Vec<BallotRecord>)>> {
    ensure!(
        recs.iter().all(|r| r.round == 1),
        "a ceiling election has only round 1"
    );
    recs.sort_by_key(|r| r.voter);
    let mut cells = cells.to_vec();
    cells.sort_by_key(|c| c.b);
    let want: usize = cells.iter().map(|c| c.bursts * scaled(c.b, scale)).sum();
    ensure!(
        want == recs.len(),
        "{} ballots for {want} burst votes",
        recs.len()
    );
    let mut it = recs.into_iter();
    let mut out = Vec::new();
    for c in cells {
        for _ in 0..c.bursts {
            out.push((c.b, it.by_ref().take(scaled(c.b, scale)).collect()));
        }
    }
    Ok(out)
}

/// `ballots/w<N>/<tag>.jsonl`, checked against `index.json`.
pub fn load_ballots(campaign: &Path, wave: u32, e: &Election) -> Result<Vec<BallotRecord>> {
    let dir = campaign.join("ballots").join(format!("w{wave}"));
    let tag = &e.spec.tag;
    let idx: BTreeMap<String, ElectionIndex> = serde_json::from_slice(
        &std::fs::read(dir.join("index.json")).context("ballots index (run merge first)")?,
    )
    .context("ballots index")?;
    let want = idx
        .get(tag)
        .with_context(|| format!("{tag} is not merged"))?;
    let path = dir.join(format!("{tag}.jsonl"));
    ensure!(
        sha256_file(&path)? == want.sha256,
        "{} does not match its index",
        path.display()
    );
    let body = std::fs::read_to_string(&path)?;
    let recs = body
        .lines()
        .map(|l| serde_json::from_str::<BallotRecord>(l).context("ballot line"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(recs.len() == want.ballots, "{tag}: ballot count");
    ensure!(
        recs.iter()
            .all(|r| &r.tag == tag && r.req.process_id == e.snap.pid),
        "{tag}: a ballot of another election"
    );
    Ok(recs)
}

/// Node log events since a scenario started: `(node, unix seconds, kind)`.
type Events = Arc<Mutex<Vec<(Role, f64, LogKind)>>>;

fn lock(e: &Events) -> std::sync::MutexGuard<'_, Vec<(Role, f64, LogKind)>> {
    e.lock().unwrap_or_else(|p| p.into_inner())
}

/// Follows the three node logs every second from where they stand.
struct LogReader {
    events: Events,
    task: tokio::task::JoinHandle<()>,
}

impl LogReader {
    fn start(rig: &Rig) -> Result<LogReader> {
        let mut tails = Vec::new();
        for r in [Role::A, Role::B, Role::O] {
            let mut t = LogTail::new(&rig.node(r)?.log);
            t.poll(); // earlier scenarios' lines
            tails.push((r, t));
        }
        let events: Events = Default::default();
        let ev = events.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(LOG_POLL).await;
                for (r, t) in &mut tails {
                    let new: Vec<_> = t
                        .poll()
                        .into_iter()
                        .filter_map(|e| metrics::kind(&e).map(|k| (*r, e.at, k)))
                        .collect();
                    lock(&ev).extend(new);
                }
            }
        });
        Ok(LogReader { events, task })
    }

    fn snapshot(&self) -> Vec<(Role, f64, LogKind)> {
        lock(&self.events).clone()
    }
}

impl Drop for LogReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The process a log event is about.
pub fn kind_pid(k: &LogKind) -> Option<&str> {
    use LogKind as K;
    match k {
        K::Sealed { pid, .. }
        | K::Settled { pid, .. }
        | K::Synced { pid, .. }
        | K::LostEvent { pid }
        | K::LostRevert { pid }
        | K::PostRetry { pid, .. }
        | K::Pause { pid, .. }
        | K::Bisect { pid, .. }
        | K::Isolated { pid, .. }
        | K::Held { pid, .. }
        | K::RefreshOverflow { pid, .. }
        | K::FinalizeFailed { pid, .. }
        | K::ProverHold { pid }
        | K::GuestRejected { pid, .. }
        | K::SealPause { pid, .. }
        | K::Transient { pid } => Some(pid),
        K::Posted { .. }
        | K::RpcFailover { .. }
        | K::BeaconFailover
        | K::PostOnRecord { .. }
        | K::PostReplaced { .. } => None,
    }
}

/// Votes node `r` sealed for `pid`, and whether it synced `pid` at or after
/// `since` (unix seconds).
/// Votes node `r` sealed for `pid` at or after `since` (unix seconds), and
/// whether it synced `pid` since. Per burst: a cumulative count would take
/// a bisection's re-seals of earlier votes for the newest burst's seal.
fn sealed_or_synced(ev: &[(Role, f64, LogKind)], r: Role, pid: &str, since: f64) -> (u64, bool) {
    let mut sealed = 0;
    let mut synced = false;
    for (_, _, k) in ev.iter().filter(|(er, at, _)| *er == r && *at >= since) {
        match k {
            LogKind::Sealed { pid: p, votes, .. } if p == pid => sealed += votes,
            LogKind::Synced { pid: p, .. } if p == pid => synced = true,
            _ => {}
        }
    }
    (sealed, synced)
}

/// What a lane task reports when it ends.
struct LaneDone {
    tag: String,
    stats: Option<SubmitStats>,
    /// Unix seconds of the election's end, when this task ended it.
    ended: Option<f64>,
    /// Why ending it failed, after every retry.
    end_failed: Option<String>,
}

/// What the lane tasks reported.
#[derive(Default)]
struct Lanes {
    stats: BTreeMap<String, SubmitStats>,
    ended: BTreeMap<String, f64>,
    end_failed: BTreeMap<String, String>,
}

impl Lanes {
    fn record(&mut self, d: LaneDone) {
        if let Some(s) = d.stats {
            self.stats.insert(d.tag.clone(), s);
        }
        if let Some(t) = d.ended {
            self.ended.insert(d.tag.clone(), t);
        }
        if let Some(e) = d.end_failed {
            self.end_failed.insert(d.tag, e);
        }
    }
}

/// Ends `pid` per `end`; the organizer's transactions go one at a time.
async fn end_election(
    org: &Organizer,
    lock: &tokio::sync::Mutex<()>,
    pid: &ProcessId,
    end: &End,
) -> Result<()> {
    let _one = lock.lock().await;
    match end {
        End::Shorten { notice, .. } => {
            let p = org.process(&pid.0).await?;
            let head = org
                .provider()
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .context("no head block")?
                .header
                .timestamp;
            // An idle anvil's head can be old: the next block is mined now.
            let now = head.max(unix_now() as u64);
            let d = (now + notice + SHORTEN_MARGIN).saturating_sub(p.start_time);
            org.set_process_duration(&pid.0, d).await?;
        }
        End::Now { .. } | End::AfterSettled => org.end_process(&pid.0).await?,
    }
    Ok(())
}

/// Tries per election end or cancel, and the pause between tries.
const END_TRIES: u32 = 5;
const END_RETRY: Duration = Duration::from_secs(10);

/// How the run closes an election: its `End`, or a cancel (budget stop).
enum Close {
    End(End),
    Cancel,
}

async fn close(
    org: &Organizer,
    lock: &tokio::sync::Mutex<()>,
    pid: &ProcessId,
    how: &Close,
) -> Result<()> {
    match how {
        Close::End(e) => end_election(org, lock, pid, e).await,
        Close::Cancel => {
            let _one = lock.lock().await;
            Ok(org.cancel_process(&pid.0).await?)
        }
    }
}

/// Closes `pid` until it succeeds, up to `END_TRIES` times; each try
/// recomputes what it sends (a shortened end whose transaction landed late
/// reverts `InvalidDuration`). A process no longer READY or PAUSED counts as
/// closed: already ended or canceled, or an earlier try landed after its
/// receipt wait gave up. The error is redacted.
async fn close_with_retries(
    org: &Organizer,
    lock: &tokio::sync::Mutex<()>,
    pid: &ProcessId,
    how: &Close,
) -> std::result::Result<(), String> {
    use davinci_client::api::ProcessStatus as S;
    let mut last = String::new();
    for i in 0..END_TRIES {
        if i > 0 {
            tokio::time::sleep(END_RETRY).await;
        }
        if org
            .process(&pid.0)
            .await
            .is_ok_and(|p| !matches!(p.status, S::Ready | S::Paused))
        {
            return Ok(());
        }
        match close(org, lock, pid, how).await {
            Ok(()) => return Ok(()),
            Err(e) => last = crate::redact_urls(&format!("{e:#}")),
        }
    }
    Err(last)
}

/// `davinci-bench end`: closes each of `pids` (tag, id) as the run's lanes
/// do, a shortened end through `close_with_retries`, printing one line per
/// tag. Tries them all, then fails if any failed; returns how many it ended
/// (a process already closed counts as none).
pub async fn end_tagged(org: &Organizer, pids: &[(String, ProcessId)]) -> Result<usize> {
    use davinci_client::api::ProcessStatus as S;
    let lock = tokio::sync::Mutex::new(());
    let how = Close::End(End::Shorten {
        after: Duration::ZERO,
        notice: super::scenarios::NOTICE,
    });
    let (mut done, mut failed) = (0, 0);
    for (tag, pid) in pids {
        let open = org
            .process(&pid.0)
            .await
            .map(|p| matches!(p.status, S::Ready | S::Paused));
        if open.as_ref().is_ok_and(|o| !*o) {
            println!("{tag}: already closed");
            continue;
        }
        match close_with_retries(org, &lock, pid, &how).await {
            Ok(()) => {
                println!("{tag}: ended");
                done += 1;
            }
            Err(e) => {
                println!("{tag}: failed: {e}");
                failed += 1;
            }
        }
    }
    ensure!(failed == 0, "{failed} of {} ends failed", pids.len());
    Ok(done)
}

/// `AfterSettled`: until the registry counts every vote the ledger accepted
/// for `pid` and neither node has one pending, or nothing is pending and
/// nothing landed for `SETTLED_STABLE` (an errored vote never settles).
async fn wait_settled(
    org: &Organizer,
    nodes: &[SequencerClient; 2],
    ledger: &Ledger,
    sub: &Submitter,
    pid: &ProcessId,
    tag: &str,
) -> Result<()> {
    let start = Instant::now();
    let mut last = (u64::MAX, Instant::now());
    loop {
        if sub.stopped() {
            return Ok(());
        }
        let accepted = ledger
            .entries()?
            .values()
            .filter(|e| e.pid == *pid && !e.accepted.is_empty())
            .count() as u64;
        if let Ok(p) = org.process(&pid.0).await {
            let settled = p.voters_count + p.overwritten_votes_count;
            let mut idle = true;
            for n in nodes {
                idle &= n
                    .process(&pid.0)
                    .await
                    .is_ok_and(|v| v.pending_votes == Some(0));
            }
            if settled >= accepted && idle {
                return Ok(());
            }
            if settled != last.0 {
                last = (settled, Instant::now());
            }
            if idle && last.1.elapsed() >= SETTLED_STABLE {
                println!(
                    "{tag}: {settled} of {accepted} votes settled, none pending for {} s: ending it",
                    SETTLED_STABLE.as_secs()
                );
                return Ok(());
            }
        }
        if start.elapsed() >= SETTLED_MAX {
            println!("{tag}: not all settled after {SETTLED_MAX:?}: ending it");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// One ceiling lane: each burst once the previous one has sealed on every
/// node it went to (or that node synced the election since).
#[allow(clippy::too_many_arguments)]
async fn ceiling_lane(
    sub: Arc<Submitter>,
    events: Events,
    tag: String,
    pid: String,
    route: Route,
    bursts: Vec<(usize, Vec<BallotRecord>)>,
    t0: Instant,
) -> Result<SubmitStats> {
    let nodes: Vec<NodeId> = match route {
        Route::Only(n) => vec![n],
        _ => vec![NodeId::A, NodeId::B],
    };
    let mut st = SubmitStats::default();
    // The previous burst: its size and when it went out.
    let mut prev = 0u64;
    let mut since = unix_now();
    for (k, (_, recs)) in bursts.into_iter().enumerate() {
        if k > 0 {
            let wait = Instant::now();
            loop {
                let ok = {
                    let ev = lock(&events);
                    nodes.iter().all(|n| {
                        let (sealed, synced) = sealed_or_synced(&ev, role_of(*n), &pid, since);
                        sealed >= prev || synced
                    })
                };
                if ok || sub.stopped() {
                    break;
                }
                if wait.elapsed() >= BURST_MAX {
                    println!("{tag}: burst {k} goes out without the previous one sealed");
                    break;
                }
                tokio::time::sleep(LOG_POLL).await;
            }
        }
        if sub.stopped() {
            break;
        }
        let n = recs.len() as u64;
        since = unix_now();
        let at = t0.elapsed();
        let s = sub
            .run(recs.into_iter().map(|r| (at, r)).collect(), route, t0)
            .await?;
        st.sent += s.sent;
        st.accepted += s.accepted;
        st.uncertain += s.uncertain;
        for (c, x) in s.refused {
            *st.refused.entry(c).or_default() += x;
        }
        prev = n;
    }
    Ok(st)
}

/// An `AfterSettled` end: once the lane's votes settled, end the election.
/// Returns when it ended, or why ending failed; nothing after a stop.
#[allow(clippy::too_many_arguments)]
async fn end_after_settled(
    org: &Organizer,
    lk: &tokio::sync::Mutex<()>,
    api: &[SequencerClient; 2],
    ledger: &Ledger,
    sub: &Submitter,
    pid: &ProcessId,
    tag: &str,
    end: &End,
) -> Result<(Option<f64>, Option<String>)> {
    if *end != End::AfterSettled || sub.stopped() {
        return Ok((None, None));
    }
    wait_settled(org, api, ledger, sub, pid, tag).await?;
    if sub.stopped() {
        return Ok((None, None));
    }
    Ok(
        match close_with_retries(org, lk, pid, &Close::End(end.clone())).await {
            Ok(()) => {
                println!("{tag}: ended after settling");
                (Some(unix_now()), None)
            }
            Err(e) => {
                println!("{tag}: end failed: {e}");
                (None, Some(e))
            }
        },
    )
}

/// Why a scenario stopped early, or never started.
#[derive(Clone, Debug, PartialEq)]
enum Stop {
    /// An account reached its cap: the run ends.
    Budget(String),
    Fees(String),
    /// The scenario's bound does not fit a key's room: skipped, the run
    /// goes on (a later scenario may fit).
    Fit(String),
}

impl Stop {
    fn label(&self) -> String {
        match self {
            Stop::Budget(w) | Stop::Fit(w) => format!("budget: {w}"),
            Stop::Fees(w) => format!("on fees: {w}"),
        }
    }
}

/// Spec §13's stop rule ahead of a scenario: its posts can cost a key up to
/// `need` wei, which must fit what each of A and B may still spend.
pub fn fits(b: &Budget, now: &Balances, need: u128) -> std::result::Result<(), String> {
    let short: Vec<String> = ["seq-a.l1", "seq-b.l1"]
        .into_iter()
        .zip(b.room_l1(now))
        .filter(|(_, room)| U256::from(need) > *room)
        .map(|(n, room)| {
            let room = u128::try_from(room).map_or(f64::INFINITY, |r| r as f64);
            format!("{n} may spend {:.1} µETH more", room / 1e12)
        })
        .collect();
    if short.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "its posts can cost up to {:.1} µETH per key; {}",
            need as f64 / 1e12,
            short.join(", ")
        ))
    }
}

/// Ethereum's fees, when there is an Ethereum to read.
async fn read_fees(net: &Net) -> Option<Fees> {
    let p = net.l1.as_ref()?.provider().ok()?;
    money::fees(&p).await.ok()
}

/// A fee breach, live only (anvil's fees mean nothing); fees that cannot be
/// read are one.
fn fee_breach(net: &Net, f: Option<Fees>) -> Option<String> {
    if !net.live || net.l1.is_none() {
        return None;
    }
    match f {
        Some(f) => money::gate(&f).err(),
        None => Some("Ethereum fees unreadable".into()),
    }
}

async fn balances(net: &Net) -> Result<Balances> {
    let op = op_provider(net)?;
    let seq = |i: usize| {
        net.sequencers
            .get(i)
            .map(|s| s.1)
            .context("two sequencer keys")
    };
    let l1 = match &net.l1 {
        Some(l) if l.keys.len() >= 2 => {
            let p = l.provider()?;
            [
                p.get_balance(l.keys[0].1).await?,
                p.get_balance(l.keys[1].1).await?,
            ]
        }
        _ => [U256::ZERO; 2],
    };
    Ok(Balances {
        l1,
        op: [
            op.get_balance(seq(0)?).await?,
            op.get_balance(seq(1)?).await?,
        ],
        organizer_op: op.get_balance(net.organizer.address()).await?,
    })
}

fn budget_breach(ctx: &Ctx, now: &Balances) -> Option<String> {
    ctx.budget.as_ref().and_then(|b| b.check(now).err())
}

/// The fee gate and the budget before a scenario: `None` on a go; a budget
/// breach stops at once, a fee breach after `GATE_WAIT`. On a fee go,
/// `need` (the scenario's per-key bound at those fees, wei) must fit what
/// each key may still spend, or the scenario is skipped.
async fn gate_wait(ctx: &Ctx, need: impl Fn(&Fees) -> u128) -> Result<Option<Stop>> {
    let start = Instant::now();
    loop {
        let now = match &ctx.budget {
            Some(_) => Some(balances(&ctx.net).await?),
            None => None,
        };
        if let Some(w) = now.as_ref().and_then(|n| budget_breach(ctx, n)) {
            return Ok(Some(Stop::Budget(w)));
        }
        let fees = read_fees(&ctx.net).await;
        let Some(w) = fee_breach(&ctx.net, fees) else {
            if let (Some(b), Some(n), Some(f)) = (&ctx.budget, &now, &fees)
                && let Err(w) = fits(b, n, need(f))
            {
                return Ok(Some(Stop::Fit(w)));
            }
            return Ok(None);
        };
        if start.elapsed() >= GATE_WAIT {
            return Ok(Some(Stop::Fees(w)));
        }
        println!("fee gate no-go ({w}); checking again in 60 s");
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

pub(super) fn prover_url(rig: &Rig, r: Role) -> Result<String> {
    Ok(rig
        .proxies(r)
        .prover
        .as_ref()
        .context("no prover proxy")?
        .upstream
        .clone())
}

async fn provers_idle(rig: &Rig) -> Result<()> {
    let urls = [prover_url(rig, Role::A)?, prover_url(rig, Role::B)?];
    crate::wait::until(
        "both provers idle",
        IDLE_WAIT,
        Duration::from_secs(30),
        || async {
            for u in &urls {
                let h = ProverClient::new(u)
                    .health()
                    .await
                    .map_err(|e| anyhow!("prover health: {e}"))?;
                if h.status != "ok" || h.queue_len != 0 {
                    return Ok(None);
                }
            }
            Ok(Some(()))
        },
    )
    .await
}

/// The progress counters from the ledger and the log events of `pids`.
#[allow(clippy::too_many_arguments)]
fn progress(
    tag: &str,
    planned: u64,
    entries: &BTreeMap<u64, Entry>,
    events: &[(Role, f64, LogKind)],
    pids: &BTreeSet<String>,
    now: f64,
    spent_l1_ueth: [u64; 2],
    fees: Option<Fees>,
) -> Progress {
    let mut p = Progress {
        tag: tag.into(),
        planned,
        spent_l1_ueth,
        ..Progress::default()
    };
    for e in entries.values() {
        p.sent += (e.accepted.len() + e.uncertain.len() + e.refused.len()) as u64;
        if !e.accepted.is_empty() {
            p.accepted += 1;
        } else if !e.refused.is_empty() && e.uncertain.is_empty() {
            p.refused += 1;
        }
    }
    let mut recent = 0;
    for (r, at, k) in events {
        if !kind_pid(k).is_some_and(|x| pids.contains(x)) {
            continue;
        }
        let i = match r {
            Role::A => 0,
            Role::B => 1,
            Role::O => continue,
        };
        match k {
            LogKind::Settled { votes, .. } => {
                p.settled += votes;
                p.tx[i] += 1;
                if *at >= now - 300.0 {
                    recent += votes;
                }
            }
            LogKind::LostEvent { .. } | LogKind::LostRevert { .. } => p.lost[i] += 1,
            LogKind::Isolated { .. } => p.errored += 1,
            LogKind::RefreshOverflow { votes, .. } => p.errored += votes,
            _ => {}
        }
    }
    p.open = p.accepted.saturating_sub(p.settled + p.errored);
    p.per_min = recent as f64 / 5.0;
    if let Some(f) = fees {
        (p.base, p.tip, p.blob) = (f.base as f64 / 1e9, f.tip as f64 / 1e9, f.blob as f64 / 1e9);
    }
    p
}

/// What a progress line needs; it goes to stderr and `progress.log`.
struct Liner {
    name: String,
    planned: u64,
    pids: BTreeSet<String>,
    bal0: Balances,
    t0: Instant,
    out: Mutex<std::fs::File>,
}

impl Liner {
    /// Prints one line; returns the balances and fees it read, for the
    /// budget and the fee gate.
    async fn print(
        &self,
        ctx: &Ctx,
        ledger: &Ledger,
        logs: &LogReader,
    ) -> Result<(Option<Balances>, Option<Fees>)> {
        let now = balances(&ctx.net).await.ok();
        let fees = read_fees(&ctx.net).await;
        let spent = now.as_ref().map_or([0; 2], |b| {
            [0, 1].map(|i| ueth(self.bal0.l1[i], b.l1[i]).max(0) as u64)
        });
        let line = progress(
            &self.name,
            self.planned,
            &ledger.entries()?,
            &logs.snapshot(),
            &self.pids,
            unix_now(),
            spent,
            fees,
        )
        .line(self.t0.elapsed());
        eprintln!("{line}");
        if let Ok(mut f) = self.out.lock() {
            let _ = writeln!(f, "{line}");
        }
        Ok((now, fees))
    }
}

/// One progress tick: the line, then the budget (a breach stops) and the
/// fee gate (a breach pauses new sends, and stops them after `FEE_PAUSE`).
async fn tick_once(
    ctx: &Ctx,
    name: &str,
    line: &Liner,
    ledger: &Ledger,
    logs: &LogReader,
    sub: &Submitter,
    fee_since: &mut Option<Instant>,
) -> Result<Option<Stop>> {
    let (now, fees) = line.print(ctx, ledger, logs).await?;
    if let Some(w) = now.as_ref().and_then(|b| budget_breach(ctx, b)) {
        return Ok(Some(Stop::Budget(w)));
    }
    if let Some(w) = fee_breach(&ctx.net, fees) {
        let since = *fee_since.get_or_insert_with(Instant::now);
        sub.pause(true);
        if since.elapsed() >= FEE_PAUSE {
            return Ok(Some(Stop::Fees(w)));
        }
        eprintln!("{name}: fee gate no-go ({w}): sends paused");
    } else if fee_since.take().is_some() {
        eprintln!("{name}: fee gate go: sends resume");
        sub.pause(false);
    }
    Ok(None)
}

/// The start line's spend estimate: the busier key's bound at the quoted
/// fees and, live, at Ethereum's current ones.
fn estimate(sc: &Scenario, specs: &[ElectionSpec], scale: f64, live: Option<Fees>) -> String {
    let b = post_bound(sc, specs, scale);
    let at = |exec, blob| {
        b.map(|x| x.ueth(exec, blob))
            .into_iter()
            .fold(0.0, f64::max)
    };
    let mut s = format!(
        "est. L1 <= {:.0} µETH per key ({} posts, {} blobs) at 0.1 gwei, blob 0.004 gwei",
        at(EST_EXEC_WEI, EST_BLOB_WEI),
        b[0].txs.max(b[1].txs),
        b[0].blobs.max(b[1].blobs)
    );
    if let Some(f) = live {
        s.push_str(&format!(
            ", {:.0} at current fees",
            at(f.base + f.tip, f.blob)
        ));
    }
    s
}

fn ueth(start: U256, now: U256) -> i64 {
    let d = |a: U256, b: U256| {
        u64::try_from((a - b) / U256::from(1_000_000_000_000u64)).map_or(i64::MAX, |v| v as i64)
    };
    if start >= now {
        d(start, now)
    } else {
        -d(now, start)
    }
}

/// Undoes what the controller in `slot` injected, if there is one.
pub async fn finish_chaos(slot: &mut ChaosSlot, rig: &mut Rig, net: &Net) -> Result<()> {
    match slot.as_mut() {
        Some((c, t0)) => {
            let r = c.finish(rig, net, t0.elapsed()).await;
            *slot = None;
            r
        }
        None => Ok(()),
    }
}

/// Undoes what the controller in `slot` injected, puts its rows in the
/// report and empties the slot; every row goes in even when an undo fails.
async fn close_chaos(
    slot: &mut ChaosSlot,
    rig: &mut Rig,
    net: &Net,
    report: &mut ScenarioReport,
) -> Result<()> {
    let Some((c, t0)) = slot.as_mut() else {
        return Ok(());
    };
    let r = c.finish(rig, net, t0.elapsed()).await;
    for row in c.rows() {
        println!("chaos {}: {}", row.name, row.line());
        report.chaos.insert(row.name.clone(), row.line());
    }
    let unfired = c.unfired();
    for (n, l) in &unfired {
        println!("chaos {n}: {l}");
        report.chaos.insert(n.clone(), l.clone());
    }
    if !unfired.is_empty() {
        let names: Vec<&str> = unfired.iter().map(|(n, _)| n.as_str()).collect();
        report
            .params
            .insert("chaos never triggered".into(), names.join(", "));
    }
    *slot = None;
    r
}

/// One chaos poll: the events since the last one, timed from the scenario's
/// start.
async fn chaos_step(
    slot: &mut ChaosSlot,
    rig: &mut Rig,
    net: &Net,
    events: &[(Role, f64, LogKind)],
    seen: &mut usize,
    t0_unix: f64,
    first_end: Option<f64>,
) -> Result<()> {
    let Some((c, t0)) = slot.as_mut() else {
        return Ok(());
    };
    let new: Vec<(Role, f64, LogKind)> = events[(*seen).min(events.len())..]
        .iter()
        .map(|(r, at, k)| (*r, at - t0_unix, k.clone()))
        .collect();
    *seen = events.len();
    let seals = [Role::A, Role::B].map(|r| {
        events
            .iter()
            .filter(|(er, _, k)| *er == r && matches!(k, LogKind::Sealed { .. }))
            .count() as u64
    });
    let o = Observed {
        elapsed: t0.elapsed(),
        ended_for: first_end.map(|e| Duration::from_secs_f64((unix_now() - e).max(0.0))),
        pending_nonce_gap: if c.wants_pending() {
            match chaos::pending_gap(net).await {
                Ok(g) => g,
                // An entry waiting on it cannot fire: say so, and the
                // report lists it as never triggered.
                Err(e) => {
                    let e = crate::redact_urls(&format!("{e:#}"));
                    if c.pending_error(&e) {
                        println!(
                            "chaos: pending-nonce read failed ({} so far): {e}",
                            c.pending_errors()
                        );
                    }
                    [false; 2]
                }
            }
        } else {
            [false; 2]
        },
        new_seals: seals,
    };
    c.step(rig, net, &o, &new).await
}

/// A race scenario's rows: `races.json` and the report's races table.
fn race_report(r: &mut ScenarioReport, dir: &Path, sc: &Scenario, o: &race::RaceOut) -> Result<()> {
    std::fs::write(dir.join("races.json"), serde_json::to_vec_pretty(&o.rows)?)?;
    let target = match &sc.kind {
        Kind::Race(s) => s,
        _ => return Ok(()),
    };
    r.races.insert(
        "races".into(),
        format!("{}/{} in {} rounds", o.races(), target.rounds, o.rows.len()),
    );
    for row in &o.rows {
        r.races
            .insert(format!("round {:02}", row.round), row.line());
    }
    if let Some((n, d)) = target.skew {
        r.races.insert(
            format!("prover {n} held {} s", d.as_secs()),
            format!("{} /prove held, {} dropped", o.held, o.dropped),
        );
    }
    for (i, n) in o.notes.iter().enumerate() {
        r.races.insert(format!("note {i}"), n.clone());
    }
    Ok(())
}

/// What `audit --run` needs of a run: written before the first send.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RunMeta {
    name: String,
    wave: u32,
    scale: f64,
    /// Unix seconds of the scenario's start (ledger `t` is from here).
    t0: f64,
    /// Per tag, the vote ids the run scheduled.
    planned: BTreeMap<String, Vec<String>>,
}

/// One election's audit.
struct Audited {
    tag: String,
    audit: AuditResult,
    rows: Vec<TransitionRow>,
    findings: Vec<Finding>,
}

/// Spec §11 for one election, once the observer and the queues are quiet
/// (or `max` passed): the chain's rows, every node's statuses, `classify`
/// over this election's ledger entries only, the tally, the results and the
/// registry counters.
#[allow(clippy::too_many_arguments)]
async fn audit_election(
    ctx: &Ctx,
    rig: &Rig,
    scenario: &str,
    e: &Election,
    entries: &BTreeMap<u64, Entry>,
    who: &HashMap<u64, (u32, Vec<u64>)>,
    planned: &BTreeSet<u64>,
    allowed: &[&str],
    events: &[(NodeId, LogKind)],
    max: Duration,
) -> Result<Audited> {
    let pid = e.snap.pid;
    let tag = e.spec.tag.as_str();
    // Other elections' votes would read as never landed.
    let mine: BTreeMap<u64, Entry> = entries
        .iter()
        .filter(|(_, en)| en.pid == pid)
        .map(|(v, en)| (*v, en.clone()))
        .collect();
    let (o, a, b) = (rig.node(Role::O)?, rig.node(Role::A)?, rig.node(Role::B)?);
    let q = chain_view::wait_quiet(
        o,
        [a, b],
        &ctx.net.rpc,
        ctx.net.registry,
        ctx.plan.start_block,
        &pid.0,
        &mine,
        max,
    )
    .await?;
    let op = op_provider(&ctx.net)?;
    let tr = chain_view::transitions(o, [a, b], &op, ctx.net.registry, &pid.0, e.spec.nf, &q.logs)
        .await
        .with_context(|| format!("{tag}: transitions"))?;
    let c = audit::classify(&mine, &tr.rows, &q.statuses, allowed, planned);
    let mut au = c.summary();
    let x = audit::expect(&tr.rows, &|v| who.get(&v).cloned(), usize::from(e.spec.nf));
    let org = ctx.net.organizer()?;
    let p = org.process(&pid.0).await?;
    let results = org.results(&pid.0).await?;
    let mut views = Vec::new();
    for (name, r) in [("a", Role::A), ("b", Role::B), ("o", Role::O)] {
        let v = rig
            .node(r)?
            .api
            .process(&pid.0)
            .await
            .ok()
            .and_then(|v| v.result);
        views.push((name.to_string(), v));
    }
    let published = Published {
        results: results.clone(),
        views,
        voters_count: p.voters_count,
        overwritten_votes_count: p.overwritten_votes_count,
    };
    au.check(&x, &tr.rows, &published, &tr.from_fallback);
    au.notes
        .extend(tr.notes.iter().map(|n| format!("{tag} {n}")));
    if q.timed_out {
        au.notes.push(format!(
            "{tag}: quiet wait timed out: observer {} of {} transitions, pendingVotes {:?}, {} votes open",
            q.observer, q.chain, q.pending, q.open
        ));
    }
    if p.status == davinci_client::api::ProcessStatus::Canceled {
        au.notes
            .push(format!("{tag}: canceled before results; none expected"));
    } else if results.is_none() {
        let key = rig.node(role_of(e.spec.key_node))?;
        let ph = pid_hex(&pid);
        let lines: Vec<String> = key
            .log_lines(&["finalize", "results"])
            .into_iter()
            .filter(|l| l.contains(&ph))
            .collect();
        let tail = &lines[lines.len().saturating_sub(10)..];
        au.notes.push(format!(
            "{tag}: no results; node {} finalize lines: {}",
            e.spec.key_node,
            if tail.is_empty() {
                "none".into()
            } else {
                tail.join(" | ")
            }
        ));
    }
    let ph = pid_hex(&pid);
    let evs: Vec<(NodeId, LogKind)> = events
        .iter()
        .filter(|(_, k)| kind_pid(k) == Some(ph.as_str()))
        .cloned()
        .collect();
    let mut findings = report::findings(scenario, tag, &au, &evs);
    if p.status == davinci_client::api::ProcessStatus::Canceled {
        findings = canceled(findings, scenario, tag);
    }
    println!(
        "audit {tag}: no_vote_lost={} clean={} tally_ok={} results_ok={} counters_ok={}{}",
        au.no_vote_lost,
        au.clean,
        au.tally_ok,
        au.results_ok,
        au.counters_ok,
        if au.observer_incomplete {
            " (observer incomplete)"
        } else {
            ""
        }
    );
    match (&results, &x) {
        (Some(_), Ok(_)) if au.results_ok => println!("results {tag} = tally"),
        (Some(r), Ok(x)) => println!("results {tag} = {r:?}, expected {:?}", x.tally),
        (None, Ok(x)) => println!("results {tag}: none, expected {:?}", x.tally),
        (_, Err(e)) => println!("results {tag}: no expected tally ({e:#})"),
    }
    if !au.errors.is_empty() {
        println!("errors {tag}: {:?}", au.errors);
    }
    Ok(Audited {
        tag: tag.into(),
        audit: au,
        rows: tr.rows,
        findings,
    })
}

/// A canceled election publishes no results: its missing results are no
/// `results_failed` but one `canceled` row.
fn canceled(mut f: Vec<Finding>, scenario: &str, tag: &str) -> Vec<Finding> {
    f.retain(|x| x.kind != report::FindingKind::ResultsFailed);
    f.push(Finding {
        scenario: scenario.into(),
        tag: tag.into(),
        kind: report::FindingKind::Canceled,
        count: 1,
        detail: "canceled before results".into(),
    });
    f
}

/// Folds one election's audit into the scenario's.
fn merge_audit(into: &mut AuditResult, a: &AuditResult) {
    for (k, v) in &a.counts {
        *into.counts.entry(k.clone()).or_default() += v;
    }
    for (k, v) in &a.errors {
        *into.errors.entry(k.clone()).or_default() += v;
    }
    for (k, v) in &a.unexpected {
        *into.unexpected.entry(k.clone()).or_default() += v;
    }
    for (k, v) in &a.stale_errors {
        *into.stale_errors.entry(*k).or_default() += v;
    }
    into.no_vote_lost &= a.no_vote_lost;
    into.clean &= a.clean;
    into.tally_ok &= a.tally_ok;
    into.results_ok &= a.results_ok;
    into.counters_ok &= a.counters_ok;
    into.observer_incomplete |= a.observer_incomplete;
    into.notes.extend(a.notes.iter().cloned());
}

fn passing() -> AuditResult {
    AuditResult {
        no_vote_lost: true,
        clean: true,
        tally_ok: true,
        results_ok: true,
        counters_ok: true,
        ..AuditResult::default()
    }
}

fn print_findings(f: &[Finding]) {
    if f.is_empty() {
        println!("findings: none");
        return;
    }
    println!("findings: {}", f.len());
    for x in f {
        let kind = serde_json::to_value(x.kind)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        println!("findings {}: {kind} {} {}", x.tag, x.count, x.detail);
    }
}

/// `(voter, choices)` of every ballot's vote id.
fn who_of(recs: &[BallotRecord]) -> HashMap<u64, (u32, Vec<u64>)> {
    recs.iter()
        .map(|r| (r.req.vote_id, (r.voter, r.choices.clone())))
        .collect()
}

/// Every run report of the campaign, by run directory name.
fn load_reports(campaign: &Path) -> Result<Vec<ScenarioReport>> {
    let mut dirs: Vec<PathBuf> = match std::fs::read_dir(campaign.join("runs")) {
        Ok(d) => d.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort();
    let mut out = Vec::new();
    for d in dirs {
        let p = d.join("report.json");
        if p.exists() {
            out.push(
                serde_json::from_slice(&std::fs::read(&p)?)
                    .with_context(|| format!("parse {}", p.display()))?,
            );
        }
    }
    Ok(out)
}

/// One transition with its prover job, Ethereum leg and costs.
#[derive(Clone, Debug, Serialize)]
struct BatchRow {
    tag: String,
    index: u64,
    block: u64,
    ts: u64,
    node: String,
    votes: u64,
    overwrites: u64,
    blobs: u64,
    job: Option<String>,
    proof_start: Option<f64>,
    proof_end: Option<f64>,
    prove_s: Option<f64>,
    /// L1-blob mode: the Ethereum block time of the last blob.
    l1_ts: Option<u64>,
    l1_txs: Vec<String>,
    l1_wei: Option<String>,
    op_wei: Option<String>,
}

/// The batch rows of `rows` (per tag): the job that proved each (by its
/// root), its Ethereum posts and both legs' costs.
async fn batch_rows(
    ctx: &Ctx,
    rows: &[(String, TransitionRow)],
    jobs: &[JobRow],
) -> Result<Vec<BatchRow>> {
    let net = &ctx.net;
    let by_root: HashMap<[u8; 32], &JobRow> = jobs
        .iter()
        .filter(|j| j.root_after != [0; 32])
        .map(|j| (j.root_after, j))
        .collect();
    let ours: BTreeSet<[u8; 31]> = rows.iter().map(|(_, r)| r.pid).collect();
    let posts: HashMap<B256, l1::L1Post> = match &net.l1 {
        Some(l) => l1::posts(&net.rpc, l.read_rpc(), net.registry, ctx.plan.start_block)
            .await?
            .into_iter()
            .filter(|p| ours.contains(&p.pid))
            .map(|p| (p.op_tx, p))
            .collect(),
        None => HashMap::new(),
    };
    let l1p = net.l1.as_ref().map(|l| l.provider()).transpose()?;
    let mut l1_ts: HashMap<u64, u64> = HashMap::new();
    let a = net.sequencers.first().map(|s| s.1);
    let mut out = Vec::new();
    for (tag, r) in rows {
        let job = by_root.get(&r.new_root);
        let mut row = BatchRow {
            tag: tag.clone(),
            index: r.index,
            block: r.block,
            ts: r.ts,
            node: if Some(r.sender.into()) == a { "a" } else { "b" }.into(),
            votes: r.voters,
            overwrites: r.overwrites,
            blobs: r.n_blobs,
            job: job.map(|j| j.job.clone()),
            proof_start: job.map(|j| j.started),
            proof_end: job.map(|j| j.finished),
            prove_s: job.map(|j| j.secs),
            l1_ts: None,
            l1_txs: Vec::new(),
            l1_wei: None,
            op_wei: cost::fetch(&net.rpc, "", B256::from(r.tx))
                .await
                .ok()
                .map(|c| c.wei().to_string()),
        };
        if let (Some(p), Some(l1p), Some(l)) = (posts.get(&B256::from(r.tx)), &l1p, &net.l1) {
            if let Some(&b) = p.l1_blocks.iter().max() {
                let t = match l1_ts.get(&b) {
                    Some(t) => *t,
                    None => {
                        let t = l1p
                            .get_block_by_number(b.into())
                            .await?
                            .with_context(|| format!("L1 block {b}"))?
                            .header
                            .timestamp;
                        l1_ts.insert(b, t);
                        t
                    }
                };
                row.l1_ts = Some(t);
            }
            let mut wei = U256::ZERO;
            for tx in &p.l1_txs {
                row.l1_txs.push(tx.to_string());
                if let Ok(c) = cost::fetch(l.read_rpc(), "", *tx).await {
                    wei += c.wei();
                }
            }
            row.l1_wei = Some(wei.to_string());
        }
        out.push(row);
    }
    Ok(out)
}

/// Per-vote stages (spec §12) from its submit to its transition's landing:
/// queued (to the proof's start), proving, the Ethereum leg (from the
/// proof's start, which it overlaps) and the OP leg (from the later of the
/// proof's end and the last blob), each as p50, p90, p99 and max.
fn latency(
    t0: f64,
    entries: &BTreeMap<u64, Entry>,
    rows: &[(String, TransitionRow)],
    batches: &[BatchRow],
) -> BTreeMap<String, [f64; 4]> {
    let mut v: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for ((_, r), b) in rows.iter().zip(batches) {
        for vid in &r.vids {
            let Some(sub) = entries.get(vid).and_then(|e| e.first_accept) else {
                continue;
            };
            let sub = t0 + sub;
            let ts = r.ts as f64;
            v.entry("total").or_default().push((ts - sub).max(0.0));
            if let (Some(s), Some(f)) = (b.proof_start, b.proof_end) {
                v.entry("queued").or_default().push((s - sub).max(0.0));
                v.entry("proving").or_default().push((f - s).max(0.0));
                let mut op_from = f;
                if let Some(l) = b.l1_ts {
                    v.entry("l1").or_default().push((l as f64 - s).max(0.0));
                    op_from = op_from.max(l as f64);
                }
                v.entry("op").or_default().push((ts - op_from).max(0.0));
            }
        }
    }
    v.into_iter()
        .map(|(k, xs)| {
            let p = metrics::percentiles(&xs, &[50.0, 90.0, 99.0, 100.0]);
            (k.to_string(), [p[0], p[1], p[2], p[3]])
        })
        .collect()
}

/// `/info` counters by node name, summed over each node's incarnations.
pub(super) async fn info(rig: &Rig) -> BTreeMap<String, BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    for (name, r) in [("a", Role::A), ("b", Role::B), ("o", Role::O)] {
        if let Some(c) = rig.counters(r).await {
            out.insert(name.to_string(), c);
        }
    }
    out
}

/// counters_ok's node part (spec §11), from deltas over the scenario summed
/// over each node's incarnations: A's and B's `settledBySelf` against the
/// transitions each sent, O's `syncedFromOthers` against all of them. A
/// killed node's settlement can land while it is down, and its next
/// incarnation counts that one as synced, so a restarted node may fall short
/// by up to its restarts (noted). A node whose counters a kill could not
/// read is not checked.
fn counters_check(
    settled: [Option<u64>; 2],
    sent: [u64; 2],
    restarts: [u32; 2],
    unread: [bool; 3],
    synced_o: Option<u64>,
    n_tx: u64,
    observer_incomplete: bool,
) -> (bool, Vec<String>) {
    let mut ok = true;
    let mut notes = Vec::new();
    for (i, n) in ["A", "B"].into_iter().enumerate() {
        if unread[i] {
            notes.push(format!(
                "settledBySelf ({n}) not checked: a kill could not read its counters"
            ));
            continue;
        }
        match settled[i] {
            Some(d) if d == sent[i] => {}
            Some(d) if d < sent[i] && sent[i] - d <= u64::from(restarts[i]) => {
                notes.push(format!(
                    "settledBySelf ({n}) {d} of the {} transitions it sent, after {} restarts: a settlement that landed while it was down counts as synced",
                    sent[i], restarts[i]
                ))
            }
            d => {
                ok = false;
                notes.push(format!(
                    "settledBySelf ({n}) {d:?} over the scenario, {} transitions sent by it",
                    sent[i]
                ));
            }
        }
    }
    if observer_incomplete {
        notes.push("syncedFromOthers (O) not checked: observer incomplete".into());
    } else if unread[2] {
        notes.push("syncedFromOthers (O) not checked: a kill could not read its counters".into());
    } else if synced_o != Some(n_tx) {
        ok = false;
        notes.push(format!(
            "syncedFromOthers (O) {synced_o:?} over the scenario, {n_tx} transitions"
        ));
    }
    (ok, notes)
}

/// The scenario's injections the rig can make. Live, an action naming a
/// proxy the rig lacks is an error; on anvil (one RPC, no beacon) it is
/// skipped and reported.
fn usable_chaos(
    name: &str,
    sc: &Scenario,
    have: &[[usize; 4]; 3],
    live: bool,
    report: &mut ScenarioReport,
) -> Result<Vec<chaos::Chaos>> {
    let mut out = Vec::new();
    for c in &sc.chaos {
        if chaos::references_exist(&c.action, have) {
            if let chaos::Action::RpcDown { node, rpc, .. }
            | chaos::Action::RpcFail { node, rpc, .. } = &c.action
                && have[*node as usize][*rpc as usize] < 2
            {
                println!(
                    "{name}: {}: the list has no other entry to fail over to",
                    c.name
                );
            }
            out.push(c.clone());
            continue;
        }
        ensure!(
            !live,
            "{name}: {} names a proxy the rig does not have",
            c.name
        );
        println!("{name}: {} skipped: no such proxy on anvil", c.name);
        report
            .chaos
            .insert(c.name.clone(), "skipped: no such proxy on anvil".into());
    }
    Ok(out)
}

fn counter(m: &BTreeMap<String, BTreeMap<String, u64>>, node: &str, k: &str) -> Option<u64> {
    m.get(node)?.get(k).copied()
}

/// A scenario that never sent: its report says why (spec §13), and a rerun
/// may use its run directory.
fn skipped(
    ctx: &Ctx,
    dir: &Path,
    mut report: ScenarioReport,
    key: &str,
    why: String,
) -> Result<ScenarioReport> {
    report.params.insert(key.into(), why);
    report::write_run(dir, &report)?;
    report::write_campaign(&ctx.campaign, &load_reports(&ctx.campaign)?)?;
    Ok(report)
}

/// Runs `sc` with no controller held outside; see [`run_scenario_in`].
pub async fn run_scenario(ctx: &Ctx, rig: &mut Rig, sc: &Scenario) -> Result<ScenarioReport> {
    let mut slot = None;
    let r = run_scenario_in(ctx, rig, sc, &mut slot).await;
    let f = finish_chaos(&mut slot, rig, &ctx.net).await;
    match (r, f) {
        (Err(e), Err(d)) => Err(e.context(format!("chaos teardown also failed: {d:#}"))),
        (r, Ok(())) => r,
        (Ok(_), Err(d)) => Err(d.context("chaos teardown")),
    }
}

/// One scenario, stages 1 to 11 of the plan: gate, idle provers, knobs,
/// served elections, ballots, sends (with the progress line, the fee gate
/// and the budget every 30 s), ends, results, the audit, the batch rows,
/// and the run and campaign reports. A controller for `sc.chaos` lives in
/// `chaos` so the caller can finish it on any exit.
pub async fn run_scenario_in(
    ctx: &Ctx,
    rig: &mut Rig,
    sc: &Scenario,
    chaos: &mut ChaosSlot,
) -> Result<ScenarioReport> {
    // A dry runs on anvil, whose OP blocks are 1 s.
    let anvil_sc;
    let sc = if ctx.net.live {
        sc
    } else {
        anvil_sc = on_anvil(sc.clone());
        &anvil_sc
    };
    let wave = ctx.plan.wave(sc.wave)?;
    // The scale the elections were planned at: bursts are sized from it.
    let scale = wave.scale;
    let specs: Vec<ElectionSpec> = wave.elections.iter().map(|e| e.spec.clone()).collect();
    let name = format!("w{}/{}", sc.wave, sc.name);
    let dir = run_dir(&ctx.campaign, sc.wave, &sc.name);
    let mut report = ScenarioReport {
        name: name.clone(),
        ..ScenarioReport::default()
    };
    let p = &mut report.params;
    p.insert("wave".into(), sc.wave.to_string());
    p.insert("scale".into(), scale.to_string());
    for (n, k) in ["a", "b"].iter().zip(&sc.knobs) {
        p.insert(
            format!("knobs {n}"),
            format!("batch_max {} batch_time {}", k.batch_max, k.batch_time),
        );
    }
    let mut lanes: Vec<(&Lane, &Election)> = Vec::new();
    for l in &sc.lanes {
        match wave.elections.iter().find(|e| e.spec.tag == l.tag) {
            Some(e) => lanes.push((l, e)),
            None => println!("{name}: {} skipped: not in plan.json", l.tag),
        }
    }
    if lanes.is_empty() {
        println!("{name}: skipped: no planned election");
        return skipped(ctx, &dir, report, "skipped", "no planned election".into());
    }
    // A skip writes only a report; a ledger means sends went out.
    ensure!(
        !dir.join("ledger.jsonl").exists(),
        "{} exists: {name} already ran and its ballots are spent (rerun it in a new wave)",
        dir.display()
    );
    let tags: Vec<String> = lanes.iter().map(|(l, _)| l.tag.clone()).collect();
    report.params.insert("elections".into(), tags.join(","));

    // 1. Fee gate and budget, and the scenario's bound against the room
    // each key has left.
    let need = |f: &Fees| {
        post_bound(sc, &specs, scale)
            .map(|b| b.wei(f.base + f.tip, f.blob))
            .into_iter()
            .max()
            .unwrap_or(0)
    };
    if let Some(s) = gate_wait(ctx, need).await? {
        let key = if matches!(s, Stop::Budget(_)) {
            println!("{name}: stopped {}", s.label());
            "stopped"
        } else {
            println!("{name}: skipped: {}", s.label());
            "skipped"
        };
        return skipped(ctx, &dir, report, key, s.label());
    }
    // 2. Provers idle.
    provers_idle(rig).await?;
    // 3. Knobs: a restart re-seeds the proving estimate, so only on a change.
    for (i, r) in [Role::A, Role::B].into_iter().enumerate() {
        if rig.knobs(r) != Some(&sc.knobs[i]) {
            println!(
                "node {}: restart with batch_max {} batch_time {}",
                ["a", "b"][i],
                sc.knobs[i].batch_max,
                sc.knobs[i].batch_time
            );
            rig.restart_with(r, sc.knobs[i].clone()).await?;
        }
    }
    // 4. Every election listed and served on A and B, and the injections
    // the rig can make.
    for (_, e) in &lanes {
        plan::wait_served(rig, e, &ctx.seed).await?;
    }
    let injections = usable_chaos(&name, sc, &rig.url_counts(), ctx.net.live, &mut report)?;
    // 5. Ballots and their order.
    enum Sends {
        Plain(Vec<(Duration, BallotRecord)>),
        Bursts(Vec<(usize, Vec<BallotRecord>)>),
        Rounds(Vec<Vec<BallotRecord>>),
    }
    if let Kind::Race(_) = &sc.kind {
        // The rounds borrow the rig beside the progress ticks: no chaos.
        ensure!(
            lanes.len() == 1 && sc.chaos.is_empty(),
            "{name}: a race runs one election and no chaos"
        );
    }
    let mut sends = Vec::new();
    let mut who: BTreeMap<String, HashMap<u64, (u32, Vec<u64>)>> = BTreeMap::new();
    let mut planned: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let mut cell_of: HashMap<u64, usize> = HashMap::new();
    for (l, e) in &lanes {
        let recs = load_ballots(&ctx.campaign, sc.wave, e)?;
        who.insert(l.tag.clone(), who_of(&recs));
        let s = match &sc.kind {
            Kind::Plain => Sends::Plain(lane_plan(l, recs, lane_seed(&l.tag))?),
            Kind::Ceiling(cells) => {
                let b = bursts(cells, scale, recs)?;
                for (cell, recs) in &b {
                    cell_of.extend(recs.iter().map(|r| (r.req.vote_id, *cell)));
                }
                Sends::Bursts(b)
            }
            Kind::Race(r) => Sends::Rounds(race::rounds(r, recs)?),
        };
        let vids: BTreeSet<u64> = match &s {
            Sends::Plain(p) => p.iter().map(|(_, r)| r.req.vote_id).collect(),
            Sends::Bursts(b) => b
                .iter()
                .flat_map(|(_, r)| r.iter().map(|r| r.req.vote_id))
                .collect(),
            Sends::Rounds(r) => r.iter().flatten().map(|r| r.req.vote_id).collect(),
        };
        planned.insert(l.tag.clone(), vids);
        sends.push(s);
    }
    // A race sends only the rounds it needs, at least `rounds` of K.
    let n_planned: u64 = match &sc.kind {
        Kind::Race(r) => (r.k * r.rounds) as u64,
        _ => planned.values().map(|v| v.len() as u64).sum(),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let ledger = Arc::new(Ledger::create(&dir.join("ledger.jsonl"))?);
    let progress_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("progress.log"))?;
    let logs = LogReader::start(rig)?;
    let org = Arc::new(ctx.net.organizer()?);
    let org_lock = Arc::new(tokio::sync::Mutex::new(()));
    let info0 = info(rig).await;
    let kills0 = [Role::A, Role::B, Role::O].map(|r| (rig.kills(r), rig.unread_kills(r)));
    let bal0 = balances(&ctx.net).await?;
    let calls0 = [Role::A, Role::B].map(|r| rig.prove_calls(r).len());
    let t0 = Instant::now();
    let t0_unix = unix_now();
    report.params.insert("t0".into(), format!("{t0_unix:.0}"));
    let meta = RunMeta {
        name: sc.name.clone(),
        wave: sc.wave,
        scale,
        t0: t0_unix,
        planned: planned
            .iter()
            .map(|(t, v)| (t.clone(), v.iter().map(|x| vote_id_hex(*x)).collect()))
            .collect(),
    };
    std::fs::write(dir.join("run.json"), serde_json::to_vec_pretty(&meta)?)?;
    if !injections.is_empty() {
        *chaos = Some((Controller::new(injections), t0));
    }
    let fees_now = if ctx.net.live {
        read_fees(&ctx.net).await
    } else {
        None
    };
    let est = estimate(sc, &specs, scale, fees_now);
    report.params.insert("estimate".into(), est.clone());
    println!(
        "{name}: {} elections, {n_planned} ballots, {est}, run dir {}",
        lanes.len(),
        dir.display()
    );

    // 6 and 7. Sends and ends, beside the progress line, the fee gate and
    // the budget.
    let sub = Arc::new(Submitter::new(
        &rig.node(Role::A)?.url,
        &rig.node(Role::B)?.url,
        ledger.clone(),
    ));
    let api = [
        rig.node(Role::A)?.api.clone(),
        rig.node(Role::B)?.api.clone(),
    ];
    let pids: BTreeSet<String> = lanes.iter().map(|(_, e)| pid_hex(&e.snap.pid)).collect();
    let mut set: JoinSet<Result<LaneDone>> = JoinSet::new();
    // The timed ends; a stop aborts them and closes every election itself.
    let mut timers = Vec::new();
    // A race lane runs in this task, beside the ticks.
    let mut race_lane = None;
    for ((l, e), s) in lanes.iter().zip(sends) {
        let s = match s {
            Sends::Rounds(r) => {
                race_lane = Some((*l, *e, r));
                continue;
            }
            s => s,
        };
        let (sub, tag, route, pid) = (sub.clone(), l.tag.clone(), l.route, e.snap.pid);
        let (events, ledger, api) = (logs.events.clone(), ledger.clone(), api.clone());
        let (org1, lk, end) = (org.clone(), org_lock.clone(), l.end.clone());
        set.spawn(async move {
            let org = org1;
            let st = match s {
                Sends::Plain(plan) => sub.run(plan, route, t0).await?,
                Sends::Bursts(b) => {
                    let pid = hex::encode(pid.0);
                    ceiling_lane(sub.clone(), events, tag.clone(), pid, route, b, t0).await?
                }
                Sends::Rounds(_) => return Err(anyhow!("{tag}: a race lane is not spawned")),
            };
            let (ended, end_failed) =
                end_after_settled(&org, &lk, &api, &ledger, &sub, &pid, &tag, &end).await?;
            Ok(LaneDone {
                tag,
                stats: Some(st),
                ended,
                end_failed,
            })
        });
        if let End::Shorten { after, .. } | End::Now { after } = l.end {
            let (org, lk, end, tag) = (org.clone(), org_lock.clone(), l.end.clone(), l.tag.clone());
            let at = t0 + l.start + after;
            timers.push(set.spawn(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                let r = close_with_retries(&org, &lk, &pid, &Close::End(end.clone())).await;
                Ok(match r {
                    Ok(()) => {
                        println!("{tag}: ended ({end:?})");
                        LaneDone {
                            tag,
                            stats: None,
                            ended: Some(unix_now()),
                            end_failed: None,
                        }
                    }
                    Err(e) => {
                        println!("{tag}: end failed: {e}");
                        LaneDone {
                            tag,
                            stats: None,
                            ended: None,
                            end_failed: Some(e),
                        }
                    }
                })
            }));
        }
    }
    let line = Liner {
        name: name.clone(),
        planned: n_planned,
        pids: pids.clone(),
        bal0: bal0.clone(),
        t0,
        out: Mutex::new(progress_log),
    };
    let mut done = Lanes::default();
    let mut stop: Option<Stop> = None;
    let mut fee_since: Option<Instant> = None;
    let mut seen = 0usize;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    let mut chaos_tick = tokio::time::interval(Duration::from_secs(1));
    let first_end = |ended: &BTreeMap<String, f64>| ended.values().copied().reduce(f64::min);
    let mut race_out: Option<race::RaceOut> = None;
    if let (Some((l, e, rounds)), Kind::Race(spec)) = (race_lane, &sc.kind) {
        let rc = race::RaceCtx {
            ctx,
            rig: &*rig,
            org: &org,
            sub: &sub,
            e,
            spec,
            t0,
        };
        let fut = race::race_lane(&rc, rounds);
        tokio::pin!(fut);
        let r = loop {
            tokio::select! {
                r = &mut fut => break Some(r),
                _ = tick.tick() => {
                    if let Some(s) = tick_once(ctx, &name, &line, &ledger, &logs, &sub, &mut fee_since).await? {
                        stop = Some(s);
                        // The rounds see the stop: sends give up, waits end.
                        sub.stop();
                        break tokio::time::timeout(STOP_DRAIN, &mut fut).await.ok();
                    }
                }
            }
        };
        match r {
            Some(Ok(out)) => race_out = Some(out),
            Some(Err(e)) if stop.is_none() => return Err(e),
            Some(Err(e)) => println!(
                "{name}: the race lane ended with {}",
                crate::redact_urls(&format!("{e:#}"))
            ),
            None => println!("{name}: the race lane still sending after {STOP_DRAIN:?}: dropped"),
        }
        let stats = race_out.as_ref().map(|o| o.stats.clone());
        let (ended, end_failed) = end_after_settled(
            &org,
            &org_lock,
            &api,
            &ledger,
            &sub,
            &e.snap.pid,
            &l.tag,
            &l.end,
        )
        .await?;
        done.record(LaneDone {
            tag: l.tag.clone(),
            stats,
            ended,
            end_failed,
        });
    }
    while stop.is_none() {
        tokio::select! {
            r = set.join_next() => {
                let Some(r) = r else { break };
                match r.map_err(|e| anyhow!("lane task: {e}")).and_then(|r| r) {
                    Ok(d) => done.record(d),
                    Err(e) => {
                        set.abort_all();
                        return Err(e);
                    }
                }
            }
            _ = tick.tick() => {
                stop = tick_once(ctx, &name, &line, &ledger, &logs, &sub, &mut fee_since).await?;
            }
            _ = chaos_tick.tick(), if chaos.is_some() => {
                chaos_step(chaos, rig, &ctx.net, &logs.snapshot(), &mut seen, t0_unix, first_end(&done.ended)).await?;
            }
        }
    }
    if let Some(s) = &stop {
        // No new send starts; sends past their permit answer, give up, or
        // are dropped after the drain and write `uncertain`.
        println!("{name}: stopped {}", s.label());
        sub.stop();
        for t in &timers {
            t.abort();
        }
        // Killed nodes come back, so sends past their permit can answer.
        if let Err(e) = close_chaos(chaos, rig, &ctx.net, &mut report).await {
            println!(
                "{name}: {}",
                crate::redact_urls(&format!("chaos teardown at the stop: {e:#}"))
            );
        }
        for (l, e) in &lanes {
            if done.ended.contains_key(&l.tag) {
                continue;
            }
            let pid = e.snap.pid;
            // `Fit` only comes before a scenario starts.
            let how = match s {
                Stop::Budget(_) => Close::Cancel,
                Stop::Fees(_) | Stop::Fit(_) => Close::End(End::Now {
                    after: Duration::ZERO,
                }),
            };
            let r = close_with_retries(&org, &org_lock, &pid, &how).await;
            match r {
                Ok(()) => {
                    done.ended.insert(l.tag.clone(), unix_now());
                    done.end_failed.remove(&l.tag);
                }
                Err(e) => {
                    println!("{}: the stop could not close it: {e}", l.tag);
                    done.end_failed.insert(l.tag.clone(), e);
                }
            }
        }
        let drained = tokio::time::timeout(STOP_DRAIN, async {
            while let Some(r) = set.join_next().await {
                match r.map_err(|e| anyhow!("lane task: {e}")).and_then(|r| r) {
                    Ok(d) => done.record(d),
                    Err(e) => println!("{name}: a lane ended with {e:#}"),
                }
            }
        })
        .await;
        if drained.is_err() {
            println!("{name}: lanes still sending after {STOP_DRAIN:?}: dropped");
        }
        set.abort_all();
        while set.join_next().await.is_some() {}
        // Dropped sends write their `uncertain` line as their task ends.
        for _ in 0..100 {
            if sub.in_flight() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let unsent = keep_started(&mut planned, &sub.started());
        println!("{name}: {unsent} planned votes never sent");
        report.params.insert("unsent".into(), unsent.to_string());
    } else if let Kind::Race(_) = &sc.kind {
        // The rounds a race did not need never left the harness.
        let unused = keep_started(&mut planned, &sub.started());
        report
            .params
            .insert("unused ballots".into(), unused.to_string());
    }
    if let Some(o) = &race_out {
        race_report(&mut report, &dir, sc, o)?;
    }
    for (tag, e) in &done.end_failed {
        report.params.insert(format!("end failed {tag}"), e.clone());
    }
    let stats = done.stats;
    let ended = done.ended;
    let end_failed = done.end_failed;
    ledger.flush()?;
    if let Some(s) = &stop {
        report.params.insert("stopped".into(), s.label());
    }
    let budget_stop = matches!(stop, Some(Stop::Budget(_)));

    // 8. Results by grace_end + 15 min, on the registry and every node.
    if !budget_stop {
        for t in end_failed.keys() {
            println!("{t}: never ended, no results to wait for");
        }
        let mut todo: Vec<&Election> = lanes
            .iter()
            .filter(|(l, _)| !end_failed.contains_key(&l.tag))
            .map(|(_, e)| *e)
            .collect();
        let started = Instant::now();
        let mut printed = Instant::now();
        while !todo.is_empty() {
            let mut left = Vec::new();
            for e in todo {
                let pid = &e.snap.pid.0;
                let r = org.results(pid).await.ok().flatten();
                let mut all = r.is_some();
                // A node chaos took down has no view yet.
                for role in [Role::A, Role::B, Role::O] {
                    all &= match rig.node(role) {
                        Ok(n) => n.api.process(pid).await.is_ok_and(|v| v.result.is_some()),
                        Err(_) => false,
                    };
                }
                let due = org
                    .grace_end(pid)
                    .await
                    .map_or(u64::MAX, |g| g.saturating_add(RESULTS_LATE));
                if all {
                    println!(
                        "{}: results on the registry and every node after {:.0} s",
                        e.spec.tag,
                        started.elapsed().as_secs_f64()
                    );
                } else if unix_now() as u64 > due || started.elapsed() >= RESULTS_MAX {
                    println!("{}: no results by grace end + 15 min", e.spec.tag);
                } else {
                    left.push(e);
                }
            }
            todo = left;
            if !todo.is_empty() && printed.elapsed() >= TICK {
                line.print(ctx, &ledger, &logs).await?;
                printed = Instant::now();
            }
            if !todo.is_empty() {
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if chaos.is_some() {
                        let ev = logs.snapshot();
                        chaos_step(
                            chaos,
                            rig,
                            &ctx.net,
                            &ev,
                            &mut seen,
                            t0_unix,
                            first_end(&ended),
                        )
                        .await?;
                    }
                }
            }
        }
    }
    // Every injection is undone before the audit reads the nodes.
    close_chaos(chaos, rig, &ctx.net, &mut report).await?;

    // 9. The audit.
    let entries = ledger.entries()?;
    let events: Vec<(NodeId, LogKind)> = logs
        .snapshot()
        .into_iter()
        .filter_map(|(r, _, k)| node_of(r).map(|n| (n, k)))
        .collect();
    let deadline = Instant::now() + QUIET_MAX;
    let mut merged = passing();
    let mut rows: Vec<(String, TransitionRow)> = Vec::new();
    for (l, e) in &lanes {
        let mut allowed: Vec<&str> = sc.allowed(&l.tag).to_vec();
        if budget_stop {
            allowed.push("process closed");
        }
        let a = audit_election(
            ctx,
            rig,
            &name,
            e,
            &entries,
            &who[&l.tag],
            &planned[&l.tag],
            &allowed,
            &events,
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(10)),
        )
        .await?;
        merge_audit(&mut merged, &a.audit);
        report.findings.extend(a.findings);
        rows.extend(a.rows.into_iter().map(|r| (a.tag.clone(), r)));
    }
    for (t, e) in &end_failed {
        merged.notes.push(format!("{t}: never ended ({e})"));
    }
    // The nodes' counters over the scenario: every transition is settled by
    // the one of A and B that sent it and synced by O. They live in memory,
    // so `info` sums each node's incarnations (a kill reads them first).
    let info1 = info(rig).await;
    let n_tx = rows.len() as u64;
    let delta = |node: &str, k: &str| -> Option<u64> {
        Some(counter(&info1, node, k)?.saturating_sub(counter(&info0, node, k)?))
    };
    let sent = [0, 1].map(|i| {
        let a = ctx.net.sequencers.get(i).map(|s| s.1);
        rows.iter()
            .filter(|(_, r)| Some(alloy::primitives::Address::from(r.sender)) == a)
            .count() as u64
    });
    let since = |i: usize, r: Role| {
        (
            rig.kills(r) - kills0[i].0,
            rig.unread_kills(r) > kills0[i].1,
        )
    };
    let (ok, notes) = counters_check(
        [delta("a", "settledBySelf"), delta("b", "settledBySelf")],
        sent,
        [since(0, Role::A).0, since(1, Role::B).0],
        [
            since(0, Role::A).1,
            since(1, Role::B).1,
            since(2, Role::O).1,
        ],
        delta("o", "syncedFromOthers"),
        n_tx,
        merged.observer_incomplete,
    );
    merged.counters_ok &= ok;
    merged.notes.extend(notes);
    println!(
        "audit {name}: no_vote_lost={} clean={} tally_ok={} results_ok={} counters_ok={}",
        merged.no_vote_lost, merged.clean, merged.tally_ok, merged.results_ok, merged.counters_ok
    );
    print_findings(&report.findings);
    report.audit = merged;

    // 10. Batch rows, latency, throughput, cells, vote loss and spend.
    let mut jobs = Vec::new();
    let mut busy = BTreeMap::new();
    let mut wasted = 0usize;
    let mut dropped = 0usize;
    let roots: BTreeSet<[u8; 32]> = rows.iter().map(|(_, r)| r.new_root).collect();
    for (i, (r, n)) in [(Role::A, NodeId::A), (Role::B, NodeId::B)]
        .into_iter()
        .enumerate()
    {
        let calls = rig.prove_calls(r);
        let mine = &calls[calls0[i].min(calls.len())..];
        // A `/prove` the client dropped never reached the prover.
        dropped += mine.iter().filter(|c| c.job.is_none()).count();
        let js = chain_view::jobs(&prover_url(rig, r)?, n, mine).await;
        wasted += js.iter().filter(|j| !roots.contains(&j.root_after)).count();
        busy.insert(format!("{n}"), js.iter().map(|j| j.secs).sum::<f64>());
        jobs.extend(js);
    }
    report
        .params
        .insert("wasted_jobs".into(), wasted.to_string());
    report
        .params
        .insert("dropped_prove".into(), dropped.to_string());
    rows.sort_by_key(|(_, r)| (r.ts, r.block));
    let batches = batch_rows(ctx, &rows, &jobs).await?;
    std::fs::write(
        dir.join("batches.json"),
        serde_json::to_vec_pretty(&batches)?,
    )?;
    report.batches = rows.len() as u64;
    report.latency = latency(t0_unix, &entries, &rows, &batches);
    let first = entries
        .values()
        .filter_map(|e| e.first_accept)
        .reduce(f64::min)
        .map(|t| t0_unix + t);
    let last = rows.iter().map(|(_, r)| r.ts as f64).reduce(f64::max);
    let votes: u64 = rows.iter().map(|(_, r)| r.voters).sum();
    let landings: Vec<(f64, u64)> = rows.iter().map(|(_, r)| (r.ts as f64, r.voters)).collect();
    let wall = first.zip(last).map(|(f, l)| l - f).filter(|w| *w > 0.0);
    report.throughput = Throughput {
        overall_per_min: wall.map(|w| votes as f64 / w * 60.0),
        steady_per_min: metrics::steady_rate(&landings),
        prover_busy: busy
            .into_iter()
            .map(|(k, s)| (k, wall.map_or(0.0, |w| s / w)))
            .collect(),
        node_share: ["a", "b"]
            .iter()
            .map(|n| {
                let c = batches.iter().filter(|b| b.node == *n).count();
                (n.to_string(), c as f64 / batches.len().max(1) as f64)
            })
            .collect(),
    };
    println!(
        "{name}: {} votes in {} transitions, {} votes/min overall, {} steady",
        votes,
        rows.len(),
        report
            .throughput
            .overall_per_min
            .map_or("n/a".into(), |v| format!("{v:.1}")),
        report
            .throughput
            .steady_per_min
            .map_or("n/a".into(), |v| format!("{v:.1}"))
    );
    if let Kind::Ceiling(cells) = &sc.kind {
        let nf = lanes.first().map_or(0, |(_, e)| u32::from(e.spec.nf));
        let mut cells = cells.clone();
        cells.sort_by_key(|c| c.b);
        for c in cells {
            // A batch belongs to the cell most of its votes came from.
            let land: Vec<(f64, u64)> = rows
                .iter()
                .filter(|(_, r)| {
                    let n = r
                        .vids
                        .iter()
                        .filter(|v| cell_of.get(v) == Some(&c.b))
                        .count();
                    2 * n > r.vids.len()
                })
                .map(|(_, r)| (r.ts as f64, r.voters))
                .collect();
            let rate = metrics::cell_rate(&land, scaled(c.b, scale) as u64);
            println!(
                "cell nf {nf} B {}: {} votes/min, mean batch {:.1}{}",
                c.b,
                rate.steady_per_min
                    .map_or("n/a".into(), |v| format!("{v:.1}")),
                rate.mean_batch,
                if rate.floor { " floor" } else { "" }
            );
            report.cells.push(report::Cell {
                nf,
                b: c.b as u64,
                rate,
            });
        }
    }
    for (n, r) in [("a", Role::A), ("b", Role::B)] {
        let lost = logs
            .snapshot()
            .iter()
            .filter(|(er, _, k)| {
                *er == r
                    && matches!(k, LogKind::LostEvent { .. } | LogKind::LostRevert { .. })
                    && kind_pid(k).is_some_and(|p| pids.contains(p))
            })
            .count();
        report
            .races
            .insert(format!("lost by {n}"), lost.to_string());
    }
    report.vote_loss = report::vote_loss(
        &events
            .iter()
            .filter(|(_, k)| kind_pid(k).is_some_and(|p| pids.contains(p)))
            .cloned()
            .collect::<Vec<_>>(),
    );
    report.nodes = info1;
    let bal1 = balances(&ctx.net).await?;
    for (k, s, e) in [
        ("seq-a.l1", bal0.l1[0], bal1.l1[0]),
        ("seq-b.l1", bal0.l1[1], bal1.l1[1]),
        ("seq-a.op", bal0.op[0], bal1.op[0]),
        ("seq-b.op", bal0.op[1], bal1.op[1]),
        ("organizer.op", bal0.organizer_op, bal1.organizer_op),
    ] {
        report.spend.insert(k.into(), ueth(s, e));
    }
    let mut sent = SubmitStats::default();
    for s in stats.values() {
        sent.sent += s.sent;
        sent.accepted += s.accepted;
        sent.uncertain += s.uncertain;
        for (c, n) in &s.refused {
            *sent.refused.entry(*c).or_default() += n;
        }
    }
    report.params.insert("sent".into(), sent.sent.to_string());
    report
        .params
        .insert("accepted".into(), sent.accepted.to_string());
    report
        .params
        .insert("uncertain".into(), sent.uncertain.to_string());
    report
        .params
        .insert("refused".into(), format!("{:?}", sent.refused));

    // 11. Reports.
    report::write_run(&dir, &report)?;
    println!("report written: {}", dir.join("report.md").display());
    report::write_campaign(&ctx.campaign, &load_reports(&ctx.campaign)?)?;
    Ok(report)
}

/// Reruns the audit of a run that crashed, from its `ledger.jsonl`, the
/// nodes and the chain. Planned votes with no ledger line never went out;
/// a send dropped mid-flight wrote `uncertain`.
pub async fn audit_run(ctx: &Ctx, rig: &Rig, run_dir: &Path) -> Result<AuditResult> {
    let meta: RunMeta =
        serde_json::from_slice(&std::fs::read(run_dir.join("run.json")).context("run.json")?)
            .context("parse run.json")?;
    let scale = ctx.plan.scale(meta.wave)?;
    ensure!(
        scale == meta.scale,
        "the run was at scale {}, plan.json says {scale}",
        meta.scale
    );
    let sc = scenarios(meta.wave, scale)
        .into_iter()
        .find(|s| s.name == meta.name)
        .with_context(|| format!("no scenario {} in wave {}", meta.name, meta.wave))?;
    let wave = ctx.plan.wave(meta.wave)?;
    let entries = Ledger::replay(&run_dir.join("ledger.jsonl"))?;
    let mut events = Vec::new();
    for (r, n) in [(Role::A, NodeId::A), (Role::B, NodeId::B)] {
        let all = LogTail::new(&rig.node(r)?.log).poll();
        events.extend(
            all.iter()
                .filter(|e| e.at >= meta.t0)
                .filter_map(|e| metrics::kind(e).map(|k| (n, k))),
        );
    }
    let name = format!("w{}/{}", meta.wave, meta.name);
    let deadline = Instant::now() + QUIET_MAX;
    let mut merged = passing();
    let mut findings = Vec::new();
    for (tag, vids) in &meta.planned {
        let e = wave
            .elections
            .iter()
            .find(|e| &e.spec.tag == tag)
            .with_context(|| format!("{tag} is not in plan.json"))?;
        wait_results_if_ended(ctx, rig, e).await?;
        let recs = load_ballots(&ctx.campaign, meta.wave, e)?;
        // A crash cuts the schedule: votes with no line never left the
        // harness, and every send that started has a line (`uncertain` when
        // it was dropped), short of a SIGKILL.
        let all = vids
            .iter()
            .map(|v| parse_vote_id(v).map_err(|e| anyhow!("{v}: {e}")))
            .collect::<Result<BTreeSet<u64>>>()?;
        let planned: BTreeSet<u64> = all
            .iter()
            .filter(|v| entries.contains_key(v))
            .copied()
            .collect();
        if planned.len() < all.len() {
            println!(
                "{tag}: {} planned votes never sent",
                all.len() - planned.len()
            );
        }
        let a = audit_election(
            ctx,
            rig,
            &name,
            e,
            &entries,
            &who_of(&recs),
            &planned,
            sc.allowed(tag),
            &events,
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(10)),
        )
        .await?;
        merge_audit(&mut merged, &a.audit);
        findings.extend(a.findings);
    }
    merged
        .notes
        .push("the nodes' /info counters are not checked by audit --run".into());
    print_findings(&findings);
    std::fs::write(
        run_dir.join("audit.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "audit": merged,
            "findings": findings,
        }))?,
    )?;
    Ok(merged)
}

/// `audit --run`: an election ended since the run (by `davinci-bench end`)
/// is still finishing; waits for its results on the registry and every node
/// as the run does, by grace end + 15 min and at most an hour. READY, PAUSED
/// and canceled elections have none to wait for.
async fn wait_results_if_ended(ctx: &Ctx, rig: &Rig, e: &Election) -> Result<()> {
    use davinci_client::api::ProcessStatus as S;
    let org = ctx.net.organizer()?;
    let (pid, tag) = (&e.snap.pid.0, &e.spec.tag);
    if matches!(
        org.process(pid).await?.status,
        S::Ready | S::Paused | S::Canceled
    ) {
        return Ok(());
    }
    let started = Instant::now();
    loop {
        let mut all = org.results(pid).await.ok().flatten().is_some();
        for role in [Role::A, Role::B, Role::O] {
            all &= match rig.node(role) {
                Ok(n) => n.api.process(pid).await.is_ok_and(|v| v.result.is_some()),
                Err(_) => false,
            };
        }
        let due = org
            .grace_end(pid)
            .await
            .map_or(u64::MAX, |g| g.saturating_add(RESULTS_LATE));
        if all {
            println!("{tag}: results on the registry and every node");
            return Ok(());
        }
        if unix_now() as u64 > due || started.elapsed() >= RESULTS_MAX {
            println!("{tag}: no results by grace end + 15 min");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::ballots::fake_req;
    use crate::bench::schedule::{Gap, Phase, Schedule};

    fn rec(voter: u32, round: u8) -> BallotRecord {
        BallotRecord {
            tag: "T".into(),
            voter,
            round,
            choices: vec![1, 0],
            req: fake_req(1 << 63 | u64::from(voter) << 8 | u64::from(round), [0; 20]),
        }
    }

    fn lane(gaps: Vec<Gap>, start: u64) -> Lane {
        Lane {
            tag: "T".into(),
            start: Duration::from_secs(start),
            route: Route::ByVoter,
            schedule: Schedule {
                phases: vec![Phase::Rate {
                    per_min: 60.0,
                    secs: 10,
                    poisson: false,
                }],
            },
            gaps,
            end: End::AfterSettled,
        }
    }

    #[test]
    fn lane_plan_splits_the_revoters_between_the_gaps() {
        let mut recs: Vec<BallotRecord> = (0..10).map(|v| rec(v, 1)).collect();
        recs.extend([2, 3, 6, 7].map(|v| rec(v, 2)));
        let gaps = vec![
            Gap::Overwrite { min: 5, max: 30 },
            Gap::Revote { secs: 900 },
        ];
        let plan = lane_plan(&lane(gaps, 100), recs, 7).unwrap();
        assert_eq!(plan.len(), 14);
        assert!(plan.windows(2).all(|w| w[0].0 <= w[1].0));
        let at = |v: u32, r: u8| {
            plan.iter()
                .find(|(_, x)| x.voter == v && x.round == r)
                .unwrap()
                .0
        };
        // Round 1 at one a second from the lane's start.
        assert_eq!(at(0, 1), Duration::from_secs(100));
        assert_eq!(at(9, 1), Duration::from_secs(109));
        // Revoters 2 and 6 overwrite, 3 and 7 revote.
        for v in [2, 6] {
            let d = at(v, 2) - at(v, 1);
            assert!((5..=30).contains(&d.as_secs()), "{v}: {d:?}");
        }
        for v in [3, 7] {
            assert_eq!(at(v, 2) - at(v, 1), Duration::from_secs(900));
        }
        // A later round needs a gap.
        let recs = vec![rec(0, 1), rec(0, 2)];
        assert!(lane_plan(&lane(Vec::new(), 0), recs, 7).is_err());
    }

    #[test]
    fn bursts_fill_the_cells_in_ascending_b() {
        let cells = [Cell { b: 256, bursts: 1 }, Cell { b: 128, bursts: 2 }];
        // At scale 0.05: 2 × 7 + 1 × 13 = 27 ballots.
        let recs: Vec<BallotRecord> = (0..27).rev().map(|v| rec(v, 1)).collect();
        let b = bursts(&cells, 0.05, recs.clone()).unwrap();
        let sizes: Vec<(usize, usize)> = b.iter().map(|(c, r)| (*c, r.len())).collect();
        assert_eq!(sizes, [(128, 7), (128, 7), (256, 13)]);
        assert_eq!(b[0].1[0].voter, 0);
        assert_eq!(b[2].1[12].voter, 26);
        assert!(bursts(&cells, 0.05, recs[1..].to_vec()).is_err());
        let mut two = recs;
        two[0].round = 2;
        assert!(bursts(&cells, 0.05, two).is_err());
    }

    #[test]
    fn select_keeps_wave_order() {
        let all = scenarios(1, 0.05);
        let names = |v: Vec<Scenario>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(
            names(select(all.clone(), &["S-16".into(), "R".into()]).unwrap()),
            ["R", "S-16"]
        );
        assert_eq!(
            names(select(all.clone(), &["all".into()]).unwrap()).len(),
            5
        );
        assert!(select(all.clone(), &["X9".into()]).is_err());
        assert!(select(all, &["all".into(), "R".into()]).is_err());
    }

    #[test]
    fn progress_counts_votes_of_its_elections() {
        let mut e = BTreeMap::new();
        let mut acc = Entry::test();
        acc.accepted.insert(NodeId::A);
        acc.refused.push((NodeId::B, 40901));
        e.insert(1, acc);
        let mut refused = Entry::test();
        refused.refused.push((NodeId::A, 41201));
        e.insert(2, refused);
        let mut unc = Entry::test();
        unc.uncertain.insert(NodeId::B);
        e.insert(3, unc);
        let pids: BTreeSet<String> = ["p".to_string()].into();
        let ev = vec![
            (
                Role::A,
                1000.0,
                LogKind::Settled {
                    pid: "p".into(),
                    votes: 1,
                    root: String::new(),
                },
            ),
            (
                Role::B,
                1000.0,
                LogKind::Settled {
                    pid: "other".into(),
                    votes: 9,
                    root: String::new(),
                },
            ),
            (Role::B, 1000.0, LogKind::LostEvent { pid: "p".into() }),
        ];
        let p = progress("w1/R", 5, &e, &ev, &pids, 1100.0, [3, 4], None);
        assert_eq!((p.sent, p.accepted, p.refused), (4, 1, 1));
        assert_eq!((p.settled, p.open, p.tx, p.lost), (1, 0, [1, 0], [0, 1]));
        assert_eq!(p.per_min, 0.2);
        assert_eq!(p.spent_l1_ueth, [3, 4]);
    }

    #[test]
    fn merged_audit_fails_if_one_election_fails() {
        let mut m = passing();
        let mut ok = passing();
        ok.counts.insert("settled".into(), 3);
        merge_audit(&mut m, &ok);
        assert!(m.no_vote_lost && m.counters_ok);
        let mut bad = passing();
        bad.counts.insert("settled".into(), 2);
        bad.no_vote_lost = false;
        bad.observer_incomplete = true;
        merge_audit(&mut m, &bad);
        assert!(!m.no_vote_lost && m.clean && m.observer_incomplete);
        assert_eq!(m.counts["settled"], 5);
    }

    #[test]
    fn ueth_is_signed() {
        let e = U256::from(10u64).pow(U256::from(18u64));
        assert_eq!(ueth(e, e - U256::from(5_000_000_000_000u64)), 5);
        assert_eq!(ueth(e, e + U256::from(2_000_000_000_000u64)), -2);
    }

    #[test]
    fn a_burst_counts_only_seals_since_it_went_out() {
        let sealed = |at: f64, votes: u64| {
            (
                Role::A,
                at,
                LogKind::Sealed {
                    pid: "p".into(),
                    votes,
                    reason: String::new(),
                },
            )
        };
        // Burst 1 (7 votes) sealed at 10 and, after a bisection, again at 20
        // and 25; burst 2 (7) went out at 30.
        let mut ev = vec![sealed(10.0, 7), sealed(20.0, 4), sealed(25.0, 3)];
        assert_eq!(sealed_or_synced(&ev, Role::A, "p", 30.0), (0, false));
        ev.push(sealed(31.0, 7));
        ev.push((
            Role::A,
            31.0,
            LogKind::Sealed {
                pid: "other".into(),
                votes: 9,
                reason: String::new(),
            },
        ));
        assert_eq!(sealed_or_synced(&ev, Role::A, "p", 30.0), (7, false));
        assert_eq!(sealed_or_synced(&ev, Role::B, "p", 30.0), (0, false));
        ev.push((
            Role::B,
            32.0,
            LogKind::Synced {
                pid: "p".into(),
                root: String::new(),
            },
        ));
        assert_eq!(sealed_or_synced(&ev, Role::B, "p", 30.0), (0, true));
        assert_eq!(sealed_or_synced(&ev, Role::B, "p", 33.0), (0, false));
    }

    #[test]
    fn a_scenario_must_fit_each_keys_room() {
        use crate::bench::money::Caps;
        use crate::bench::scenarios::wave_specs;
        let x1 = &scenarios(2, 1.0)[0];
        let b = post_bound(x1, &wave_specs(2, 1.0), 1.0);
        let need = b[0]
            .wei(EST_EXEC_WEI, EST_BLOB_WEI)
            .max(b[1].wei(EST_EXEC_WEI, EST_BLOB_WEI));
        // X1: 20 one-blob posts, about 52.5 µETH per key.
        assert_eq!(need, 52_485_760_000_000);
        let ueth = |x: u64| U256::from(x) * U256::from(1_000_000_000_000u64);
        let budget = Budget {
            start: Balances {
                l1: [ueth(1000), ueth(1000)],
                ..Balances::default()
            },
            inflows: Balances::default(),
            caps: Caps {
                l1_per_key: ueth(900),
                op_per_key: ueth(150),
                organizer_op: ueth(300),
            },
        };
        // B spent 850 of its 900: 50 left, below X1's bound.
        let mut now = budget.start.clone();
        now.l1[1] -= ueth(850);
        let err = fits(&budget, &now, need).unwrap_err();
        assert!(
            err.contains("seq-b.l1 may spend 50.0") && !err.contains("seq-a"),
            "{err}"
        );
        assert!(err.contains("52.5 µETH per key"), "{err}");
        // 60 left fits.
        now.l1[1] += ueth(10);
        assert!(fits(&budget, &now, need).is_ok());
        assert_eq!(Stop::Fit(err).label().split(':').next(), Some("budget"));
    }

    #[test]
    fn counters_are_checked_per_node_and_incarnation() {
        let ok = |s, r, u, o, inc| counters_check(s, [3, 2], r, u, o, 5, inc);
        let none = [false; 3];
        // Each node settled what it sent, O synced all.
        assert_eq!(
            ok([Some(3), Some(2)], [0, 0], none, Some(5), false),
            (true, vec![])
        );
        // A sum that matches is not enough: A settled one of B's.
        assert!(!ok([Some(4), Some(1)], [0, 0], none, Some(5), false).0);
        // A restarted A may count one of its own as synced, noted.
        let (pass, notes) = ok([Some(2), Some(2)], [1, 0], none, Some(5), false);
        assert!(pass && notes.len() == 1 && notes[0].contains("restarts"));
        // Not two short after one restart, nor one short without one.
        assert!(!ok([Some(1), Some(2)], [1, 0], none, Some(5), false).0);
        assert!(!ok([Some(3), Some(1)], [0, 0], none, Some(5), false).0);
        // A node that never answered fails; an unread kill skips the node.
        assert!(!ok([None, Some(2)], [0, 0], none, Some(5), false).0);
        let (pass, notes) = ok(
            [Some(0), Some(2)],
            [1, 0],
            [true, false, false],
            Some(5),
            false,
        );
        assert!(pass && notes[0].contains("not checked"));
        // O: exact, unless the observer is incomplete.
        assert!(!ok([Some(3), Some(2)], [0, 0], none, Some(4), false).0);
        let (pass, notes) = ok([Some(3), Some(2)], [0, 0], none, Some(4), true);
        assert!(pass && notes[0].contains("observer incomplete"));
    }

    #[test]
    fn a_canceled_election_has_no_results_failed() {
        let a = AuditResult {
            no_vote_lost: true,
            ..AuditResult::default()
        };
        let f = report::findings("w1/R", "R1", &a, &[]);
        assert!(
            f.iter()
                .any(|x| x.kind == report::FindingKind::ResultsFailed)
        );
        let f = canceled(f, "w1/R", "R1");
        let kinds: Vec<_> = f.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, [report::FindingKind::Canceled]);
    }
}
