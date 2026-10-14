//! Forced race rounds (spec §9): both nodes seal the same K fresh votes on
//! the same root, one lands, and the loser's detection, recovery and waste
//! are read from the logs, the chain and the prover tap.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use alloy::primitives::B256;
use anyhow::{Context, Result, ensure};
use davinci_client::organizer::Organizer;
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::types::{JobId, JobStatus};
use serde::Serialize;

use super::ballots::BallotRecord;
use super::chain_view::{self, JobRow, TransitionRow};
use super::metrics::{Kind, LogEvent, LogTail, kind};
use super::plan::{Election, NodeId};
use super::rig::{Rig, Role};
use super::run::{Ctx, info, op_provider, prover_url, role_of};
use super::submit::{Route, SubmitStats, Submitter};
use crate::proxy::Proxy;

/// One race scenario's rounds.
#[derive(Clone, Debug, PartialEq)]
pub struct RaceSpec {
    /// Fresh votes per round, sent to both nodes at once; both run with
    /// `batch_max = k`, so both seal on the k-th vote.
    pub k: usize,
    /// Races to collect; the run tries at most twice as many rounds.
    pub rounds: usize,
    /// The loser and how long its prover proxy holds `POST /prove`.
    pub skew: Option<(NodeId /* loser */, Duration)>,
    /// The loser's `confirmations` (X3): it sees the winner's event late, so
    /// its pre-flight reverts.
    pub lag_confirmations: Option<u64>,
}

/// How the loser found out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LossPath {
    /// The winner's event arrived while its batch was in flight.
    Event,
    /// Its pre-flight or settlement reverted `InvalidStateRoot`.
    Revert,
    /// No race: a seal gap above [`MAX_SEAL_GAP`], a node that never
    /// sealed, or no loss logged. Does not count toward the rounds.
    None,
}

/// One round, as the report shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RaceRow {
    pub round: usize,
    pub winner: Option<NodeId>,
    pub path: LossPath,
    /// Between the two `batch sealed reason=at_max` lines; infinite when a
    /// node never sealed.
    pub seal_gap_s: f64,
    /// The loser's loss line minus the landing's OP block time.
    pub detect_s: Option<f64>,
    /// The loser's sync of the landing minus its OP block time.
    pub recover_s: Option<f64>,
    /// The loser's prover jobs that proved no landed root.
    pub wasted_job_s: Option<f64>,
    /// The loser's `blobs posted on L1` lines.
    pub wasted_posts: u64,
    /// Their receipts' cost (filled in by the caller).
    pub wasted_wei: u128,
    /// Per node (`a`, `b`, `o`), the round's `/info` deltas of
    /// [`ROUND_COUNTERS`] (filled in by the caller).
    pub counters: BTreeMap<String, BTreeMap<String, u64>>,
}

/// The `/info` counters a race moves (spec §9, step 4).
pub const ROUND_COUNTERS: [&str; 3] = ["lostRaces", "settledBySelf", "syncedFromOthers"];

/// The round's deltas of [`ROUND_COUNTERS`] per node, from `/info` read
/// before the burst and after the round; a node missing from either read is
/// left out.
pub fn counter_deltas(
    before: &BTreeMap<String, BTreeMap<String, u64>>,
    after: &BTreeMap<String, BTreeMap<String, u64>>,
) -> BTreeMap<String, BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    for (node, a) in after {
        let Some(b) = before.get(node) else { continue };
        let d: BTreeMap<String, u64> = ROUND_COUNTERS
            .iter()
            .filter_map(|k| {
                let (x, y) = (a.get(*k)?, b.get(*k)?);
                Some((k.to_string(), x.saturating_sub(*y)))
            })
            .collect();
        out.insert(node.clone(), d);
    }
    out
}

/// A seal gap above this is no race (spec §9).
pub const MAX_SEAL_GAP: f64 = 6.0;

fn other(n: NodeId) -> NodeId {
    match n {
        NodeId::A => NodeId::B,
        NodeId::B => NodeId::A,
    }
}

/// A root as the node logs it (`0x` hex), compared without the prefix.
fn same_root(logged: &str, root: &[u8; 32]) -> bool {
    logged
        .trim_start_matches("0x")
        .eq_ignore_ascii_case(&hex::encode(root))
}

/// The round's row from the landing and both nodes' log lines since the
/// burst. Pure; `wasted_wei` is left at zero.
pub fn race_row(
    round: usize,
    landing: &TransitionRow,
    a: &[LogEvent],
    b: &[LogEvent],
    jobs: &[JobRow],
) -> RaceRow {
    let pid = hex::encode(landing.pid);
    let kinds = |es: &[LogEvent]| -> Vec<(f64, Kind)> {
        es.iter()
            .filter_map(|e| kind(e).map(|k| (e.at, k)))
            .collect()
    };
    let ev = [kinds(a), kinds(b)];
    let at = |n: NodeId| &ev[usize::from(n == NodeId::B)];
    let sealed = |n: NodeId| {
        at(n).iter().find_map(|(t, k)| match k {
            Kind::Sealed { pid: p, reason, .. } if *p == pid && reason == "at_max" => Some(*t),
            _ => None,
        })
    };
    let seal_gap_s = match (sealed(NodeId::A), sealed(NodeId::B)) {
        (Some(x), Some(y)) => (x - y).abs(),
        _ => f64::INFINITY,
    };
    let landed = |n: NodeId| {
        at(n).iter().any(|(_, k)| {
            matches!(k, Kind::Settled { pid: p, root, .. } if *p == pid && same_root(root, &landing.new_root))
        })
    };
    // The loss line of node `n`: when and which path.
    let loss = |n: NodeId| {
        at(n).iter().find_map(|(t, k)| match k {
            Kind::LostEvent { pid: p } if *p == pid => Some((*t, LossPath::Event)),
            Kind::LostRevert { pid: p } if *p == pid => Some((*t, LossPath::Revert)),
            _ => None,
        })
    };
    // The node that settled the landed root; else the one that did not lose.
    let winner = [NodeId::A, NodeId::B]
        .into_iter()
        .find(|n| landed(*n))
        .or_else(|| match (loss(NodeId::A), loss(NodeId::B)) {
            (Some(_), None) => Some(NodeId::B),
            (None, Some(_)) => Some(NodeId::A),
            _ => None,
        });
    let mut row = RaceRow {
        round,
        winner,
        path: LossPath::None,
        seal_gap_s,
        detect_s: None,
        recover_s: None,
        wasted_job_s: None,
        wasted_posts: 0,
        wasted_wei: 0,
        counters: BTreeMap::new(),
    };
    let Some(loser) = winner.map(other) else {
        return row;
    };
    let ts = landing.ts as f64;
    let lost = loss(loser);
    if seal_gap_s <= MAX_SEAL_GAP
        && let Some((_, path)) = lost
    {
        row.path = path;
    }
    row.detect_s = lost.map(|(t, _)| t - ts);
    row.recover_s = at(loser).iter().find_map(|(t, k)| match k {
        Kind::Synced { pid: p, root } if *p == pid && same_root(root, &landing.new_root) => {
            Some(t - ts)
        }
        _ => None,
    });
    let wasted: Vec<f64> = jobs
        .iter()
        .filter(|j| j.node == loser && j.root_after != landing.new_root)
        .map(|j| j.secs)
        .collect();
    row.wasted_job_s = (!wasted.is_empty()).then(|| wasted.iter().sum());
    row.wasted_posts = at(loser)
        .iter()
        .filter(|(_, k)| matches!(k, Kind::Posted { .. }))
        .count() as u64;
    row
}

/// The loser's `blobs posted on L1` transaction hashes since the burst.
pub fn loser_posts(row: &RaceRow, a: &[LogEvent], b: &[LogEvent]) -> Vec<String> {
    let Some(loser) = row.winner.map(other) else {
        return Vec::new();
    };
    let es = if loser == NodeId::A { a } else { b };
    es.iter()
        .filter_map(|e| match kind(e) {
            Some(Kind::Posted { tx, .. }) => Some(tx),
            _ => None,
        })
        .collect()
}

/// The round-1 ballots of a race election in voter order, as rounds of K:
/// at most twice the target rounds, full rounds only.
pub fn rounds(spec: &RaceSpec, mut recs: Vec<BallotRecord>) -> Result<Vec<Vec<BallotRecord>>> {
    ensure!(
        recs.iter().all(|r| r.round == 1),
        "a race election has only round 1"
    );
    ensure!(spec.k > 0, "K is zero");
    recs.sort_by_key(|r| r.voter);
    let mut out = Vec::new();
    let mut it = recs.into_iter();
    while out.len() < 2 * spec.rounds {
        let r: Vec<BallotRecord> = it.by_ref().take(spec.k).collect();
        if r.len() < spec.k {
            break;
        }
        out.push(r);
    }
    ensure!(
        out.len() >= spec.rounds,
        "{} rounds of {} ballots for {} races",
        out.len(),
        spec.k,
        spec.rounds
    );
    Ok(out)
}

/// The levelling wait before a round (the X3 loser syncs about 5 min
/// late), the landing and every sync after the burst, and the round's
/// prover jobs.
const LEVEL_MAX: Duration = Duration::from_secs(30 * 60);
const ROUND_MAX: Duration = Duration::from_secs(20 * 60);
const JOBS_MAX: Duration = Duration::from_secs(10 * 60);
const POLL: Duration = Duration::from_secs(2);

/// What a race lane reads; it runs beside the progress ticks, so the rig is
/// only borrowed.
pub struct RaceCtx<'a> {
    pub ctx: &'a Ctx,
    pub rig: &'a Rig,
    pub org: &'a Organizer,
    pub sub: &'a Submitter,
    pub e: &'a Election,
    pub spec: &'a RaceSpec,
    /// The scenario's start: ledger times count from here.
    pub t0: Instant,
}

/// One round: its row, its sends, and whether it ran to the loser's sync.
pub struct Round {
    pub row: RaceRow,
    pub stats: SubmitStats,
    pub complete: bool,
}

/// What a race lane did.
#[derive(Default)]
pub struct RaceOut {
    pub stats: SubmitStats,
    pub rows: Vec<RaceRow>,
    /// The skewed loser's prover proxy over the lane: `/prove` requests
    /// held, and those dropped because the node gave up on them.
    pub held: u64,
    pub dropped: u64,
    pub notes: Vec<String>,
}

impl RaceOut {
    pub fn races(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.path != LossPath::None)
            .count()
    }
}

/// Clears the loser's `/prove` hold when dropped, on every exit.
struct Skew<'a>(&'a Proxy);

impl Drop for Skew<'_> {
    fn drop(&mut self) {
        self.0.set_delay("/prove", Duration::ZERO);
    }
}

fn prover_proxy(rig: &Rig, n: NodeId) -> Result<&Proxy> {
    rig.proxies(role_of(n))
        .prover
        .as_ref()
        .context("no prover proxy")
}

fn add(into: &mut SubmitStats, s: &SubmitStats) {
    into.sent += s.sent;
    into.accepted += s.accepted;
    into.uncertain += s.uncertain;
    for (c, n) in &s.refused {
        *into.refused.entry(*c).or_default() += n;
    }
}

fn opt_s(v: Option<f64>) -> String {
    v.map_or("n/a".into(), |v| format!("{v:.1} s"))
}

impl RaceRow {
    /// One report line.
    pub fn line(&self) -> String {
        let mut counters = String::new();
        for k in ROUND_COUNTERS {
            let v: Vec<String> = self
                .counters
                .iter()
                .filter_map(|(n, c)| c.get(k).map(|v| format!("{n} {v}")))
                .collect();
            if !v.is_empty() {
                counters.push_str(&format!(", {k} {}", v.join(" ")));
            }
        }
        format!(
            "winner {} path {} gap {} detect {} recover {} wasted job {} posts {} ({:.2} µETH){counters}",
            self.winner.map_or("none".into(), |n| n.to_string()),
            serde_json::to_value(self.path)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default(),
            if self.seal_gap_s.is_finite() {
                format!("{:.1} s", self.seal_gap_s)
            } else {
                "n/a".into()
            },
            opt_s(self.detect_s),
            opt_s(self.recover_s),
            opt_s(self.wasted_job_s),
            self.wasted_posts,
            self.wasted_wei as f64 / 1e12,
        )
    }
}

/// Runs rounds until `spec.rounds` raced, the ballots run out (twice the
/// target) or the submitter stops. A round that fails or never completes
/// ends the lane with a note; the rows so far are kept.
pub async fn race_lane(rc: &RaceCtx<'_>, rounds: Vec<Vec<BallotRecord>>) -> Result<RaceOut> {
    let tag = &rc.e.spec.tag;
    let mut out = RaceOut::default();
    let loser = match rc.spec.skew {
        Some((n, _)) => Some(prover_proxy(rc.rig, n)?),
        None => None,
    };
    let (held0, dropped0) = loser.map_or((0, 0), |p| (p.delayed(), p.dropped()));
    for (i, votes) in rounds.into_iter().enumerate() {
        if out.races() >= rc.spec.rounds || rc.sub.stopped() {
            break;
        }
        // A round that fails ends the lane; the election still ends and
        // is audited.
        let r = match race_round(rc, i + 1, votes).await {
            Ok(r) => r,
            Err(e) => {
                let n = crate::redact_urls(&format!("{tag}: round {} failed: {e:#}", i + 1));
                println!("{n}");
                out.notes.push(n);
                break;
            }
        };
        add(&mut out.stats, &r.stats);
        println!("race {tag} round {}: {}", i + 1, r.row.line());
        out.rows.push(r.row);
        if !r.complete {
            if !rc.sub.stopped() {
                let n = format!(
                    "{tag}: round {} did not land and sync within {ROUND_MAX:?}",
                    i + 1
                );
                println!("{n}");
                out.notes.push(n);
            }
            break;
        }
    }
    if let Some(p) = loser {
        (out.held, out.dropped) = (p.delayed() - held0, p.dropped() - dropped0);
    }
    let winners = |n: NodeId| out.rows.iter().filter(|r| r.winner == Some(n)).count();
    let paths = |p: LossPath| out.rows.iter().filter(|r| r.path == p).count();
    println!(
        "races {tag}: {}/{} in {} rounds; winners a {} b {}; paths event {} revert {} none {}{}",
        out.races(),
        rc.spec.rounds,
        out.rows.len(),
        winners(NodeId::A),
        winners(NodeId::B),
        paths(LossPath::Event),
        paths(LossPath::Revert),
        paths(LossPath::None),
        match rc.spec.skew {
            Some((n, _)) => format!(
                "; prover {n}: {} /prove held, {} dropped",
                out.held, out.dropped
            ),
            None => String::new(),
        }
    );
    Ok(out)
}

/// Whether A, B and O all hold `root` as the chain's and their own, and A
/// and B have nothing pending. A node that does not answer is not there.
async fn at_root(rig: &Rig, pid: &[u8; 31], root: &[u8; 32]) -> bool {
    for r in [Role::A, Role::B, Role::O] {
        let Ok(n) = rig.node(r) else { return false };
        let Ok(v) = n.api.process(pid).await else {
            return false;
        };
        let idle = r == Role::O || v.pending_votes == Some(0);
        if v.state_root != *root || !v.synced || !idle {
            return false;
        }
    }
    true
}

/// Both provers report an empty queue.
async fn queues_empty(rig: &Rig) -> bool {
    for n in [NodeId::A, NodeId::B] {
        let Ok(u) = prover_url(rig, role_of(n)) else {
            return false;
        };
        match ProverClient::new(&u).health().await {
            Ok(h) if h.status == "ok" && h.queue_len == 0 => {}
            _ => return false,
        }
    }
    true
}

/// Stage 1: the registry's root once A, B and O hold it with nothing
/// pending and both provers are idle.
async fn level(rc: &RaceCtx<'_>) -> Result<[u8; 32]> {
    let pid = &rc.e.snap.pid.0;
    let start = Instant::now();
    loop {
        ensure!(
            !rc.sub.stopped(),
            "{}: stopped while levelling",
            rc.e.spec.tag
        );
        if let Ok(p) = rc.org.process(pid).await
            && at_root(rc.rig, pid, &p.state_root).await
            && queues_empty(rc.rig).await
        {
            return Ok(p.state_root);
        }
        ensure!(
            start.elapsed() < LEVEL_MAX,
            "{}: A, B and O not level at the chain root with idle provers after {LEVEL_MAX:?}",
            rc.e.spec.tag
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Until every job of `calls` (per node) is done or failed, or `JOBS_MAX`.
/// An aborted flight's job keeps running on its prover.
async fn jobs_done(rig: &Rig, calls: &[Vec<super::rig::ProveCall>; 2]) {
    let start = Instant::now();
    while start.elapsed() < JOBS_MAX {
        let mut running = false;
        for (n, cs) in [NodeId::A, NodeId::B].into_iter().zip(calls) {
            let Ok(u) = prover_url(rig, role_of(n)) else {
                continue;
            };
            let pc = ProverClient::new(&u);
            for c in cs {
                let Some(id) = c.job.as_deref().and_then(|j| JobId::parse(j).ok()) else {
                    continue;
                };
                if pc
                    .job(&id)
                    .await
                    .is_ok_and(|j| matches!(j.status, JobStatus::Queued | JobStatus::Running))
                {
                    running = true;
                }
            }
        }
        if !running {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// One forced race (spec §9): level the nodes, skew the loser, send K
/// fresh votes to both at once, wait for the landing and every node's
/// sync, undo the skew and build the row from the logs since the burst,
/// the landed transition and the round's prover jobs.
pub async fn race_round(rc: &RaceCtx<'_>, round: usize, votes: Vec<BallotRecord>) -> Result<Round> {
    let (rig, pid) = (rc.rig, rc.e.snap.pid);
    let tag = &rc.e.spec.tag;
    // 1. Same root.
    let root0 = level(rc).await?;
    let net = &rc.ctx.net;
    let count0 =
        chain_view::chain_count(&net.rpc, net.registry, rc.ctx.plan.start_block, &pid.0).await?;
    // 2. Skew.
    let skew = match rc.spec.skew {
        Some((n, d)) => {
            let p = prover_proxy(rig, n)?;
            p.set_delay("/prove", d);
            Some(Skew(p))
        }
        None => None,
    };
    let mut tails = [
        LogTail::new(&rig.node(Role::A)?.log),
        LogTail::new(&rig.node(Role::B)?.log),
    ];
    for t in &mut tails {
        t.poll();
    }
    let calls0 = [Role::A, Role::B].map(|r| rig.prove_calls(r).len());
    let info0 = info(rig).await;
    let mut ev: [Vec<LogEvent>; 2] = Default::default();
    // 3. Burst: both nodes at once.
    let at = rc.t0.elapsed();
    let stats = rc
        .sub
        .run(
            votes.into_iter().map(|v| (at, v)).collect(),
            Route::Both,
            rc.t0,
        )
        .await?;
    // 4 and 5. The landing, then every node at its root.
    let start = Instant::now();
    let mut landed = None;
    let complete = loop {
        for (t, e) in tails.iter_mut().zip(&mut ev) {
            e.extend(t.poll());
        }
        if rc.sub.stopped() {
            break false;
        }
        if let Ok(p) = rc.org.process(&pid.0).await
            && p.state_root != root0
        {
            landed = Some(p.state_root);
            if at_root(rig, &pid.0, &p.state_root).await {
                break true;
            }
        }
        if start.elapsed() >= ROUND_MAX {
            break false;
        }
        tokio::time::sleep(POLL).await;
    };
    // 6. Undo the skew.
    drop(skew);
    let calls = [Role::A, Role::B].map(|r| {
        let c = rig.prove_calls(r);
        c[calls0[usize::from(r == Role::B)].min(c.len())..].to_vec()
    });
    if complete {
        jobs_done(rig, &calls).await;
    }
    let counters = counter_deltas(&info0, &info(rig).await);
    for (t, e) in tails.iter_mut().zip(&mut ev) {
        e.extend(t.poll());
    }
    // 7. The row.
    let mut row = RaceRow {
        round,
        winner: None,
        path: LossPath::None,
        seal_gap_s: f64::INFINITY,
        detect_s: None,
        recover_s: None,
        wasted_job_s: None,
        wasted_posts: 0,
        wasted_wei: 0,
        counters: BTreeMap::new(),
    };
    if landed.is_some() {
        let logs =
            chain_view::chain_txs(&net.rpc, net.registry, rc.ctx.plan.start_block, &pid.0).await?;
        let op = op_provider(net)?;
        let (o, a, b) = (rig.node(Role::O)?, rig.node(Role::A)?, rig.node(Role::B)?);
        let tr = chain_view::transitions(o, [a, b], &op, net.registry, &pid.0, rc.e.spec.nf, &logs)
            .await
            .with_context(|| format!("{tag} round {round}: transitions"))?;
        let landing = tr
            .rows
            .get(count0 as usize)
            .with_context(|| format!("{tag} round {round}: no transition {count0}"))?;
        let mut jobs = Vec::new();
        for (n, cs) in [NodeId::A, NodeId::B].into_iter().zip(&calls) {
            jobs.extend(chain_view::jobs(&prover_url(rig, role_of(n))?, n, cs).await);
        }
        row = race_row(round, landing, &ev[0], &ev[1], &jobs);
        if let Some(l1) = &net.l1 {
            for tx in loser_posts(&row, &ev[0], &ev[1]) {
                if let Ok(h) = tx.parse::<B256>()
                    && let Ok(c) = crate::cost::fetch(l1.read_rpc(), "", h).await
                {
                    row.wasted_wei += u128::try_from(c.wei()).unwrap_or(u128::MAX);
                }
            }
        }
    }
    row.counters = counters;
    Ok(Round {
        row,
        stats,
        complete,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn le(at: f64, msg: &str, fields: &[(&str, &str)]) -> LogEvent {
        LogEvent {
            at,
            level: "INFO".into(),
            msg: msg.into(),
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn rec(voter: u32) -> BallotRecord {
        BallotRecord {
            tag: "X".into(),
            voter,
            round: 1,
            choices: vec![1, 0],
            req: crate::bench::ballots::fake_req(1 << 63 | u64::from(voter), [0; 20]),
        }
    }

    #[test]
    fn rounds_are_full_and_at_most_twice_the_target() {
        let spec = RaceSpec {
            k: 3,
            rounds: 2,
            skew: None,
            lag_confirmations: None,
        };
        // 14 ballots: four rounds of 3 in voter order, the rest unused.
        let r = rounds(&spec, (0..14).rev().map(rec).collect()).unwrap();
        let voters: Vec<Vec<u32>> = r
            .iter()
            .map(|x| x.iter().map(|b| b.voter).collect())
            .collect();
        assert_eq!(voters, [[0, 1, 2], [3, 4, 5], [6, 7, 8], [9, 10, 11]]);
        // Fewer than the target rounds is an error, as is a revote.
        assert!(rounds(&spec, (0..5).map(rec).collect()).is_err());
        let mut v: Vec<BallotRecord> = (0..6).map(rec).collect();
        v[0].round = 2;
        assert!(rounds(&spec, v).is_err());
    }

    #[test]
    fn counters_move_per_round() {
        let m = |v: &[(&str, &[(&str, u64)])]| -> BTreeMap<String, BTreeMap<String, u64>> {
            v.iter()
                .map(|(n, c)| {
                    let c = c.iter().map(|(k, x)| (k.to_string(), *x)).collect();
                    (n.to_string(), c)
                })
                .collect()
        };
        let before = m(&[
            (
                "a",
                &[("lostRaces", 1), ("settledBySelf", 4), ("bisections", 0)],
            ),
            ("b", &[("lostRaces", 2), ("settledBySelf", 3)]),
        ]);
        let after = m(&[
            (
                "a",
                &[("lostRaces", 1), ("settledBySelf", 5), ("bisections", 1)],
            ),
            ("b", &[("lostRaces", 3), ("settledBySelf", 3)]),
            ("o", &[("syncedFromOthers", 9)]),
        ]);
        let d = counter_deltas(&before, &after);
        // Only the race's counters, and no node missing a read.
        assert_eq!(
            d,
            m(&[
                ("a", &[("lostRaces", 0), ("settledBySelf", 1)]),
                ("b", &[("lostRaces", 1), ("settledBySelf", 0)]),
            ])
        );
        let row = RaceRow {
            round: 1,
            winner: Some(NodeId::A),
            path: LossPath::Event,
            seal_gap_s: 0.5,
            detect_s: None,
            recover_s: None,
            wasted_job_s: None,
            wasted_posts: 1,
            wasted_wei: 0,
            counters: d,
        };
        assert!(
            row.line()
                .ends_with(", lostRaces a 0 b 1, settledBySelf a 1 b 0"),
            "{}",
            row.line()
        );
    }

    #[test]
    fn race_row_event_and_revert() {
        let pid = [7u8; 31];
        let root = [9u8; 32];
        let (ph, rh) = (hex::encode(pid), format!("0x{}", hex::encode(root)));
        let landing = TransitionRow {
            pid,
            index: 3,
            ts: 100,
            new_root: root,
            ..TransitionRow::test()
        };
        let sealed = |at| {
            le(
                at,
                "batch sealed",
                &[("pid", &ph), ("votes", "64"), ("reason", "at_max")],
            )
        };
        // A wins: it seals at 80.0 and its transition lands at 100.
        let a = vec![
            sealed(80.0),
            le(
                99.0,
                "transition settled",
                &[("pid", &ph), ("votes", "64"), ("root", &rh)],
            ),
        ];
        // B seals 0.6 s later, posts its blobs, learns at 101.5 and syncs
        // the landing at 104.
        let loser = |loss: &str| {
            vec![
                sealed(80.6),
                le(
                    85.0,
                    "blobs posted on L1",
                    &[("nonce", "4"), ("tx", "0xab"), ("blobs", "1")],
                ),
                le(101.5, loss, &[("pid", &ph)]),
                le(
                    104.0,
                    "synced transition from another sequencer",
                    &[("pid", &ph), ("votes", "64"), ("root", &rh)],
                ),
            ]
        };
        let job = |node, secs, root_after| JobRow {
            node,
            job: "j".into(),
            secs,
            started: 81.0,
            finished: 81.0 + secs,
            root_after,
        };
        // The winner's job proved the landed root; the loser's did not.
        let jobs = [job(NodeId::A, 20.0, root), job(NodeId::B, 22.0, [1; 32])];

        let b = loser("foreign transition beat the in-flight batch");
        let r = race_row(1, &landing, &a, &b, &jobs);
        assert_eq!(r.round, 1);
        assert_eq!(r.winner, Some(NodeId::A));
        assert_eq!(r.path, LossPath::Event);
        assert!((r.seal_gap_s - 0.6).abs() < 1e-9, "{}", r.seal_gap_s);
        assert_eq!(r.detect_s, Some(1.5));
        assert_eq!(r.recover_s, Some(4.0));
        assert_eq!(r.wasted_job_s, Some(22.0));
        assert_eq!((r.wasted_posts, r.wasted_wei), (1, 0));
        assert!(r.counters.is_empty());

        let b = loser("lost the settlement race, waiting for sync");
        let r = race_row(2, &landing, &a, &b, &jobs);
        assert_eq!((r.winner, r.path), (Some(NodeId::A), LossPath::Revert));
        assert_eq!((r.detect_s, r.recover_s), (Some(1.5), Some(4.0)));

        // B never sealed: no race, whatever it logged after.
        let b: Vec<LogEvent> = loser("foreign transition beat the in-flight batch")[1..].to_vec();
        let r = race_row(3, &landing, &a, &b, &jobs);
        assert_eq!(r.path, LossPath::None);
        assert!(r.seal_gap_s.is_infinite());
        assert_eq!(r.winner, Some(NodeId::A));

        // Sealed 6.5 s apart: no race either.
        let mut b = loser("foreign transition beat the in-flight batch");
        b[0].at = 86.5;
        assert_eq!(race_row(4, &landing, &a, &b, &jobs).path, LossPath::None);

        // A sync of another root is no recovery; another election's lines
        // are not this race's.
        let mut b = loser("foreign transition beat the in-flight batch");
        b[3].fields
            .insert("root".into(), format!("0x{}", hex::encode([3u8; 32])));
        b.insert(
            0,
            le(
                80.1,
                "batch sealed",
                &[
                    ("pid", &hex::encode([8u8; 31])),
                    ("votes", "64"),
                    ("reason", "at_max"),
                ],
            ),
        );
        let r = race_row(5, &landing, &a, &b, &jobs);
        assert_eq!(r.recover_s, None);
        assert!((r.seal_gap_s - 0.6).abs() < 1e-9, "{}", r.seal_gap_s);

        // The winner is also known from the loser alone.
        let b = loser("foreign transition beat the in-flight batch");
        let r = race_row(6, &landing, &a[..1], &b, &[]);
        assert_eq!((r.winner, r.path), (Some(NodeId::A), LossPath::Event));
        assert_eq!(r.wasted_job_s, None);
    }
}
