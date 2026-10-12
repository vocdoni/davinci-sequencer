//! Ethereum for the L1-blob mode: the [`L1Chain`] operations the poster and
//! the settle loop use, and [`L1Rpc`], which runs them on execution RPCs:
//! reads and proofs on `--l1-rpc-url` (sticky failover), blob sends on
//! `--l1-send-rpc-url` when set (many public RPCs refuse 1.5 MB bodies).

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::consensus::TxEnvelope;
use alloy::eips::BlockNumberOrTag;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::network::{
    EthereumWallet, NetworkTransactionBuilder, TransactionBuilder, TransactionBuilder4844,
};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::{RpcError, TransportErrorKind};
use async_trait::async_trait;
use davinci_zkvm_sdk::blob::TransitionBlobs;
use serde::{Deserialize, Serialize};
use url::Url;

use super::failover::known_tx;
use super::l1proof::{L1BlobProof, L1ProofError};
use super::tx::{
    blob_base_fee, build_sidecar, check_shape, gapped, maybe_sent, sidecar_version_error,
    underpriced,
};
use super::{Result, Web3Error, rpc_err};
use crate::config::SecretString;

/// Gas of a blob transaction to an EOA without data.
pub const BLOB_TX_GAS: u64 = 21_000;
/// Blob gas of one blob.
pub const GAS_PER_BLOB: u128 = 131_072;
/// Floor of the priority fee: 0.001 gwei.
pub const MIN_TIP: u128 = 1_000_000;

/// What the L1 market asks now, in wei.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1Market {
    pub base_fee: u128,
    pub tip: u128,
    pub blob_base_fee: u128,
}

/// The three fees a blob transaction offers, in wei.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct L1Fees {
    pub max_fee: u128,
    pub tip: u128,
    pub blob_fee: u128,
}

impl L1Fees {
    /// A fresh offer: a tip of at least [`MIN_TIP`], twice the base fee plus
    /// the tip, twice the blob base fee.
    pub fn offer(m: &L1Market) -> Self {
        let tip = m.tip.max(MIN_TIP);
        L1Fees {
            max_fee: m.base_fee.saturating_mul(2).saturating_add(tip),
            tip,
            blob_fee: m.blob_base_fee.saturating_mul(2).max(1),
        }
    }

    /// The blob pool's replacement price: every fee doubled.
    pub fn doubled(&self) -> Self {
        let d = |v: u128| v.saturating_mul(2).max(1);
        L1Fees {
            max_fee: d(self.max_fee),
            tip: d(self.tip),
            blob_fee: d(self.blob_fee),
        }
    }

    /// Each fee at least `other`'s.
    pub fn max_each(self, other: L1Fees) -> Self {
        L1Fees {
            max_fee: self.max_fee.max(other.max_fee),
            tip: self.tip.max(other.tip),
            blob_fee: self.blob_fee.max(other.blob_fee),
        }
    }

    /// Worst-case cost in wei of a post of `n` blobs at these fees.
    pub fn cost(&self, n: usize) -> u128 {
        u128::from(BLOB_TX_GAS)
            .saturating_mul(self.max_fee)
            .saturating_add(
                (n as u128)
                    .saturating_mul(GAS_PER_BLOB)
                    .saturating_mul(self.blob_fee),
            )
    }
}

/// Where a transaction is mined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1Receipt {
    pub block: u64,
    pub index: u64,
}

/// A signed blob transaction, ready to send.
#[derive(Clone)]
pub struct SignedBlobTx {
    pub hash: B256,
    pub nonce: u64,
    pub fees: L1Fees,
    /// The network form with its sidecar (about 130 KB per blob); test
    /// doubles leave it out.
    pub envelope: Option<Arc<TxEnvelope>>,
}

// The envelope carries every blob: never printed.
impl std::fmt::Debug for SignedBlobTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedBlobTx")
            .field("hash", &self.hash)
            .field("nonce", &self.nonce)
            .field("fees", &self.fees)
            .finish_non_exhaustive()
    }
}

/// Why a blob send did not go through.
#[derive(Debug, thiserror::Error)]
pub enum L1SendError {
    /// A transaction of ours holds this nonce and this one does not outbid
    /// it by 100 % on every fee.
    #[error("underpriced: {0}")]
    Underpriced(String),
    /// The nonce is mined.
    #[error("nonce taken: {0}")]
    NonceTaken(String),
    /// A lower nonce of the account is missing.
    #[error("nonce gap: {0}")]
    Gap(String),
    /// The node wants the other sidecar version; the next signature uses it.
    #[error("sidecar version refused: {0}")]
    Sidecar(String),
    /// No endpoint answered; the transaction may be in.
    #[error("unanswered: {0}")]
    Unanswered(String),
    /// Refused for good (funds, size, malformed).
    #[error("refused: {0}")]
    Refused(String),
}

/// The Ethereum operations of the L1-blob mode: [`L1Rpc`] in production, a
/// scripted chain in tests.
#[async_trait]
pub trait L1Chain: Send + Sync {
    /// The account that posts blobs; `None` without an Ethereum key.
    fn sender(&self) -> Option<Address>;
    async fn head(&self) -> Result<u64>;
    /// The sender's mined transaction count (`latest`).
    async fn nonce(&self) -> Result<u64>;
    async fn fees(&self) -> Result<L1Market>;
    /// Signs a blob transaction from the sender to itself carrying `blobs`.
    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx>;
    /// Sends it; an endpoint that already holds it counts as accepted.
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> std::result::Result<(), L1SendError>;
    /// Where `tx` is mined; `None` while pending or unknown.
    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>>;
    /// Whether the L1 node knows `tx`, pending or mined.
    async fn tx_known(&self, tx: B256) -> Result<bool>;
    /// The L1 proof of `txs` with headers from `top` down to the oldest of
    /// their blocks.
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof>;
}

/// Classifies a refused blob send; `None` when the endpoint already holds it.
pub(crate) fn send_error(e: &RpcError<TransportErrorKind>) -> Option<L1SendError> {
    let msg = e.to_string();
    if known_tx(&msg) {
        return None;
    }
    if maybe_sent(e) {
        return Some(L1SendError::Unanswered(msg));
    }
    let m = msg.to_ascii_lowercase();
    Some(if sidecar_version_error(&msg) {
        L1SendError::Sidecar(msg)
    } else if underpriced(&msg) || m.contains("blob gas fee") {
        L1SendError::Underpriced(msg)
    } else if m.contains("nonce too low") {
        L1SendError::NonceTaken(msg)
    } else if gapped(&msg) {
        L1SendError::Gap(msg)
    } else {
        L1SendError::Refused(msg)
    })
}

/// Ethereum over execution RPCs.
pub struct L1Rpc {
    provider: DynProvider,
    send: DynProvider,
    chain_id: u64,
    signer: Option<Address>,
    wallet: Option<EthereumWallet>,
    /// Flipped by every sidecar-version refusal: an endpoint set mixing
    /// versions (around a fork) may want either.
    cell_proofs: AtomicBool,
    blob_cap: Option<usize>,
    /// The last sidecar built (one post at a time): its replacements reuse it.
    sidecar: tokio::sync::Mutex<Option<(B256, bool, Arc<BlobTransactionSidecarVariant>)>>,
}

impl std::fmt::Debug for L1Rpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L1Rpc")
            .field("chain_id", &self.chain_id)
            .field("sender", &self.signer)
            .field("cell_proofs", &self.cell_proofs())
            .field("blob_cap", &self.blob_cap)
            .finish_non_exhaustive()
    }
}

impl L1Rpc {
    /// Connects to `rpcs` and, for blob sends, `send` (empty: `rpcs`). Fails
    /// if an endpoint set is on another chain or none answers; picks the
    /// sidecar version and reads the blob cap of this chain.
    pub async fn connect(
        rpcs: &[Url],
        send: &[Url],
        privkey: Option<&SecretString>,
    ) -> Result<Self> {
        let (wallet, signer) = match privkey {
            Some(k) => {
                // The parse error is not echoed: it could quote the key.
                let s = PrivateKeySigner::from_str(k.expose().trim()).map_err(|_| {
                    Web3Error::Config("the L1 private key is not 32-byte hex".into())
                })?;
                let a = s.address();
                (Some(EthereumWallet::from(s)), Some(a))
            }
            None => (None, None),
        };
        let chain_id = super::rpc_chain_id(rpcs).await?;
        if !send.is_empty() {
            let id = super::rpc_chain_id(send).await?;
            if id != chain_id {
                return Err(Web3Error::Config(format!(
                    "the L1 send endpoints are on chain {id}, the L1 RPCs on chain {chain_id}"
                )));
            }
        }
        let provider = super::rpc_provider(rpcs);
        let send = if send.is_empty() {
            provider.clone()
        } else {
            super::rpc_provider(send)
        };
        let (blob_cap, cell_proofs) =
            super::contracts::blob_support(rpcs, &provider, chain_id).await?;
        Ok(L1Rpc {
            provider,
            send,
            chain_id,
            signer,
            wallet,
            cell_proofs: AtomicBool::new(cell_proofs),
            blob_cap,
            sidecar: Default::default(),
        })
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Whether blob transactions carry EIP-7594 cell proofs (v1 sidecars).
    pub fn cell_proofs(&self) -> bool {
        self.cell_proofs.load(Ordering::Relaxed)
    }

    /// Blobs per transaction from `eth_config`, capped at six; `None` if no
    /// endpoint serves it.
    pub fn chain_blob_cap(&self) -> Option<usize> {
        self.blob_cap
    }

    async fn sidecar_for(
        &self,
        blobs: &TransitionBlobs,
    ) -> Result<Arc<BlobTransactionSidecarVariant>> {
        let v1 = self.cell_proofs();
        let key = blobs
            .versioned_hashes
            .first()
            .map(|h| B256::from(*h))
            .unwrap_or_default();
        let mut cache = self.sidecar.lock().await;
        if let Some((k, v, sc)) = cache.as_ref()
            && *k == key
            && *v == v1
        {
            return Ok(sc.clone());
        }
        // KZG cell proofs take a while: off the async workers.
        let owned = blobs.clone();
        let sc = tokio::task::spawn_blocking(move || build_sidecar(v1, &owned))
            .await
            .map_err(|e| Web3Error::Blob(format!("sidecar task: {e}")))??;
        let sc = Arc::new(sc);
        *cache = Some((key, v1, sc.clone()));
        Ok(sc)
    }
}

#[async_trait]
impl L1Chain for L1Rpc {
    fn sender(&self) -> Option<Address> {
        self.signer
    }

    async fn head(&self) -> Result<u64> {
        self.provider.get_block_number().await.map_err(rpc_err)
    }

    async fn nonce(&self) -> Result<u64> {
        let from = self.signer.ok_or(Web3Error::NoSigner)?;
        self.provider
            .get_transaction_count(from)
            .latest()
            .await
            .map_err(rpc_err)
    }

    async fn fees(&self) -> Result<L1Market> {
        let b = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(rpc_err)?
            .ok_or_else(|| Web3Error::Data("no latest L1 block".into()))?;
        // Not every endpoint serves it; the floor covers that.
        let tip = self
            .provider
            .get_max_priority_fee_per_gas()
            .await
            .unwrap_or(0);
        Ok(L1Market {
            base_fee: u128::from(b.header.base_fee_per_gas.unwrap_or(0)),
            tip,
            blob_base_fee: blob_base_fee(&self.provider).await?,
        })
    }

    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx> {
        let (Some(from), Some(wallet)) = (self.signer, self.wallet.as_ref()) else {
            return Err(Web3Error::NoSigner);
        };
        check_shape(blobs)?;
        let sidecar = self.sidecar_for(blobs).await?;
        let mut tx = TransactionRequest::default()
            .with_from(from)
            .with_to(from)
            .with_chain_id(self.chain_id)
            .with_nonce(nonce)
            .with_gas_limit(BLOB_TX_GAS)
            .with_value(U256::ZERO)
            .with_max_fee_per_gas(fees.max_fee)
            .with_max_priority_fee_per_gas(fees.tip)
            .with_max_fee_per_blob_gas(fees.blob_fee);
        tx.blob_versioned_hashes = Some(
            blobs
                .versioned_hashes
                .iter()
                .map(|h| B256::from(*h))
                .collect(),
        );
        tx.set_blob_sidecar((*sidecar).clone());
        let env = tx
            .build(wallet)
            .await
            .map_err(|e| Web3Error::Rpc(format!("signing: {e}")))?;
        Ok(SignedBlobTx {
            hash: *env.tx_hash(),
            nonce,
            fees: *fees,
            envelope: Some(Arc::new(env)),
        })
    }

    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> std::result::Result<(), L1SendError> {
        let env = tx
            .envelope
            .as_deref()
            .cloned()
            .ok_or_else(|| L1SendError::Refused("not signed by this client".into()))?;
        let Err(e) = self.send.send_tx_envelope(env).await else {
            return Ok(());
        };
        match send_error(&e) {
            None => Ok(()),
            Some(L1SendError::Sidecar(m)) => {
                // The next signature uses the other version.
                self.cell_proofs.fetch_xor(true, Ordering::Relaxed);
                Err(L1SendError::Sidecar(m))
            }
            Some(e) => Err(e),
        }
    }

    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>> {
        let r = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(rpc_err)?;
        Ok(r.and_then(|r| {
            Some(L1Receipt {
                block: r.block_number?,
                index: r.transaction_index?,
            })
        }))
    }

    async fn tx_known(&self, tx: B256) -> Result<bool> {
        let t = self
            .provider
            .get_transaction_by_hash(tx)
            .await
            .map_err(rpc_err)?;
        Ok(t.is_some())
    }

    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof> {
        super::l1proof::build(&self.provider, txs, top)
            .await
            .map_err(|e| match e {
                L1ProofError::Rpc(m) => Web3Error::Rpc(m),
                e => Web3Error::Data(e.to_string()),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::HttpError;
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::blob::{TransitionData, build_blobs};
    use davinci_zkvm_sdk::crypto::field::Fr;
    use davinci_zkvm_sdk::limits::VOTE_ID_MIN;
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn send_errors_are_classified() {
        let resp = |m: &str| {
            RpcError::<TransportErrorKind>::ErrorResp(ErrorPayload {
                code: -32000,
                message: m.to_string().into(),
                data: None,
            })
        };
        let class = |m: &str| send_error(&resp(m)).map(|e| e.to_string());
        assert!(send_error(&resp("already known")).is_none());
        for (msg, want) in [
            ("replacement transaction underpriced", "underpriced"),
            (
                "max fee per blob gas less than block blob gas fee",
                "underpriced",
            ),
            ("nonce too low: next nonce 5, tx nonce 4", "nonce taken"),
            ("nonce too high", "nonce gap"),
            ("unexpected blob sidecar version", "sidecar version refused"),
            ("insufficient funds for gas * price + value", "refused"),
        ] {
            let got = class(msg).unwrap();
            assert!(got.starts_with(want), "{msg}: {got}");
        }
        let http = |status| {
            RpcError::<TransportErrorKind>::Transport(TransportErrorKind::HttpError(HttpError {
                status,
                body: String::new(),
            }))
        };
        assert!(matches!(
            send_error(&http(503)),
            Some(L1SendError::Unanswered(_))
        ));
        // A body too large for the endpoint is refused at the door, never "maybe sent".
        assert!(matches!(
            send_error(&http(413)),
            Some(L1SendError::Refused(_))
        ));
    }

    #[test]
    fn fee_offers_and_replacements() {
        let m = L1Market {
            base_fee: 10,
            tip: 0,
            blob_base_fee: 3,
        };
        let f = L1Fees::offer(&m);
        assert_eq!(
            f,
            L1Fees {
                max_fee: 20 + MIN_TIP,
                tip: MIN_TIP,
                blob_fee: 6
            }
        );
        let m = L1Market {
            base_fee: 0,
            tip: 5_000_000,
            blob_base_fee: 0,
        };
        assert_eq!(
            L1Fees::offer(&m),
            L1Fees {
                max_fee: 5_000_000,
                tip: 5_000_000,
                blob_fee: 1
            }
        );
        let d = f.doubled();
        assert_eq!(
            d,
            L1Fees {
                max_fee: 2 * f.max_fee,
                tip: 2 * MIN_TIP,
                blob_fee: 12
            }
        );
        assert_eq!(
            f.max_each(L1Fees {
                max_fee: 1,
                tip: 9 * MIN_TIP,
                blob_fee: 1
            })
            .tip,
            9 * MIN_TIP
        );
        assert_eq!(f.cost(2), 21_000 * f.max_fee + 2 * 131_072 * 6);
        let huge = L1Fees {
            max_fee: u128::MAX,
            tip: 1,
            blob_fee: u128::MAX,
        };
        assert_eq!(huge.cost(6), u128::MAX);
        assert_eq!(huge.doubled().max_fee, u128::MAX);
    }

    // An Ethereum RPC stub on `chain`: Osaka `eth_config`, six blobs per
    // transaction; counts `eth_sendRawTransaction`.
    async fn stub(chain: u64) -> (Url, Arc<AtomicUsize>) {
        let sends = Arc::new(AtomicUsize::new(0));
        let n = sends.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| {
                let n = n.clone();
                async move {
                    let id = req["id"].clone();
                    let result = match req["method"].as_str().unwrap_or_default() {
                        "eth_chainId" => json!(format!("{chain:#x}")),
                        "eth_config" => json!({"current": {"blobSchedule": {"max": 6},
                            "precompiles": {"P256VERIFY": "0x0000000000000000000000000000000000000100"}}}),
                        "eth_sendRawTransaction" => {
                            n.fetch_add(1, Ordering::SeqCst);
                            json!(B256::repeat_byte(5))
                        }
                        m => panic!("unexpected {m}"),
                    };
                    axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}").parse().unwrap(), sends)
    }

    fn one_blob() -> TransitionBlobs {
        let t = TransitionData {
            vote_ids: vec![VOTE_ID_MIN + 1],
            updates: vec![],
            accumulator: Ballot::identity(),
            num_fields: 1,
        };
        build_blobs(&t, &Fr::from(7u64), &[2u8; 32]).unwrap()
    }

    #[tokio::test]
    async fn blob_sends_go_to_the_send_endpoints() {
        let (reads, read_sends) = stub(1).await;
        let (send, sends) = stub(1).await;
        let key = SecretString::new(hex::encode([0x11u8; 32]));
        let l1 = L1Rpc::connect(std::slice::from_ref(&reads), &[send], Some(&key))
            .await
            .unwrap();
        assert_eq!(
            (l1.chain_id(), l1.cell_proofs(), l1.chain_blob_cap()),
            (1, true, Some(6))
        );
        assert!(l1.sender().is_some());
        let fees = L1Fees {
            max_fee: 10,
            tip: 1,
            blob_fee: 2,
        };
        let tx = l1.sign_blob_tx(0, &fees, &one_blob()).await.unwrap();
        assert_eq!((tx.nonce, tx.fees), (0, fees));
        assert!(
            !format!("{tx:?}").contains("envelope"),
            "no sidecar in Debug"
        );
        l1.send_blob_tx(&tx).await.unwrap();
        assert_eq!(
            (
                read_sends.load(Ordering::SeqCst),
                sends.load(Ordering::SeqCst)
            ),
            (0, 1)
        );
        // A replacement re-signs over the cached sidecar: another hash.
        let again = l1
            .sign_blob_tx(0, &fees.doubled(), &one_blob())
            .await
            .unwrap();
        assert_ne!(again.hash, tx.hash);
        // Send endpoints on another chain are refused.
        let (other, _) = stub(5).await;
        assert!(
            L1Rpc::connect(&[reads], &[other], Some(&key))
                .await
                .is_err()
        );
    }
}
