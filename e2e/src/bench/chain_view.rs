//! What the chain and the nodes say after a run: transitions with their
//! decoded vote ids, every vote's status on the nodes that took it, and the
//! prover jobs. The audit reads only what this module returns.
//!
//! The ground truth for "settled" is the chain: every transition row is
//! checked against its `ProcessStateTransitioned` event before its vote ids
//! are trusted.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256};
use alloy::providers::{DynProvider, Provider};
use anyhow::{Context, Result, anyhow, bail, ensure};
use davinci_client::SequencerClient;
use davinci_client::api::{ProcessId, TransitionView, VoteStatus};
use davinci_client::organizer::ProcessRegistry;
use davinci_zkvm_sdk::blob::{BLOB_SIZE, Blob, decode_blobs};
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::{JobId, JobStatus};
use tokio::task::JoinSet;

use super::audit::NodeStatus;
use super::ledger::Entry;
use super::plan::NodeId;
use super::rig::ProveCall;
use crate::chain;
use crate::node::Node;

/// One settled transition of an election, checked against the chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionRow {
    pub pid: [u8; 31],
    /// Position in the process's transition sequence, from 0.
    pub index: u64,
    pub block: u64,
    /// The block's timestamp.
    pub ts: u64,
    pub sender: [u8; 20],
    pub new_root: [u8; 32],
    /// Votes in the batch, overwrites included.
    pub voters: u64,
    pub overwrites: u64,
    pub n_blobs: u64,
    /// The new votes, decoded from the blobs.
    pub vids: Vec<u64>,
    /// The settlement transaction.
    pub tx: [u8; 32],
}

impl TransitionRow {
    /// An empty row, for tests.
    #[cfg(test)]
    pub fn test() -> TransitionRow {
        TransitionRow {
            pid: [0; 31],
            index: 0,
            block: 0,
            ts: 0,
            sender: [0; 20],
            new_root: [0; 32],
            voters: 0,
            overwrites: 0,
            n_blobs: 0,
            vids: Vec::new(),
            tx: [0; 32],
        }
    }
}

/// The vote ids of a transition's blobs (in order) at `nf` fields.
pub fn decode_vids(blobs: &[Vec<u8>], nf: u8) -> Result<Vec<u64>> {
    let blobs = blobs
        .iter()
        .map(|b| -> Result<Blob> {
            b.clone()
                .into_boxed_slice()
                .try_into()
                .map_err(|_| anyhow!("blob of {} bytes, want {BLOB_SIZE}", b.len()))
        })
        .collect::<Result<Vec<Blob>>>()?;
    Ok(decode_blobs(&blobs, nf)
        .map_err(|e| anyhow!("decode blobs: {e}"))?
        .vote_ids)
}

/// `ProcessStateTransitioned` events of `pid` from block `from`.
pub async fn chain_count(rpc: &str, registry: Address, from: u64, pid: &[u8; 31]) -> Result<u64> {
    Ok(chain::transitions(rpc, registry, from)
        .await?
        .iter()
        .filter(|t| t.pid == *pid)
        .count() as u64)
}

/// The last values [`wait_quiet`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quiet {
    /// Transitions of the process in the registry logs.
    pub chain: u64,
    /// Transitions in the observer's archive.
    pub observer: u64,
    /// `pendingVotes` of A and B; `u64::MAX` when a node never answered
    /// with one (unreachable, or no actor owns the process).
    pub pending: [u64; 2],
    pub timed_out: bool,
}

const QUIET_POLL: Duration = Duration::from_secs(10);

/// Polls every 10 s until O's transition count equals the registry's and A
/// and B report `pendingVotes == 0`, or `max` passes. A read that fails
/// counts as not quiet; the chain count must be read at least once.
pub async fn wait_quiet(
    obs: &Node,
    ab: [&Node; 2],
    rpc: &str,
    registry: Address,
    from: u64,
    pid: &[u8; 31],
    max: Duration,
) -> Result<Quiet> {
    let start = Instant::now();
    let mut q = Quiet {
        chain: 0,
        observer: 0,
        pending: [u64::MAX; 2],
        timed_out: false,
    };
    let mut chain_err: Option<anyhow::Error> = None;
    let mut chain_read = false;
    loop {
        let chain = match chain_count(rpc, registry, from, pid).await {
            Ok(n) => Some(n),
            Err(e) => {
                chain_err = Some(e);
                None
            }
        };
        let observer = obs.api.transitions(pid).await.ok().map(|t| t.len() as u64);
        let mut pending = [None; 2];
        for (p, n) in pending.iter_mut().zip(ab) {
            *p = n.api.process(pid).await.ok().and_then(|v| v.pending_votes);
        }
        if let Some(c) = chain {
            q.chain = c;
            chain_read = true;
        }
        if let Some(o) = observer {
            q.observer = o;
        }
        for (q, p) in q.pending.iter_mut().zip(pending) {
            if let Some(p) = p {
                *q = p;
            }
        }
        if chain.is_some() && observer == chain && pending == [Some(0); 2] {
            return Ok(q);
        }
        if start.elapsed() >= max {
            if !chain_read {
                let e = chain_err.unwrap_or_else(|| anyhow!("no answer"));
                return Err(e.context(format!(
                    "the registry's transition count of {} was never read",
                    ProcessId(*pid)
                )));
            }
            q.timed_out = true;
            return Ok(q);
        }
        tokio::time::sleep(QUIET_POLL).await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transitions {
    /// In index order, one per chain transition.
    pub rows: Vec<TransitionRow>,
    /// Indices O lacked (or held a record of that did not match the chain),
    /// read from A or B instead.
    pub from_fallback: Vec<u64>,
    /// Why O's record was not used, per fallback index.
    pub notes: Vec<String>,
}

/// One `ProcessStateTransitioned`, read from its transaction's receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landed {
    pub block: u64,
    pub sender: [u8; 20],
    pub old_root: [u8; 32],
    pub new_root: [u8; 32],
    /// The registry's running `votersCount` and `overwrittenVotesCount`.
    pub voters_count: u64,
    pub overwrites_count: u64,
    pub n_blobs: u64,
}

/// A node's record of transition `v.index`, with `n_vids` vote ids decoded
/// from its blobs, against the chain's event `l`; `prev` is the chain's
/// event of the previous index (none for index 0).
pub fn check_row(
    v: &TransitionView,
    n_vids: usize,
    l: &Landed,
    prev: Option<&Landed>,
) -> Result<()> {
    ensure!(
        v.block_number == l.block,
        "block {} on the node, {} on chain",
        v.block_number,
        l.block
    );
    ensure!(v.sender == l.sender, "the sender differs from the event's");
    ensure!(
        v.old_root == l.old_root && v.new_root == l.new_root,
        "the roots differ from the event's"
    );
    ensure!(
        v.n_blobs == l.n_blobs,
        "{} blobs on the node, {} on chain",
        v.n_blobs,
        l.n_blobs
    );
    let (pv, po) = match prev {
        Some(p) => {
            ensure!(
                l.old_root == p.new_root,
                "the chain's old root does not follow the previous transition"
            );
            (p.voters_count, p.overwrites_count)
        }
        None => (0, 0),
    };
    let new_voters = l
        .voters_count
        .checked_sub(pv)
        .context("the registry's voter count went down")?;
    let ow = l
        .overwrites_count
        .checked_sub(po)
        .context("the registry's overwrite count went down")?;
    let votes = new_voters + ow;
    ensure!(
        v.overwrites == ow && v.voters == votes,
        "the node records {} votes and {} overwrites, the chain {votes} and {ow}",
        v.voters,
        v.overwrites
    );
    ensure!(
        n_vids as u64 == votes,
        "{n_vids} vote ids in the blobs, the chain counts {votes} votes"
    );
    Ok(())
}

/// The event of `pid` in the settlement transaction `tx`.
async fn landed(op: &DynProvider, pid: &[u8; 31], tx: [u8; 32]) -> Result<Landed> {
    use ProcessRegistry::ProcessStateTransitioned as Ev;
    let h = B256::from(tx);
    let r = op
        .get_transaction_receipt(h)
        .await?
        .with_context(|| format!("no receipt for {h}"))?;
    ensure!(r.status(), "{h} reverted");
    let to = r.to.context("receipt without a recipient")?;
    let block = r.block_number.context("receipt without a block")?;
    let mut evs = r
        .inner
        .logs()
        .iter()
        .filter(|l| l.address() == to)
        .filter_map(|l| l.log_decode::<Ev>().ok())
        .filter(|l| l.inner.data.processId.0 == *pid);
    let e = evs
        .next()
        .with_context(|| format!("{h} has no transition of this process"))?;
    ensure!(
        evs.next().is_none(),
        "{h} has two transitions of this process"
    );
    let d = &e.inner.data;
    Ok(Landed {
        block,
        sender: d.sender.into_array(),
        old_root: d.oldStateRoot.0,
        new_root: d.newStateRoot.0,
        voters_count: u64::try_from(d.newVotersCount)?,
        overwrites_count: u64::try_from(d.newOverwrittenVotesCount)?,
        n_blobs: u64::try_from(d.nBlobs)?,
    })
}

/// Tries per node read before giving up on it.
const READ_TRIES: u32 = 3;

async fn list(api: &SequencerClient, pid: &[u8; 31]) -> Result<BTreeMap<u64, TransitionView>> {
    let mut last = None;
    for i in 0..READ_TRIES {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match api.transitions(pid).await {
            Ok(l) => return Ok(l.into_iter().map(|t| (t.index, t)).collect()),
            Err(e) => last = Some(e),
        }
    }
    Err(anyhow!(
        "list transitions: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Row `v.index` from one node's blobs, checked against the chain.
async fn row_of(
    api: &SequencerClient,
    op: &DynProvider,
    pid: &[u8; 31],
    nf: u8,
    v: &TransitionView,
    prev: Option<&Landed>,
    ts: &mut HashMap<u64, u64>,
) -> Result<(TransitionRow, Landed)> {
    let l = landed(op, pid, v.tx_hash).await?;
    let mut blobs = None;
    for i in 0..READ_TRIES {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match api.transition_blobs(pid, v.index).await {
            Ok(b) => {
                blobs = Some(b);
                break;
            }
            Err(davinci_client::Error::Api { status: 404, .. }) => bail!("no blobs stored"),
            Err(e) if i + 1 == READ_TRIES => bail!("blobs: {e}"),
            Err(_) => {}
        }
    }
    let blobs = blobs.context("blobs")?;
    // Decoding parses every point: off the async workers.
    let vids = tokio::task::spawn_blocking(move || decode_vids(&blobs, nf)).await??;
    check_row(v, vids.len(), &l, prev)?;
    let t = match ts.get(&l.block) {
        Some(t) => *t,
        None => {
            let b = op
                .get_block_by_number(l.block.into())
                .await?
                .with_context(|| format!("block {}", l.block))?;
            ts.insert(l.block, b.header.timestamp);
            b.header.timestamp
        }
    };
    let row = TransitionRow {
        pid: *pid,
        index: v.index,
        block: l.block,
        ts: t,
        sender: l.sender,
        new_root: l.new_root,
        voters: v.voters,
        overwrites: v.overwrites,
        n_blobs: v.n_blobs,
        vids,
        tx: v.tx_hash,
    };
    Ok((row, l))
}

/// The first `chain` transitions of `pid`: each from O's archive, or, when O
/// lacks it or its record does not match the chain, from A's or B's. An
/// index no node can give is an error, as is an observer that knows more
/// transitions than `chain` (the count is stale).
pub async fn transitions(
    obs: &Node,
    fallback: [&Node; 2],
    op: &DynProvider,
    pid: &[u8; 31],
    nf: u8,
    chain: u64,
) -> Result<Transitions> {
    let srcs = [
        ("O", &obs.api),
        ("A", &fallback[0].api),
        ("B", &fallback[1].api),
    ];
    let mut lists: [Option<Result<BTreeMap<u64, TransitionView>>>; 3] = [None, None, None];
    let o = list(&obs.api, pid).await;
    if let Ok(l) = &o
        && let Some(i) = l.keys().find(|i| **i >= chain)
    {
        bail!("the observer holds transition {i}, beyond the chain's {chain}: the count is stale");
    }
    lists[0] = Some(o);
    let mut rows = Vec::new();
    let mut from_fallback = Vec::new();
    let mut notes = Vec::new();
    let mut prev: Option<Landed> = None;
    let mut ts = HashMap::new();
    for i in 0..chain {
        let mut why = Vec::new();
        let mut found = None;
        for (k, (name, api)) in srcs.iter().enumerate() {
            if lists[k].is_none() {
                lists[k] = Some(list(api, pid).await);
            }
            let v = match &lists[k] {
                Some(Ok(l)) => l.get(&i),
                Some(Err(e)) => {
                    why.push(format!("{name}: {e:#}"));
                    continue;
                }
                None => None,
            };
            let Some(v) = v else {
                why.push(format!("{name}: no record"));
                continue;
            };
            match row_of(api, op, pid, nf, v, prev.as_ref(), &mut ts).await {
                Ok(r) => {
                    found = Some((k, r));
                    break;
                }
                Err(e) => why.push(format!("{name}: {e:#}")),
            }
        }
        let Some((k, (row, l))) = found else {
            bail!(
                "transition {i} of {}: no node has it as the chain does ({})",
                ProcessId(*pid),
                why.join("; ")
            );
        };
        if k != 0 {
            from_fallback.push(i);
            notes.push(format!("transition {i}: {}", why.join("; ")));
        }
        rows.push(row);
        prev = Some(l);
    }
    Ok(Transitions {
        rows,
        from_fallback,
        notes,
    })
}

const STATUS_BATCH: usize = 64;

async fn status_of(api: &SequencerClient, pid: &[u8; 31], vid: u64) -> Option<NodeStatus> {
    for i in 0..READ_TRIES {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match api.vote_status_full(pid, vid).await {
            Ok(r) => {
                return Some(match r.status {
                    VoteStatus::Settled => NodeStatus::Settled,
                    VoteStatus::Error => NodeStatus::Error(r.error.unwrap_or_default()),
                    s => NodeStatus::Open(s.as_str().into()),
                });
            }
            Err(davinci_client::Error::Api { status: 404, .. }) => {
                return Some(NodeStatus::Unknown);
            }
            Err(_) => {}
        }
    }
    None
}

/// The status of every accepted or uncertain vote (settled ones too, for
/// `stale_error`) on each node that took it, 64 at a time. A node that never
/// answers leaves its key out: no answer is not a 404.
pub async fn statuses(
    nodes: &[(NodeId, &Node)],
    e: &BTreeMap<u64, Entry>,
) -> BTreeMap<(u64, NodeId), NodeStatus> {
    let mut todo = Vec::new();
    for (vid, en) in e {
        for n in en.accepted.union(&en.uncertain) {
            if let Some((_, node)) = nodes.iter().find(|(id, _)| id == n) {
                todo.push((*vid, *n, en.pid.0, node.api.clone()));
            }
        }
    }
    let mut out = BTreeMap::new();
    let mut set = JoinSet::new();
    let mut todo = todo.into_iter();
    loop {
        while set.len() < STATUS_BATCH {
            let Some((vid, n, pid, api)) = todo.next() else {
                break;
            };
            set.spawn(async move { (vid, n, status_of(&api, &pid, vid).await) });
        }
        let Some(r) = set.join_next().await else {
            break;
        };
        if let Ok((vid, n, Some(s))) = r {
            out.insert((vid, n), s);
        }
    }
    out
}

/// One prover job a node started.
#[derive(Clone, Debug, PartialEq)]
pub struct JobRow {
    pub node: NodeId,
    pub job: String,
    pub secs: f64,
    /// Unix seconds.
    pub started: f64,
    pub finished: f64,
    /// The proved root, zero for a job that did not finish with publics.
    pub root_after: [u8; 32],
}

/// Seconds since the epoch of the prover's RFC 3339 UTC times
/// (`2026-09-28T10:34:32.105321716Z`).
// TODO(task 10): use `bench::metrics::rfc3339` once it exists.
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
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + se) as f64 + frac)
}

/// The finished jobs among `calls` on `prover`; a job the prover no longer
/// knows, or one still running, is left out.
pub async fn jobs(prover: &str, node: NodeId, calls: &[ProveCall]) -> Vec<JobRow> {
    let pc = ProverClient::new(prover);
    let mut out = Vec::new();
    for c in calls {
        let Some(raw) = &c.job else { continue };
        let Ok(id) = JobId::parse(raw) else { continue };
        let Ok(j) = pc.job(&id).await else { continue };
        let (Some(started), Some(finished)) = (
            j.started_at.as_deref().and_then(rfc3339),
            j.finished_at.as_deref().and_then(rfc3339),
        ) else {
            continue;
        };
        let root_after = if j.status == JobStatus::Done {
            match pc.publics(&id).await {
                Ok(p) => BatchPublics::parse(&p)
                    .map(|p| p.root_after)
                    .unwrap_or([0; 32]),
                Err(_) => [0; 32],
            }
        } else {
            [0; 32]
        };
        out.push(JobRow {
            node,
            job: raw.clone(),
            secs: j
                .elapsed_ms
                .map_or(finished - started, |ms| ms as f64 / 1000.0),
            started,
            finished,
            root_after,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::blob::{TransitionData, blobs_from_cells, cells};

    use super::*;

    #[test]
    fn decode_vids_reads_the_vote_ids() {
        let vids = vec![1 << 63 | 5, 1 << 63 | 9];
        let t = TransitionData {
            vote_ids: vids.clone(),
            updates: vec![(0x10, Ballot::identity()), (0x11, Ballot::identity())],
            num_fields: 4,
        };
        let blobs: Vec<Vec<u8>> = blobs_from_cells(&cells(&t))
            .unwrap()
            .iter()
            .map(|b| b.to_vec())
            .collect();
        assert_eq!(decode_vids(&blobs, 4).unwrap(), vids);
        assert!(decode_vids(&[vec![0u8; 100]], 4).is_err());
    }

    #[test]
    fn rows_must_match_the_chain() {
        let v = TransitionView {
            index: 1,
            old_root: [1; 32],
            new_root: [2; 32],
            tx_hash: [9; 32],
            block_number: 70,
            sender: [5; 20],
            voters: 3,
            overwrites: 1,
            n_blobs: 1,
        };
        let prev = Landed {
            block: 60,
            sender: [6; 20],
            old_root: [0; 32],
            new_root: [1; 32],
            voters_count: 10,
            overwrites_count: 4,
            n_blobs: 1,
        };
        let l = Landed {
            block: 70,
            sender: [5; 20],
            old_root: [1; 32],
            new_root: [2; 32],
            voters_count: 12,
            overwrites_count: 5,
            n_blobs: 1,
        };
        check_row(&v, 3, &l, Some(&prev)).unwrap();
        // The decoded vote ids must be the chain's count of votes.
        assert!(check_row(&v, 2, &l, Some(&prev)).is_err());
        // Any field the node recorded must be the event's.
        let bad = [
            TransitionView {
                block_number: 71,
                ..v.clone()
            },
            TransitionView {
                sender: [7; 20],
                ..v.clone()
            },
            TransitionView {
                new_root: [3; 32],
                ..v.clone()
            },
            TransitionView {
                old_root: [3; 32],
                ..v.clone()
            },
            TransitionView {
                overwrites: 0,
                ..v.clone()
            },
            TransitionView {
                n_blobs: 2,
                ..v.clone()
            },
        ];
        for b in &bad {
            assert!(check_row(b, 3, &l, Some(&prev)).is_err(), "{b:?}");
        }
        // The chain's roots must link, in index order.
        let unlinked = Landed {
            new_root: [8; 32],
            ..prev.clone()
        };
        assert!(check_row(&v, 3, &l, Some(&unlinked)).is_err());
        // The first transition counts from zero.
        let first = Landed {
            voters_count: 2,
            overwrites_count: 1,
            ..l.clone()
        };
        check_row(
            &TransitionView {
                index: 0,
                ..v.clone()
            },
            3,
            &first,
            None,
        )
        .unwrap();
    }
}
