//! Multi-prover throughput benchmark, gated by `DAVINCI_E2E_BENCH=throughput`.
//! One node per prover in `DAVINCI_E2E_BENCH_PROVERS`, each sequencing its
//! own origin-1 processes, so the provers never race on one root chain.
//! Every ballot is proved before the clock starts; the timed section runs
//! from the first submit to the last settlement. Each node reaches its
//! prover through a loopback proxy that records the `/prove` jobs; their
//! queue and proving times come from the prover's job API afterwards.
//!
//! Env (defaults in brackets): `DAVINCI_E2E_BENCH_PROVERS` (comma list,
//! [`DAVINCI_ZKVM_URL`]), `DAVINCI_E2E_BENCH_VOTES` votes per process [2048],
//! `DAVINCI_E2E_BENCH_BATCH` `--batch-max` [512], `DAVINCI_E2E_BENCH_NF`
//! [2], `DAVINCI_E2E_BENCH_PROCS` processes per node [1],
//! `DAVINCI_E2E_BENCH_BATCH_TIME` [2m], `DAVINCI_E2E_BENCH_SUBMITTERS`
//! concurrent submits per node [8], `DAVINCI_E2E_BENCH_THREADS` concurrent
//! ballot proofs [a quarter of the hardware threads], `DAVINCI_E2E_BENCH_RESULTS`
//! end the processes and check the tally [1]. Rows go to stderr, the
//! Markdown report to `DAVINCI_E2E_BENCH_OUT` when set.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::primitives::{B256, U256, utils::format_ether};
use alloy::sol_types::SolEvent;
use anyhow::{Context, Result, bail, ensure};
use davinci_client::SequencerClient;
use davinci_client::api::{ProcessId, VoteRequest, VoteStatus};
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, ProcessRegistry, RegistryReader};
use davinci_client::prover::BallotProver;
use davinci_e2e::chain;
use davinci_e2e::cost::{self, TxCost};
use davinci_e2e::fixture as fx;
use davinci_e2e::net::{self, Net};
use davinci_e2e::node::{self, Node, WorkDir};
use davinci_e2e::proxy::{Exchange, Proxy, Tap};
use davinci_e2e::wait;
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::limits::MAX_BATCH_SIZE;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::{JobId, JobStatus};
use serde::Deserialize;
use serde::de::IgnoredAny;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

/// Process lifetime: far beyond any run; the results check ends them early.
const DURATION: u64 = 24 * 3600;
/// Longest the chain may go without a new transition of ours.
const STALL: Duration = Duration::from_secs(30 * 60);
/// Longest one vote may be pushed back before the run fails.
const SUBMIT_LIMIT: Duration = Duration::from_secs(10 * 60);
/// Status sweep cadence (fails the run on the first errored vote).
const SWEEP_EVERY: Duration = Duration::from_secs(15);

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.into())
}

fn num<T: FromStr>(k: &str, d: T) -> Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(k).ok().filter(|v| !v.is_empty()) {
        Some(v) => v.trim().parse().with_context(|| format!("{k}={v}")),
        None => Ok(d),
    }
}

struct Cfg {
    provers: Vec<String>,
    votes: usize,
    batch: usize,
    nf: u8,
    procs: usize,
    batch_time: String,
    submitters: usize,
    threads: usize,
    results: bool,
}

impl Cfg {
    fn load() -> Result<Cfg> {
        let default = env_or("DAVINCI_ZKVM_URL", "http://127.0.0.1:8080");
        let provers: Vec<String> = env_or("DAVINCI_E2E_BENCH_PROVERS", &default)
            .split(',')
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect();
        ensure!(!provers.is_empty(), "DAVINCI_E2E_BENCH_PROVERS is empty");
        for p in &provers {
            ensure!(
                p.starts_with("http://") || p.starts_with("https://"),
                "prover {p}: not an http(s) URL"
            );
        }
        // Each proof already spreads its MSMs over every core (rayon); past
        // one proof per four hardware threads they only contend for memory
        // (16-core 9950X3D: 6.3/s at 8, 4.9/s at 32).
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let c = Cfg {
            provers,
            votes: num("DAVINCI_E2E_BENCH_VOTES", 2048)?,
            batch: num("DAVINCI_E2E_BENCH_BATCH", 512)?,
            nf: num("DAVINCI_E2E_BENCH_NF", 2)?,
            procs: num("DAVINCI_E2E_BENCH_PROCS", 1)?,
            batch_time: env_or("DAVINCI_E2E_BENCH_BATCH_TIME", "2m"),
            submitters: num("DAVINCI_E2E_BENCH_SUBMITTERS", 8)?,
            threads: num("DAVINCI_E2E_BENCH_THREADS", (cores / 4).max(1))?,
            results: env_or("DAVINCI_E2E_BENCH_RESULTS", "1") != "0",
        };
        ensure!(c.votes >= 1, "DAVINCI_E2E_BENCH_VOTES must be at least 1");
        ensure!(
            (1..=MAX_BATCH_SIZE).contains(&c.batch),
            "DAVINCI_E2E_BENCH_BATCH {}: 1 to {MAX_BATCH_SIZE}",
            c.batch
        );
        ensure!(
            (2..=16).contains(&c.nf),
            "DAVINCI_E2E_BENCH_NF {}: 2 to 16",
            c.nf
        );
        ensure!(c.procs >= 1, "DAVINCI_E2E_BENCH_PROCS must be at least 1");
        ensure!(
            c.submitters >= 1 && c.threads >= 1,
            "DAVINCI_E2E_BENCH_SUBMITTERS and _THREADS must be at least 1"
        );
        Ok(c)
    }

    /// Distinct prover URLs, in first-use order.
    fn unique_provers(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for p in &self.provers {
            if !out.contains(&p.as_str()) {
                out.push(p);
            }
        }
        out
    }
}

/// A `/prove` call a node made, as its proxy saw it.
struct Sealed {
    node: usize,
    at: Instant,
    votes: Option<usize>,
    refreshes: Option<usize>,
    status: u16,
    job: Option<String>,
}

// Only what the report needs from a `/prove` body and its answer.
#[derive(Deserialize)]
struct ProveShape {
    proofs: Vec<IgnoredAny>,
    #[serde(default)]
    state: Option<StateShape>,
}

#[derive(Deserialize)]
struct StateShape {
    #[serde(default)]
    refresh_smt: Option<Vec<IgnoredAny>>,
}

#[derive(Deserialize)]
struct Ack {
    job_id: String,
}

fn prove_tap(node: usize, log: Arc<Mutex<Vec<Sealed>>>) -> Tap {
    Arc::new(move |x: &Exchange<'_>| {
        if x.method != "POST" || x.path != "/prove" {
            return;
        }
        let req = serde_json::from_slice::<ProveShape>(x.request).ok();
        let job = serde_json::from_slice::<Ack>(x.response)
            .ok()
            .map(|a| a.job_id);
        let s = Sealed {
            node,
            at: x.at,
            votes: req.as_ref().map(|r| r.proofs.len()),
            refreshes: req.as_ref().map(|r| {
                r.state
                    .as_ref()
                    .and_then(|s| s.refresh_smt.as_ref())
                    .map_or(0, Vec::len)
            }),
            status: x.status,
            job,
        };
        if let Ok(mut l) = log.lock() {
            l.push(s);
        }
    })
}

/// A prover job of the run, from the job API.
struct JobInfo {
    sealed: usize,
    prover: String,
    status: JobStatus,
    /// `started_at - created_at`, s.
    queue: Option<f64>,
    /// `elapsed_ms`, s.
    prove: Option<f64>,
    /// Prover clock, s since the epoch.
    created: Option<f64>,
    started: Option<f64>,
    finished: Option<f64>,
    root_after: Option<[u8; 32]>,
}

/// A `ProcessStateTransitioned` of one of the run's processes.
struct Transition {
    pid: [u8; 31],
    tx: B256,
    new_root: [u8; 32],
    voters: u64,
    overwrites: u64,
}

async fn transitions(net: &Net, from: u64, pids: &HashSet<[u8; 31]>) -> Result<Vec<Transition>> {
    use ProcessRegistry::ProcessStateTransitioned as Ev;
    let mut out = Vec::new();
    for l in chain::registry_logs(&net.rpc, net.registry, Ev::SIGNATURE_HASH, from).await? {
        let d = l.log_decode::<Ev>()?.inner.data;
        if !pids.contains(&d.processId.0) {
            continue;
        }
        out.push(Transition {
            pid: d.processId.0,
            tx: l.transaction_hash.context("log without tx hash")?,
            new_root: d.newStateRoot.0,
            voters: u64::try_from(d.newVotersCount)?,
            overwrites: u64::try_from(d.newOverwrittenVotesCount)?,
        });
    }
    Ok(out)
}

/// Seconds since the epoch of an RFC 3339 UTC time, as the prover's chrono
/// writes it (`2026-09-28T10:34:32.105321716Z`).
fn rfc3339(s: &str) -> Option<f64> {
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    let frac: f64 = if frac.is_empty() {
        0.0
    } else {
        format!("0.{frac}").parse().ok()?
    };
    // Days from the civil date (Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + se) as f64 + frac)
}

/// One process of the run.
struct Proc {
    node: usize,
    pid: [u8; 31],
}

/// One settled batch.
struct Row {
    node: usize,
    proc_ix: usize,
    seq: usize,
    votes: u64,
    refreshes: Option<usize>,
    queue: Option<f64>,
    prove: Option<f64>,
    /// Seconds from the first submit.
    sealed: Option<f64>,
    proved: Option<f64>,
    settled: f64,
    cost: TxCost,
}

fn opt(v: Option<f64>) -> String {
    v.map_or("n/a".into(), |v| format!("{v:.1}"))
}

impl Row {
    fn line(&self) -> String {
        format!(
            "| {} | p{} | {} | {} | {} | {} | {} | {} | {} | {:.1} | {} | {} | {} |",
            self.node + 1,
            self.proc_ix,
            self.seq,
            self.votes,
            self.refreshes.map_or("n/a".into(), |r| r.to_string()),
            opt(self.queue),
            opt(self.prove),
            opt(self.sealed),
            opt(self.proved),
            self.settled,
            self.cost.blobs(),
            self.cost.gas,
            format_ether(self.cost.wei()),
        )
    }
}

const BATCH_HEADER: &str = "| node | process | batch | votes | refreshes | queue s | prove s | sealed s | proved s | settled s | blobs | gas | native |\n|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|";

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn bench_throughput() -> Result<()> {
    if std::env::var("DAVINCI_E2E_BENCH").as_deref() != Ok("throughput") {
        eprintln!("DAVINCI_E2E_BENCH=throughput not set, skipping");
        return Ok(());
    }
    let cfg = Cfg::load()?;
    for url in cfg.unique_provers() {
        let h = ProverClient::new(url)
            .health()
            .await
            .with_context(|| format!("prover health at {url}"))?;
        ensure!(h.status == "ok", "prover {url}: {h:?}");
        ensure!(
            h.queue_len == 0,
            "prover {url} has {} queued jobs; the benchmark needs it idle",
            h.queue_len
        );
    }

    // Kept with the node logs unless the run passes.
    let mut dir = WorkDir::new("davinci-throughput-")?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir.path()).await?;
    net.check_balances(cfg.provers.len()).await?;
    let org = net.organizer()?;
    let reader = RegistryReader::connect(&net.rpc, net.registry)?;
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let prover = Arc::new(ballot_prover.await??);
    let census_dir = dir.path().join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;

    let r = run(
        &cfg,
        &net,
        &org,
        &reader,
        &bin,
        prover,
        &census_dir,
        dir.path(),
    )
    .await;
    // Nothing stays open for a later run.
    let c = org.cancel_open().await;
    r?;
    c.context("cancel the open processes")?;
    dir.ok = true;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run(
    cfg: &Cfg,
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    bin: &Path,
    prover: Arc<BallotProver>,
    census_dir: &Path,
    dir: &Path,
) -> Result<()> {
    let out = std::env::var_os("DAVINCI_E2E_BENCH_OUT").map(PathBuf::from);
    let n_nodes = cfg.provers.len();

    // Nodes, each behind a proxy that records its `/prove` calls.
    let log: Arc<Mutex<Vec<Sealed>>> = Arc::new(Mutex::new(Vec::new()));
    let mut proxies = Vec::new();
    let mut nodes = Vec::new();
    for (i, url) in cfg.provers.iter().enumerate() {
        let proxy = Proxy::tapped(url, prove_tap(i, log.clone())).await?;
        let mut nc = net.node_config(
            &format!("node{}", i + 1),
            Some(i),
            i,
            &proxy.url,
            census_dir,
            dir,
        )?;
        nc.batch_max = cfg.batch;
        nc.batch_time = cfg.batch_time.clone();
        nodes.push(Node::start(bin, &nc).await?);
        proxies.push(proxy);
    }

    // One census for every process: all the voters.
    let voters = fx::bench_voters(cfg.votes);
    let (census, tree) = fx::merkle_census_of(&voters)?;
    let uri = fx::write_census(census_dir, "throughput.json", &census)?;
    let (metadata, metadata_hash) =
        fx::write_metadata(census_dir, "throughput-metadata.json", "throughput")?;
    let mut procs = Vec::new();
    for (n, node) in nodes.iter().enumerate() {
        for _ in 0..cfg.procs {
            let next = org.next_process_id().await?;
            let pk = node.api.new_key(&next).await?;
            let pid = org
                .create_process(&NewProcess {
                    process_id: next,
                    start_time: 0,
                    duration: DURATION,
                    max_voters: cfg.votes as u64,
                    ballot_mode: fx::ballot_mode_nf(cfg.nf),
                    census_origin: 1,
                    census_root: tree.root(),
                    census_contract: [0; 20],
                    census_uri: uri.clone(),
                    metadata: metadata.clone(),
                    metadata_hash,
                    key_mode: KeyMode::Sequencer(pk),
                })
                .await?
                .pid;
            eprintln!("p{} on {}: {}", procs.len(), node.name, ProcessId(pid));
            procs.push(Proc { node: n, pid });
        }
    }

    // Every ballot before the clock starts, on every core.
    let mut jobs = Vec::with_capacity(procs.len() * cfg.votes);
    for (k, p) in procs.iter().enumerate() {
        let onchain = reader.process(&p.pid).await?;
        for (v, voter) in voters.iter().enumerate() {
            jobs.push(fx::Job {
                label: format!("p{k} voter {v}"),
                key: voter.key.clone(),
                process: onchain.clone(),
                fields: fx::choices_nf(v, cfg.nf as usize),
                census: fx::merkle_witness(&tree, v)?,
                weight: fx::WEIGHT,
                k: fx::vote_k(0x40 + k as u64, v, 1),
            });
        }
    }
    let n_ballots = jobs.len();
    let done = Arc::new(AtomicUsize::new(0));
    let t = Instant::now();
    let mut h = {
        let (done, threads) = (done.clone(), cfg.threads);
        tokio::task::spawn_blocking(move || fx::prove_all_on(&prover, &jobs, threads, &done))
    };
    let reqs = loop {
        tokio::select! {
            r = &mut h => break r??,
            _ = tokio::time::sleep(Duration::from_secs(15)) => eprintln!(
                "ballots: {}/{n_ballots} proved after {:.0} s",
                done.load(Ordering::Relaxed),
                t.elapsed().as_secs_f64()
            ),
        }
    };
    let gen_s = t.elapsed().as_secs_f64();
    eprintln!(
        "ballots: {n_ballots} proved in {gen_s:.1} s, {} at a time ({:.1}/s)",
        cfg.threads,
        n_ballots as f64 / gen_s
    );
    let mut reqs = reqs.into_iter();
    let per_proc: Vec<Arc<Vec<VoteRequest>>> = procs
        .iter()
        .map(|_| Arc::new(reqs.by_ref().take(cfg.votes).collect()))
        .collect();

    let last = voters.last().context("no voters")?.address();
    for p in &procs {
        let node = &nodes[p.node];
        wait::until(
            &format!("{} to take votes for {}", node.name, ProcessId(p.pid)),
            net::scaled(Duration::from_secs(180)),
            Duration::from_secs(1),
            || async {
                node.check_alive()?;
                let ready = node.api.participant(&p.pid, &last).await.is_ok()
                    && node
                        .api
                        .process(&p.pid)
                        .await
                        .is_ok_and(|v| v.is_accepting_votes);
                Ok(ready.then_some(()))
            },
        )
        .await?;
    }

    // The timed section.
    let from = net.start_block.unwrap_or(net.from_block);
    let pids: HashSet<[u8; 31]> = procs.iter().map(|p| p.pid).collect();
    let busy = Arc::new(AtomicU64::new(0));
    let limits: Vec<Arc<Semaphore>> = (0..n_nodes)
        .map(|_| Arc::new(Semaphore::new(cfg.submitters)))
        .collect();
    let t0 = Instant::now();
    let mut submitters: Vec<Option<JoinHandle<Result<Instant>>>> = procs
        .iter()
        .zip(&per_proc)
        .map(|(p, reqs)| {
            Some(tokio::spawn(submit_all(
                nodes[p.node].api.clone(),
                reqs.clone(),
                limits[p.node].clone(),
                busy.clone(),
            )))
        })
        .collect();
    let mut accepted: Vec<Option<Instant>> = vec![None; procs.len()];
    let mut sweep = Some(tokio::spawn(sweep(
        nodes.iter().map(|n| n.api.clone()).collect(),
        procs
            .iter()
            .zip(&per_proc)
            .map(|(p, r)| (p.node, p.pid, r.iter().map(|v| v.vote_id).collect()))
            .collect(),
    )));
    let mut seen: HashMap<B256, (Instant, Transition)> = HashMap::new();
    let mut last_progress = Instant::now();
    let want = cfg.votes as u64;
    loop {
        for n in &nodes {
            n.check_alive()?;
        }
        for (slot, acc) in submitters.iter_mut().zip(accepted.iter_mut()) {
            if slot.as_ref().is_some_and(JoinHandle::is_finished)
                && let Some(h) = slot.take()
            {
                *acc = Some(h.await??);
            }
        }
        // Only an error ends the sweep before every vote is settled.
        if sweep.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(h) = sweep.take()
        {
            h.await??;
        }
        let now = Instant::now();
        let mut fresh = 0;
        for tr in transitions(net, from, &pids).await? {
            seen.entry(tr.tx).or_insert_with(|| {
                fresh += 1;
                (now, tr)
            });
        }
        let mut settled: HashMap<[u8; 31], u64> = HashMap::new();
        for (_, tr) in seen.values() {
            let s = settled.entry(tr.pid).or_default();
            *s = (*s).max(tr.voters + tr.overwrites);
        }
        let total: u64 = settled.values().sum();
        if fresh > 0 {
            last_progress = now;
            eprintln!(
                "{:>7.1} s: {} transitions, {total}/{} votes settled",
                t0.elapsed().as_secs_f64(),
                seen.len(),
                want * procs.len() as u64
            );
        }
        if procs
            .iter()
            .all(|p| settled.get(&p.pid).copied().unwrap_or(0) >= want)
        {
            break;
        }
        ensure!(
            last_progress.elapsed() < net::scaled(STALL),
            "no transition for {:?}; {total} of {} votes settled",
            net::scaled(STALL),
            want * procs.len() as u64
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let t_end = seen.values().map(|(t, _)| *t).max().unwrap_or(t0);
    let wall = t_end.duration_since(t0).as_secs_f64();
    for (slot, acc) in submitters.iter_mut().zip(accepted.iter_mut()) {
        if let Some(h) = slot.take() {
            *acc = Some(h.await??);
        }
    }
    let accepted_s = accepted
        .iter()
        .flatten()
        .map(|t| t.duration_since(t0).as_secs_f64())
        .fold(0.0, f64::max);

    // Nothing lost: on-chain counters and every vote settled at its node.
    for p in &procs {
        let c = reader.process(&p.pid).await?;
        ensure!(
            (c.voters_count, c.overwritten_votes_count) == (want, 0),
            "{}: votersCount {} overwrittenVotesCount {}, want {want} and 0",
            ProcessId(p.pid),
            c.voters_count,
            c.overwritten_votes_count
        );
    }
    if let Some(h) = sweep {
        tokio::time::timeout(SWEEP_EVERY * 4 + Duration::from_secs(60), h)
            .await
            .context("the final status sweep did not finish")???;
    }
    eprintln!("verified: every vote settled, on-chain counters match");

    // Job timings, matched to the transitions by the root they prove.
    let sealed: Vec<Sealed> = std::mem::take(
        &mut *log
            .lock()
            .map_err(|_| anyhow::anyhow!("tap log poisoned"))?,
    );
    let mut jobs_info = Vec::new();
    for (i, s) in sealed.iter().enumerate() {
        let Some(id) = &s.job else { continue };
        let url = &cfg.provers[s.node];
        let pc = ProverClient::new(url);
        let id = JobId::parse(id)?;
        let j = pc
            .job(&id)
            .await
            .with_context(|| format!("job {id} at {url}"))?;
        let root_after = if j.status == JobStatus::Done {
            BatchPublics::parse(&pc.publics(&id).await?)
                .ok()
                .map(|p| p.root_after)
        } else {
            None
        };
        let (created, started, finished) = (
            j.created_at.as_deref().and_then(rfc3339),
            j.started_at.as_deref().and_then(rfc3339),
            j.finished_at.as_deref().and_then(rfc3339),
        );
        jobs_info.push(JobInfo {
            sealed: i,
            prover: url.clone(),
            status: j.status,
            queue: created.zip(started).map(|(c, s)| s - c),
            prove: j.elapsed_ms.map(|ms| ms as f64 / 1000.0),
            created,
            started,
            finished,
            root_after,
        });
    }

    // Batch rows, in settlement order (a process's counters only grow).
    let mut by_time: Vec<&(Instant, Transition)> = seen.values().collect();
    by_time.sort_by_key(|(t, tr)| (*t, tr.voters + tr.overwrites));
    let mut rows: Vec<Row> = Vec::new();
    let mut prev: HashMap<[u8; 31], u64> = HashMap::new();
    let mut seq: HashMap<[u8; 31], usize> = HashMap::new();
    for (t, tr) in by_time {
        let k = procs
            .iter()
            .position(|p| p.pid == tr.pid)
            .context("transition of an unknown process")?;
        let node = procs[k].node;
        let before = prev.insert(tr.pid, tr.voters + tr.overwrites).unwrap_or(0);
        let n = seq.entry(tr.pid).or_default();
        *n += 1;
        let job = jobs_info
            .iter()
            .find(|j| sealed[j.sealed].node == node && j.root_after == Some(tr.new_root));
        let s = job.map(|j| &sealed[j.sealed]);
        let votes = tr.voters + tr.overwrites - before;
        if let Some(v) = s.and_then(|s| s.votes) {
            ensure!(
                v as u64 == votes,
                "p{k} batch {n}: {votes} votes on-chain, {v} in its prove request"
            );
        }
        let sealed_s = s.map(|s| s.at.duration_since(t0).as_secs_f64());
        // Proved on the harness clock: the post plus the job's own
        // created-to-finished span, which is skew-free.
        let span = job.and_then(|j| Some(j.finished? - j.created?));
        let cost = cost::fetch(&net.rpc, format!("p{k} batch {n}"), tr.tx).await?;
        rows.push(Row {
            node,
            proc_ix: k,
            seq: *n,
            votes,
            refreshes: s.and_then(|s| s.refreshes),
            queue: job.and_then(|j| j.queue),
            prove: job.and_then(|j| j.prove),
            sealed: sealed_s,
            proved: sealed_s.zip(span).map(|(a, b)| a + b),
            settled: t.duration_since(t0).as_secs_f64(),
            cost,
        });
    }
    for r in &rows {
        eprintln!("{}", r.line());
    }

    // Per node.
    let total_votes = want * procs.len() as u64;
    let mut node_lines = Vec::new();
    let mut steady_sum = 0.0;
    let mut steady_all = true;
    for i in 0..n_nodes {
        let mine: Vec<&Row> = rows.iter().filter(|r| r.node == i).collect();
        let votes: u64 = mine.iter().map(|r| r.votes).sum();
        let last = mine.iter().map(|r| r.settled).fold(0.0, f64::max);
        let steady = match (mine.first(), mine.len()) {
            (Some(first), n) if n >= 2 && last > first.settled => {
                Some((votes - first.votes) as f64 / (last - first.settled))
            }
            _ => None,
        };
        match steady {
            Some(s) => steady_sum += s,
            None => steady_all = false,
        }
        node_lines.push(format!(
            "| {} | {} | {} | {votes} | {} | {last:.1} | {:.2} | {} | {:.0}% |",
            i + 1,
            cfg.provers[i],
            cfg.procs,
            mine.len(),
            votes as f64 / last.max(f64::EPSILON),
            steady.map_or("n/a".into(), |s| format!("{s:.2}")),
            100.0 * votes as f64 / total_votes as f64,
        ));
    }

    // Per prover: GPU busy time is the sum of its jobs' proving times (the
    // worker runs one job at a time).
    let mut prover_lines = Vec::new();
    for url in cfg.unique_provers() {
        let js: Vec<&JobInfo> = jobs_info.iter().filter(|j| j.prover == url).collect();
        let busy_s: f64 = js.iter().filter_map(|j| j.prove).sum();
        let start = js.iter().filter_map(|j| j.started).fold(f64::MAX, f64::min);
        let end = js
            .iter()
            .filter_map(|j| j.finished)
            .fold(f64::MIN, f64::max);
        let active = (end > start).then_some(end - start);
        let queues: Vec<f64> = js.iter().filter_map(|j| j.queue).collect();
        let failed = js.iter().filter(|j| j.status != JobStatus::Done).count();
        let nodes_of: Vec<String> = (0..n_nodes)
            .filter(|&i| cfg.provers[i] == url)
            .map(|i| (i + 1).to_string())
            .collect();
        prover_lines.push(format!(
            "| {url} | {} | {} | {failed} | {busy_s:.1} | {:.0}% | {} | {} |",
            nodes_of.join(", "),
            js.len(),
            100.0 * busy_s / wall,
            active.map_or("n/a".into(), |a| format!("{:.0}%", 100.0 * busy_s / a)),
            if queues.is_empty() {
                "n/a".into()
            } else {
                format!("{:.1}", queues.iter().sum::<f64>() / queues.len() as f64)
            },
        ));
    }
    let refused = sealed.iter().filter(|s| s.status != 202).count();
    let unmatched = rows.iter().filter(|r| r.prove.is_none()).count();
    let wasted = jobs_info.len().saturating_sub(rows.len() - unmatched);

    let vps = total_votes as f64 / wall;
    let gas: u64 = rows.iter().map(|r| r.cost.gas).sum();
    let wei: U256 = rows.iter().map(|r| r.cost.wei()).sum();
    let blobs: u64 = rows.iter().map(|r| r.cost.blobs()).sum();
    let mut summary = vec![
        format!(
            "**{total_votes} votes in {wall:.1} s: {vps:.2} votes/s** ({:.0} votes/min), first submit to last settlement",
            vps * 60.0
        ),
        format!(
            "steady state: {} votes/s, the sum over nodes of the votes after each node's first batch over the time from its first to its last settlement",
            if steady_all {
                format!("{steady_sum:.2}")
            } else {
                format!("n/a (a node settled a single batch; {steady_sum:.2} over the others)")
            }
        ),
        format!(
            "ballots: {n_ballots} proved in {gen_s:.1} s, {} at a time over every core ({:.1}/s), before the clock",
            cfg.threads,
            n_ballots as f64 / gen_s
        ),
        format!(
            "submit: every vote accepted {accepted_s:.1} s after the first submit, {} concurrent per node, {} busy answers retried",
            cfg.submitters,
            busy.load(Ordering::Relaxed)
        ),
        format!(
            "{} transitions, {blobs} blobs, {gas} gas, {} native",
            rows.len(),
            format_ether(wei)
        ),
    ];
    if refused > 0 || wasted > 0 || unmatched > 0 {
        summary.push(format!(
            "prover calls: {refused} refused, {wasted} jobs that settled nothing, {unmatched} transitions without a matched job"
        ));
    }
    let mut notes = Vec::new();
    for (k, p) in procs.iter().enumerate() {
        notes.push(format!(
            "p{k}: process {} on node {}",
            ProcessId(p.pid),
            p.node + 1
        ));
    }
    for n in &nodes {
        notes.push(format!(
            "{}: blob cap {}",
            n.name,
            n.logged_blob_cap()
                .map_or("not logged".into(), |c| c.to_string()),
        ));
    }

    let date = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let label = std::env::var("DAVINCI_E2E_BENCH_PROVER")
        .ok()
        .filter(|s| !s.is_empty())
        .map_or(String::new(), |l| format!(" ({l})"));
    let head = format!(
        "# Throughput benchmark, chain {}\n\n\
         Date {date}. Registry {}. {n_nodes} node(s), one per prover{label}, {} origin-1\n\
         process(es) each with {} voters, nf={}, `--batch-max {}`, `--batch-time {}`,\n\
         {} confirmations. Every ballot was proved before the clock started, then\n\
         each process's votes went to its node as fast as it took them. Times are\n\
         seconds from the first submit. `queue` and `prove` come from the prover's\n\
         job API (`started_at - created_at`, `elapsed_ms`); `sealed` is when the\n\
         node posted the job, `proved` adds the job's created-to-finished span to it,\n\
         `settled` is when the harness saw the transition on-chain. GPU busy is the\n\
         sum of the prover's job times, over the wall time and over the span from\n\
         its first job start to its last job end. `native` is gas plus blob fee.\n\n",
        net.chain_id,
        net.registry,
        cfg.procs,
        cfg.votes,
        cfg.nf,
        cfg.batch,
        cfg.batch_time,
        net.confirmations,
    );
    let report = |extra: &[String]| -> String {
        format!(
            "{head}## Summary\n\n{}\n\n## Per node\n\n\
             | node | prover | processes | votes | batches | last settled s | votes/s | steady votes/s | share |\n\
             |---:|---|---:|---:|---:|---:|---:|---:|---:|\n{}\n\n## Per prover\n\n\
             | prover | nodes | jobs | failed | GPU busy s | busy / wall | busy / active | avg queue s |\n\
             |---|---|---:|---:|---:|---:|---:|---:|\n{}\n\n## Batches\n\n{BATCH_HEADER}\n{}\n\n## Notes\n\n{}\n",
            summary
                .iter()
                .chain(extra)
                .map(|s| format!("- {s}"))
                .collect::<Vec<_>>()
                .join("\n"),
            node_lines.join("\n"),
            prover_lines.join("\n"),
            rows.iter().map(Row::line).collect::<Vec<_>>().join("\n"),
            notes
                .iter()
                .map(|n| format!("- {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    let write = |body: &str| -> Result<()> {
        if let Some(p) = &out {
            std::fs::write(p, body).with_context(|| format!("write {}", p.display()))?;
            eprintln!("wrote {}", p.display());
        }
        Ok(())
    };
    let body = report(&[]);
    eprintln!("\n{body}");
    write(&body)?;

    if cfg.results {
        let line = check_results(cfg, org, &nodes, &procs).await?;
        eprintln!("{line}");
        write(&report(&[line]))?;
    }
    drop(proxies);
    Ok(())
}

/// Submits `reqs` to one node, at most `sem` at a time; returns when the
/// last one was accepted.
async fn submit_all(
    api: SequencerClient,
    reqs: Arc<Vec<VoteRequest>>,
    sem: Arc<Semaphore>,
    busy: Arc<AtomicU64>,
) -> Result<Instant> {
    let mut set = tokio::task::JoinSet::new();
    for i in 0..reqs.len() {
        let permit = sem.clone().acquire_owned().await?;
        let (api, reqs, busy) = (api.clone(), reqs.clone(), busy.clone());
        set.spawn(async move {
            let _permit = permit;
            submit_one(&api, &reqs[i], &busy).await
        });
        while let Some(r) = set.try_join_next() {
            r??;
        }
    }
    while let Some(r) = set.join_next().await {
        r??;
    }
    Ok(Instant::now())
}

/// One vote, retried while the node pushes back (429) or when the request
/// may not have arrived (transport error, 408). After such a resend, a 409
/// means the first attempt got in: the vote id exists (40901) or its slot
/// holds a queued vote (40902), which in this run can only be this one.
async fn submit_one(api: &SequencerClient, r: &VoteRequest, busy: &AtomicU64) -> Result<()> {
    use davinci_client::Error as E;
    let start = Instant::now();
    let mut resent = false;
    let mut pause = Duration::from_millis(50);
    loop {
        let e = match api.submit_vote(r).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        match &e {
            E::Api { status: 429, .. } => {
                busy.fetch_add(1, Ordering::Relaxed);
            }
            E::Api { status: 408, .. } | E::Http(_) => resent = true,
            E::Api {
                status: 409,
                code: Some(40901 | 40902),
                ..
            } if resent => return Ok(()),
            _ => return Err(anyhow::Error::from(e).context(format!("vote {:#x}", r.vote_id))),
        }
        ensure!(
            start.elapsed() < SUBMIT_LIMIT,
            "vote {:#x} still refused after {SUBMIT_LIMIT:?}: {e}",
            r.vote_id
        );
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(2));
    }
}

/// Every `SWEEP_EVERY`, the status of each vote not yet settled; fails on
/// the first `error`, returns once all are settled.
async fn sweep(apis: Vec<SequencerClient>, procs: Vec<(usize, [u8; 31], Vec<u64>)>) -> Result<()> {
    let mut open: Vec<(usize, [u8; 31], u64)> = procs
        .into_iter()
        .flat_map(|(n, pid, vids)| vids.into_iter().map(move |v| (n, pid, v)))
        .collect();
    loop {
        tokio::time::sleep(SWEEP_EVERY).await;
        let mut still = Vec::new();
        for (n, pid, vid) in open {
            match apis[n].vote_status_full(&pid, vid).await {
                Ok(s) => match s.status {
                    VoteStatus::Settled => {}
                    VoteStatus::Error => bail!(
                        "vote {vid:#x} of {} errored: {}",
                        ProcessId(pid),
                        s.error.unwrap_or_default()
                    ),
                    _ => still.push((n, pid, vid)),
                },
                // Not submitted yet, or a hiccup: next round.
                Err(davinci_client::Error::Api { status: 404, .. })
                | Err(davinci_client::Error::Http(_)) => still.push((n, pid, vid)),
                Err(davinci_client::Error::Api { status, .. }) if status >= 500 => {
                    still.push((n, pid, vid))
                }
                Err(e) => return Err(anyhow::Error::from(e).context(format!("vote {vid:#x}"))),
            }
        }
        open = still;
        if open.is_empty() {
            return Ok(());
        }
    }
}

/// Ends every process and waits for its results on-chain; the tally must be
/// the sum of the ballots cast.
async fn check_results(
    cfg: &Cfg,
    org: &Organizer,
    nodes: &[Node],
    procs: &[Proc],
) -> Result<String> {
    let nf = cfg.nf as usize;
    let mut want = vec![0u64; nf];
    for v in 0..cfg.votes {
        for (o, x) in want.iter_mut().zip(fx::choices_nf(v, nf)) {
            *o += x;
        }
    }
    let t = Instant::now();
    for p in procs {
        org.end_process(&p.pid)
            .await
            .with_context(|| format!("end {}", ProcessId(p.pid)))?;
    }
    let mut times = BTreeMap::new();
    for (k, p) in procs.iter().enumerate() {
        let got = wait::until(
            &format!("the results of {}", ProcessId(p.pid)),
            net::scaled(Duration::from_secs(15 * 60)),
            Duration::from_secs(2),
            || async {
                for n in nodes {
                    n.check_alive()?;
                }
                Ok(org.results(&p.pid).await?)
            },
        )
        .await?;
        ensure!(
            got.len() >= nf && got[..nf] == want[..] && got[nf..].iter().all(|x| *x == 0),
            "p{k} results {got:?}, expected tally {want:?}"
        );
        times.insert(k, t.elapsed().as_secs_f64());
    }
    let last = times.values().copied().fold(0.0, f64::max);
    Ok(format!(
        "results: all {} processes ended and tallied on-chain {last:.1} s after the first end, each {want:?} as cast",
        procs.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn parses_prover_times() {
        assert_eq!(rfc3339("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(rfc3339("2000-03-01T00:00:00Z"), Some(951_868_800.0));
        let a = rfc3339("2026-09-28T23:59:59.5Z").unwrap();
        let b = rfc3339("2026-09-29T00:00:01.25+00:00").unwrap();
        assert!((b - a - 1.75).abs() < 1e-6, "{}", b - a);
        assert_eq!(rfc3339("2026-09-28 10:00:00"), None);
    }
}
