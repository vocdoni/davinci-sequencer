//! L1-blob mode (OP Stack registries): a batch's blobs are posted on
//! Ethereum while it is proved, and the settlement on OP carries a proof that
//! the blob transaction is in Ethereum's canonical chain. The actor calls
//! into this module only when `Deps.l1` is set; blob mode never does.

mod post;
mod settle;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use davinci_zkvm_sdk::blob::TransitionBlobs;
use davinci_zkvm_sdk::limits::{MAX_BLOBS, TX_BLOB_CAP};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::actor::Chain;
use crate::storage::{Db, StorageError};
use crate::web3::{L1Chain, L1Origin, TxReceipt, Web3Error};

pub use post::{PostError, Posted, Poster, REPLACE_AFTER};
pub use settle::{
    Anchor, CURRENT_MAX_HEADERS, GONE_FOR, L1_MAX_AGE, L1_SETTLE_LAG, MAX_L1_PROOF_HEADERS,
    MAX_PROOF_RETRIES, MAX_REPOSTS, PROOF_FAIL_FOR, REORG_MARGIN, TOP_ABOVE_ORIGIN, choose_anchor,
    settle,
};

/// Longest wait between fee-capped post attempts: fees move once per L1
/// block (12 s) and a spike lasts minutes.
pub const FEE_CAP_BACKOFF: Duration = Duration::from_secs(60);

/// Least gas a `submitStateTransitionL1` takes: one blob transaction and a
/// 3-header L1 proof measured 800-860k on OP (docs/benchmarks.md). The
/// check before a post prices this much, so it holds back only posts whose
/// settlement `--op-max-tx-fee` is sure to withhold; a bigger one may still
/// be withheld at its send.
pub const MIN_SETTLE_GAS: u64 = 800_000;

/// The registry chain's fee check before an L1 post.
#[async_trait]
pub trait FeeGate: Send + Sync {
    /// `Web3Error::FeeCap` when a send estimated at `gas` would be withheld
    /// by `--op-max-tx-fee` at the current fees.
    async fn check(&self, gas: u64) -> Result<(), Web3Error>;
}

/// Seconds each chunk after the first adds to the Ethereum leg: its post
/// waits until the one before is mined (about two L1 blocks).
pub const POST_TIME: u64 = 24;

/// Most blobs one transition may use: in L1-blob mode the guest's
/// `MAX_BLOBS`, posted in chunks of the per-transaction cap; otherwise the
/// per-transaction cap (`TX_BLOB_CAP` when unknown).
pub fn batch_blob_cap(l1_mode: bool, per_tx: Option<usize>) -> usize {
    if l1_mode {
        MAX_BLOBS
    } else {
        per_tx.unwrap_or(TX_BLOB_CAP)
    }
}

/// Most chunks whose Ethereum leg lands within `window` seconds:
/// `lag + (chunks - 1) * POST_TIME + margin <= window`. `None` when not
/// even one does.
pub fn chunks_fit(window: u64, lag: u64, margin: u64) -> Option<usize> {
    let spare = window.checked_sub(lag.saturating_add(margin))?;
    usize::try_from(1 + spare / POST_TIME).ok()
}

/// The Ethereum side of a node in L1-blob mode.
pub struct L1Da {
    pub l1: Arc<dyn L1Chain>,
    pub poster: Poster,
    /// Pace of the settle loop and the poster (the chain poll interval).
    pub poll: Duration,
    /// Most blobs one Ethereum transaction carries (the per-transaction
    /// cap): a transition goes out in chunks of this many.
    pub chunk: usize,
    /// The registry's fee check before a post; `None` posts unchecked.
    pub op_fees: Option<Arc<dyn FeeGate>>,
}

impl L1Da {
    /// `fee_cap`: the most one post may cost at the fees it offers, in wei;
    /// `chunk`: the per-transaction blob cap (at least 1).
    pub fn new(
        l1: Arc<dyn L1Chain>,
        db: Db,
        fee_cap: u128,
        poll: Duration,
        chunk: usize,
    ) -> Result<Self, StorageError> {
        Ok(L1Da {
            poster: Poster::new(l1.clone(), db, fee_cap, poll)?,
            l1,
            poll,
            chunk: chunk.max(1),
            op_fees: None,
        })
    }
}

/// `blobs` split in order into transactions of at most `per_tx` blobs (the
/// last may be shorter), each with its commitments, openings and versioned
/// hashes. Chunk `i` holds blobs `per_tx * i ..`, so the chunks' versioned
/// hashes in chunk order are the transition's: the order the registry opens
/// its commitments in. The per-blob vectors must have the length of `blobs`
/// (`build_blobs` output always does); unequal ones would panic on a slice.
pub fn chunks(blobs: &TransitionBlobs, per_tx: usize) -> Vec<TransitionBlobs> {
    let per_tx = per_tx.max(1);
    let n = blobs.blobs.len();
    debug_assert!(
        [
            blobs.commitments.len(),
            blobs.zs.len(),
            blobs.ys.len(),
            blobs.proofs.len(),
            blobs.versioned_hashes.len()
        ]
        .iter()
        .all(|&l| l == n),
        "per-blob vectors of unequal length"
    );
    (0..blobs.blobs.len())
        .step_by(per_tx)
        .map(|a| {
            let b = (a + per_tx).min(blobs.blobs.len());
            TransitionBlobs {
                blobs: blobs.blobs[a..b].to_vec(),
                commitments: blobs.commitments[a..b].to_vec(),
                zs: blobs.zs[a..b].to_vec(),
                ys: blobs.ys[a..b].to_vec(),
                proofs: blobs.proofs[a..b].to_vec(),
                versioned_hashes: blobs.versioned_hashes[a..b].to_vec(),
                digest: blobs.digest,
            }
        })
        .collect()
}

impl L1Da {
    /// Waits one poll; false once `cancel` fires.
    pub(crate) async fn pause(&self, cancel: &CancellationToken) -> bool {
        self.pause_for(self.poll, cancel).await
    }

    /// Before a post: `OpFeeCap` when `--op-max-tx-fee` would withhold the
    /// settlement at the current OP fees, so the post would only spend. A
    /// failed fee read lets the post go: the send checks again.
    pub(crate) async fn settle_fee_check(&self) -> Result<(), PostError> {
        let Some(g) = &self.op_fees else {
            return Ok(());
        };
        match g.check(MIN_SETTLE_GAS).await {
            Err(Web3Error::FeeCap { want, cap }) => Err(PostError::OpFeeCap { want, cap }),
            Err(e) => {
                debug!(error = %e, "OP fee read failed; posting unchecked");
                Ok(())
            }
            Ok(()) => Ok(()),
        }
    }

    /// Waits `d`; false once `cancel` fires.
    pub(crate) async fn pause_for(&self, d: Duration, cancel: &CancellationToken) -> bool {
        tokio::select! {
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(d) => true,
        }
    }
}

/// A run of failed posts (fee cap, L1 trouble) of one job: paces the
/// retries and logs the run at WARN when it starts and when a post ends it,
/// DEBUG in between. Fee-cap refusals wait one poll, doubling up to
/// [`FEE_CAP_BACKOFF`]; L1 trouble keeps the poll.
#[derive(Default)]
pub(crate) struct Retries {
    since: Option<tokio::time::Instant>,
    checks: u32,
    wait: Duration,
}

impl Retries {
    /// Logs `e` and sets the wait before the next attempt.
    pub(crate) fn failed(&mut self, pid: &str, chunk: usize, e: &PostError, poll: Duration) {
        self.checks = self.checks.saturating_add(1);
        if self.since.is_none() {
            self.since = Some(tokio::time::Instant::now());
            warn!(pid, chunk, error = %e, "L1 post failed; retrying (logged again once it goes through)");
        } else {
            debug!(pid, chunk, checks = self.checks, error = %e, "L1 post failed again");
        }
        self.wait = match e {
            PostError::FeeCap { .. } | PostError::OpFeeCap { .. } if !self.wait.is_zero() => {
                self.wait.saturating_mul(2).min(FEE_CAP_BACKOFF.max(poll))
            }
            _ => poll,
        };
    }

    /// A post went through: logs the run it ended, if any, and resets.
    pub(crate) fn cleared(&mut self, pid: &str, chunk: usize) {
        if let Some(t) = self.since.take() {
            warn!(pid, chunk, after = ?t.elapsed(), checks = self.checks,
                "L1 post went through after failing");
        }
        *self = Retries::default();
    }

    /// Waits before the next attempt, never past `job`'s deadline: false
    /// once the deadline passed or the flight ended.
    pub(crate) async fn wait(&self, da: &L1Da, job: &L1Job<'_>) -> bool {
        let left = job
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        !left.is_zero() && da.pause_for(self.wait.min(left), &job.cancel).await
    }
}

/// One sealed batch on its way to OP.
pub struct L1Job<'a> {
    pub pid: [u8; 31],
    pub root_before: [u8; 32],
    pub blobs: &'a TransitionBlobs,
    /// No settlement after this (the grace end known at seal time).
    pub deadline: tokio::time::Instant,
    /// Fired when the actor drops the flight (rollback, shutdown).
    pub cancel: CancellationToken,
    /// Set once any chunk of this job is mined: the job spent on Ethereum.
    pub posted: AtomicBool,
}

/// What [`with_post`] leaves for [`settle`].
#[derive(Debug, Default)]
pub struct Posting {
    /// The chunks mined while proving, in chunk order (the first ones).
    pub posts: Vec<Posted>,
    /// Why posting stopped before the last chunk, if it did.
    pub error: Option<PostError>,
    /// OP's origin recorded with `record()` while the proof was not ready.
    pub recorded: Option<L1Origin>,
}

/// How an L1-mode settlement ended; the actor maps it onto its job outcome.
#[derive(Debug)]
pub enum Settled {
    Landed(TxReceipt),
    /// The registry's error name (`Error(<message>)` for a string revert).
    Reverted(String),
    Lost(String),
    Transient(String),
    Failed(String),
}

/// Posts `chunks` in order, one transaction at a time: each waits until the
/// one before is mined (the window-1 poster), and only while OP's fee lets
/// the settlement through ([`L1Da::settle_fee_check`]). Either fee cap (L1's
/// or OP's) and L1 trouble are retried as [`Retries`] paces them; any other
/// error, or the flight's end, stops the run. Returns the mined posts and the
/// error that stopped it early.
async fn post_chunks(
    da: &L1Da,
    job: &L1Job<'_>,
    chunks: &[TransitionBlobs],
) -> (Vec<Posted>, Option<PostError>) {
    let pid = hex::encode(job.pid);
    let mut posts = Vec::with_capacity(chunks.len());
    let mut retries = Retries::default();
    for (i, c) in chunks.iter().enumerate() {
        loop {
            let res = match da.settle_fee_check().await {
                Ok(()) => {
                    da.poster
                        .post(&job.pid, &job.root_before, c, job.deadline)
                        .await
                }
                Err(e) => Err(e),
            };
            match res {
                Ok(p) => {
                    retries.cleared(&pid, i);
                    posts.push(p);
                    job.posted.store(true, Ordering::Relaxed);
                    break;
                }
                // Fee cap (L1's, or OP's settlement), L1 trouble: wait and
                // post again while the proof runs.
                Err(
                    e @ (PostError::FeeCap { .. } | PostError::OpFeeCap { .. } | PostError::L1(_)),
                ) => {
                    retries.failed(&pid, i, &e, da.poll);
                    // A fee spike between chunks must not hold the flight past
                    // the job's deadline; `settle` then posts the missing chunk,
                    // but its top-of-loop deadline check fires first and
                    // reports `Deadline`.
                    if tokio::time::Instant::now() >= job.deadline {
                        return (posts, Some(PostError::Deadline));
                    }
                    if !retries.wait(da, job).await {
                        if job.cancel.is_cancelled() {
                            return (posts, Some(e));
                        }
                        return (posts, Some(PostError::Deadline));
                    }
                }
                Err(e) => return (posts, Some(e)),
            }
        }
    }
    (posts, None)
}

/// Runs `prove` while posting the batch's blobs, in chunks of the
/// per-transaction cap. Once the last chunk is mined, if OP's origin reaches
/// its block before the proof is ready, records that origin on OP once, so
/// a late proof still anchors close to the blobs.
pub async fn with_post<T>(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    prove: impl Future<Output = T>,
) -> (T, Posting) {
    let chunks = chunks(job.blobs, da.chunk);
    let post = post_chunks(da, job, &chunks);
    tokio::pin!(prove);
    tokio::pin!(post);
    let mut posted: Option<(Vec<Posted>, Option<PostError>)> = None;
    let mut recorded = None;
    let mut record_tried = false;
    let proved = loop {
        // The newest chunk's block, once every chunk is mined: a recorded
        // origin below it could not anchor the transactions above it.
        let newest = match &posted {
            Some((p, None)) if !record_tried => p.iter().map(|p| p.block).max(),
            _ => None,
        };
        tokio::select! {
            out = &mut prove => break out,
            p = &mut post, if posted.is_none() => posted = Some(p),
            _ = tokio::time::sleep(da.poll), if newest.is_some() => {
                if let Some(n) = newest
                    && let Ok(o) = chain.l1_origin().await
                    && o.number >= n
                {
                    record_tried = true;
                    match chain.record_l1_origin().await {
                        Ok(r) => {
                            info!(pid = %hex::encode(job.pid), origin = r.number,
                                "OP's origin reached the blobs before the proof; origin recorded");
                            recorded = Some(r);
                        }
                        Err(e) => warn!(pid = %hex::encode(job.pid), error = %e,
                            "record() failed; settling without it"),
                    }
                }
            }
        }
    };
    let (posts, error) = match posted {
        Some(p) => p,
        None => post.await,
    };
    (
        proved,
        Posting {
            posts,
            error,
            recorded,
        },
    )
}

/// Why the registry's DA mode and the configuration disagree, if they do.
pub fn mode_error(da: &crate::web3::RegistryDa, l1_configured: bool) -> Option<String> {
    use crate::web3::RegistryDa;
    match (da, l1_configured) {
        (RegistryDa::Blobs, true) => Some(
            "the registry settles with blobs on its own chain (no L1BlobVerifier); \
             remove --l1-rpc-url (DAVINCI_L1_RPC_URL)"
                .into(),
        ),
        (RegistryDa::L1Blobs { verifier, .. }, false) => Some(format!(
            "the registry settles with Ethereum blobs through L1BlobVerifier {verifier}; \
             set --l1-rpc-url (DAVINCI_L1_RPC_URL)"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;
    use davinci_zkvm_sdk::blob::{BLOB_SIZE, Blob};

    use super::*;
    use crate::web3::RegistryDa;

    fn fake_blobs(n: usize) -> TransitionBlobs {
        let blob = |i: usize| -> Blob {
            vec![i as u8; BLOB_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap()
        };
        TransitionBlobs {
            blobs: (0..n).map(blob).collect(),
            commitments: (0..n).map(|i| [i as u8; 48]).collect(),
            zs: (0..n).map(|i| [i as u8 ^ 0x10; 32]).collect(),
            ys: (0..n).map(|i| [i as u8 ^ 0x20; 32]).collect(),
            proofs: (0..n).map(|i| [i as u8 ^ 0x30; 48]).collect(),
            versioned_hashes: (0..n).map(|i| [i as u8 ^ 0x40; 32]).collect(),
            digest: [7; 32],
        }
    }

    #[test]
    fn each_chunk_after_the_first_needs_post_time() {
        // OP defaults: a 110 s leg and a 30 s margin.
        assert_eq!(chunks_fit(139, 110, 30), None);
        assert_eq!(chunks_fit(140, 110, 30), Some(1));
        assert_eq!(chunks_fit(163, 110, 30), Some(1));
        assert_eq!(chunks_fit(164, 110, 30), Some(2));
        assert_eq!(chunks_fit(188, 110, 30), Some(3));
        // The 240 s floor while voting is open: five chunks, 30 blobs.
        assert_eq!(chunks_fit(240, 110, 30), Some(5));
        assert_eq!(chunks_fit(600, u64::MAX, 30), None);
        assert_eq!(batch_blob_cap(true, Some(6)), MAX_BLOBS);
        assert_eq!(batch_blob_cap(false, Some(2)), 2);
        assert_eq!(batch_blob_cap(false, None), TX_BLOB_CAP);
    }

    #[test]
    fn chunks_keep_the_blob_order() {
        let t = fake_blobs(13);
        let c = chunks(&t, 6);
        let sizes = |c: &[TransitionBlobs]| c.iter().map(|c| c.blobs.len()).collect::<Vec<_>>();
        assert_eq!(sizes(&c), [6, 6, 1]);
        for (k, c) in c.iter().enumerate() {
            for j in 0..c.blobs.len() {
                let i = 6 * k + j;
                assert_eq!(c.blobs[j][0], i as u8);
                assert_eq!(c.commitments[j], t.commitments[i]);
                assert_eq!((c.zs[j], c.ys[j]), (t.zs[i], t.ys[i]));
                assert_eq!(c.proofs[j], t.proofs[i]);
                assert_eq!(c.versioned_hashes[j], t.versioned_hashes[i]);
            }
            assert_eq!(c.digest, t.digest);
        }
        // Concatenated in chunk order: the transition's versioned hashes.
        let vhs: Vec<[u8; 32]> = c.iter().flat_map(|c| c.versioned_hashes.clone()).collect();
        assert_eq!(vhs, t.versioned_hashes);
        assert_eq!(sizes(&chunks(&t, 32)), [13]);
        assert_eq!(sizes(&chunks(&fake_blobs(9), 6)), [6, 3]);
        assert_eq!(sizes(&chunks(&fake_blobs(6), 6)), [6]);
        // A zero cap is one blob per transaction.
        assert_eq!(sizes(&chunks(&fake_blobs(2), 0)), [1, 1]);
    }

    #[test]
    fn the_registry_mode_must_match_the_configuration() {
        let l1 = RegistryDa::L1Blobs {
            verifier: Address::repeat_byte(1),
            l1_block: Address::repeat_byte(2),
        };
        assert!(mode_error(&RegistryDa::Blobs, false).is_none());
        assert!(mode_error(&l1, true).is_none());
        assert!(
            mode_error(&RegistryDa::Blobs, true)
                .unwrap()
                .contains("--l1-rpc-url")
        );
        assert!(mode_error(&l1, false).unwrap().contains("--l1-rpc-url"));
    }
}
