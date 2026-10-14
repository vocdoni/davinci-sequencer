//! Settling a proved batch whose blobs are on Ethereum, in one or more blob
//! transactions (chunks): wait for every chunk's block and for OP's origin,
//! anchor the L1 proof where OP accepts it, check it locally, simulate and
//! send. A chunk whose transaction is gone is posted again alone; a proof
//! that would be too deep posts every chunk again.

use std::sync::atomic::Ordering;
use std::time::Duration;

use alloy::primitives::B256;
use davinci_zkvm_sdk::client::PlonkSnark;
use tokio::time::Instant;
use tracing::{info, warn};

use super::{L1Da, L1Job, PostError, Posted, Posting, Settled, chunks};
use crate::actor::Chain;
use crate::web3::l1proof::{L1ProofError, verify_local};
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
/// blobs are posted again (a fresh blob block usually fixes RPC trouble), or
/// the settle ends when no post can fix the failure.
pub const PROOF_FAIL_FOR: Duration = Duration::from_secs(60);
/// Re-posts in one settlement before the batch goes back to the queue: one
/// chunk posted again counts one, every chunk posted again counts one.
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

/// The anchor of a proof whose blob transactions sit in blocks `oldest`
/// to `newest`. Depth counts from `oldest`: the current origin while that
/// takes at most [`CURRENT_MAX_HEADERS`] headers (up to [`TOP_ABOVE_ORIGIN`]
/// above it, within our L1 head), else a recorded origin at or above
/// `newest` (the verifier rejects a transaction above its anchor), else the
/// current origin up to [`MAX_L1_PROOF_HEADERS`]; deeper, or [`L1_MAX_AGE`]
/// behind, a re-post. Waits while OP's origin or our L1 head is below
/// `newest`.
pub fn choose_anchor(
    oldest: u64,
    newest: u64,
    head: u64,
    origin: L1Origin,
    recorded: Option<L1Origin>,
) -> Anchor {
    let behind = origin.number.saturating_sub(oldest);
    if behind >= L1_MAX_AGE {
        return Anchor::Repost;
    }
    let current = behind + 1 + TOP_ABOVE_ORIGIN;
    if current > CURRENT_MAX_HEADERS
        && let Some(r) = recorded
        && r.number >= newest
        && r.number - oldest < MAX_L1_PROOF_HEADERS
    {
        return Anchor::At {
            top: r.number,
            hash: r.hash,
        };
    }
    if current > MAX_L1_PROOF_HEADERS {
        return Anchor::Repost;
    }
    if origin.number < newest || head < origin.number {
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

/// Settles `job`'s proved batch on OP. Posts the chunks [`with_post`]
/// left unposted, then waits (within the deadline) for every chunk's
/// transaction to have [`REORG_MARGIN`] blocks on top and for OP's origin to
/// reach the newest, and sends the transition with one L1 proof of every
/// chunk transaction, in chunk order, anchored by [`choose_anchor`]. A chunk
/// whose transaction vanished is posted again alone; a too-deep proof, one
/// that fails to build (RPC trouble) or anchor for [`PROOF_FAIL_FOR`], or
/// `TxTooOld` posts every chunk again; each counts one of [`MAX_REPOSTS`].
/// A proof that keeps failing otherwise (bad header, broken chain, bad trie
/// proof) ends the settle after [`PROOF_FAIL_FOR`] without a post.
/// `L1OriginMismatch` rebuilds the proof from fresh reads, up to
/// [`MAX_PROOF_RETRIES`] times. Other reverts end the job with their name,
/// as in blob mode.
///
/// [`with_post`]: super::with_post
pub async fn settle(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    snark: &PlonkSnark,
    posting: Posting,
) -> Settled {
    let pid = hex::encode(job.pid);
    let chunks = chunks(job.blobs, da.chunk);
    let want: Vec<B256> = job
        .blobs
        .versioned_hashes
        .iter()
        .map(|h| B256::from(*h))
        .collect();
    if let Some(e) = &posting.error {
        warn!(%pid, error = %e, "L1 post during proving failed; posting again");
    }
    // One entry per chunk, in chunk order; `None` is (re)posted first.
    let mut posts: Vec<Option<Posted>> = (0..chunks.len())
        .map(|i| posting.posts.get(i).copied())
        .collect();
    let mut recorded = posting.recorded;
    let mut retries = 0u32;
    let mut reposts = 0u32;
    // Since when each chunk's receipt reads as unknown.
    let mut gone: Vec<Option<Instant>> = vec![None; chunks.len()];
    // Since when the proof of these posts fails to build or anchor.
    let mut proof_failing: Option<Instant> = None;
    loop {
        if job.cancel.is_cancelled() {
            return Settled::Transient("flight aborted".into());
        }
        if Instant::now() >= job.deadline {
            return Settled::Transient("L1 settlement past the batch deadline".into());
        }
        if let Some(i) = posts.iter().position(Option::is_none) {
            match da
                .poster
                .post(&job.pid, &job.root_before, &chunks[i], job.deadline)
                .await
            {
                Ok(p) => {
                    posts[i] = Some(p);
                    job.posted.store(true, Ordering::Relaxed);
                    // A recorded origin below the new post anchors nothing.
                    recorded = None;
                    proof_failing = None;
                }
                Err(e @ (PostError::Deadline | PostError::NoSigner | PostError::Storage(_))) => {
                    return Settled::Transient(e.to_string());
                }
                // Fee cap, L1 trouble: wait and post again within the deadline.
                Err(e) => {
                    warn!(%pid, chunk = i, error = %e, "L1 post failed; retrying");
                    pause_or_abort!(da, job);
                }
            }
            continue;
        }
        let txs: Vec<Posted> = posts.iter().flatten().copied().collect();
        let mut blocks = Vec::with_capacity(txs.len());
        let mut lost = None;
        for (i, p) in txs.iter().enumerate() {
            match da.l1.receipt(p.tx).await {
                Ok(Some(rc)) => {
                    gone[i] = None;
                    blocks.push(rc.block);
                }
                Ok(None) if gone[i].is_none_or(|t| t.elapsed() < GONE_FOR) => {
                    if gone[i].is_none() {
                        gone[i] = Some(Instant::now());
                        warn!(%pid, chunk = i, tx = %p.tx, "L1 blob transaction not found; re-checking");
                    }
                }
                Ok(None) => {
                    lost = Some(i);
                    break;
                }
                Err(e) => warn!(%pid, error = %e, "L1 receipt read failed"),
            }
        }
        if let Some(i) = lost {
            gone[i] = None;
            repost!(reposts, "a blob transaction is gone");
            warn!(%pid, chunk = i, tx = %txs[i].tx,
                "L1 blob transaction gone (dropped or reorged out); posting its chunk again");
            posts[i] = None;
            continue;
        }
        // A receipt still unknown or unread: look again next poll.
        if blocks.len() < txs.len() {
            pause_or_abort!(da, job);
            continue;
        }
        let (Some(&oldest), Some(&newest)) = (blocks.iter().min(), blocks.iter().max()) else {
            return Settled::Failed("a transition without blobs".into());
        };
        let (head, origin) = match (da.l1.head().await, chain.l1_origin().await) {
            (Ok(h), Ok(o)) => (h, o),
            (Err(e), _) | (_, Err(e)) => {
                warn!(%pid, error = %e, "L1 head or OP origin read failed");
                pause_or_abort!(da, job);
                continue;
            }
        };
        if head < newest + REORG_MARGIN || origin.number < newest {
            pause_or_abort!(da, job);
            continue;
        }
        let (top, anchor) = match choose_anchor(oldest, newest, head, origin, recorded) {
            Anchor::At { top, hash } => (top, hash),
            Anchor::Wait => {
                pause_or_abort!(da, job);
                continue;
            }
            Anchor::Repost => {
                repost!(reposts, "no anchor close enough");
                warn!(%pid, oldest, origin = origin.number,
                    "the L1 proof would be too deep; posting every chunk again");
                posts.fill(None);
                continue;
            }
        };
        let hashes: Vec<B256> = txs.iter().map(|p| p.tx).collect();
        // The error, and whether a fresh post may fix it: endpoint trouble
        // and an anchor that moved may; a header or proof this build cannot
        // reproduce fails the same way on any post.
        let built = match da.l1.build_proof(&hashes, top).await {
            Ok(proof) => match verify_local(&proof, anchor) {
                Ok((vhs, _)) if vhs == want => Ok(proof),
                Ok(_) => {
                    return Settled::Failed("the L1 blob transactions carry other blobs".into());
                }
                Err(e @ (L1ProofError::NoAnchor(_) | L1ProofError::AboveOrigin(_))) => {
                    Err((true, format!("the L1 proof does not anchor: {e}")))
                }
                Err(e) => Err((false, format!("the L1 proof does not verify: {e}"))),
            },
            Err(e @ Web3Error::Rpc(_)) => Err((true, format!("the L1 proof build failed: {e}"))),
            Err(e) => Err((false, format!("the L1 proof build failed: {e}"))),
        };
        let proof = match built {
            Ok(proof) => {
                proof_failing = None;
                proof
            }
            // Retried each poll (a reorg between reads clears); past the
            // window the batch goes back to the queue without a post.
            Err((false, e)) if proof_failing.is_some_and(|t| t.elapsed() >= PROOF_FAIL_FOR) => {
                return Settled::Transient(format!("{e}; no new post can fix it"));
            }
            // A lagging or pruning endpoint: retry each poll; past the
            // window new blob blocks usually help.
            Err((true, e)) if proof_failing.is_some_and(|t| t.elapsed() >= PROOF_FAIL_FOR) => {
                repost!(reposts, "the L1 proof could not be built or anchored");
                warn!(%pid, error = %e, "the L1 proof kept failing; posting every chunk again");
                posts.fill(None);
                continue;
            }
            Err((_, e)) => {
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
                    info!(%pid, l1_block = newest, l1_oldest = oldest, l1_txs = txs.len(),
                        headers = proof.headers.len(), tx = %rc.tx_hash, "L1-blob transition sent");
                    return Settled::Landed(rc);
                }
                Err(Web3Error::Revert(RevertReason::Revert { name, data })) => named(name, &data),
                Err(e @ Web3Error::Lost { .. }) => return Settled::Lost(e.to_string()),
                Err(e @ Web3Error::FeeCap { .. }) => return Settled::Transient(e.to_string()),
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
                warn!(%pid, oldest, "a blob transaction is too old for the verifier; posting every chunk again");
                posts.fill(None);
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
            choose_anchor(n, n, 101, o(100), None),
            Anchor::At {
                top: 101,
                hash: o(100).hash
            }
        );
        assert_eq!(
            choose_anchor(n, n, 120, o(110), None),
            Anchor::At {
                top: 112,
                hash: o(110).hash
            }
        );
        // 16 headers from the current origin is the limit (origin + 2 - n + 1)...
        assert_eq!(
            choose_anchor(n, n, 200, o(113), Some(o(101))),
            Anchor::At {
                top: 115,
                hash: o(113).hash
            }
        );
        // ...past it, the recorded origin anchors: headers 101 down to 100.
        assert_eq!(
            choose_anchor(n, n, 200, o(114), Some(o(101))),
            Anchor::At {
                top: 101,
                hash: o(101).hash
            }
        );
        // Without a record, up to 64 headers from the current origin...
        assert_eq!(
            choose_anchor(n, n, 200, o(161), None),
            Anchor::At {
                top: 163,
                hash: o(161).hash
            }
        );
        // ...then the blobs go out again.
        assert_eq!(choose_anchor(n, n, 200, o(162), None), Anchor::Repost);
        // A record from before the blob block anchors nothing for it.
        assert_eq!(
            choose_anchor(n, n, 200, o(162), Some(o(99))),
            Anchor::Repost
        );
        // Our L1 RPC behind OP's origin: wait for it.
        assert_eq!(choose_anchor(n, n, 105, o(110), None), Anchor::Wait);
        // As old as the verifier's MAX_AGE: post again, record or not.
        assert_eq!(
            choose_anchor(n, n, n + 8000, o(n + L1_MAX_AGE), Some(o(101))),
            Anchor::Repost
        );
    }

    // Chunks in blocks 100 (oldest) and 105 (newest).
    #[test]
    fn depth_counts_from_the_oldest_chunk_and_anchors_cover_the_newest() {
        let (old, new) = (100, 105);
        // OP's origin between the two blocks: wait for it to reach the newest.
        assert_eq!(choose_anchor(old, new, 200, o(103), None), Anchor::Wait);
        // At the newest: headers from origin + 2 down to the oldest.
        assert_eq!(
            choose_anchor(old, new, 200, o(105), None),
            Anchor::At {
                top: 107,
                hash: o(105).hash
            }
        );
        // A record at oldest + 1 < newest must not anchor: the verifier would
        // reject the newest transaction above it. The current origin does.
        assert_eq!(
            choose_anchor(old, new, 200, o(150), Some(o(101))),
            Anchor::At {
                top: 152,
                hash: o(150).hash
            }
        );
        // ...and past the header budget nothing anchors: post again.
        assert_eq!(
            choose_anchor(old, new, 300, o(170), Some(o(101))),
            Anchor::Repost
        );
        // A record at the newest anchors, headers 105 down to 100.
        assert_eq!(
            choose_anchor(old, new, 300, o(170), Some(o(105))),
            Anchor::At {
                top: 105,
                hash: o(105).hash
            }
        );
        // The header budget counts from the oldest chunk: 64 headers from
        // origin + 2 down to block 100 is the most, however recent the newest.
        assert_eq!(
            choose_anchor(old, 140, 300, o(161), None),
            Anchor::At {
                top: 163,
                hash: o(161).hash
            }
        );
        assert_eq!(choose_anchor(old, 140, 300, o(162), None), Anchor::Repost);
        // So does MAX_AGE.
        assert_eq!(
            choose_anchor(
                old,
                old + 50,
                old + 8000,
                o(old + L1_MAX_AGE),
                Some(o(old + 50))
            ),
            Anchor::Repost
        );
    }
}
