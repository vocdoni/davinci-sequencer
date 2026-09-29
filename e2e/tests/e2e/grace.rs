//! The grace window and the batching policy, on the acceptance run's chain,
//! nodes and prover: four sequencer-key processes on process 1's census.
//!
//! - AGM: three queued votes of one voter through one node (a resend 40901,
//!   a fourth 40902), a voter routed by `pick_node` whose node is killed with
//!   a revote queued (the next node in its order takes the one after; the
//!   last-settled ballot counts), then votes on all three nodes and an END
//!   with a batch in flight: every vote settles in the grace, nodes lose
//!   races on the way, results follow the window.
//! - Shorten: a lone vote waits `solo_wait`, then the end moves to now +
//!   `noticeMin`; the nodes flush during the notice, one package goes to two
//!   nodes (settled once, both report it), a vote after the new end is
//!   refused 41201.
//! - Backlog: node C restarted with `batch_max = 2` seals a burst back to
//!   back, then holds three rounds at an END: each landing moves
//!   `getProcessGraceEnd`, which freezes after the last.
//! - Cap: the same node trickles rounds under a 60 s grace; the window stops
//!   at end + `graceMaxTotal`, what it could not land errors.
//!
//! Every process ends with its results after `getProcessGraceEnd` and the
//! `eth_call` gates around it ([`negative::grace`]).

use super::*;
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use davinci_client::organizer::GraceParams;
use davinci_client::voter::pick_node;

const CODE_SLOT_BUSY: u32 = 40902;
const CODE_NOT_ACCEPTING: u32 = 41201;
/// `fx::vote_k` tags of the four processes' ballots.
const TAG_AGM: u64 = 10;
const TAG_SHORTEN: u64 = 11;
const TAG_BACKLOG: u64 = 12;
const TAG_CAP: u64 = 13;
/// AGM voters: one revoting through one node, one moved by a failover, and
/// two waves spread round-robin.
const REVOTER: usize = 0;
const MOVER: usize = 1;
const WAVE1: std::ops::Range<usize> = 2..11;
const WAVE2: std::ops::Range<usize> = 11..17;
/// Shorten voters: the lone one, the flush, the one sent to two nodes and
/// the late one.
const SPARSE: usize = 0;
const FLUSH: std::ops::Range<usize> = 1..7;
const TWICE: usize = 7;
const LATE: usize = 8;
/// Seconds past `noticeMin` the shortened end leaves for the flush votes.
/// Live it also covers the head read lagging the chain and the shorten
/// transaction's inclusion, or the end lands inside the notice and reverts.
const SHORTEN_SLACK: u64 = 6;
const SHORTEN_SLACK_LIVE: u64 = 45;
/// Backlog node's batch cap, and its two sets of votes.
const SMALL_BATCH: usize = 2;
const BURST: std::ops::Range<usize> = 0..6;
const BACKLOG: std::ops::Range<usize> = 6..12;
const TRICKLE: std::ops::Range<usize> = 0..10;
/// Longest `graceMaxTotal` the cap scenario waits out.
const CAP_MAX_TOTAL: u32 = 600;

/// Latest block number and timestamp.
pub(super) async fn head(rpc: &str) -> Result<(u64, u64)> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    let b = p
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .context("no head block")?;
    Ok((b.header.number, b.header.timestamp))
}

async fn block_time(rpc: &str, n: u64) -> Result<u64> {
    let p = ProviderBuilder::new().connect_client(chain::rpc(rpc)?);
    Ok(p.get_block_by_number(n.into())
        .await?
        .with_context(|| format!("block {n}"))?
        .header
        .timestamp)
}

/// A two-hour sequencer-key process on process 1's census, keyed by
/// `nodes[key]`, once every node lists it.
async fn create(
    nodes: &[Node],
    org: &Organizer,
    reader: &RegistryReader,
    p1: &OnchainProcess,
    key: usize,
) -> Result<OnchainProcess> {
    let next = org.next_process_id().await?;
    let pk = nodes[key].api.new_key(&next).await?;
    let np = NewProcess {
        key_mode: KeyMode::Sequencer(pk),
        ..dkg_new_process(next, p1, 2 * 3600)
    };
    let pid = org.create_process(&np).await?.pid;
    wait_listed(nodes, reader, &pid).await
}

/// Round-1 ballots of `voters`, then `extra` (voter, round) pairs, proved.
fn ballots(
    prover: &BallotProver,
    tag: u64,
    p: &OnchainProcess,
    tree: &LeanImt,
    voters: impl IntoIterator<Item = usize>,
    extra: &[(usize, usize)],
) -> Result<Vec<VoteRequest>> {
    let all = fx::merkle_voters();
    let mut jobs = Vec::new();
    let pairs = voters
        .into_iter()
        .map(|v| (v, 1))
        .chain(extra.iter().copied());
    for (v, round) in pairs {
        jobs.push(job(tag, &all[v], v, round, p, fx::merkle_witness(tree, v)?));
    }
    fx::prove_all(prover, &jobs)
}

/// Submits `reqs` of `voters` (round `round`) to `node(v)`.
async fn send(
    nodes: &[Node],
    what: &str,
    voters: impl IntoIterator<Item = usize>,
    reqs: &[VoteRequest],
    node: impl Fn(usize) -> usize,
) -> Result<Vec<Sent>> {
    let mut out = Vec::new();
    for (v, r) in voters.into_iter().zip(reqs) {
        let (label, n) = (format!("{what} voter {v}"), node(v));
        submit(&nodes[n], r, &label).await?;
        out.push(sent(label, n, r));
    }
    Ok(out)
}

/// Waits until one of `sent` has left the pending queue; returns its label
/// and status.
pub(super) async fn wait_sealed(nodes: &[Node], sent: &[Sent]) -> Result<(String, VoteStatus)> {
    wait::until(
        "a vote to be sealed",
        settle_timeout(),
        Duration::from_millis(200),
        || async {
            alive(nodes)?;
            for s in sent {
                let r = nodes[s.node].api.vote_status_full(&s.pid, s.vid).await?;
                match r.status {
                    VoteStatus::Pending => {}
                    VoteStatus::Error => {
                        bail!("{} in error: {}", s.label, r.error.unwrap_or_default())
                    }
                    st => return Ok(Some((s.label.clone(), st))),
                }
            }
            Ok(None)
        },
    )
    .await
}

/// Waits until every vote of `sent` is settled or errored; the error of
/// each, `None` when settled.
async fn wait_final(nodes: &[Node], sent: &[Sent], what: &str) -> Result<Vec<Option<String>>> {
    let start = Instant::now();
    let mut out: Vec<Option<Option<String>>> = vec![None; sent.len()];
    loop {
        alive(nodes)?;
        for (s, o) in sent.iter().zip(out.iter_mut()) {
            if o.is_some() {
                continue;
            }
            if let Ok(r) = nodes[s.node].api.vote_status_full(&s.pid, s.vid).await {
                match r.status {
                    VoteStatus::Settled => *o = Some(None),
                    VoteStatus::Error => *o = Some(Some(r.error.unwrap_or_default())),
                    _ => {}
                }
            }
        }
        let open = out.iter().filter(|o| o.is_none()).count();
        if open == 0 {
            return Ok(out.into_iter().flatten().collect());
        }
        ensure!(
            start.elapsed() < settle_timeout(),
            "{what}: {open} votes neither settled nor errored after {:?}",
            settle_timeout()
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `getProcessGraceEnd` of `pid`, polled each second until its results
/// land: the values seen, in order, without repeats.
async fn watch_grace(
    nodes: &[Node],
    org: &Organizer,
    reader: &RegistryReader,
    pid: &[u8; 31],
) -> Result<Vec<u64>> {
    let start = Instant::now();
    let mut seen = Vec::new();
    loop {
        alive(nodes)?;
        let ge = reader.grace_end(pid).await?;
        if seen.last() != Some(&ge) {
            say!("{}: grace end {ge}", ProcessId(*pid));
            seen.push(ge);
        }
        if org.results(pid).await?.is_some() {
            return Ok(seen);
        }
        ensure!(
            start.elapsed() < results_timeout(),
            "{}: no results after {:?}",
            ProcessId(*pid),
            results_timeout()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `pid`'s transitions since block `from`: (block, timestamp, sender).
pub(super) async fn landings(
    net: &Net,
    pid: &[u8; 31],
    from: u64,
) -> Result<Vec<(u64, u64, Address)>> {
    let mut out = Vec::new();
    for t in chain::transitions(&net.rpc, net.registry, from).await? {
        if t.pid == *pid {
            out.push((t.block, block_time(&net.rpc, t.block).await?, t.sender));
        }
    }
    Ok(out)
}

/// The grace ends an observer polling from the END sees:
/// `min(end + graceMaxTotal, max(end, lastVoteAt) + grace)`, first with the
/// END's `lastVoteAt` (never past the end), then after each landing.
fn grace_series(end: u64, grace: u64, max_total: u64, landed: &[u64]) -> Vec<u64> {
    let f = |last: u64| (end + max_total).min(last.max(end) + grace);
    let mut out = vec![f(end)];
    for &ts in landed {
        let g = f(ts);
        if out.last() != Some(&g) {
            out.push(g);
        }
    }
    out
}

/// `seen` is `want`, less a first value a landing replaced before the first poll.
fn series_matches(seen: &[u64], want: &[u64]) -> bool {
    !seen.is_empty() && seen.len() + 1 >= want.len() && want.ends_with(seen)
}

/// `pid`'s results (or DKG request) landed at or after its grace end, and
/// the `eth_call` gates around it hold ([`negative::grace`]).
pub(super) async fn closed_after_grace(
    net: &Net,
    reader: &RegistryReader,
    pid: &[u8; 31],
) -> Result<()> {
    let from = net.start_block.unwrap_or(net.from_block);
    let closing = if reader.process(pid).await?.dkg.is_some() {
        chain::dkg_requests(&net.rpc, net.registry, from).await?
    } else {
        chain::results_txs(&net.rpc, net.registry, from).await?
    };
    let block = closing
        .iter()
        .find(|t| t.pid == *pid)
        .context("no closing call")?
        .block;
    let (ts, ge) = (
        block_time(&net.rpc, block).await?,
        reader.grace_end(pid).await?,
    );
    ensure!(
        ts >= ge,
        "{}: closed at {ts}, before its grace end {ge}",
        ProcessId(*pid)
    );
    say!(
        "{}: closed at block {block} ({ts}), grace end {ge}",
        ProcessId(*pid)
    );
    // A live RPC may not simulate at old blocks: gated like the negatives.
    if !net.live || flag("DAVINCI_E2E_NEGATIVE") {
        negative::grace(&net.rpc, net.registry, *pid, from)
            .await
            .context("grace gates")?;
    }
    Ok(())
}

/// Every scenario, in order. Leaves node C running with `batch_max = 2`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    nodes: &mut [Node],
    cfgs: &[NodeConfig],
    bin: &Path,
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
) -> Result<()> {
    let params = reader.grace_params().await?;
    say!("grace: {params:?}");
    agm(nodes, cfgs, bin, net, org, reader, (p1, tree), prover)
        .await
        .context("AGM END")?;
    shorten(nodes, cfgs, net, org, reader, (p1, tree), prover, &params)
        .await
        .context("shorten with notice")?;
    nodes[C].kill()?;
    let small = NodeConfig {
        batch_max: SMALL_BATCH,
        ..cfgs[C].clone()
    };
    nodes[C] = Node::start(bin, &small).await.context("restart node-c")?;
    say!("node-c restarted with batch_max {SMALL_BATCH}");
    backlog(nodes, net, org, reader, (p1, tree), prover, &params)
        .await
        .context("backlog")?;
    if params.grace_max_total > CAP_MAX_TOTAL {
        say!(
            "extension cap skipped: graceMaxTotal {} s is too long to wait out",
            params.grace_max_total
        );
        return Ok(());
    }
    cap(nodes, net, org, reader, (p1, tree), prover, &params)
        .await
        .context("extension cap")
}

#[allow(clippy::too_many_arguments)]
async fn agm(
    nodes: &mut [Node],
    cfgs: &[NodeConfig],
    bin: &Path,
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
) -> Result<()> {
    let p = create(nodes, org, reader, p1, A).await?;
    let pid = p.id;
    say!("AGM process {}", ProcessId(pid));
    let rounds: Vec<_> = [REVOTER, MOVER]
        .into_iter()
        .flat_map(|v| (1..=4).map(move |r| (v, r)))
        .collect();
    let reqs = ballots(prover, TAG_AGM, &p, tree, WAVE1.chain(WAVE2), &rounds)?;
    let (waves, rest) = reqs.split_at(WAVE1.len() + WAVE2.len());
    let (w1, w2) = waves.split_at(WAVE1.len());
    let (rev, mov) = rest.split_at(4);
    let voters = fx::merkle_voters();
    let urls: Vec<&str> = nodes[..OBS].iter().map(|n| n.url.as_str()).collect();
    let order = |v: usize| -> Vec<usize> {
        pick_node(&voters[v].address(), &pid, &urls)
            .into_iter()
            .filter_map(|u| urls.iter().position(|x| x == u))
            .collect()
    };

    // Three votes of one voter queue on its node and settle in order, one a
    // transition; the queued first is a duplicate, a fourth finds the slot full.
    let n0 = order(REVOTER)[0];
    let mut queued = Vec::new();
    for (i, r) in rev[..3].iter().enumerate() {
        let label = format!("agm voter {REVOTER} round {}", i + 1);
        submit(&nodes[n0], r, &label).await?;
        queued.push(sent(label, n0, r));
    }
    let dup = (409, CODE_DUPLICATE);
    expect_rejected(&nodes[n0], &rev[0], dup, "a queued vote resent").await?;
    let busy = (409, CODE_SLOT_BUSY);
    expect_rejected(&nodes[n0], &rev[3], busy, "a fourth queued vote").await?;
    wait_settled(nodes, &queued, "three queued votes of one voter").await?;
    let c = reader.process(&pid).await?;
    ensure!(
        (c.voters_count, c.overwritten_votes_count) == (1, 2),
        "after the queued votes: votersCount {} overwrittenVotesCount {}",
        c.voters_count,
        c.overwritten_votes_count
    );
    say!(
        "three queued votes of one voter settled through {}",
        nodes[n0].name
    );

    // Routing: every cast of the voter goes to the first node of its order,
    // until that node dies with a revote queued; the voter moves down the list.
    let o = order(MOVER);
    let (pn, qn) = (o[0], o[1]);
    let mut moved = Vec::new();
    for (i, r) in mov[..2].iter().enumerate() {
        let label = format!("agm voter {MOVER} round {}", i + 1);
        submit(&nodes[pn], r, &label).await?;
        moved.push(sent(label, pn, r));
    }
    wait_settled(nodes, &moved, "two votes through the pinned node").await?;
    let (kill_block, _) = head(&net.rpc).await?;
    let (l3, l4) = (
        format!("agm voter {MOVER} round 3"),
        format!("agm voter {MOVER} round 4"),
    );
    submit(&nodes[pn], &mov[2], &l3).await?;
    nodes[pn].kill()?;
    say!("{} killed with {l3} queued", nodes[pn].name);
    submit(&nodes[qn], &mov[3], &l4).await?;
    nodes[pn] = Node::start(bin, &cfgs[pn])
        .await
        .with_context(|| format!("restart {}", cfgs[pn].name))?;
    let pair = [sent(l3, pn, &mov[2]), sent(l4, qn, &mov[3])];
    wait_settled(nodes, &pair, "the votes around the failover").await?;
    let trs = landings(net, &pid, kill_block + 1).await?;
    let block_of = |n: usize| {
        trs.iter()
            .find(|t| nodes[n].address == Some(t.2))
            .map(|t| t.0)
            .with_context(|| format!("no transition from {} after the kill", nodes[n].name))
    };
    let (b3, b4) = (block_of(pn)?, block_of(qn)?);
    ensure!(b3 != b4, "two transitions of one process in block {b3}");
    let (last_round, last) = if b3 > b4 {
        (3, &pair[0])
    } else {
        (4, &pair[1])
    };
    say!(
        "failover: round 3 settled by {} at block {b3}, round 4 by {} at {b4}; round {last_round} counts",
        nodes[pn].name,
        nodes[qn].name
    );

    // The AGM: a wave on every node, a second one queued behind the first
    // batch, and END while it proves.
    let s1 = send(nodes, "agm wave 1", WAVE1, w1, fx::first_node).await?;
    let (label, st) = wait_sealed(nodes, &s1).await?;
    let s2 = send(nodes, "agm wave 2", WAVE2, w2, fx::first_node).await?;
    let before = all_metrics(&nodes[..OBS]).await?;
    let (end_block, _) = head(&net.rpc).await?;
    org.end_process(&pid).await.context("END")?;
    say!(
        "END with {label} {st} and {} votes queued; grace end {}",
        s2.len(),
        reader.grace_end(&pid).await?
    );
    let wave: Vec<_> = s1.into_iter().chain(s2).collect();
    wait_settled(nodes, &wave, "votes pending at the END").await?;
    let after = all_metrics(&nodes[..OBS]).await?;
    let lost = |m: &[Metrics]| m.iter().filter_map(|m| m.lost_races).sum::<u64>();
    let in_grace = landings(net, &pid, end_block + 1).await?;
    ensure!(!in_grace.is_empty(), "no transition landed after the END");
    let senders: BTreeSet<_> = in_grace.iter().map(|t| t.2).collect();
    say!(
        "{} transitions in the grace from {} senders; lost races {} -> {}",
        in_grace.len(),
        senders.len(),
        lost(&before),
        lost(&after)
    );
    let raced = lost(&after) > lost(&before);
    if !raced {
        ensure!(net.live, "no node lost a race during the grace");
        say!("note (live, not asserted): no node lost a race during the grace");
    }

    let mut all = queued;
    all.extend(moved);
    all.push(last.clone());
    all.extend(wave);
    let n_voters = (2 + WAVE1.len() + WAVE2.len()) as u64;
    check_settled(nodes, reader, &pid, &all, (n_voters, 5)).await?;
    // Every node holds the same ballot in the moved voter's slot.
    let addr = voters[MOVER].address();
    let first_ballot = nodes[A].api.ballot(&pid, &addr).await?.ballot;
    for n in &nodes[..OBS] {
        ensure!(
            n.api.ballot(&pid, &addr).await?.ballot == first_ballot,
            "{} serves another ballot for voter {MOVER}",
            n.name
        );
    }
    let last_ballots: Vec<_> = [fx::choices(REVOTER, 3), fx::choices(MOVER, last_round)]
        .into_iter()
        .chain(WAVE1.chain(WAVE2).map(|v| fx::choices(v, 1)))
        .collect();
    wait_results(nodes, org, &pid, &fx::expected_tally(&last_ballots)).await?;
    closed_after_grace(net, reader, &pid).await
}

#[allow(clippy::too_many_arguments)]
async fn shorten(
    nodes: &[Node],
    cfgs: &[NodeConfig],
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
    params: &GraceParams,
) -> Result<()> {
    let p = create(nodes, org, reader, p1, B).await?;
    let pid = p.id;
    say!("shorten process {}", ProcessId(pid));
    let voters = [SPARSE, TWICE, LATE].into_iter().chain(FLUSH);
    let reqs = ballots(prover, TAG_SHORTEN, &p, tree, voters, &[])?;
    let (sparse, twice, late, flush) = (&reqs[0], &reqs[1], &reqs[2], &reqs[3..]);

    // A lone vote seals after solo_wait, not batch_time.
    let secs = |d: &str| d.trim_end_matches('s').parse::<f64>().unwrap_or(0.0);
    let cfg = &cfgs[A];
    let solo = cfg
        .solo_wait
        .as_deref()
        .map_or(3.0 * secs(&cfg.batch_time), secs);
    let t = Instant::now();
    let lone = [sent("shorten lone vote", A, sparse)];
    submit(&nodes[A], sparse, &lone[0].label).await?;
    let (_, st) = wait_sealed(nodes, &lone).await?;
    let waited = t.elapsed().as_secs_f64();
    ensure!(
        waited >= 0.9 * solo - 0.5,
        "a lone vote sealed after {waited:.1} s, solo_wait {solo} s"
    );
    say!("a lone vote sealed ({st}) after {waited:.1} s (solo_wait {solo} s)");
    wait_settled(nodes, &lone, "the lone vote").await?;

    // The end moves to the notice's edge; votes and a package sent to two
    // nodes go in before it; the nodes flush.
    let (_, now) = head(&net.rpc).await?;
    let slack = if net.live {
        SHORTEN_SLACK_LIVE
    } else {
        SHORTEN_SLACK
    };
    let end = now + u64::from(params.notice_min) + slack;
    org.set_process_duration(&pid, end - p.start_time)
        .await
        .context("shorten")?;
    say!("end moved to {end}, {} s from now", end - now);
    let mut all = send(nodes, "shorten flush", FLUSH, flush, fx::first_node).await?;
    for n in [A, B] {
        let label = format!("shorten voter {TWICE} via {}", nodes[n].name);
        submit(&nodes[n], twice, &label).await?;
        all.push(sent(label, n, twice));
    }
    wait::until(
        "node-c to stop accepting votes",
        sync_timeout(),
        Duration::from_millis(500),
        || async {
            alive(nodes)?;
            Ok((!nodes[C].api.process(&pid).await?.is_accepting_votes).then_some(()))
        },
    )
    .await?;
    let refused = (412, CODE_NOT_ACCEPTING);
    expect_rejected(&nodes[C], late, refused, "a vote after the new end").await?;
    wait_settled(nodes, &all, "votes of the notice").await?;
    all.push(lone[0].clone());
    let n_voters = (1 + FLUSH.len() + 1) as u64;
    check_settled(nodes, reader, &pid, &all, (n_voters, 0)).await?;
    let last: Vec<_> = [SPARSE, TWICE]
        .into_iter()
        .chain(FLUSH)
        .map(|v| fx::choices(v, 1))
        .collect();
    wait_results(nodes, org, &pid, &fx::expected_tally(&last)).await?;
    closed_after_grace(net, reader, &pid).await
}

#[allow(clippy::too_many_arguments)]
async fn backlog(
    nodes: &[Node],
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
    params: &GraceParams,
) -> Result<()> {
    let p = create(nodes, org, reader, p1, A).await?;
    let pid = p.id;
    say!("backlog process {}", ProcessId(pid));
    let reqs = ballots(prover, TAG_BACKLOG, &p, tree, BURST.chain(BACKLOG), &[])?;
    let (burst, held) = reqs.split_at(BURST.len());

    // A burst above the batch cap seals at the cap, round after round.
    let s = send(nodes, "burst", BURST, burst, |_| C).await?;
    wait_settled(nodes, &s, "the burst").await?;
    let trs = nodes[C].api.transitions(&pid).await?;
    let sizes: Vec<_> = trs.iter().map(|t| t.voters).collect();
    let blocks: Vec<_> = trs.iter().map(|t| t.block_number).collect();
    ensure!(
        trs.len() >= BURST.len().div_ceil(SMALL_BATCH)
            && sizes.iter().all(|n| *n <= SMALL_BATCH as u64),
        "burst of {} under batch_max {SMALL_BATCH}: transitions of {sizes:?} votes",
        BURST.len()
    );
    say!("burst: transitions of {sizes:?} votes at blocks {blocks:?}");

    // Three rounds held at the END: each landing moves the window.
    let mut all = s;
    let s = send(nodes, "backlog", BACKLOG, held, |_| C).await?;
    let (label, st) = wait_sealed(nodes, &s).await?;
    let (end_block, _) = head(&net.rpc).await?;
    org.end_process(&pid).await.context("END")?;
    say!("END with {label} {st}");
    let seen = watch_grace(nodes, org, reader, &pid).await?;
    wait_settled(nodes, &s, "the backlog").await?;
    all.extend(s);
    let c = reader.process(&pid).await?;
    let end = c.start_time + c.duration;
    let landed: Vec<_> = landings(net, &pid, end_block + 1)
        .await?
        .iter()
        .map(|t| t.1)
        .collect();
    let want = grace_series(end, c.grace, params.grace_max_total.into(), &landed);
    ensure!(
        landed.len() >= 2 && series_matches(&seen, &want),
        "grace ends {seen:?}, want {want:?} from {} landings after the END",
        landed.len()
    );
    say!(
        "backlog: {} landings in the grace moved the window {seen:?}",
        landed.len()
    );
    let n = (BURST.len() + BACKLOG.len()) as u64;
    check_settled(nodes, reader, &pid, &all, (n, 0)).await?;
    let last: Vec<_> = BURST.chain(BACKLOG).map(|v| fx::choices(v, 1)).collect();
    wait_results(nodes, org, &pid, &fx::expected_tally(&last)).await?;
    closed_after_grace(net, reader, &pid).await
}

#[allow(clippy::too_many_arguments)]
async fn cap(
    nodes: &[Node],
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
    params: &GraceParams,
) -> Result<()> {
    let p = create(nodes, org, reader, p1, A).await?;
    let pid = p.id;
    org.set_process_grace(&pid, params.grace_ceil)
        .await
        .context("setProcessGrace")?;
    say!(
        "cap process {}, grace {} s",
        ProcessId(pid),
        params.grace_ceil
    );
    let reqs = ballots(prover, TAG_CAP, &p, tree, TRICKLE, &[])?;
    let s = send(nodes, "trickle", TRICKLE, &reqs, |_| C).await?;
    let (label, st) = wait_sealed(nodes, &s).await?;
    let (end_block, _) = head(&net.rpc).await?;
    org.end_process(&pid).await.context("END")?;
    say!("END with {label} {st}");
    let seen = watch_grace(nodes, org, reader, &pid).await?;
    let fates = wait_final(nodes, &s, "the trickle").await?;
    let c = reader.process(&pid).await?;
    let end = c.start_time + c.duration;
    let max = u64::from(params.grace_max_total);
    let landed: Vec<_> = landings(net, &pid, end_block + 1)
        .await?
        .iter()
        .map(|t| t.1)
        .collect();
    let want = grace_series(end, c.grace, max, &landed);
    ensure!(
        series_matches(&seen, &want) && seen.last() == Some(&(end + max)),
        "grace ends {seen:?}, want {want:?} ending at the cap {}",
        end + max
    );
    let settled: Vec<usize> = TRICKLE
        .zip(&fates)
        .filter(|(_, f)| f.is_none())
        .map(|(v, _)| v)
        .collect();
    for (s, f) in s.iter().zip(&fates) {
        if let Some(e) = f {
            ensure!(e.contains("closed"), "{}: {e}", s.label);
        }
    }
    say!(
        "cap: {} landings after the END, window stopped at end + {max} s; {} of {} votes settled, {} closed out",
        landed.len(),
        settled.len(),
        TRICKLE.len(),
        TRICKLE.len() - settled.len()
    );
    ensure!(
        c.voters_count == settled.len() as u64,
        "votersCount {}, {} settled",
        c.voters_count,
        settled.len()
    );
    let last: Vec<_> = settled.iter().map(|v| fx::choices(*v, 1)).collect();
    wait_results(nodes, org, &pid, &fx::expected_tally(&last)).await?;
    closed_after_grace(net, reader, &pid).await
}

#[cfg(test)]
mod tests {
    use super::{grace_series, series_matches};

    #[test]
    fn series() {
        // Idle grace 45, cap 90: landings at +20 and +50 move it, one at +70
        // hits the cap, another after it changes nothing.
        let s = grace_series(1000, 45, 90, &[1020, 1050, 1070, 1085]);
        assert_eq!(s, vec![1045, 1065, 1090]);
        // A landing in the END's own block leaves it.
        assert_eq!(grace_series(1000, 45, 90, &[1000]), vec![1045]);
        assert!(series_matches(&s[1..], &s) && series_matches(&s, &s));
        assert!(!series_matches(&s[2..], &s) && !series_matches(&[1045, 1090], &s));
    }
}
