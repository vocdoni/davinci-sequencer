//! One 512-vote, 16-field transition in L1-blob mode, with the real prover.
//! Gated by `DAVINCI_E2E_L1_BIG=1`. On two anvils it needs
//! `DAVINCI_E2E_DA=l1-blobs` and the OP Mainnet registry's grace window,
//! `DAVINCI_E2E_GRACE=300,240,600,1800,60`; live (`DAVINCI_E2E_LIVE=1`) it
//! runs on the `optimism` preset with the `op_live` environment.
//!
//! One node with the L1-blob defaults (`--l1-settle-lag 110s`,
//! `--settle-margin 30s`, `--prove-base 30s`) and `--batch-max 512`. 512
//! first votes settle in whatever batches the seal budget allows. Then the
//! organizer pauses the process, 512 new voters vote, and the organizer ends
//! it: the flush seals all 512 votes with 512 silent refreshes, nine blobs,
//! which go out as two Ethereum blob transactions and settle in one proof
//! within the grace. The process takes the registry's ceiling grace (600 s),
//! so the node's cold proving estimate (0.8 s a vote at 16 fields) fits 512
//! votes after the end. The node then publishes the results, which must
//! equal the tally.
//!
//! With `DAVINCI_E2E_RECORD_DIR`, the first batch job's snark and register
//! dump are written there as `snark_batch.json` and `publics_batch.bin`, the
//! format of davinci-zkvm's `rust-sdk/testdata`. About 30 minutes (half of
//! it the grace window); the shared prover's queue must be empty first.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use davinci_client::api::{ProcessId, ProcessStatus, VoteRequest, VoteStatus};
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, RegistryReader};
use davinci_client::prover::BallotProver;
use davinci_e2e::fixture as fx;
use davinci_e2e::l1::{self, L1Post};
use davinci_e2e::net::{self, Net};
use davinci_e2e::node::{self, Node, WorkDir};
use davinci_e2e::proxy::{Exchange, Proxy, Tap};
use davinci_e2e::wait;
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::JobId;

/// The OP Mainnet registry's grace window.
const OP_GRACE: (u32, u32, u32, u32, u32) = (300, 240, 600, 1800, 60);
const N: usize = 512;
const NF: u8 = 16;

/// The job ids of the node's `/prove` calls, in order.
fn prove_tap(jobs: Arc<Mutex<Vec<String>>>) -> Tap {
    Arc::new(move |x: &Exchange<'_>| {
        if x.method != "POST" || x.path != "/prove" {
            return;
        }
        let id = serde_json::from_slice::<serde_json::Value>(x.response)
            .ok()
            .and_then(|v| v["job_id"].as_str().map(String::from));
        if let (Some(id), Ok(mut j)) = (id, jobs.lock()) {
            j.push(id);
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l1_big_batch() -> Result<()> {
    if std::env::var("DAVINCI_E2E_L1_BIG").as_deref() != Ok("1") {
        eprintln!("DAVINCI_E2E_L1_BIG not set, skipping");
        return Ok(());
    }
    let prover_url =
        std::env::var("DAVINCI_ZKVM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let health = ProverClient::new(&prover_url).health().await?;
    ensure!(
        health.status == "ok" && health.queue_len == 0,
        "the shared prover is busy or down: {health:?}"
    );
    let mut dir = WorkDir::new("davinci-l1-big-")?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir.path()).await?;
    ensure!(
        net.l1.is_some(),
        "DAVINCI_E2E_L1_BIG runs in L1-blob mode: DAVINCI_E2E_DA=l1-blobs on anvil"
    );
    net.check_balances(1).await?;
    let org = net.organizer()?;
    let g = org.grace_params().await?;
    ensure!(
        (
            g.default_grace,
            g.grace_floor,
            g.grace_ceil,
            g.grace_max_total,
            g.notice_min
        ) == OP_GRACE,
        "the registry's grace is {g:?}: on anvil set DAVINCI_E2E_GRACE=300,240,600,1800,60"
    );
    let r = run(&net, &org, &prover_url, dir.path(), ballot_prover).await;
    match &r {
        Ok(()) => eprintln!("PASSED"),
        Err(e) => eprintln!("FAILED: {}", davinci_e2e::redact_urls(&format!("{e:#}"))),
    }
    let c = org.cancel_open().await;
    dir.ok = r.is_ok();
    // The chain may embed an RPC URL: only the redacted copy above is printed.
    ensure!(r.is_ok(), "l1_big failed; see the FAILED line");
    c.context("cancel the open processes")?;
    Ok(())
}

async fn run(
    net: &Net,
    org: &Organizer,
    prover_url: &str,
    dir: &Path,
    ballot_prover: tokio::task::JoinHandle<Result<BallotProver>>,
) -> Result<()> {
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let census_dir = dir.join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;
    let jobs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let proxy = Proxy::tapped(prover_url, prove_tap(jobs.clone())).await?;
    let mut c = net.node_config("node-big", Some(0), 0, &proxy.url, &census_dir, dir)?;
    // The node's own L1-blob budget: 110 s Ethereum leg, 30 s margin, 30 s
    // prove base, 3 m flush horizon.
    (
        c.l1_settle_lag,
        c.settle_margin,
        c.prove_base,
        c.flush_horizon,
    ) = (None, None, None, None);
    c.batch_max = N;
    c.batch_time = "30s".into();
    let node = Node::start(&bin, &c).await?;
    eprintln!("node at {} log {}", node.url, node.log.display());

    let voters = fx::bench_voters(2 * N);
    let (census, tree) = fx::merkle_census_of(&voters)?;
    let uri = fx::write_census(&census_dir, "big.json", &census)?;
    let (metadata, metadata_hash) = fx::write_metadata(&census_dir, "big-metadata.json", "big")?;
    let next = org.next_process_id().await?;
    let pk = node.api.new_key(&next).await?;
    let pid = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 2 * 3600,
            max_voters: 2 * N as u64,
            ballot_mode: fx::ballot_mode_nf(NF),
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
    org.set_process_grace(&pid, OP_GRACE.2).await?;
    eprintln!("process {} with a {} s grace", ProcessId(pid), OP_GRACE.2);

    let p = RegistryReader::connect(&net.rpc, net.registry)?
        .process(&pid)
        .await?;
    let mut list = Vec::new();
    for (v, voter) in voters.iter().enumerate() {
        list.push(fx::Job {
            label: format!("voter {v}"),
            key: voter.key.clone(),
            process: p.clone(),
            fields: fx::choices_nf(v, NF as usize),
            census: fx::merkle_witness(&tree, v)?,
            weight: fx::WEIGHT,
            k: fx::vote_k(0x51, v, 1),
        });
    }
    let prover = ballot_prover.await??;
    let t = Instant::now();
    let reqs = tokio::task::block_in_place(|| fx::prove_all(&prover, &list))?;
    eprintln!(
        "{} ballots proved in {:.0} s",
        reqs.len(),
        t.elapsed().as_secs_f64()
    );
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
    let l1 = net.l1.as_ref().context("L1-blob mode")?;
    let scan_from = net.start_block.unwrap_or(net.from_block);
    let ours = async || -> Result<Vec<L1Post>> {
        let all = l1::posts(&net.rpc, l1.read_rpc(), net.registry, scan_from).await?;
        Ok(all.into_iter().filter(|p| p.pid == pid).collect())
    };

    // Phase 1: the first 512 voters, in the batches the seal budget allows.
    let phase1 = Instant::now();
    submit(&node, &reqs[..N]).await?;
    // The root of the first settled transition, for the fixture check.
    settled(&node, &pid, &reqs[..1]).await?;
    let root1 = RegistryReader::connect(&net.rpc, net.registry)?
        .process(&pid)
        .await?
        .state_root;
    settled(&node, &pid, &reqs[..N]).await?;
    let first = ours().await?;
    eprintln!("first 512 votes settled in {} transitions", first.len());
    // Wall time, not the proving rate: it includes the batch timer and the
    // Ethereum and OP legs of every transition.
    eprintln!(
        "phase 1 wall time: {:.2} s per vote from submit to settled ({:.0} s for {N})",
        phase1.elapsed().as_secs_f64() / N as f64,
        phase1.elapsed().as_secs_f64()
    );
    prover_rates(prover_url, &jobs).await;

    // Phase 2: 512 new voters while paused, then the end; the flush seals them all.
    org.pause_process(&pid).await?;
    wait_status(&node, &pid, ProcessStatus::Paused).await?;
    submit(&node, &reqs[N..]).await?;
    org.end_process(&pid).await?;
    let ended = Instant::now();
    settled(&node, &pid, &reqs[N..]).await?;
    eprintln!(
        "512 x 16 settled {:.0} s after the end",
        ended.elapsed().as_secs_f64()
    );
    let posts = ours().await?;
    ensure!(
        posts.len() == first.len() + 1,
        "{} settlements after the end, want one",
        posts.len() - first.len()
    );
    let big = posts.last().context("no L1 settlement")?;
    ensure!(big.vhs.len() == 9, "{} blobs, want 9", big.vhs.len());
    ensure!(
        big.l1_txs.len() == 2,
        "{} Ethereum transactions, want 2",
        big.l1_txs.len()
    );
    let sealed = node.log_lines(&["batch sealed"]);
    let line = sealed.last().context("no seal logged")?;
    ensure!(
        line.contains("votes=512") && line.contains("reason=flush"),
        "the last seal: {line}"
    );
    let settled_p = RegistryReader::connect(&net.rpc, net.registry)?
        .process(&pid)
        .await?;
    ensure!(
        settled_p.voters_count == 2 * N as u64,
        "{} voters",
        settled_p.voters_count
    );
    eprintln!(
        "nine blobs in Ethereum blocks {:?} ({} transactions), OP tx {}",
        big.l1_blocks,
        big.l1_txs.len(),
        big.op_tx
    );

    // The node holds the key: after the grace, the results equal the tally.
    let ballots: Vec<Vec<u64>> = (0..2 * N).map(|v| fx::choices_nf(v, NF as usize)).collect();
    let want = fx::expected_tally(&ballots);
    let got = wait::until(
        "the results",
        net::scaled(Duration::from_secs(25 * 60)),
        Duration::from_secs(10),
        || async {
            node.check_alive()?;
            Ok(org.results(&pid).await?)
        },
    )
    .await?;
    ensure!(got == want, "results {got:?}, tally {want:?}");
    eprintln!("results on-chain = tally");
    drop(node);
    prover_rates(prover_url, &jobs).await;

    // The fixture last, so a failed recording never hides the assertions above.
    if let Some(out) = std::env::var_os("DAVINCI_E2E_RECORD_DIR") {
        let id = jobs.lock().ok().and_then(|j| j.first().cloned());
        let r = match id {
            Some(id) => record(prover_url, &id, Path::new(&out), &root1).await,
            None => Err(anyhow::anyhow!("no /prove call seen")),
        };
        if let Err(e) = r {
            eprintln!(
                "WARNING: fixture not recorded: {}",
                davinci_e2e::redact_urls(&format!("{e:#}"))
            );
            bail!("the batch fixture was not recorded (see the WARNING line)");
        }
    }
    Ok(())
}

/// The prover's measured seconds per vote for each batch job the node
/// posted, and their mean (the warmed figure for the 300 s grace question).
/// A failed read is a warning.
async fn prover_rates(prover_url: &str, jobs: &Mutex<Vec<String>>) {
    let ids: Vec<String> = jobs.lock().map(|j| j.clone()).unwrap_or_default();
    let pc = ProverClient::new(prover_url);
    let mut rates = Vec::new();
    for id in &ids {
        let r: Result<f64> = async {
            let jid = JobId::parse(id)?;
            let ms = pc.job(&jid).await?.elapsed_ms.context("no elapsed_ms")?;
            let p = BatchPublics::parse(&pc.publics(&jid).await?)?;
            ensure!(p.voters > 0, "no voters");
            Ok(ms as f64 / 1000.0 / f64::from(p.voters))
        }
        .await;
        match r {
            Ok(v) => {
                eprintln!("prover job {id}: {v:.3} s per vote");
                rates.push(v);
            }
            Err(e) => eprintln!(
                "WARNING: prover job {id}: {}",
                davinci_e2e::redact_urls(&format!("{e:#}"))
            ),
        }
    }
    if !rates.is_empty() {
        eprintln!(
            "prover rate: mean {:.3} s per vote over {} jobs",
            rates.iter().sum::<f64>() / rates.len() as f64,
            rates.len()
        );
    }
}

async fn submit(node: &Node, reqs: &[VoteRequest]) -> Result<()> {
    for r in reqs {
        node.api
            .submit_vote(r)
            .await
            .with_context(|| format!("vote {}", r.vote_id))?;
    }
    Ok(())
}

/// Waits until every vote of `reqs` is settled (30 minutes at most).
async fn settled(node: &Node, pid: &[u8; 31], reqs: &[VoteRequest]) -> Result<()> {
    let mut open: Vec<u64> = reqs.iter().map(|r| r.vote_id).collect();
    let limit = net::scaled(Duration::from_secs(30 * 60));
    let t = Instant::now();
    while !open.is_empty() {
        ensure!(
            t.elapsed() < limit,
            "{} votes not settled after {limit:?}",
            open.len()
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        node.check_alive()?;
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
    }
    Ok(())
}

/// Waits until the node reports the process with `status`.
async fn wait_status(node: &Node, pid: &[u8; 31], status: ProcessStatus) -> Result<()> {
    wait::until(
        "the node to see the status",
        net::scaled(Duration::from_secs(60)),
        Duration::from_secs(1),
        || async { Ok((node.api.process(pid).await?.status == status).then_some(())) },
    )
    .await
}

/// Writes job `id`'s snark (the key order of davinci-zkvm's testdata) and
/// its 256-byte register dump into `out`.
async fn record(prover_url: &str, id: &str, out: &Path, root1: &[u8; 32]) -> Result<()> {
    let pc = ProverClient::new(prover_url);
    let id = JobId::parse(id)?;
    let s = pc.snark(&id).await?;
    let regs = pc.publics(&id).await?;
    ensure!(regs.len() == 256, "{} register bytes", regs.len());
    let p = BatchPublics::parse(&regs)?;
    let mut rev = p.root_after;
    rev.reverse();
    ensure!(
        p.ok && (p.root_after == *root1 || rev == *root1),
        "job {id} is not the first settled transition of this run"
    );
    std::fs::create_dir_all(out)?;
    let json = format!(
        "{{\n  \"program_vk\": \"0x{}\",\n  \"root_c_vadcop_final\": \"0x{}\",\n  \"public_values\": \"0x{}\",\n  \"proof_bytes\": \"0x{}\"\n}}\n",
        hex::encode(s.program_vk),
        hex::encode(s.root_c_vadcop_final),
        hex::encode(&s.public_values),
        hex::encode(&s.proof_bytes)
    );
    std::fs::write(out.join("snark_batch.json"), json)?;
    std::fs::write(out.join("publics_batch.bin"), &regs)?;
    eprintln!(
        "recorded job {id} in {}: voters {} overwrites {} n_blobs {} occupied_before {} \
         nproofs {} n_public {} log_n {}",
        out.display(),
        p.voters,
        p.overwrites,
        p.n_blobs,
        p.occupied_before,
        p.nproofs,
        p.n_public,
        p.log_n
    );
    Ok(())
}
