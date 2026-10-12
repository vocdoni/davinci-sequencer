//! L1-blob mode on anvil: node B killed while its blob transaction waits in
//! Ethereum's pool (the Ethereum anvil paused) and restarted on the same
//! datadir. Its batch secrets died with it, so it proves the batch again,
//! and its first post replaces the pending transaction at the same nonce with
//! every fee doubled instead of queueing behind it. The settled transition
//! carries the replacement's blobs.

use super::*;
use alloy::providers::Provider;

/// `fx::vote_k` tag of this process's ballots.
const TAG_RESTART: u64 = 14;
/// Its voters, all sent to node B.
const VOTERS: std::ops::Range<usize> = 0..4;

#[allow(clippy::too_many_arguments)]
pub(super) async fn kill_after_post(
    nodes: &mut [Node],
    cfgs: &[NodeConfig],
    bin: &Path,
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
) -> Result<()> {
    let l1 = net.l1.as_ref().context("L1-blob mode only")?;
    let eth = l1
        .anvil
        .as_ref()
        .context("anvil only: it pauses Ethereum")?;
    let (_, from) = l1.keys[B];
    let read = l1.provider()?;
    let p = grace::create(nodes, org, reader, p1, B).await?;
    let reqs = grace::ballots(prover, TAG_RESTART, &p, tree, VOTERS, &[])?;
    let nonce = read.get_transaction_count(from).await?;
    ensure!(
        read.get_transaction_count(from).pending().await? == nonce,
        "node-b has an Ethereum transaction pending already"
    );
    eth.pause_mining().await?;
    say!("Ethereum paused at L1 nonce {nonce} of node-b ({from})");
    let r: Result<Vec<Sent>> = async {
        let sent = grace::send(nodes, "restart", VOTERS, &reqs, |_| B).await?;
        let first = wait::until(
            "node-b's blob post in Ethereum's pool",
            settle_timeout(),
            Duration::from_millis(200),
            || async {
                alive(nodes)?;
                eth.pending(from, nonce).await
            },
        )
        .await?;
        nodes[B].kill()?;
        say!(
            "node-b killed with {} pending at L1 nonce {nonce}",
            first.hash
        );
        nodes[B] = Node::start(bin, &cfgs[B]).await.context("restart node-b")?;
        let second = wait::until(
            "node-b's replacement at the same L1 nonce",
            settle_timeout(),
            Duration::from_millis(200),
            || async {
                alive(nodes)?;
                Ok(eth
                    .pending(from, nonce)
                    .await?
                    .filter(|t| t.hash != first.hash))
            },
        )
        .await?;
        ensure!(
            second.max_fee >= 2 * first.max_fee
                && second.tip >= 2 * first.tip
                && second.blob_fee >= 2 * first.blob_fee,
            "replacement fees {second:?}, want each at least twice {first:?}"
        );
        ensure!(
            !nodes[B]
                .log_lines(&["L1 post of an earlier run on record"])
                .is_empty(),
            "node-b did not pick its post record up"
        );
        say!(
            "node-b replaced {} with {} at L1 nonce {nonce}, every fee doubled",
            first.hash,
            second.hash
        );
        eth.resume_mining().await?;
        wait_settled(nodes, &sent, "restart batch").await?;
        ensure!(
            read.get_transaction_receipt(first.hash).await?.is_none(),
            "the replaced post was mined"
        );
        read.get_transaction_receipt(second.hash)
            .await?
            .context("the replacement was not mined")?;
        let from_block = net.start_block.unwrap_or(net.from_block);
        let posts: Vec<_> = l1::posts(&net.rpc, l1.read_rpc(), net.registry, from_block)
            .await?
            .into_iter()
            .filter(|t| t.pid == p.id)
            .collect();
        ensure!(
            posts.len() == 1 && posts[0].l1_txs == [second.hash],
            "the settlement's blobs are not the replacement's: {posts:?}"
        );
        Ok(sent)
    }
    .await;
    // Ethereum runs again whatever happened.
    let resumed = eth.resume_mining().await;
    let sent = r?;
    resumed?;
    check_settled(nodes, reader, &p.id, &sent, (VOTERS.len() as u64, 0)).await?;
    org.cancel_process(&p.id).await.context("cancel")?;
    say!(
        "{}: restart with a pending post ok; OP origin at Ethereum block {}, {} failed relay steps",
        ProcessId(p.id),
        eth.relay.origin(),
        eth.relay.errors()
    );
    Ok(())
}
