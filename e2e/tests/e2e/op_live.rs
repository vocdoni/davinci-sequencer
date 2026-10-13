//! The live run on an L1-blob deployment (OP Mainnet), gated by
//! `DAVINCI_E2E_OP_LIVE=1` with `DAVINCI_E2E_LIVE=1`: one signing node and an
//! observer that only follows, a Merkle process (eight voters, three
//! revotes) and a CSP process (three voters), batches of four. Node A is
//! killed once its first blob transaction is in Ethereum's pool and
//! restarted. The observer rebuilds every transition from Ethereum beacon
//! blobs. Both processes end with their results; with
//! `DAVINCI_E2E_NEGATIVE=1` the L1 negatives run through `eth_call`. About
//! six blob posts, a few cents on Ethereum.

use super::*;
use alloy::providers::Provider;

/// Process 1 voters of the live run (round 1), all through node A.
const LIVE_VOTERS: std::ops::Range<usize> = 0..8;
/// Votes per batch: two round-1 batches, one CSP and one revote batch.
const LIVE_BATCH: usize = 4;
/// The node's batch timer: a partial batch (CSP, revotes) seals after it.
const LIVE_BATCH_TIME: &str = "20s";

pub(super) async fn run(dir: &Path) -> Result<()> {
    let prover_url =
        std::env::var("DAVINCI_ZKVM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let health = ProverClient::new(&prover_url)
        .health()
        .await
        .with_context(|| format!("prover health at {prover_url}"))?;
    ensure!(health.status == "ok", "prover health: {health:?}");
    say!("prover ok, queue_len {}", health.queue_len);
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir).await?;
    let l1 = net
        .l1
        .as_ref()
        .filter(|_| net.live)
        .context("DAVINCI_E2E_OP_LIVE needs DAVINCI_E2E_LIVE=1 on an L1-blob deployment")?;
    net.check_balances(1).await?;
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let census_dir = dir.join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;
    let cfg = |name: &str, seq: Option<usize>, rpc: usize| -> Result<NodeConfig> {
        let mut c = net.node_config(name, seq, rpc, &prover_url, &census_dir, dir)?;
        c.batch_max = LIVE_BATCH;
        c.batch_time = LIVE_BATCH_TIME.into();
        Ok(c)
    };
    let a_cfg = cfg("node-a", Some(0), 0)?;
    let mut nodes = vec![
        Node::start(&bin, &a_cfg).await?,
        Node::start(&bin, &cfg("node-obs", None, 1)?).await?,
    ];
    for n in &nodes {
        let info = n
            .api
            .info()
            .await
            .with_context(|| format!("{} /info", n.name))?;
        ensure!(
            info.chain_id == net.chain_id && info.process_registry == net.registry.0.0,
            "{}: chain {} or registry",
            n.name,
            info.chain_id
        );
        ensure!(
            info.observer == n.address.is_none(),
            "{}: observer flag",
            n.name
        );
        // Ethereum's blob limit, capped at six per transaction.
        if let Some(c) = n.logged_blob_cap() {
            ensure!(c == 6, "{}: blob cap {c}, want 6", n.name);
        }
        say!("{} at {} log {}", n.name, n.url, n.log.display());
    }

    let org = net.organizer()?;
    let reader = RegistryReader::connect(&net.rpc, net.registry)?;
    let scan_from = net.start_block.unwrap_or(net.from_block);
    let r: Result<()> = async {
        let Elections {
            pid1,
            pid2,
            p1,
            p2,
            tree,
            csp,
        } = create_processes(
            &org,
            &reader,
            &census_dir,
            async |pid| Ok(nodes[A].api.new_key(&pid).await?),
            async |pid| Ok(nodes[A].api.new_key(&pid).await?),
        )
        .await?;
        wait_listed(&nodes, &reader, &pid1).await?;
        wait_listed(&nodes, &reader, &pid2).await?;
        let prover = ballot_prover.await??;
        let b = fx::build_ballots(&prover, &p1, &tree, &p2, &csp)?;

        // Round 1 and the CSP votes, all to node A.
        let read = l1.provider()?;
        let (_, l1_addr) = l1.keys[0];
        let nonce = read.get_transaction_count(l1_addr).await?;
        let mut round1 = Vec::new();
        for v in LIVE_VOTERS {
            let label = format!("p1 voter {v} round 1");
            submit(&nodes[A], &b.round1[v], &label).await?;
            round1.push(sent(label, A, &b.round1[v]));
        }
        for (i, req) in b.csp.iter().enumerate() {
            let label = format!("p2 voter {i}");
            submit(&nodes[A], req, &label).await?;
            round1.push(sent(label, A, req));
        }
        say!("round 1 submitted: {} votes", round1.len());

        // Restart mid-flight: once node A's first post is in the pool.
        wait::until(
            "node-a's first blob post in Ethereum's pool",
            settle_timeout(),
            Duration::from_millis(500),
            || async {
                alive(&nodes)?;
                let pending = read.get_transaction_count(l1_addr).pending().await?;
                Ok((pending > nonce).then_some(()))
            },
        )
        .await?;
        nodes[A].kill()?;
        say!("node-a killed with an Ethereum blob post sent (nonce {nonce})");
        nodes[A] = Node::start(&bin, &a_cfg).await.context("restart node-a")?;
        wait_settled(&nodes, &round1, "round 1").await?;
        let posted = posted_nonces(&nodes[A]);
        let picked_up = !nodes[A]
            .log_lines(&["L1 post of an earlier run on record"])
            .is_empty();
        say!(
            "after the restart: post record picked up {picked_up}; mined posts at nonces {posted:?} \
             ({})",
            if posted.contains(&nonce) {
                "the pending post was replaced at its nonce, or mined"
            } else {
                "the orphan was mined before the restart's first post"
            }
        );
        ensure!(
            picked_up,
            "the restart did not pick up the earlier L1 post record"
        );

        // Round 2: the revotes of voters 0..8.
        let mut round2 = Vec::new();
        for (j, v) in fx::REVOTERS.into_iter().enumerate() {
            if LIVE_VOTERS.contains(&v) {
                let label = format!("p1 voter {v} round 2");
                submit(&nodes[A], &b.round2[j], &label).await?;
                round2.push(sent(label, A, &b.round2[j]));
            }
        }
        wait_settled(&nodes, &round2, "round 2").await?;

        // The observer rebuilt every transition from Ethereum beacon blobs.
        let root1 = wait_roots(&nodes, &reader, &pid1).await?;
        let root2 = wait_roots(&nodes, &reader, &pid2).await?;
        check_local_trees(&nodes, &pid1, b.round1[0].vote_id, root1).await?;
        check_local_trees(&nodes, &pid2, b.csp[0].vote_id, root2).await?;
        let events = chain::transitions(&net.rpc, net.registry, scan_from).await?;
        let ours = events
            .iter()
            .filter(|t| t.pid == pid1 || t.pid == pid2)
            .count() as u64;
        let ms = all_metrics(&nodes).await?;
        say!("transitions {ours}; {}", show_metrics(&nodes, &ms));
        ensure!(
            ms[1].settled_by_self == 0 && ms[1].synced_from_others == ours,
            "observer: {}",
            show_metrics(&nodes[1..], &ms[1..])
        );

        // Results of both processes, keyed by node A.
        let last1: Vec<_> = LIVE_VOTERS
            .map(|v| fx::choices(v, if fx::REVOTERS.contains(&v) { 2 } else { 1 }))
            .collect();
        let last2: Vec<_> = (0..fx::N_CSP).map(|i| fx::choices(i, 1)).collect();
        for (pid, want) in [
            (pid1, fx::expected_tally(&last1)),
            (pid2, fx::expected_tally(&last2)),
        ] {
            org.end_process(&pid).await.context("end process")?;
            wait_results(&nodes, &org, &pid, &want).await?;
            grace::closed_after_grace(&net, &reader, &pid).await?;
        }
        if flag("DAVINCI_E2E_NEGATIVE") {
            negative::run(&net.rpc, net.registry, scan_from, Some(pid1))
                .await
                .context("negative checks")?;
        }
        Ok(())
    }
    .await;
    let r = cancel_open(&org, r).await;
    let bill = costs(&net, &org, Vec::new()).await;
    say!("{}", bill.unwrap_or_else(|e| format!("gas bill: {e:#}")));
    drop(nodes);
    r
}

/// The nonces of the node's mined blob posts, from its log
/// (`blobs posted on L1` with `nonce=N`).
fn posted_nonces(n: &Node) -> Vec<u64> {
    n.log_lines(&["blobs posted on L1"])
        .iter()
        .filter_map(|l| {
            let v = l.split("nonce=").nth(1)?;
            v.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .collect()
}
