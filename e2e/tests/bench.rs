//! Batch-size benchmark on one node, gated by `DAVINCI_E2E_BENCH=sizes`.
//! Anvil by default, `DAVINCI_E2E_LIVE=1` on an existing chain (see
//! `davinci_e2e::net`). Per case (size N, nf): an origin-1 process with a
//! 2N-voter census and a fresh node with `--batch-max N`; a first batch of
//! voters 0..N, then a steady batch of voters N..2N, which carries N silent
//! refreshes. Sizes from `DAVINCI_E2E_BENCH_SIZES` (default 4,16,64,128,256)
//! at `DAVINCI_E2E_BENCH_NF` fields (default 4), plus 64 at nf=16 (two blobs
//! when steady). Rows go to stderr, and the Markdown table to the file named
//! by `DAVINCI_E2E_BENCH_OUT` when set. Ballots bind the pid and the election
//! key, so they are proved per run; their time is reported apart.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alloy::primitives::{U256, utils::format_ether};
use anyhow::{Context, Result, bail, ensure};
use davinci_client::api::{ProcessId, VoteRequest, VoteStatus};
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, RegistryReader};
use davinci_client::prover::BallotProver;
use davinci_e2e::chain;
use davinci_e2e::cost::{self, TxCost};
use davinci_e2e::fixture as fx;
use davinci_e2e::net::{self, Net};
use davinci_e2e::node::{self, Node, WorkDir};
use davinci_e2e::wait;
use davinci_zkvm_sdk::client::ProverClient;

const DEFAULT_SIZES: &str = "4,16,64,128,256";

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.into())
}

/// (N, nf) per case.
fn cases() -> Result<Vec<(usize, u8)>> {
    let nf: u8 = env_or("DAVINCI_E2E_BENCH_NF", "4")
        .parse()
        .context("DAVINCI_E2E_BENCH_NF")?;
    ensure!((2..=16).contains(&nf), "DAVINCI_E2E_BENCH_NF {nf}: 2 to 16");
    let mut out = Vec::new();
    for s in env_or("DAVINCI_E2E_BENCH_SIZES", DEFAULT_SIZES).split(',') {
        let n: usize = s.trim().parse().context("DAVINCI_E2E_BENCH_SIZES")?;
        ensure!(
            (1..=512).contains(&n),
            "size {n}: 1 to 512 (the census is 2N)"
        );
        out.push((n, nf));
    }
    if !out.contains(&(64, 16)) {
        out.push((64, 16));
    }
    Ok(out)
}

/// One batch of one case.
struct Row {
    n: usize,
    nf: u8,
    phase: &'static str,
    refreshes: usize,
    /// Seconds from the first submit.
    submitted: f64,
    sealed: f64,
    settled: f64,
    txs: Vec<TxCost>,
}

impl Row {
    fn line(&self) -> String {
        let blocks: Vec<String> = self.txs.iter().map(|t| t.block.to_string()).collect();
        let blobs: Vec<String> = self.txs.iter().map(|t| t.blobs().to_string()).collect();
        let gas: u64 = self.txs.iter().map(|t| t.gas).sum();
        let wei: U256 = self.txs.iter().map(TxCost::wei).sum();
        format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.1} |",
            self.n,
            self.nf,
            self.phase,
            self.refreshes,
            self.txs.len(),
            blocks.join(", "),
            blobs.join("+"),
            gas,
            format_ether(wei),
            self.submitted,
            self.sealed,
            self.settled,
        )
    }
}

const HEADER: &str = "| N | nf | batch | refreshes (expected) | txs | blocks | blobs | gas | native | submitted s | sealed s | settled s |\n|---:|---:|---|---:|---:|---|---|---:|---:|---:|---:|---:|";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_sizes() -> Result<()> {
    if std::env::var("DAVINCI_E2E_BENCH").as_deref() != Ok("sizes") {
        eprintln!("DAVINCI_E2E_BENCH=sizes not set, skipping");
        return Ok(());
    }
    let cases = cases()?;
    let prover_url = env_or("DAVINCI_ZKVM_URL", "http://127.0.0.1:8080");
    let health = ProverClient::new(&prover_url)
        .health()
        .await
        .with_context(|| format!("prover health at {prover_url}"))?;
    ensure!(health.status == "ok", "prover health: {health:?}");

    // Kept with the node logs unless every case passes.
    let mut dir = WorkDir::new("davinci-bench-")?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir.path()).await?;
    net.check_balances(1).await?;
    let org = net.organizer()?;
    let reader = RegistryReader::connect(&net.rpc, net.registry)?;
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let prover = ballot_prover.await??;
    let census_dir = dir.path().join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;

    let out = std::env::var_os("DAVINCI_E2E_BENCH_OUT").map(PathBuf::from);
    let date = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let head = format!(
        "# Batch-size benchmark, chain {}\n\n\
         Date {date}. Registry {}, prover {} ({}). One node per case,\n\
         `--batch-max N`, `--batch-time {}`, {} confirmations. A case is an\n\
         origin-1 process with a 2N-voter census: the first batch is voters\n\
         0..N (no refreshes), the steady batch voters N..2N, which carries N\n\
         silent refreshes (2N slot updates); the refreshes column is that\n\
         expectation, not a measurement. Times are seconds from the first\n\
         submit of the batch; `native` is gas plus blob fee, summed over the\n\
         batch's transactions (more than one is a blob-cap split).\n\n",
        net.chain_id,
        net.registry,
        prover_url,
        env_or("DAVINCI_E2E_BENCH_PROVER", "RTX 5090, single"),
        batch_time(),
        net.confirmations,
    );
    let mut rows: Vec<Row> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let r: Result<()> = async {
        for (i, &(n, nf)) in cases.iter().enumerate() {
            let (r, note) = case(
                &net,
                &org,
                &reader,
                &bin,
                &prover,
                &prover_url,
                &census_dir,
                dir.path(),
                (i, n, nf),
            )
            .await
            .with_context(|| format!("case N={n} nf={nf}"))?;
            eprintln!("{note}");
            for r in &r {
                eprintln!("{}", r.line());
            }
            rows.extend(r);
            notes.push(note);
            if let Some(p) = &out {
                let table: Vec<String> = rows.iter().map(Row::line).collect();
                let body = format!(
                    "{head}{HEADER}\n{}\n\n{}\n",
                    table.join("\n"),
                    notes
                        .iter()
                        .map(|n| format!("- {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                std::fs::write(p, body).with_context(|| format!("write {}", p.display()))?;
            }
        }
        Ok(())
    }
    .await;
    // A failed case may leave its process open; nothing stays for a later run.
    let c = org.cancel_open().await;
    if let Some(p) = &out {
        eprintln!("wrote {}", p.display());
    }
    r?;
    c.context("cancel the open processes")?;
    dir.ok = true;
    Ok(())
}

/// `DAVINCI_E2E_BENCH_BATCH_TIME` (default 2m): long enough that every vote
/// of a batch is queued before it seals, short enough that the remainder of
/// a blob-cap split does not stall the case.
fn batch_time() -> String {
    env_or("DAVINCI_E2E_BENCH_BATCH_TIME", "2m")
}

#[allow(clippy::too_many_arguments)]
async fn case(
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    bin: &Path,
    prover: &BallotProver,
    prover_url: &str,
    census_dir: &Path,
    dir: &Path,
    (i, n, nf): (usize, usize, u8),
) -> Result<(Vec<Row>, String)> {
    let name = format!("bench-{n}-nf{nf}");
    let mut cfg = net.node_config(&name, Some(0), 0, prover_url, census_dir, dir)?;
    cfg.batch_max = n;
    cfg.batch_time = batch_time();
    let node = Node::start(bin, &cfg).await?;
    let cap = node.logged_blob_cap();
    if net.live {
        ensure!(
            cap.is_none_or(|c| c == 2),
            "{name}: blob cap {cap:?}, want 2"
        );
    }

    let voters = fx::bench_voters(2 * n);
    let (census, tree) = fx::merkle_census_of(&voters)?;
    let uri = fx::write_census(census_dir, &format!("{name}.json"), &census)?;
    let (metadata, metadata_hash) =
        fx::write_metadata(census_dir, &format!("{name}-metadata.json"), &name)?;
    let next = org.next_process_id().await?;
    let pk = node.api.new_key(&next).await?;
    let pid = org
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
    let r = measure(net, reader, &node, prover, &tree, &voters, pid, (i, n, nf)).await;
    // Nothing stays open for a later node to tally.
    let c = org.cancel_process(&pid).await;
    let (rows, gen_s) = r?;
    c.context("cancel")?;
    let note = format!(
        "N={n} nf={nf}: process {}, {} ballots proved in {gen_s:.1} s, node blob cap {}",
        ProcessId(pid),
        2 * n,
        cap.map_or("not logged".into(), |c| c.to_string())
    );
    Ok((rows, note))
}

#[allow(clippy::too_many_arguments)]
async fn measure(
    net: &Net,
    reader: &RegistryReader,
    node: &Node,
    prover: &BallotProver,
    tree: &davinci_zkvm_sdk::census::LeanImt,
    voters: &[davinci_client::voter::Voter],
    pid: [u8; 31],
    (i, n, nf): (usize, usize, u8),
) -> Result<(Vec<Row>, f64)> {
    let p = reader.process(&pid).await?;
    let t = Instant::now();
    let mut jobs = Vec::new();
    for (v, voter) in voters.iter().enumerate() {
        jobs.push(fx::Job {
            label: format!("bench voter {v}"),
            key: voter.key.clone(),
            process: p.clone(),
            fields: fx::choices_nf(v, nf as usize),
            census: fx::merkle_witness(tree, v)?,
            weight: fx::WEIGHT,
            k: fx::vote_k(0x20 + i as u64, v, 1),
        });
    }
    let reqs = fx::prove_all(prover, &jobs)?;
    let gen_s = t.elapsed().as_secs_f64();

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

    let from = net.start_block.unwrap_or(net.from_block);
    let mut rows = Vec::new();
    let mut seen = 0;
    for (phase, part, refreshes) in [("first", &reqs[..n], 0), ("steady", &reqs[n..], n)] {
        let (submitted, sealed, settled) = batch(node, &pid, part).await?;
        let txs: Vec<_> = chain::transitions(&net.rpc, net.registry, from)
            .await?
            .into_iter()
            .filter(|t| t.pid == pid)
            .collect();
        let mut costs = Vec::new();
        for t in &txs[seen..] {
            costs.push(cost::fetch(&net.rpc, format!("{phase} N={n}"), t.tx).await?);
        }
        seen = txs.len();
        rows.push(Row {
            n,
            nf,
            phase,
            refreshes,
            submitted,
            sealed,
            settled,
            txs: costs,
        });
    }
    Ok((rows, gen_s))
}

/// Submits `reqs` and waits until every one is settled; seconds from the
/// first submit to the last accepted, to none pending, and to all settled.
async fn batch(node: &Node, pid: &[u8; 31], reqs: &[VoteRequest]) -> Result<(f64, f64, f64)> {
    let t = Instant::now();
    for r in reqs {
        node.api
            .submit_vote(r)
            .await
            .with_context(|| format!("vote {}", r.vote_id))?;
    }
    let submitted = t.elapsed().as_secs_f64();
    let mut open: Vec<u64> = reqs.iter().map(|r| r.vote_id).collect();
    let mut sealed = None;
    let limit = net::scaled(Duration::from_secs(30 * 60));
    loop {
        node.check_alive()?;
        let mut pending = false;
        let mut still = Vec::new();
        for vid in open {
            let s = node.api.vote_status_full(pid, vid).await?;
            match s.status {
                VoteStatus::Settled => {}
                VoteStatus::Error => bail!("vote {vid}: {}", s.error.unwrap_or_default()),
                VoteStatus::Pending => {
                    pending = true;
                    still.push(vid);
                }
                _ => still.push(vid),
            }
        }
        open = still;
        if !pending && sealed.is_none() {
            sealed = Some(t.elapsed().as_secs_f64());
        }
        if open.is_empty() {
            let settled = t.elapsed().as_secs_f64();
            return Ok((submitted, sealed.unwrap_or(settled), settled));
        }
        ensure!(
            t.elapsed() < limit,
            "{} votes not settled after {limit:?}",
            open.len()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
