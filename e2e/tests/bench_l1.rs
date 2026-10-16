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
