//! What the chain and the nodes say after a run: transitions with their
//! decoded vote ids, every vote's status on the nodes that took it, and the
//! prover jobs. The audit reads only what this module returns.
//!
//! The ground truth for "settled" is the chain: before its vote ids are
//! trusted, every transition row is bound to the registry's log at its index
//! and to its `ProcessStateTransitioned` event, and every blob to the
//! versioned hash the chain committed to.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use alloy::consensus::Transaction as _;
use alloy::primitives::{Address, B256, FixedBytes};
use alloy::providers::{DynProvider, Provider};
use anyhow::{Context, Result, anyhow, bail, ensure};
use davinci_client::SequencerClient;
use davinci_client::api::{ProcessId, TransitionView, VoteStatus, VoteStatusResponse};
use davinci_client::organizer::ProcessRegistry;
use davinci_zkvm_sdk::blob::{BLOB_SIZE, Blob, decode_blobs, verify_blob_commitment};
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::{JobId, JobStatus};
use tokio::task::JoinSet;

use super::audit::NodeStatus;
use super::ledger::Entry;
use super::metrics::rfc3339;
use super::plan::NodeId;
use super::rig::ProveCall;
use crate::chain::{self, RegistryTx};
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

fn to_blobs(blobs: &[Vec<u8>]) -> Result<Vec<Blob>> {
    blobs
        .iter()
        .map(|b| -> Result<Blob> {
            b.clone()
                .into_boxed_slice()
                .try_into()
                .map_err(|_| anyhow!("blob of {} bytes, want {BLOB_SIZE}", b.len()))
        })
        .collect()
}

/// The vote ids of a transition's blobs (in order) at `nf` fields.
pub fn decode_vids(blobs: &[Vec<u8>], nf: u8) -> Result<Vec<u64>> {
    Ok(decode_blobs(&to_blobs(blobs)?, nf)
        .map_err(|e| anyhow!("decode blobs: {e}"))?
        .vote_ids)
}

/// [`decode_vids`], once every blob is shown to be the one the chain
/// committed to: blob `i` must hash to versioned hash `vhs[i]`.
pub fn bound_vids(blobs: &[Vec<u8>], vhs: &[[u8; 32]], nf: u8) -> Result<Vec<u64>> {
    ensure!(
        blobs.len() == vhs.len(),
        "{} blobs for {} versioned hashes",
        blobs.len(),
        vhs.len()
    );
    let bs = to_blobs(blobs)?;
    for (i, (b, h)) in bs.iter().zip(vhs).enumerate() {
        verify_blob_commitment(b, h).map_err(|e| anyhow!("blob {i}: {e}"))?;
    }
    Ok(decode_blobs(&bs, nf)
        .map_err(|e| anyhow!("decode blobs: {e}"))?
        .vote_ids)
}

/// The `ProcessStateTransitioned` logs of `pid` from block `from`, in chain
/// order.
pub async fn chain_txs(
    rpc: &str,
    registry: Address,
    from: u64,
    pid: &[u8; 31],
) -> Result<Vec<RegistryTx>> {
    Ok(chain::transitions(rpc, registry, from)
        .await?
        .into_iter()
        .filter(|t| t.pid == *pid)
        .collect())
}

/// `ProcessStateTransitioned` events of `pid` from block `from`.
pub async fn chain_count(rpc: &str, registry: Address, from: u64, pid: &[u8; 31]) -> Result<u64> {
    Ok(chain_txs(rpc, registry, from, pid).await?.len() as u64)
}

/// The last values [`wait_quiet`] read.
#[derive(Clone, Debug)]
pub struct Quiet {
    /// Transitions of the process in the registry logs (`logs.len()`).
    pub chain: u64,
    /// Those logs, in chain order: what [`transitions`] binds rows to.
    pub logs: Vec<RegistryTx>,
    /// Transitions in the observer's archive.
    pub observer: u64,
    /// `pendingVotes` of A and B; `u64::MAX` when a node never answered
    /// with one (unreachable, or no actor owns the process).
    pub pending: [u64; 2],
    /// Ledger votes of the process that a node that took them still reports
    /// open (`pending`, `aggregated`, `processed`) or does not answer for.
    /// `pendingVotes` leaves out the batch in flight, so quiet also needs
    /// this at zero: otherwise votes still proving would read as `Lost`.
    pub open: usize,
    /// The statuses last read for those votes; give them to `classify`.
    pub statuses: BTreeMap<(u64, NodeId), NodeStatus>,
    pub timed_out: bool,
}

const QUIET_POLL: Duration = Duration::from_secs(10);

/// A status that no longer changes.
fn is_final(s: Option<&NodeStatus>) -> bool {
    matches!(
        s,
        Some(NodeStatus::Settled | NodeStatus::Error(_) | NodeStatus::Unknown)
    )
}

/// `(vote id, node, pid)` of every accepted or uncertain send in `e`, on the
/// nodes that took it; only `pid`'s when given.
fn status_keys(e: &BTreeMap<u64, Entry>, pid: Option<&[u8; 31]>) -> Vec<(u64, NodeId, [u8; 31])> {
    e.iter()
        .filter(|(_, en)| pid.is_none_or(|p| en.pid.0 == *p))
        .flat_map(|(vid, en)| {
            en.accepted
                .union(&en.uncertain)
                .map(|n| (*vid, *n, en.pid.0))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Polls every 10 s until O's transition count equals the registry's, A and
/// B report `pendingVotes == 0` and no ledger vote of `pid` in `e` is open on
/// a node that took it, or `max` passes. A read that fails counts as not
/// quiet; the registry logs must be read at least once.
#[allow(clippy::too_many_arguments)] // the chain, the nodes and the ledger
pub async fn wait_quiet(
    obs: &Node,
    ab: [&Node; 2],
    rpc: &str,
    registry: Address,
    from: u64,
    pid: &[u8; 31],
    e: &BTreeMap<u64, Entry>,
    max: Duration,
) -> Result<Quiet> {
    let start = Instant::now();
    let nodes = [(NodeId::A, ab[0]), (NodeId::B, ab[1])];
    let keys = status_keys(e, Some(pid));
    let mut q = Quiet {
        chain: 0,
        logs: Vec::new(),
        observer: 0,
        pending: [u64::MAX; 2],
        open: keys.len(),
        statuses: BTreeMap::new(),
        timed_out: false,
    };
    let mut chain_err: Option<anyhow::Error> = None;
    let mut chain_read = false;
    loop {
        let logs = match chain_txs(rpc, registry, from, pid).await {
            Ok(l) => Some(l),
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
        let chain = logs.as_ref().map(|l| l.len() as u64);
        if let Some(l) = logs {
            q.chain = l.len() as u64;
            q.logs = l;
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
        let counts = chain.is_some() && observer == chain && pending == [Some(0); 2];
        let late = start.elapsed() >= max;
        // Statuses only once the counts are quiet (or at the end): a full
        // read per poll would hammer the nodes during a long wait.
        if counts || late {
            let todo: Vec<_> = keys
                .iter()
                .filter(|(v, n, _)| !is_final(q.statuses.get(&(*v, *n))))
                .copied()
                .collect();
            q.statuses.extend(read_statuses(&nodes, todo).await);
            q.open = keys
                .iter()
                .filter(|(v, n, _)| !is_final(q.statuses.get(&(*v, *n))))
                .count();
        }
        if counts && q.open == 0 {
            return Ok(q);
        }
        if late {
            if !chain_read {
                let e = chain_err.unwrap_or_else(|| anyhow!("no answer"));
                return Err(e.context(format!(
                    "the registry's transitions of {} were never read",
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

/// One `ProcessStateTransitioned`, read from its transaction's receipt, and
/// what binds its blobs to the chain.
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
    /// The blobs' versioned hashes: the transaction's in blob mode, the
    /// `ProcessStateTransitionedL1` event's in L1-blob mode.
    pub vhs: Vec<[u8; 32]>,
    /// L1-blob mode: the Ethereum block of each blob, from the same event.
    pub l1_blocks: Option<Vec<u64>>,
}

/// What precedes a transition on chain.
#[derive(Clone, Copy, Debug)]
pub enum Before<'a> {
    /// Index 0: the root the registry created the process with.
    Genesis([u8; 32]),
    /// The chain's event of the previous index.
    Prev(&'a Landed),
}

/// A node's record `v` of the transition the registry logged at `log`, with
/// `n_vids` vote ids decoded from its blobs, against the event `l` read from
/// the receipt of `v`'s transaction.
pub fn check_row(
    v: &TransitionView,
    n_vids: usize,
    l: &Landed,
    before: Before<'_>,
    log: &RegistryTx,
) -> Result<()> {
    ensure!(
        v.tx_hash == log.tx.0 && l.block == log.block,
        "the node's transaction is not the chain's transition {}",
        v.index
    );
    ensure!(
        l.sender == log.sender.into_array() && l.n_blobs == log.n_blobs,
        "the receipt's event differs from the log"
    );
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
    ensure!(
        l.vhs.len() as u64 == l.n_blobs,
        "{} versioned hashes for {} blobs",
        l.vhs.len(),
        l.n_blobs
    );
    if let Some(b) = &l.l1_blocks {
        ensure!(
            b.len() == l.vhs.len(),
            "{} L1 blocks for {} blobs",
            b.len(),
            l.vhs.len()
        );
    }
    let (pv, po) = match before {
        Before::Genesis(g) => {
            ensure!(
                l.old_root == g,
                "transition 0 does not start from the process's genesis root"
            );
            (0, 0)
        }
        Before::Prev(p) => {
            ensure!(
                l.old_root == p.new_root,
                "the chain's old root does not follow the previous transition"
            );
            (p.voters_count, p.overwrites_count)
        }
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

/// The chain context of one process's rows.
struct Ctx<'a> {
    op: &'a DynProvider,
    registry: Address,
    pid: &'a [u8; 31],
    nf: u8,
}

/// The root the registry created `pid` with: `genesisRoot` over the
/// process's own parameters, a view with no historical state.
async fn genesis_root(op: &DynProvider, registry: Address, pid: &[u8; 31]) -> Result<[u8; 32]> {
    let r = ProcessRegistry::new(registry, op.clone());
    let p = r
        .getProcess(FixedBytes(*pid))
        .call()
        .await
        .context("getProcess")?;
    ensure!(p.organizationId != Address::ZERO, "process not found");
    let g = r
        .genesisRoot(
            FixedBytes(*pid),
            p.ballotMode,
            p.encryptionKey,
            p.census.censusOrigin,
        )
        .call()
        .await
        .context("genesisRoot")?;
    Ok(g.0)
}

/// The event of the process in the settlement transaction `tx`, which must
/// have called the registry.
async fn landed(c: &Ctx<'_>, tx: [u8; 32]) -> Result<Landed> {
    use ProcessRegistry::{ProcessStateTransitioned as Ev, ProcessStateTransitionedL1 as EvL1};
    let h = B256::from(tx);
    let r =
        c.op.get_transaction_receipt(h)
            .await?
            .with_context(|| format!("no receipt for {h}"))?;
    ensure!(r.status(), "{h} reverted");
    ensure!(r.to == Some(c.registry), "{h} did not call the registry");
    let block = r.block_number.context("receipt without a block")?;
    let ours = |l: &&alloy::rpc::types::Log| l.address() == c.registry;
    let mut evs = r
        .inner
        .logs()
        .iter()
        .filter(ours)
        .filter_map(|l| l.log_decode::<Ev>().ok())
        .filter(|l| l.inner.data.processId.0 == *c.pid);
    let e = evs
        .next()
        .with_context(|| format!("{h} has no transition of this process"))?;
    ensure!(
        evs.next().is_none(),
        "{h} has two transitions of this process"
    );
    let mut l1s = r
        .inner
        .logs()
        .iter()
        .filter(ours)
        .filter_map(|l| l.log_decode::<EvL1>().ok())
        .filter(|l| l.inner.data.processId.0 == *c.pid);
    let l1 = l1s.next();
    ensure!(
        l1s.next().is_none(),
        "{h} has two L1 events of this process"
    );
    let (vhs, l1_blocks) = match l1 {
        Some(l) => {
            let d = &l.inner.data;
            let blocks = d
                .l1BlockNumbers
                .iter()
                .map(|b| u64::try_from(*b).map_err(|_| anyhow!("L1 block out of range")))
                .collect::<Result<Vec<u64>>>()?;
            (
                d.versionedHashes.iter().map(|v| v.0).collect(),
                Some(blocks),
            )
        }
        None => {
            let t =
                c.op.get_transaction_by_hash(h)
                    .await?
                    .with_context(|| format!("no transaction {h}"))?;
            let hs = t
                .inner
                .blob_versioned_hashes()
                .with_context(|| format!("{h} carries no blobs and no L1 event"))?;
            (hs.iter().map(|v| v.0).collect(), None)
        }
    };
    let d = &e.inner.data;
    Ok(Landed {
        block,
        sender: d.sender.into_array(),
        old_root: d.oldStateRoot.0,
        new_root: d.newStateRoot.0,
        voters_count: u64::try_from(d.newVotersCount)?,
        overwrites_count: u64::try_from(d.newOverwrittenVotesCount)?,
        n_blobs: u64::try_from(d.nBlobs)?,
        vhs,
        l1_blocks,
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

/// Row `v.index` from one node's blobs, bound to the chain: the record to
/// the registry's log, the blobs to their versioned hashes.
async fn row_of(
    c: &Ctx<'_>,
    api: &SequencerClient,
    v: &TransitionView,
    before: Before<'_>,
    log: &RegistryTx,
    ts: &mut HashMap<u64, u64>,
) -> Result<(TransitionRow, Landed)> {
    ensure!(
        v.tx_hash == log.tx.0,
        "the node's transaction is not the chain's transition {}",
        v.index
    );
    let l = landed(c, v.tx_hash).await?;
    let mut blobs = None;
    for i in 0..READ_TRIES {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match api.transition_blobs(c.pid, v.index).await {
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
    // KZG commitments and point parsing: off the async workers.
    let (vhs, nf) = (l.vhs.clone(), c.nf);
    let vids = tokio::task::spawn_blocking(move || bound_vids(&blobs, &vhs, nf)).await??;
    check_row(v, vids.len(), &l, before, log)?;
    let t = match ts.get(&l.block) {
        Some(t) => *t,
        None => {
            let b =
                c.op.get_block_by_number(l.block.into())
                    .await?
                    .with_context(|| format!("block {}", l.block))?;
            ts.insert(l.block, b.header.timestamp);
            b.header.timestamp
        }
    };
    let row = TransitionRow {
        pid: *c.pid,
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

/// One row per entry of `logs` (`pid`'s `ProcessStateTransitioned` logs on
/// `registry`, in chain order, as [`wait_quiet`] returns them). Each comes
/// from O's archive or, when O lacks it or its record does not match the
/// chain, from A's or B's. An index no node can give is an error, as is an
/// observer that knows more transitions than `logs` (they are stale).
pub async fn transitions(
    obs: &Node,
    fallback: [&Node; 2],
    op: &DynProvider,
    registry: Address,
    pid: &[u8; 31],
    nf: u8,
    logs: &[RegistryTx],
) -> Result<Transitions> {
    ensure!(
        logs.iter().all(|l| l.pid == *pid),
        "logs of another process"
    );
    let c = Ctx {
        op,
        registry,
        pid,
        nf,
    };
    let genesis = genesis_root(op, registry, pid).await?;
    let chain = logs.len() as u64;
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
        bail!("the observer holds transition {i}, beyond the chain's {chain}: the logs are stale");
    }
    lists[0] = Some(o);
    let mut rows = Vec::new();
    let mut from_fallback = Vec::new();
    let mut notes = Vec::new();
    let mut prev: Option<Landed> = None;
    let mut ts = HashMap::new();
    for (i, log) in (0u64..).zip(logs) {
        let before = match &prev {
            Some(p) => Before::Prev(p),
            None => Before::Genesis(genesis),
        };
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
            match row_of(&c, api, v, before, log, &mut ts).await {
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

/// A node's definitive answer for one vote, or `None`. Only 40401 (vote not
/// found) says the node does not know the vote: a 40402 (unknown process),
/// any other error or no answer proves nothing.
fn node_status(r: &Result<VoteStatusResponse, davinci_client::Error>) -> Option<NodeStatus> {
    match r {
        Ok(r) => Some(match r.status {
            VoteStatus::Settled => NodeStatus::Settled,
            VoteStatus::Error => NodeStatus::Error(r.error.clone().unwrap_or_default()),
            s => NodeStatus::Open(s.as_str().into()),
        }),
        Err(davinci_client::Error::Api {
            code: Some(40401), ..
        }) => Some(NodeStatus::Unknown),
        Err(_) => None,
    }
}

async fn status_of(api: &SequencerClient, pid: &[u8; 31], vid: u64) -> Option<NodeStatus> {
    for i in 0..READ_TRIES {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if let Some(s) = node_status(&api.vote_status_full(pid, vid).await) {
            return Some(s);
        }
    }
    None
}

/// Reads `keys` on their nodes, 64 at a time; a key with no answer is left
/// out.
async fn read_statuses(
    nodes: &[(NodeId, &Node)],
    keys: Vec<(u64, NodeId, [u8; 31])>,
) -> BTreeMap<(u64, NodeId), NodeStatus> {
    let mut out = BTreeMap::new();
    let mut set = JoinSet::new();
    let mut todo = keys.into_iter().filter_map(|(vid, n, pid)| {
        let api = nodes.iter().find(|(id, _)| *id == n)?.1.api.clone();
        Some((vid, n, pid, api))
    });
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

/// The status of every accepted or uncertain vote (settled ones too, for
/// `stale_error`) on each node that took it, 64 at a time. A node that never
/// answers leaves its key out: no answer is not a 404.
pub async fn statuses(
    nodes: &[(NodeId, &Node)],
    e: &BTreeMap<u64, Entry>,
) -> BTreeMap<(u64, NodeId), NodeStatus> {
    read_statuses(nodes, status_keys(e, None)).await
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
    use davinci_zkvm_sdk::blob::{TransitionData, blobs_from_cells, build_blobs, cells};

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

    fn log(tx: u8, block: u64) -> RegistryTx {
        RegistryTx {
            pid: [3; 31],
            sender: Address::from([5; 20]),
            tx: B256::from([tx; 32]),
            block,
            n_blobs: 1,
        }
    }

    #[test]
    fn rows_must_match_the_chain() {
        // The chain: transition 0 in tx 8 at block 60, transition 1 in tx 9 at block 70.
        let logs = [log(8, 60), log(9, 70)];
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
            sender: [5; 20],
            old_root: [0; 32],
            new_root: [1; 32],
            voters_count: 10,
            overwrites_count: 4,
            n_blobs: 1,
            vhs: vec![[1; 32]],
            l1_blocks: None,
        };
        let l = Landed {
            block: 70,
            sender: [5; 20],
            old_root: [1; 32],
            new_root: [2; 32],
            voters_count: 12,
            overwrites_count: 5,
            n_blobs: 1,
            vhs: vec![[2; 32]],
            l1_blocks: None,
        };
        let at1 = Before::Prev(&prev);
        check_row(&v, 3, &l, at1, &logs[1]).unwrap();
        // L1-blob mode: one Ethereum block per blob.
        let l1 = Landed {
            l1_blocks: Some(vec![900]),
            ..l.clone()
        };
        check_row(&v, 3, &l1, at1, &logs[1]).unwrap();
        let l1_bad = Landed {
            l1_blocks: Some(vec![900, 901]),
            ..l.clone()
        };
        assert!(check_row(&v, 3, &l1_bad, at1, &logs[1]).is_err());
        // Blob hashes must cover exactly the blobs.
        let few = Landed {
            vhs: vec![],
            ..l.clone()
        };
        assert!(check_row(&v, 3, &few, at1, &logs[1]).is_err());
        // The decoded vote ids must be the chain's count of votes.
        assert!(check_row(&v, 2, &l, at1, &logs[1]).is_err());
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
            assert!(check_row(b, 3, &l, at1, &logs[1]).is_err(), "{b:?}");
        }
        // A real transition at the wrong index: tx 8 is the chain's
        // transition 0, recorded by the node as 1, even with a matching event.
        let swapped = TransitionView {
            tx_hash: [8; 32],
            ..v.clone()
        };
        assert!(check_row(&swapped, 3, &l, at1, &logs[1]).is_err());
        assert!(check_row(&v, 3, &l, at1, &logs[0]).is_err());
        // The chain's roots must link, in index order.
        let unlinked = Landed {
            new_root: [8; 32],
            ..prev.clone()
        };
        assert!(check_row(&v, 3, &l, Before::Prev(&unlinked), &logs[1]).is_err());
        // The first transition counts from zero and starts at the genesis root.
        let first = Landed {
            block: 60,
            old_root: [0; 32],
            voters_count: 2,
            overwrites_count: 1,
            ..l.clone()
        };
        let v0 = TransitionView {
            index: 0,
            tx_hash: [8; 32],
            block_number: 60,
            old_root: [0; 32],
            ..v.clone()
        };
        check_row(&v0, 3, &first, Before::Genesis([0; 32]), &logs[0]).unwrap();
        assert!(check_row(&v0, 3, &first, Before::Genesis([4; 32]), &logs[0]).is_err());
    }

    #[test]
    fn blobs_must_match_their_versioned_hashes() {
        let vids = vec![1 << 63 | 5, 1 << 63 | 9];
        let t = TransitionData {
            vote_ids: vids.clone(),
            updates: vec![(0x10, Ballot::identity()), (0x11, Ballot::identity())],
            num_fields: 4,
        };
        let tb = build_blobs(&t, &ProcessId([3; 31]).to_fr(), &[7; 32]).unwrap();
        let blobs: Vec<Vec<u8>> = tb.blobs.iter().map(|b| b.to_vec()).collect();
        assert_eq!(bound_vids(&blobs, &tb.versioned_hashes, 4).unwrap(), vids);
        // A wrong versioned hash rejects the blob.
        let mut wrong = tb.versioned_hashes.clone();
        wrong[0][31] ^= 1;
        let e = bound_vids(&blobs, &wrong, 4).unwrap_err();
        assert!(format!("{e:#}").contains("versioned hash"), "{e:#}");
        // So does a blob the chain did not commit to.
        let mut other = blobs.clone();
        other[0][100] ^= 1;
        assert!(bound_vids(&other, &tb.versioned_hashes, 4).is_err());
        // And a count that differs.
        assert!(bound_vids(&blobs, &[], 4).is_err());
    }

    #[test]
    fn only_40401_means_the_node_does_not_know_the_vote() {
        use davinci_client::Error as E;
        let api = |status, code| -> Result<VoteStatusResponse, E> {
            Err(E::Api {
                status,
                code,
                message: String::new(),
            })
        };
        let ok = |status, error: Option<&str>| -> Result<VoteStatusResponse, E> {
            Ok(VoteStatusResponse {
                status,
                error: error.map(String::from),
            })
        };
        assert_eq!(
            node_status(&api(404, Some(40401))),
            Some(NodeStatus::Unknown)
        );
        assert_eq!(node_status(&api(404, Some(40402))), None);
        assert_eq!(node_status(&api(404, None)), None);
        assert_eq!(node_status(&api(500, Some(50001))), None);
        assert_eq!(node_status(&Err(E::Http("reset".into()))), None);
        assert_eq!(
            node_status(&ok(VoteStatus::Settled, None)),
            Some(NodeStatus::Settled)
        );
        assert_eq!(
            node_status(&ok(VoteStatus::Error, Some("process closed"))),
            Some(NodeStatus::Error("process closed".into()))
        );
        for (s, name) in [
            (VoteStatus::Pending, "pending"),
            (VoteStatus::Aggregated, "aggregated"),
            (VoteStatus::Processed, "processed"),
        ] {
            assert_eq!(
                node_status(&ok(s, None)),
                Some(NodeStatus::Open(name.into()))
            );
        }
        assert!(!is_final(None) && !is_final(Some(&NodeStatus::Open("processed".into()))));
        assert!(is_final(Some(&NodeStatus::Unknown)) && is_final(Some(&NodeStatus::Settled)));
    }
}
