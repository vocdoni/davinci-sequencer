//! The L1 blob poster: one type-3 transaction at a time from the node's
//! dedicated Ethereum account (the blob pool forbids nonce gaps and wants
//! every fee doubled to replace), each recorded before it is sent so a
//! restart replaces it instead of queueing behind it.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::B256;
use davinci_zkvm_sdk::blob::TransitionBlobs;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::storage::{Db, L1PostRecord, StorageError};
use crate::web3::{L1Chain, L1Fees, L1SendError, Web3Error};

/// L1 blocks a post waits before it is replaced at its nonce.
pub const REPLACE_AFTER: u64 = 3;
/// Refused sends (underpriced, nonce trouble) one post tolerates.
const MAX_REFUSALS: u32 = 6;

/// A mined blob transaction of the node's account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Posted {
    pub tx: B256,
    pub nonce: u64,
    pub block: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PostError {
    #[error("no Ethereum key: set --l1-privkey-file or DAVINCI_L1_PRIVKEY")]
    NoSigner,
    #[error("an L1 post would cost up to {want} wei, above the cap of {cap} wei")]
    FeeCap { want: u128, cap: u128 },
    #[error("the L1 post was not mined before the batch deadline")]
    Deadline,
    #[error("L1: {0}")]
    L1(String),
    #[error("storage: {0}")]
    Storage(String),
}

impl From<Web3Error> for PostError {
    fn from(e: Web3Error) -> Self {
        match e {
            Web3Error::NoSigner => PostError::NoSigner,
            e => PostError::L1(e.to_string()),
        }
    }
}

impl From<StorageError> for PostError {
    fn from(e: StorageError) -> Self {
        PostError::Storage(e.to_string())
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Posts blobs from one Ethereum account, one transaction at a time.
pub struct Poster {
    l1: Arc<dyn L1Chain>,
    db: Db,
    fee_cap: u128,
    poll: Duration,
    /// The account's newest post: it prices a replacement at its nonce. Held
    /// for a whole post, so a second post waits until the first is mined.
    // ponytail: a window of one post per account (about 5 posts a minute).
    // More in flight needs a record per nonce and gap repair on drops.
    last: Mutex<Option<L1PostRecord>>,
}

impl Poster {
    /// Picks up the newest record an earlier run left: if that transaction
    /// is still pending, the first post replaces it.
    pub fn new(
        l1: Arc<dyn L1Chain>,
        db: Db,
        fee_cap: u128,
        poll: Duration,
    ) -> Result<Self, StorageError> {
        let me = l1.sender().map(|a| a.0.0);
        let mut mine = Vec::new();
        for r in db.l1_posts()? {
            if Some(r.from) == me {
                mine.push(r);
            } else {
                warn!(nonce = r.nonce, tx = %r.tx_hash,
                    "dropping an L1 post record of another sender (the L1 key changed)");
                db.remove_l1_post(r.nonce)?;
            }
        }
        let last = mine.into_iter().max_by_key(|r| r.nonce);
        if let Some(r) = &last {
            info!(nonce = r.nonce, tx = %r.tx_hash,
                "L1 post of an earlier run on record; the next post replaces it if still pending");
        }
        Ok(Poster {
            l1,
            db,
            fee_cap,
            poll,
            last: Mutex::new(last),
        })
    }

    fn within_cap(&self, fees: &L1Fees, n: usize) -> Result<(), PostError> {
        let want = fees.cost(n);
        if want > self.fee_cap {
            return Err(PostError::FeeCap {
                want,
                cap: self.fee_cap,
            });
        }
        Ok(())
    }

    /// Posts `blobs` in one blob transaction at the account's next nonce and
    /// waits until it is mined. A transaction of ours already at that nonce
    /// (a replaced post, an earlier run's) is outbid with every fee doubled;
    /// one not mined after [`REPLACE_AFTER`] blocks is replaced the same way
    /// while that stays within the fee cap. `deadline` bounds the wait for
    /// the account too.
    pub async fn post(
        &self,
        pid: &[u8; 31],
        root_before: &[u8; 32],
        blobs: &TransitionBlobs,
        deadline: Instant,
    ) -> Result<Posted, PostError> {
        let mut last = tokio::time::timeout_at(deadline, self.last.lock())
            .await
            .map_err(|_| PostError::Deadline)?;
        let Some(sender) = self.l1.sender() else {
            return Err(PostError::NoSigner);
        };
        let vhs: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let n = vhs.len();
        let mut refusals = 0u32;
        let mut sidecar_retried = false;
        'nonce: loop {
            let nonce = self.l1.nonce().await?;
            if nonce > 0 && last.as_ref().is_some_and(|r| r.nonce < nonce) {
                // Mined (or replaced by a tx of ours): nothing left to replace.
                self.db.remove_l1_posts_upto(nonce - 1)?;
            }
            let mut fees = L1Fees::offer(&self.l1.fees().await?);
            if let Some(r) = last.as_ref().filter(|r| r.nonce == nonce) {
                // A tx of ours may hold this nonce: outbid it.
                fees = fees.max_each(r.fees.doubled());
            }
            self.within_cap(&fees, n)?;
            let mut hashes: Vec<B256> = Vec::new();
            'send: loop {
                if Instant::now() >= deadline {
                    return Err(PostError::Deadline);
                }
                let tx = self.l1.sign_blob_tx(nonce, &fees, blobs).await?;
                // Recorded before the send: a crash right after it still knows the tx.
                let rec = L1PostRecord {
                    pid: *pid,
                    root_before: *root_before,
                    nonce,
                    tx_hash: tx.hash,
                    fees,
                    vhs: vhs.clone(),
                    posted_at: unix_now(),
                    from: sender.0.0,
                };
                self.db.put_l1_post(&rec)?;
                *last = Some(rec);
                match self.l1.send_blob_tx(&tx).await {
                    Ok(()) => hashes.push(tx.hash),
                    Err(L1SendError::Unanswered(e)) => {
                        warn!(nonce, error = %e, "L1 send unanswered; waiting for its receipt");
                        hashes.push(tx.hash);
                    }
                    Err(L1SendError::Sidecar(e)) if !sidecar_retried => {
                        sidecar_retried = true;
                        warn!(error = %e, "the L1 node refused the sidecar version; re-signing with the other");
                        continue 'send;
                    }
                    Err(L1SendError::Underpriced(e)) => {
                        refusals += 1;
                        if refusals > MAX_REFUSALS {
                            return Err(PostError::L1(e));
                        }
                        // Something of ours holds the nonce: double until it is outbid.
                        fees = fees.doubled();
                        self.within_cap(&fees, n)?;
                        continue 'send;
                    }
                    Err(L1SendError::NonceTaken(e) | L1SendError::Gap(e)) => {
                        refusals += 1;
                        if refusals > MAX_REFUSALS {
                            return Err(PostError::L1(e));
                        }
                        if hashes.is_empty() {
                            tokio::time::sleep(self.poll).await;
                            continue 'nonce;
                        }
                        // A send of ours at this nonce may be the one mined: wait for it.
                    }
                    Err(e @ (L1SendError::Sidecar(_) | L1SendError::Refused(_))) => {
                        return Err(PostError::L1(e.to_string()));
                    }
                }
                let mut since: Option<u64> = None;
                let mut taken = false;
                loop {
                    for h in &hashes {
                        match self.l1.receipt(*h).await {
                            Ok(Some(r)) => {
                                self.db.remove_l1_posts_upto(nonce)?;
                                if let Some(rec) = last.as_mut() {
                                    rec.tx_hash = *h;
                                }
                                info!(nonce, tx = %h, block = r.block, blobs = n, "blobs posted on L1");
                                return Ok(Posted {
                                    tx: *h,
                                    nonce,
                                    block: r.block,
                                });
                            }
                            Ok(None) => {}
                            // A failed read says nothing about a tx already out.
                            Err(e) => debug!(error = %e, "L1 receipt read failed; still waiting"),
                        }
                    }
                    if taken {
                        // Re-checked: none of ours is mined at this nonce.
                        refusals += 1;
                        if refusals > MAX_REFUSALS {
                            return Err(PostError::L1(format!(
                                "nonce {nonce} taken by an unknown transaction"
                            )));
                        }
                        warn!(
                            nonce,
                            "L1 nonce taken by a transaction this post did not send"
                        );
                        tokio::time::sleep(self.poll).await;
                        continue 'nonce;
                    }
                    if Instant::now() >= deadline {
                        return Err(PostError::Deadline);
                    }
                    if let Ok(head) = self.l1.head().await {
                        let start = *since.get_or_insert(head);
                        if head >= start + REPLACE_AFTER
                            && let (Ok(count), Ok(market)) =
                                (self.l1.nonce().await, self.l1.fees().await)
                        {
                            if count > nonce {
                                // Ours may have been mined since the receipt pass: look again first.
                                taken = true;
                                continue;
                            }
                            let up = fees.doubled().max_each(L1Fees::offer(&market));
                            if up.cost(n) <= self.fee_cap {
                                warn!(
                                    nonce,
                                    blocks = REPLACE_AFTER,
                                    "L1 post not mined; replacing it with doubled fees"
                                );
                                fees = up;
                                continue 'send;
                            }
                            warn!(
                                nonce,
                                cap = self.fee_cap,
                                "an L1 replacement would pass the fee cap; waiting at the current price"
                            );
                            since = Some(head);
                        }
                    }
                    tokio::time::sleep(self.poll).await;
                }
            }
        }
    }
}
