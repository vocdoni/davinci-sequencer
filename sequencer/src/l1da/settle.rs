//! Settling a proved batch whose blobs are on Ethereum: wait for the blob
//! transaction's block and for OP's origin, anchor the L1 proof where OP
//! accepts it, check it locally, simulate and send. The blobs are posted
//! again when their transaction is gone or the proof would be too deep.

use std::time::Duration;

use alloy::primitives::B256;
use davinci_zkvm_sdk::client::PlonkSnark;
use tokio::time::Instant;
use tracing::{info, warn};

use super::{L1Da, L1Job, PostError, Posting, Settled};
use crate::actor::Chain;
use crate::web3::l1proof::verify_local;
use crate::web3::{L1Origin, RevertReason, Web3Error, revert_message};

/// L1 blocks on top of the blob block before a proof is built (reorg margin).
pub const REORG_MARGIN: u64 = 1;
/// Headers above OP's origin a proof carries, so it survives the origin
/// advancing while the settlement is in flight.
pub const TOP_ABOVE_ORIGIN: u64 = 2;
/// Most headers anchored at the current origin before a recorded origin is
/// preferred (about 60-75k gas each on OP).
pub const CURRENT_MAX_HEADERS: u64 = 16;
/// Most headers in any proof (about 5M gas and 40 KB of calldata on OP).
pub const MAX_L1_PROOF_HEADERS: u64 = 64;
/// `L1BlobVerifier.MAX_AGE`: how far behind the origin a proven
/// transaction's block may be.
pub const L1_MAX_AGE: u64 = 7200;
/// Proof rebuilds after `L1OriginMismatch`, and pre-flight RPC failures,
/// before the batch goes back to the queue.
pub const MAX_PROOF_RETRIES: u32 = 5;
/// How long a mined post may read as unknown before it counts as gone:
/// load-balanced endpoints lag a block or two (12 s each).
pub const GONE_FOR: Duration = Duration::from_secs(36);
/// How long building or anchoring the L1 proof may keep failing before the
/// blobs are posted again (a fresh blob block usually fixes it).
pub const PROOF_FAIL_FOR: Duration = Duration::from_secs(60);
/// Re-posts of the blobs in one settlement before the batch goes back to the
/// queue (each is an L1 blob transaction).
pub const MAX_REPOSTS: u32 = 3;
/// Seal to landing in L1-blob mode cannot beat the Ethereum leg: blob
/// inclusion plus OP's L1 origin lag (~7 blocks) plus the reorg margin,
/// overlapping the proof. Seconds; the default of `--l1-settle-lag`.
pub const L1_SETTLE_LAG: u64 = 110;

/// Where a proof is anchored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Headers from `top` down to the blob block; one of them hashes to `hash`.
    At { top: u64, hash: B256 },
    /// Our L1 RPC is behind OP's origin.
    Wait,
    /// No anchor is close enough: post the blobs again.
    Repost,
}

/// The anchor of a proof for blob block `n` (with `origin.number >= n`):
/// the current origin while that takes at most [`CURRENT_MAX_HEADERS`]
/// headers (up to [`TOP_ABOVE_ORIGIN`] above it, within our L1 head), else a
/// recorded origin at or above `n`, else the current origin up to
/// [`MAX_L1_PROOF_HEADERS`]; deeper, or [`L1_MAX_AGE`] behind, a re-post.
pub fn choose_anchor(n: u64, head: u64, origin: L1Origin, recorded: Option<L1Origin>) -> Anchor {
    let behind = origin.number.saturating_sub(n);
    if behind >= L1_MAX_AGE {
        return Anchor::Repost;
    }
    let current = behind + 1 + TOP_ABOVE_ORIGIN;
    if current > CURRENT_MAX_HEADERS
        && let Some(r) = recorded
        && r.number >= n
        && r.number - n < MAX_L1_PROOF_HEADERS
    {
        return Anchor::At {
            top: r.number,
            hash: r.hash,
        };
    }
    if current > MAX_L1_PROOF_HEADERS {
        return Anchor::Repost;
    }
    if head < origin.number {
        return Anchor::Wait;
    }
    Anchor::At {
        top: head.min(origin.number + TOP_ABOVE_ORIGIN),
        hash: origin.hash,
    }
}

// A revert's name, with the message of a string one.
fn named(name: String, data: &[u8]) -> String {
    match revert_message(data) {
        Some(m) if name == "Error" => format!("Error({m})"),
        _ => name,
    }
}

/// Waits one poll, or returns `Transient("flight aborted")` once the flight is dropped.
macro_rules! pause_or_abort {
    ($da:expr, $job:expr) => {
        if !$da.pause(&$job.cancel).await {
            return Settled::Transient("flight aborted".into());
        }
    };
}

/// Counts a re-post of the blobs; ends the settle past [`MAX_REPOSTS`].
macro_rules! repost {
    ($reposts:ident, $why:expr) => {
        $reposts += 1;
        if $reposts > MAX_REPOSTS {
            return Settled::Transient(format!(
                "the L1 proof stayed too deep after {MAX_REPOSTS} re-posts ({})",
                $why
            ));
        }
    };
}

/// Settles `job`'s proved batch on OP. Waits (within the deadline) for the
/// blob transaction to have [`REORG_MARGIN`] blocks on top and for OP's
/// origin to reach it, then sends the transition with an L1 proof anchored by
/// [`choose_anchor`]. A vanished blob transaction, a too-deep proof, one
/// that fails to build or anchor for [`PROOF_FAIL_FOR`], or `TxTooOld` post
/// the blobs again; `L1OriginMismatch` rebuilds the proof from fresh reads,
/// up to [`MAX_PROOF_RETRIES`] times. Other reverts end the job with their
/// name, as in blob mode.
pub async fn settle(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    snark: &PlonkSnark,
    posting: Posting,
) -> Settled {
    let pid = hex::encode(job.pid);
    let want: Vec<B256> = job
        .blobs
        .versioned_hashes
        .iter()
        .map(|h| B256::from(*h))
        .collect();
    let mut post = match posting.post {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(%pid, error = %e, "L1 post during proving failed; posting again");
            None
        }
    };
    let mut recorded = posting.recorded;
    let mut retries = 0u32;
    let mut reposts = 0u32;
    // Since when the post's receipt reads as unknown.
    let mut gone: Option<Instant> = None;
    // Since when the proof of this post fails to build or anchor.
    let mut proof_failing: Option<Instant> = None;
    loop {
        if job.cancel.is_cancelled() {
            return Settled::Transient("flight aborted".into());
        }
        if Instant::now() >= job.deadline {
            return Settled::Transient("L1 settlement past the batch deadline".into());
        }
        let p = match post {
            Some(p) => p,
            None => match da
                .poster
                .post(&job.pid, &job.root_before, job.blobs, job.deadline)
                .await
            {
                Ok(p) => {
                    post = Some(p);
                    recorded = None;
                    proof_failing = None;
                    p
                }
                Err(e @ (PostError::Deadline | PostError::NoSigner | PostError::Storage(_))) => {
                    return Settled::Transient(e.to_string());
                }
                // Fee cap, L1 trouble: wait and post again within the deadline.
                Err(e) => {
                    warn!(%pid, error = %e, "L1 post failed; retrying");
                    pause_or_abort!(da, job);
                    continue;
                }
            },
        };
        let rc = match da.l1.receipt(p.tx).await {
            Ok(Some(rc)) => {
                gone = None;
                rc
            }
            Ok(None) if gone.is_none_or(|t| t.elapsed() < GONE_FOR) => {
                if gone.is_none() {
                    gone = Some(Instant::now());
                    warn!(%pid, tx = %p.tx, "L1 blob transaction not found; re-checking");
                }
                pause_or_abort!(da, job);
                continue;
            }
            Ok(None) => {
                gone = None;
                repost!(reposts, "the blob transaction is gone");
                warn!(%pid, tx = %p.tx, "L1 blob transaction gone (dropped or reorged out); posting again");
                post = None;
                continue;
            }
            Err(e) => {
                warn!(%pid, error = %e, "L1 receipt read failed");
                pause_or_abort!(da, job);
                continue;
            }
        };
        let n = rc.block;
        let (head, origin) = match (da.l1.head().await, chain.l1_origin().await) {
            (Ok(h), Ok(o)) => (h, o),
            (Err(e), _) | (_, Err(e)) => {
                warn!(%pid, error = %e, "L1 head or OP origin read failed");
                pause_or_abort!(da, job);
                continue;
            }
        };
        if head < n + REORG_MARGIN || origin.number < n {
            pause_or_abort!(da, job);
            continue;
        }
        let (top, anchor) = match choose_anchor(n, head, origin, recorded) {
            Anchor::At { top, hash } => (top, hash),
            Anchor::Wait => {
                pause_or_abort!(da, job);
                continue;
            }
            Anchor::Repost => {
                repost!(reposts, "no anchor close enough");
                warn!(%pid, n, origin = origin.number, "the L1 proof would be too deep; posting the blobs again");
                post = None;
                continue;
            }
        };
        let built = match da.l1.build_proof(&[p.tx], top).await {
            Ok(proof) => match verify_local(&proof, anchor) {
                Ok((vhs, _)) if vhs == want => Ok(proof),
                Ok(_) => {
                    return Settled::Failed("the L1 blob transaction carries other blobs".into());
                }
                Err(e) => Err(format!("the L1 proof does not anchor: {e}")),
            },
            Err(e) => Err(format!("the L1 proof build failed: {e}")),
        };
        let proof = match built {
            Ok(proof) => {
                proof_failing = None;
                proof
            }
            // A lagging or pruning endpoint: retry each poll; past the
            // window a new blob block usually helps.
            Err(e) if proof_failing.is_some_and(|t| t.elapsed() >= PROOF_FAIL_FOR) => {
                repost!(reposts, "the L1 proof could not be built or anchored");
                warn!(%pid, error = %e, "the L1 proof kept failing; posting the blobs again");
                post = None;
                continue;
            }
            Err(e) => {
                proof_failing.get_or_insert_with(Instant::now);
                warn!(%pid, error = %e, "retrying the L1 proof");
                pause_or_abort!(da, job);
                continue;
            }
        };
        let refused = match chain
            .simulate_transition_l1(&job.pid, snark, job.blobs, &proof)
            .await
        {
            Ok(()) => match chain
                .submit_transition_l1(&job.pid, snark, job.blobs, &proof)
                .await
            {
                Ok(rc) => {
                    info!(%pid, l1_block = n, headers = proof.headers.len(), tx = %rc.tx_hash,
                        "L1-blob transition sent");
                    return Settled::Landed(rc);
                }
                Err(Web3Error::Revert(RevertReason::Revert { name, data })) => named(name, &data),
                Err(e @ Web3Error::Lost { .. }) => return Settled::Lost(e.to_string()),
                Err(e) => return Settled::Transient(e.to_string()),
            },
            Err(RevertReason::Revert { name, data }) => named(name, &data),
            Err(RevertReason::Rpc(e)) => {
                retries += 1;
                if retries > MAX_PROOF_RETRIES {
                    return Settled::Transient(e);
                }
                warn!(%pid, error = %e, "pre-flight RPC failure; retrying");
                pause_or_abort!(da, job);
                continue;
            }
        };
        match refused.as_str() {
            // The origin moved past the proof, or our L1 RPC lags it: rebuild.
            "L1OriginMismatch" => {
                retries += 1;
                if retries > MAX_PROOF_RETRIES {
                    return Settled::Transient(format!(
                        "L1OriginMismatch after {MAX_PROOF_RETRIES} rebuilds"
                    ));
                }
                info!(%pid, "OP's origin moved past the proof; rebuilding");
                pause_or_abort!(da, job);
            }
            "TxTooOld" => {
                repost!(reposts, "TxTooOld");
                warn!(%pid, n, "the blob transaction is too old for the verifier; posting again");
                post = None;
            }
            _ => return Settled::Reverted(refused),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(number: u64) -> L1Origin {
        L1Origin {
            number,
            hash: B256::with_last_byte(number as u8),
        }
    }

    #[test]
    fn the_anchor_follows_the_origin_then_the_record() {
        let n = 100;
        // At the origin: two headers above it, capped by our head.
        assert_eq!(
            choose_anchor(n, 101, o(100), None),
            Anchor::At {
                top: 101,
                hash: o(100).hash
            }
        );
        assert_eq!(
            choose_anchor(n, 120, o(110), None),
            Anchor::At {
                top: 112,
                hash: o(110).hash
            }
        );
        // 16 headers from the current origin is the limit (origin + 2 - n + 1)...
        assert_eq!(
            choose_anchor(n, 200, o(113), Some(o(101))),
            Anchor::At {
                top: 115,
                hash: o(113).hash
            }
        );
        // ...past it, the recorded origin anchors: headers 101 down to 100.
        assert_eq!(
            choose_anchor(n, 200, o(114), Some(o(101))),
            Anchor::At {
                top: 101,
                hash: o(101).hash
            }
        );
        // Without a record, up to 64 headers from the current origin...
        assert_eq!(
            choose_anchor(n, 200, o(161), None),
            Anchor::At {
                top: 163,
                hash: o(161).hash
            }
        );
        // ...then the blobs go out again.
        assert_eq!(choose_anchor(n, 200, o(162), None), Anchor::Repost);
        // A record from before the blob block anchors nothing for it.
        assert_eq!(choose_anchor(n, 200, o(162), Some(o(99))), Anchor::Repost);
        // Our L1 RPC behind OP's origin: wait for it.
        assert_eq!(choose_anchor(n, 105, o(110), None), Anchor::Wait);
        // As old as the verifier's MAX_AGE: post again, record or not.
        assert_eq!(
            choose_anchor(n, n + 8000, o(n + L1_MAX_AGE), Some(o(101))),
            Anchor::Repost
        );
    }
}
