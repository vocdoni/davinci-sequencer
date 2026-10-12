//! L1-blob mode (OP Stack registries): a batch's blobs are posted on
//! Ethereum while it is proved, and the settlement on OP carries a proof that
//! the blob transaction is in Ethereum's canonical chain. The actor calls
//! into this module only when `Deps.l1` is set; blob mode never does.

mod post;
mod settle;

use std::sync::Arc;
use std::time::Duration;

use davinci_zkvm_sdk::blob::TransitionBlobs;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::actor::Chain;
use crate::storage::{Db, StorageError};
use crate::web3::{L1Chain, L1Origin, TxReceipt};

pub use post::{PostError, Posted, Poster, REPLACE_AFTER};
pub use settle::{
    Anchor, CURRENT_MAX_HEADERS, GONE_FOR, L1_MAX_AGE, MAX_L1_PROOF_HEADERS, MAX_PROOF_RETRIES,
    MAX_REPOSTS, PROOF_FAIL_FOR, REORG_MARGIN, TOP_ABOVE_ORIGIN, choose_anchor, settle,
};

/// The Ethereum side of a node in L1-blob mode.
pub struct L1Da {
    pub l1: Arc<dyn L1Chain>,
    pub poster: Poster,
    /// Pace of the settle loop and the poster (the chain poll interval).
    pub poll: Duration,
}

impl L1Da {
    /// `fee_cap`: the most one post may cost at the fees it offers, in wei.
    pub fn new(
        l1: Arc<dyn L1Chain>,
        db: Db,
        fee_cap: u128,
        poll: Duration,
    ) -> Result<Self, StorageError> {
        Ok(L1Da {
            poster: Poster::new(l1.clone(), db, fee_cap, poll)?,
            l1,
            poll,
        })
    }
}

impl L1Da {
    /// Waits one poll; false once `cancel` fires.
    pub(crate) async fn pause(&self, cancel: &CancellationToken) -> bool {
        tokio::select! {
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(self.poll) => true,
        }
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
}

/// What [`with_post`] leaves for [`settle`].
#[derive(Debug)]
pub struct Posting {
    pub post: Result<Posted, PostError>,
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

/// Runs `prove` while posting the batch's blobs. Once the post is mined, if
/// OP's origin reaches its block before the proof is ready, records that
/// origin on OP once, so a late proof still anchors close to the blobs.
pub async fn with_post<T>(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    prove: impl Future<Output = T>,
) -> (T, Posting) {
    let post = async {
        loop {
            match da
                .poster
                .post(&job.pid, &job.root_before, job.blobs, job.deadline)
                .await
            {
                // Fee cap, L1 trouble: wait and post again while the proof runs.
                Err(e @ (PostError::FeeCap { .. } | PostError::L1(_))) => {
                    warn!(pid = %hex::encode(job.pid), error = %e, "L1 post failed; retrying");
                    if !da.pause(&job.cancel).await {
                        return Err(e);
                    }
                }
                r => return r,
            }
        }
    };
    tokio::pin!(prove);
    tokio::pin!(post);
    let mut posted: Option<Result<Posted, PostError>> = None;
    let mut recorded = None;
    let mut record_tried = false;
    let proved = loop {
        let block = match &posted {
            Some(Ok(p)) if !record_tried => Some(p.block),
            _ => None,
        };
        tokio::select! {
            out = &mut prove => break out,
            p = &mut post, if posted.is_none() => posted = Some(p),
            _ = tokio::time::sleep(da.poll), if block.is_some() => {
                if let Some(n) = block
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
    let post = match posted {
        Some(p) => p,
        None => post.await,
    };
    (proved, Posting { post, recorded })
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

    use super::*;
    use crate::web3::RegistryDa;

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
