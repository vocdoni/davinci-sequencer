//! Settlement transactions: the blob-carrying `submitStateTransition`,
//! `setProcessResults`, the DKG results calls, the pre-flight `eth_call`,
//! and revert naming.

use alloy::consensus::{BlobTransactionSidecar, BlobTransactionSidecarEip7594};
use alloy::eips::eip4844::Blob as AlloyBlob;
use alloy::eips::eip4844::env_settings::EnvKzgSettings;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::network::{NetworkTransactionBuilder, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{B256, Bytes, FixedBytes};
use alloy::providers::Provider;
use alloy::rpc::types::{Log, TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolCall;
use alloy::transports::{RpcError, TransportErrorKind};
use davinci_zkvm_sdk::blob::TransitionBlobs;
use davinci_zkvm_sdk::client::PlonkSnark;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::ProcessRegistry as PR;
use super::contracts::Contracts;
use super::{Result, RevertReason, TxReceipt, Web3Error, rpc_err};

const ERROR_STRING: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
const PANIC: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];
/// Gas limit headroom over the estimate, in percent.
const GAS_MARGIN: u64 = 20;

fn transition_call(pid: &[u8; 31], snark: &PlonkSnark, blobs: &TransitionBlobs) -> Bytes {
    PR::submitStateTransitionCall {
        processId: FixedBytes(*pid),
        publicValues: snark.public_values.clone().into(),
        proofBytes: snark.proof_bytes.clone().into(),
        commitments: blobs
            .commitments
            .iter()
            .map(|c| Bytes::copy_from_slice(c))
            .collect(),
        ys: blobs.ys.iter().map(|y| FixedBytes(*y)).collect(),
        kzgProofs: blobs
            .proofs
            .iter()
            .map(|p| Bytes::copy_from_slice(p))
            .collect(),
    }
    .abi_encode()
    .into()
}

fn versioned_hashes(blobs: &TransitionBlobs) -> Vec<B256> {
    blobs
        .versioned_hashes
        .iter()
        .map(|h| B256::from(*h))
        .collect()
}

fn check_shape(blobs: &TransitionBlobs) -> Result<()> {
    let n = blobs.blobs.len();
    if n == 0
        || blobs.commitments.len() != n
        || blobs.ys.len() != n
        || blobs.proofs.len() != n
        || blobs.versioned_hashes.len() != n
    {
        return Err(Web3Error::Blob(
            "inconsistent transition blob arrays".into(),
        ));
    }
    Ok(())
}

/// v1 (cell proofs) or v0 sidecar over the transition's blobs, checked
/// against the transition's commitments.
fn build_sidecar(
    cell_proofs: bool,
    blobs: &TransitionBlobs,
) -> Result<BlobTransactionSidecarVariant> {
    let raw: Vec<AlloyBlob> = blobs
        .blobs
        .iter()
        .map(|b| AlloyBlob::from_slice(&b[..]))
        .collect();
    let kzg = |e: alloy::eips::eip4844::c_kzg::Error| Web3Error::Blob(format!("kzg: {e}"));
    let settings = EnvKzgSettings::Default.get();
    let sc = if cell_proofs {
        BlobTransactionSidecarVariant::Eip7594(
            BlobTransactionSidecarEip7594::try_from_blobs_with_settings(raw, settings)
                .map_err(kzg)?,
        )
    } else {
        BlobTransactionSidecarVariant::Eip4844(
            BlobTransactionSidecar::try_from_blobs_with_settings(raw, settings).map_err(kzg)?,
        )
    };
    let same = sc.commitments().len() == blobs.commitments.len()
        && sc
            .commitments()
            .iter()
            .zip(&blobs.commitments)
            .all(|(a, b)| a.0 == *b);
    if !same {
        return Err(Web3Error::Blob(
            "sidecar commitments differ from the transition's".into(),
        ));
    }
    Ok(sc)
}

/// Sends made before a replacement round gives up.
const MAX_ATTEMPTS: u32 = 4;

/// The sender's memory across sends: a tx of ours that never got mined and
/// sits at `nonce` with these fees. The next send replaces it.
#[derive(Clone, Debug, Default)]
pub(crate) struct SendState {
    stuck: Option<Fees>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fees {
    nonce: u64,
    max_fee: u128,
    priority: u128,
    blob_fee: u128,
}

// Replacement pricing: +12.5% (+1 wei) on the execution fees, x2 on the blob
// fee, and never below what the node asks now.
fn bumped(old: &Fees, now_max: u128, now_prio: u128, now_blob: u128) -> Fees {
    let up = |v: u128| v.saturating_add(v.div_ceil(8)).saturating_add(1);
    Fees {
        nonce: old.nonce,
        max_fee: up(old.max_fee).max(now_max),
        priority: up(old.priority).max(now_prio),
        blob_fee: old.blob_fee.saturating_mul(2).max(1).max(now_blob),
    }
}

/// Whether a node refused a blob tx for its sidecar version (v0 vs v1).
pub(crate) fn sidecar_version_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "sidecar",
        "cell proof",
        "eip-7594",
        "eip7594",
        "blob version",
        "wrapper version",
    ]
    .iter()
    .any(|p| m.contains(p))
}

/// A send refused because this nonce is taken: by an earlier send of ours, or
/// by this very tx (geth "already known", anvil/reth "already imported",
/// Nethermind "AlreadyKnown", Besu "Known transaction").
fn raced(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "nonce too low",
        "already known",
        "already imported",
        "alreadyknown",
        "known transaction",
    ]
    .iter()
    .any(|p| m.contains(p))
}

fn underpriced(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("underpriced") || m.contains("fee too low") || m.contains("fee cap")
}

impl Contracts {
    /// Names a revert from the registry and verifier ABIs, `Error`/`Panic`, or `unknown`.
    pub fn revert_name(&self, data: &[u8]) -> String {
        let Some(sel) = data.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
            return "unknown".into();
        };
        match sel {
            ERROR_STRING => "Error".into(),
            PANIC => "Panic".into(),
            _ => self
                .errors
                .get(&sel)
                .cloned()
                .unwrap_or_else(|| "unknown".into()),
        }
    }

    pub(super) fn reason(&self, e: &RpcError<TransportErrorKind>) -> RevertReason {
        match e.as_error_resp().and_then(|p| p.as_revert_data()) {
            Some(data) => RevertReason::Revert {
                name: self.revert_name(&data),
                data,
            },
            None => RevertReason::Rpc(e.to_string()),
        }
    }

    fn classify(&self, e: &RpcError<TransportErrorKind>) -> Web3Error {
        match self.reason(e) {
            r @ RevertReason::Revert { .. } => Web3Error::Revert(r),
            RevertReason::Rpc(s) if underpriced(&s) => Web3Error::Underpriced(s),
            RevertReason::Rpc(s) => Web3Error::Rpc(s),
        }
    }

    // KZG cell proofs take a while: off the async workers.
    async fn sidecar(
        &self,
        blobs: &TransitionBlobs,
        v1: bool,
    ) -> Result<BlobTransactionSidecarVariant> {
        let blobs = blobs.clone();
        tokio::task::spawn_blocking(move || build_sidecar(v1, &blobs))
            .await
            .map_err(|e| Web3Error::Blob(format!("sidecar task: {e}")))?
    }

    /// `eth_call` of the settlement with the blob hashes attached, so
    /// `blobhash(i)` resolves. Returns the revert name on failure.
    pub async fn simulate_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> std::result::Result<(), RevertReason> {
        check_shape(blobs).map_err(|e| RevertReason::Rpc(e.to_string()))?;
        let mut tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(transition_call(pid, snark, blobs));
        tx.blob_versioned_hashes = Some(versioned_hashes(blobs));
        if let Some(from) = self.signer {
            tx = tx.with_from(from);
        }
        self.provider
            .call(tx)
            .block(alloy::eips::BlockId::latest())
            .await
            .map(|_| ())
            .map_err(|e| self.reason(&e))
    }

    /// Sends the blob transaction and waits for a successful receipt.
    pub async fn submit_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<TxReceipt> {
        if self.signer.is_none() {
            return Err(Web3Error::NoSigner);
        }
        check_shape(blobs)?;
        let mut tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(transition_call(pid, snark, blobs));
        tx.blob_versioned_hashes = Some(versioned_hashes(blobs));
        Ok(self.send(tx, Some(blobs)).await?.0)
    }

    /// `setProcessResults(pid, publicValues, proofBytes)`.
    pub async fn submit_results(&self, pid: &[u8; 31], snark: &PlonkSnark) -> Result<TxReceipt> {
        let call = PR::setProcessResultsCall {
            processId: FixedBytes(*pid),
            publicValues: snark.public_values.clone().into(),
            proofBytes: snark.proof_bytes.clone().into(),
        };
        let tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(call.abi_encode());
        Ok(self.send(tx, None).await?.0)
    }

    /// `requestResultsDecryption(pid, accumulator, siblings)`: the final
    /// accumulator (64 BE coordinates, leaf-hash order) and its key-0x04
    /// siblings (root to leaf, zero-padded). The gas estimate is the
    /// pre-flight: a revert comes back named.
    pub async fn request_results_decryption(
        &self,
        pid: &[u8; 31],
        accumulator: &[[u8; 32]; 64],
        siblings: &[[u8; 32]],
    ) -> Result<TxReceipt> {
        let call = PR::requestResultsDecryptionCall {
            processId: FixedBytes(*pid),
            accumulator: accumulator.map(alloy::primitives::U256::from_be_bytes),
            siblings: siblings.iter().map(|s| FixedBytes(*s)).collect(),
        };
        let tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(call.abi_encode());
        Ok(self.send(tx, None).await?.0)
    }

    /// `finalizeResultsFromDKG(pid)`; reverts `ResultsNotReady` until every
    /// submitted ciphertext is combined.
    pub async fn finalize_results_from_dkg(&self, pid: &[u8; 31]) -> Result<TxReceipt> {
        let call = PR::finalizeResultsFromDKGCall {
            processId: FixedBytes(*pid),
        };
        let tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(call.abi_encode());
        Ok(self.send(tx, None).await?.0)
    }

    // Longest a send queues behind others before giving up with `Busy`.
    fn busy_timeout(&self) -> Duration {
        self.receipt_timeout
            .saturating_mul(MAX_ATTEMPTS + 1)
            .saturating_add(Duration::from_secs(30))
    }

    // Polls the receipts of every hash sent at this nonce until one shows up.
    async fn wait_any(
        &self,
        hashes: &[B256],
        timeout: Duration,
    ) -> Result<Option<TransactionReceipt>> {
        let deadline = tokio::time::Instant::now() + timeout;
        // 100 ms, doubling up to 2 s: fast on dev chains, light on real ones.
        let mut poll = Duration::from_millis(100);
        loop {
            for h in hashes {
                if let Some(r) = self
                    .provider
                    .get_transaction_receipt(*h)
                    .await
                    .map_err(rpc_err)?
                {
                    return Ok(Some(r));
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(poll.min(deadline - now)).await;
            poll = (poll * 2).min(Duration::from_secs(2));
        }
    }

    async fn fees_now(&self, blob: bool) -> Result<(u128, u128, u128)> {
        let f = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(rpc_err)?;
        let b = if blob {
            self.blob_base_fee().await?.saturating_mul(2).max(1)
        } else {
            0
        };
        Ok((f.max_fee_per_gas, f.max_priority_fee_per_gas, b))
    }

    async fn blob_base_fee(&self) -> Result<u128> {
        blob_base_fee(&self.provider).await
    }

    /// The node's only way to send: one tx at a time from its key. Fills
    /// nonce (the mined count, so a tx of ours stuck in the pool gets
    /// replaced), gas and fees; on a receipt timeout resends at the same nonce
    /// with bumped fees, up to `MAX_ATTEMPTS` times. A refused sidecar version
    /// is retried once with the other one.
    pub(super) async fn send(
        &self,
        tx: TransactionRequest,
        blobs: Option<&TransitionBlobs>,
    ) -> Result<(TxReceipt, Vec<Log>)> {
        let (Some(from), Some(wallet)) = (self.signer, self.wallet.as_ref()) else {
            return Err(Web3Error::NoSigner);
        };
        let mut state = tokio::time::timeout(self.busy_timeout(), self.send_lock.lock())
            .await
            .map_err(|_| Web3Error::Busy)?;
        let p = &self.provider;
        let mut sidecar = match blobs {
            Some(b) => Some(self.sidecar(b, self.cell_proofs()).await?),
            None => None,
        };
        let nonce = p
            .get_transaction_count(from)
            .latest()
            .await
            .map_err(rpc_err)?;
        if state.stuck.is_some_and(|s| s.nonce != nonce) {
            state.stuck = None;
        }
        let (max_fee, priority, blob_fee) = self.fees_now(blobs.is_some()).await?;
        let mut fees = Fees {
            nonce,
            max_fee,
            priority,
            blob_fee,
        };
        if let Some(old) = &state.stuck {
            fees = bumped(old, max_fee, priority, blob_fee);
        }
        let mut tx = tx.with_from(from).with_chain_id(self.chain_id);
        tx.set_nonce(nonce);
        if blobs.is_some() {
            tx.set_max_fee_per_blob_gas(fees.blob_fee);
        }
        let gas = p
            .estimate_gas(tx.clone())
            .block(alloy::eips::BlockId::latest())
            .await
            .map_err(|e| self.classify(&e))?;
        tx.set_gas_limit(gas.saturating_add(gas / 100 * GAS_MARGIN));

        let mut hashes: Vec<B256> = Vec::new();
        let mut version_retried = false;
        let mut v1 = self.cell_proofs();
        let mut attempt = 0;
        while attempt < MAX_ATTEMPTS {
            attempt += 1;
            let mut req = tx.clone();
            req.set_max_fee_per_gas(fees.max_fee);
            req.set_max_priority_fee_per_gas(fees.priority);
            if blobs.is_some() {
                req.set_max_fee_per_blob_gas(fees.blob_fee);
            }
            if let Some(sc) = &sidecar {
                req.set_blob_sidecar(sc.clone());
            }
            // Signed here so the hash is known even if the send errors.
            let env = req
                .build(wallet)
                .await
                .map_err(|e| Web3Error::Rpc(format!("signing: {e}")))?;
            let hash = *env.tx_hash();
            match p.send_tx_envelope(env).await {
                Ok(_) => {
                    hashes.push(hash);
                    // A tx of ours now holds this nonce at this price, whatever
                    // happens below (fees only go up between attempts).
                    state.stuck = Some(fees);
                    if version_retried {
                        // The other version was accepted: use it from now on.
                        self.cell_proofs.store(v1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if let Some(b) =
                        blobs.filter(|_| !version_retried && sidecar_version_error(&msg))
                    {
                        version_retried = true;
                        v1 = !v1;
                        tracing::warn!(error = %msg, v1, "sidecar version refused, retrying with the other");
                        sidecar = Some(self.sidecar(b, v1).await?);
                        attempt -= 1;
                        continue;
                    }
                    if underpriced(&msg) {
                        if attempt == MAX_ATTEMPTS {
                            // Something of ours holds this nonce at least this price.
                            state.stuck = Some(fees);
                            return Err(Web3Error::Underpriced(msg));
                        }
                        let (m, pr, bl) = self.fees_now(blobs.is_some()).await?;
                        fees = bumped(&fees, m, pr, bl);
                        continue;
                    }
                    let raced = raced(&msg);
                    // After an RPC failover the send may have reached the
                    // failed endpoint too: this very tx may be pooled or mined.
                    let ours =
                        raced && matches!(p.get_transaction_by_hash(hash).await, Ok(Some(_)));
                    if ours {
                        hashes.push(hash);
                        state.stuck = Some(fees);
                    } else if !raced || hashes.is_empty() {
                        return Err(self.classify(&e));
                    }
                    // A send of ours at this nonce is in the pool or mined: keep waiting on it.
                }
            }
            if let Some(r) = self.wait_any(&hashes, self.receipt_timeout).await? {
                state.stuck = None;
                drop(state);
                return self.finish(r, tx, hashes.len()).await;
            }
            tracing::warn!(nonce, attempt, "no receipt yet, replacing with higher fees");
            let (m, pr, bl) = self.fees_now(blobs.is_some()).await?;
            fees = bumped(&fees, m, pr, bl);
        }
        // `state.stuck` keeps the last price sent, so the next send replaces it.
        Err(Web3Error::Stuck {
            nonce,
            attempts: MAX_ATTEMPTS,
        })
    }

    async fn finish(
        &self,
        receipt: TransactionReceipt,
        mut tx: TransactionRequest,
        sent: usize,
    ) -> Result<(TxReceipt, Vec<Log>)> {
        let tx_hash = receipt.transaction_hash;
        let block = receipt
            .block_number
            .ok_or_else(|| Web3Error::Data("receipt without a block".into()))?;
        if !receipt.status() {
            tx.nonce = None;
            let e = self.mined_revert(tx, tx_hash, block).await;
            // Callers log the verdict: a lost race is routine, not a warning.
            tracing::info!(%tx_hash, block, error = %e, "transaction reverted");
            return Err(e);
        }
        let logs = receipt.inner.logs().to_vec();
        Ok((
            TxReceipt {
                tx_hash,
                block,
                gas_used: receipt.gas_used,
                replacements: sent.saturating_sub(1) as u32,
            },
            logs,
        ))
    }

    /// Names a mined revert by replaying the call on the state its block
    /// left, where the transactions that beat it are applied. An RPC that
    /// cannot serve that block (a lagging backend, pruned state) replays
    /// at `latest` instead.
    async fn mined_revert(&self, tx: TransactionRequest, tx_hash: B256, block: u64) -> Web3Error {
        let replay = async |at: alloy::eips::BlockId| {
            self.provider
                .call(tx.clone())
                .block(at)
                .await
                .map(|_| ())
                .map_err(|e| self.reason(&e))
        };
        let mut res = replay(alloy::eips::BlockId::number(block)).await;
        if let Err(RevertReason::Rpc(e)) = &res {
            tracing::debug!(%tx_hash, block, error = %e, "replay at the receipt's block failed, trying latest");
            res = replay(alloy::eips::BlockId::latest()).await;
        }
        replay_verdict(res, tx_hash, block)
    }
}

/// What the replay of a mined revert says: the revert it names, else
/// `Lost`. A replay that passes can only mean a state change it does not
/// see (a lagging endpoint still before the winner's transaction); one no
/// endpoint can run names nothing either. Callers retry both; a revert that
/// holds shows up named in the next pre-flight.
fn replay_verdict(
    res: std::result::Result<(), RevertReason>,
    tx_hash: B256,
    block: u64,
) -> Web3Error {
    match res {
        Err(r @ RevertReason::Revert { .. }) => Web3Error::Revert(r),
        Ok(()) => Web3Error::Lost { tx_hash, block },
        Err(RevertReason::Rpc(e)) => {
            tracing::debug!(%tx_hash, block, error = %e, "replay of a mined revert failed");
            Web3Error::Lost { tx_hash, block }
        }
    }
}

/// `eth_blobBaseFee`, or the next block's blob fee from `eth_feeHistory`
/// on RPCs without the former (rpc.gnosischain.com answers -32601).
async fn blob_base_fee<P: Provider>(p: &P) -> Result<u128> {
    let direct = match p.get_blob_base_fee().await {
        Ok(fee) => return Ok(fee),
        Err(e) => e,
    };
    let history = p
        .get_fee_history(1, alloy::eips::BlockNumberOrTag::Latest, &[])
        .await
        .map_err(|e| Web3Error::Rpc(format!("eth_blobBaseFee: {direct}; eth_feeHistory: {e}")))?;
    history
        .next_block_blob_base_fee()
        .ok_or_else(|| rpc_err(direct))
}

#[cfg(test)]
mod tests {
    use alloy::eips::eip7594::CELLS_PER_EXT_BLOB;
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::blob::{TransitionData, build_blobs};
    use davinci_zkvm_sdk::crypto::field::Fr;
    use davinci_zkvm_sdk::limits::VOTE_ID_MIN;

    use super::*;

    fn two_blobs() -> TransitionBlobs {
        let t = TransitionData {
            vote_ids: (0..500).map(|i| VOTE_ID_MIN + i).collect(),
            updates: (0..500).map(|i| (0x10 + i, Ballot::identity())).collect(),
            accumulator: Ballot::identity(),
            num_fields: 4,
        };
        build_blobs(&t, &Fr::from(3u64), &[4u8; 32]).unwrap()
    }

    #[test]
    fn sidecar_version_follows_the_fork() {
        let b = two_blobs();
        match build_sidecar(true, &b).unwrap() {
            BlobTransactionSidecarVariant::Eip7594(s) => {
                assert_eq!(s.blobs.len(), 2);
                assert_eq!(s.cell_proofs.len(), 2 * CELLS_PER_EXT_BLOB);
                s.validate(
                    &b.versioned_hashes
                        .iter()
                        .map(|h| B256::from(*h))
                        .collect::<Vec<_>>(),
                    EnvKzgSettings::Default.get(),
                )
                .unwrap();
            }
            _ => panic!("want a v1 sidecar"),
        }
        match build_sidecar(false, &b).unwrap() {
            BlobTransactionSidecarVariant::Eip4844(s) => {
                assert_eq!(s.proofs.len(), 2);
                let vh: Vec<B256> = s.versioned_hashes().collect();
                assert_eq!(
                    vh,
                    b.versioned_hashes
                        .iter()
                        .map(|h| B256::from(*h))
                        .collect::<Vec<_>>()
                );
            }
            _ => panic!("want a v0 sidecar"),
        }
    }

    #[test]
    fn sidecar_must_match_the_transition() {
        let mut b = two_blobs();
        b.commitments.swap(0, 1);
        assert!(build_sidecar(true, &b).is_err());
        let mut b = two_blobs();
        b.commitments.pop();
        assert!(build_sidecar(false, &b).is_err());
        let mut b = two_blobs();
        b.blobs[1][40] ^= 1;
        assert!(build_sidecar(true, &b).is_err());
    }

    #[test]
    fn replacement_fees_go_up_enough() {
        let old = Fees {
            nonce: 7,
            max_fee: 1_000,
            priority: 80,
            blob_fee: 3,
        };
        let b = bumped(&old, 0, 0, 0);
        assert_eq!(b.nonce, 7);
        // At least +12.5% (nodes want +10%) and x2 on blobs.
        assert!(b.max_fee * 8 >= old.max_fee * 9 && b.max_fee > old.max_fee);
        assert!(b.priority * 8 >= old.priority * 9 && b.priority > old.priority);
        assert!(b.blob_fee >= 2 * old.blob_fee);
        // Never below the current price, and progress from zero.
        let b = bumped(&old, 5_000, 900, 40);
        assert_eq!((b.max_fee, b.priority, b.blob_fee), (5_000, 900, 40));
        let zero = Fees {
            nonce: 0,
            max_fee: 0,
            priority: 0,
            blob_fee: 0,
        };
        let b = bumped(&zero, 0, 0, 0);
        assert!(b.max_fee > 0 && b.priority > 0 && b.blob_fee > 0);
        let max = Fees {
            nonce: 0,
            max_fee: u128::MAX,
            priority: u128::MAX,
            blob_fee: u128::MAX,
        };
        assert_eq!(bumped(&max, 0, 0, 0).max_fee, u128::MAX);
    }

    #[test]
    fn send_error_classes() {
        assert!(sidecar_version_error(
            "unexpected blob sidecar version, want EIP-7594 cell proofs"
        ));
        assert!(sidecar_version_error(
            "eip4844 blob tx wrapper version not supported, use eip7594"
        ));
        assert!(!sidecar_version_error("nonce too low"));
        assert!(underpriced("replacement transaction underpriced"));
        assert!(underpriced(
            "max fee per gas less than block base fee: fee cap too low"
        ));
        assert!(!underpriced("execution reverted"));
    }

    #[test]
    fn inconsistent_arrays_are_refused() {
        let mut b = two_blobs();
        b.ys.pop();
        assert!(check_shape(&b).is_err());
        let mut b = two_blobs();
        b.blobs.clear();
        b.commitments.clear();
        b.ys.clear();
        b.proofs.clear();
        b.versioned_hashes.clear();
        assert!(check_shape(&b).is_err());
        assert!(check_shape(&two_blobs()).is_ok());
    }

    // A stub RPC without eth_blobBaseFee, like rpc.gnosischain.com.
    async fn fee_stub(has_blob_base_fee: bool) -> url::Url {
        use axum::{Json, Router, routing::post};
        let app = Router::new().route(
            "/",
            post(move |Json(req): Json<serde_json::Value>| async move {
                let id = req["id"].clone();
                Json(match req["method"].as_str() {
                    Some("eth_blobBaseFee") if has_blob_base_fee => {
                        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": "0x3"})
                    }
                    Some("eth_feeHistory") => serde_json::json!({"jsonrpc": "2.0", "id": id,
                        "result": {"oldestBlock": "0x10", "baseFeePerGas": ["0x1", "0x1"],
                            "gasUsedRatio": [0.0], "baseFeePerBlobGas": ["0x3b9aca00", "0x77359400"],
                            "blobGasUsedRatio": [0.0]}}),
                    _ => serde_json::json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}}),
                })
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}").parse().unwrap()
    }

    /// How the replay stub answers an `eth_call`.
    #[derive(Clone, Copy)]
    enum Replay {
        Pass,
        Revert([u8; 4]),
        /// A backend without the block, like a lagging load-balanced one.
        NoBlock,
    }

    // An RPC answering `eth_call` at block 0x10 with `at_block` and at
    // "latest" with `at_latest`; records the block tags it was asked for.
    async fn replay_stub(
        at_block: Replay,
        at_latest: Replay,
    ) -> (url::Url, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::{Json, Router, routing::post};
        let tags = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = tags.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(req): Json<serde_json::Value>| async move {
                let id = req["id"].clone();
                let tag = req["params"][1].as_str().unwrap_or_default().to_string();
                seen.lock().unwrap().push(tag.clone());
                let answer = if tag == "latest" { at_latest } else { at_block };
                Json(match answer {
                    Replay::Pass => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": "0x"}),
                    Replay::Revert(sel) => serde_json::json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": 3, "message": "execution reverted",
                            "data": format!("0x{}", hex::encode(sel))}}),
                    Replay::NoBlock => serde_json::json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32000, "message": "header not found"}}),
                })
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}").parse().unwrap(), tags)
    }

    // A read-only registry handle over `url`, without the boot checks.
    fn contracts_at(url: url::Url) -> Contracts {
        let (provider, epoch) = crate::web3::failover_provider(&[url]);
        Contracts {
            provider,
            epoch,
            registry: alloy::primitives::Address::repeat_byte(1),
            signer: None,
            wallet: None,
            chain_id: 1,
            cell_proofs: Default::default(),
            blob_cap: None,
            errors: std::sync::Arc::new(crate::web3::contracts::error_names().unwrap()),
            send_lock: Default::default(),
            receipt_timeout: Duration::from_secs(1),
            dkg_adapter: Default::default(),
        }
    }

    // A mined revert is named from the state its block left. Replaying at
    // `latest` on an endpoint that lags behind that block passes, which once
    // errored the votes of a lost race as "unknown".
    #[tokio::test]
    async fn mined_revert_is_replayed_at_its_block() {
        use alloy::sol_types::SolError;
        let root = PR::InvalidStateRoot::SELECTOR;
        let status = PR::InvalidStatus::SELECTOR;
        let hash = B256::repeat_byte(7);
        let verdict = async |at_block, at_latest| {
            let (url, tags) = replay_stub(at_block, at_latest).await;
            let e = contracts_at(url)
                .mined_revert(TransactionRequest::default(), hash, 0x10)
                .await;
            (e, tags.lock().unwrap().clone())
        };

        let (e, tags) = verdict(Replay::Revert(root), Replay::Pass).await;
        assert!(
            matches!(&e, Web3Error::Revert(r) if r.name() == Some("InvalidStateRoot")),
            "{e}"
        );
        assert_eq!(tags, ["0x10"], "replayed at the receipt's block only");

        // The block is out of the endpoint's reach: latest names it.
        let (e, tags) = verdict(Replay::NoBlock, Replay::Revert(status)).await;
        assert!(
            matches!(&e, Web3Error::Revert(r) if r.name() == Some("InvalidStatus")),
            "{e}"
        );
        assert_eq!(tags, ["0x10", "latest"]);

        // Nothing reverts on the state the endpoint has: a lost race.
        let (e, _) = verdict(Replay::NoBlock, Replay::Pass).await;
        assert!(
            matches!(e, Web3Error::Lost { tx_hash, block: 0x10 } if tx_hash == hash),
            "{e}"
        );
        let (e, _) = verdict(Replay::Pass, Replay::Pass).await;
        assert!(matches!(e, Web3Error::Lost { .. }), "{e}");

        // No endpoint can run it: nothing named, retried the same way.
        let (e, _) = verdict(Replay::NoBlock, Replay::NoBlock).await;
        assert!(matches!(e, Web3Error::Lost { .. }), "{e}");
    }

    #[tokio::test]
    async fn blob_fee_falls_back_to_fee_history() {
        let p = crate::web3::rpc_provider(&[fee_stub(false).await]);
        // The next block's fee is the last entry.
        assert_eq!(blob_base_fee(&p).await.unwrap(), 0x77359400);
        let p = crate::web3::rpc_provider(&[fee_stub(true).await]);
        assert_eq!(blob_base_fee(&p).await.unwrap(), 3);
    }
}
