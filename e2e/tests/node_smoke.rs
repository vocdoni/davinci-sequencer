//! A small election through a node that already runs (a production node
//! after a switch), gated by `DAVINCI_E2E_NODE_URL`. On the live deployment
//! of `davinci_e2e::net::live_target` (`DAVINCI_E2E_NETWORK`, default
//! `optimism`; `DAVINCI_E2E_RPC`, `DAVINCI_E2E_REGISTRY`), organized with the
//! key file `DAVINCI_E2E_ORGANIZER_KEY`: a CSP process with three voters and
//! the node's election key; the organizer ends it once the votes are queued,
//! the node seals them in its flush, settles them within the grace window
//! and publishes the tally. Spawns nothing. About 10 minutes, most of it the
//! registry's default grace.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use davinci_client::SequencerClient;
use davinci_client::api::{ProcessId, VoteStatus};
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, RegistryReader, verify_registry};
use davinci_e2e::fixture as fx;
use davinci_e2e::net;
use davinci_e2e::node::WorkDir;
use davinci_e2e::wait;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_smoke() -> Result<()> {
    let Ok(url) = std::env::var("DAVINCI_E2E_NODE_URL") else {
        eprintln!("DAVINCI_E2E_NODE_URL not set, skipping");
        return Ok(());
    };
    let (rpc, registry, _) = net::live_target()?;
    let pins = verify_registry(&rpc, registry)
        .await
        .context("registry pins")?;
    let key_file = std::env::var("DAVINCI_E2E_ORGANIZER_KEY")
        .context("DAVINCI_E2E_ORGANIZER_KEY (a key file) is required")?;
    let org = Organizer::connect(&rpc, net::read_key(Path::new(&key_file))?, registry)?;
    let node = SequencerClient::new(&url);
    let info = node.info().await.context("node /info")?;
    ensure!(
        info.chain_id == pins.chain_id && info.process_registry == registry.0.0 && !info.observer,
        "{url}: chain {}, registry or observer mode differ",
        info.chain_id
    );
    let mut dir = WorkDir::new("davinci-node-smoke-")?;
    let csp = fx::csp_key();
    // Nothing fetches the document; keep the operator's paths off the chain.
    let (_, metadata_hash) = fx::write_metadata(dir.path(), "smoke.json", "node smoke")?;
    let metadata = "davinci-e2e://node-smoke/metadata.json".to_string();
    let next = org.next_process_id().await?;
    let pk = node.new_key(&next).await?;
    let pid = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 3600,
            max_voters: fx::N_CSP as u64,
            ballot_mode: fx::ballot_mode(),
            census_origin: 4,
            census_root: fx::csp_root(&csp)?,
            census_contract: [0; 20],
            census_uri: "csp://davinci-node-smoke".into(),
            metadata,
            metadata_hash,
            key_mode: KeyMode::Sequencer(pk),
        })
        .await?
        .pid;
    eprintln!("process {}", ProcessId(pid));
    let limit = net::scaled(Duration::from_secs(45 * 60));
    let r = match tokio::time::timeout(limit, vote(&org, &node, &rpc, registry, &pid, &csp)).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("the smoke election took over {limit:?}")),
    };
    if r.is_err() {
        // Only READY or PAUSED processes can be cancelled; after an end this fails.
        let _ = org.cancel_process(&pid).await;
    }
    dir.ok = r.is_ok();
    if let Err(e) = &r {
        eprintln!("FAILED: {}", davinci_e2e::redact_urls(&format!("{e:#}")));
    }
    // The chain may embed an RPC URL: only the redacted copy above is printed.
    ensure!(r.is_ok(), "node smoke failed; see the FAILED line");
    Ok(())
}

async fn vote(
    org: &Organizer,
    node: &SequencerClient,
    rpc: &str,
    registry: alloy::primitives::Address,
    pid: &[u8; 31],
    csp: &k256::ecdsa::SigningKey,
) -> Result<()> {
    let p = RegistryReader::connect(rpc, registry)?.process(pid).await?;
    let prover = tokio::task::spawn_blocking(fx::load_prover).await??;
    let mut jobs = Vec::new();
    for (i, voter) in fx::csp_voters().iter().enumerate() {
        jobs.push(fx::Job {
            label: format!("voter {i}"),
            key: voter.key.clone(),
            process: p.clone(),
            fields: fx::choices(i, 1),
            census: fx::csp_witness(csp, &p, voter, i as u64),
            weight: fx::WEIGHT,
            k: fx::vote_k(0x60, i, 1),
        });
    }
    let reqs = tokio::task::block_in_place(|| fx::prove_all(&prover, &jobs))?;
    wait::until(
        "the node to list the process",
        net::scaled(Duration::from_secs(120)),
        Duration::from_secs(2),
        || async { Ok(node.process(pid).await.ok().map(|_| ())) },
    )
    .await?;
    for r in &reqs {
        node.submit_vote(r)
            .await
            .with_context(|| format!("vote {}", r.vote_id))?;
    }
    // Queued: the end flushes them.
    org.end_process(pid).await?;
    for r in &reqs {
        wait::until(
            &format!("vote {} to settle", r.vote_id),
            net::scaled(Duration::from_secs(600)),
            Duration::from_secs(5),
            || async {
                let s = node.vote_status_full(pid, r.vote_id).await?;
                ensure!(
                    s.status != VoteStatus::Error,
                    "vote {}: {}",
                    r.vote_id,
                    s.error.unwrap_or_default()
                );
                Ok((s.status == VoteStatus::Settled).then_some(()))
            },
        )
        .await?;
    }
    eprintln!("3 votes settled");
    let last: Vec<Vec<u64>> = (0..fx::N_CSP).map(|i| fx::choices(i, 1)).collect();
    let want = fx::expected_tally(&last);
    let got = wait::until(
        "the results",
        net::scaled(Duration::from_secs(20 * 60)),
        Duration::from_secs(10),
        || async { Ok(org.results(pid).await?) },
    )
    .await?;
    ensure!(got == want, "results {got:?}, tally {want:?}");
    eprintln!("results on-chain = tally {want:?}");
    Ok(())
}
