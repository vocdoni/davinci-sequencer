//! Organizer helper: the census file and the process lifecycle on the zkVM
//! ProcessRegistry (create in any key mode, replace the census or the
//! metadata, pause, change the duration or max voters, end, reveal a DKG
//! organizer key, read results), and the registry pin check.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy::providers::fillers::{FillProvider, JoinFill, WalletFiller};
use alloy::providers::utils::JoinedRecommendedFillers;
use alloy::providers::{
    DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder, RootProvider,
};
use alloy::rpc::client::{ClientBuilder, RpcClient};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::transports::TransportError;
use alloy::transports::http::{Http, reqwest};
use alloy::transports::layers::{RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{LeanImt, census_leaf};
use davinci_zkvm_sdk::crypto::babyjubjub::{Point, SUBGROUP_ORDER};
use davinci_zkvm_sdk::crypto::field::{
    U256 as SdkU256, fr_from_be, fr_to_be, u256_from_be, u256_to_be,
};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::{dkg, release};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use crate::api::{CensusFile, CensusParticipant, Fr, ProcessId, ProcessStatus};
use crate::{Error, Result};

sol!(
    #[sol(rpc, all_derives)]
    #[allow(missing_docs, clippy::too_many_arguments)]
    ProcessRegistry,
    "../sequencer/abi/ProcessRegistry.json"
);

sol! {
    #[sol(rpc)]
    interface IZiskVerifier {
        error InvalidProof();
        function getRootCVadcopFinal() external pure returns (bytes32);
    }
}

mod adapter {
    alloy::sol!(
        #[sol(rpc, all_derives)]
        #[allow(missing_docs)]
        DavinciDKGAdapter,
        "../sequencer/abi/DavinciDKGAdapter.json"
    );
}
use adapter::DavinciDKGAdapter::{self, DavinciDKGAdapterErrors};

sol! {
    /// davinci-dkg manager and app-manager errors the registry bubbles up.
    #[sol(all_derives)]
    interface IDKG {
        error InvalidApplication();
        error ApplicationAlreadyExists();
        error InvalidSchnorrProof();
        error PointNotInSubgroup();
        error InvalidEpoch();
        error InvalidPhase();
        error InvalidOrganizerSecret();
        error InvalidPolicy();
        error AlreadyRevealed();
        error PoolExhausted();
        error InvalidProofInput();
        error InvalidCiphertext();
        error CiphertextAlreadySubmitted();
        error DecryptionLimitReached();
        error Unauthorized();
    }
}

use ProcessRegistry::{ProcessCreated, ProcessRegistryErrors, ProcessRegistryInstance};

/// `User-Agent` of the client's RPC requests: some public RPCs answer 403
/// without one.
pub const USER_AGENT: &str = concat!("davinci-client/", env!("CARGO_PKG_VERSION"));

/// Default wait for a transaction receipt.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(180);

/// Retries of a rate-limited request, a second apart: the public Gnosis RPCs
/// refuse a busy host for a minute or two at a time, so three minutes.
const RATE_LIMIT_RETRIES: u32 = 180;
const RATE_LIMIT_BACKOFF_MS: u64 = 1_000;

/// alloy's rate-limit policy with the server's backoff hint (`Retry-After`,
/// up to 5 min in alloy) capped at [`RATE_LIMIT_BACKOFF_MS`], so the
/// retries stay within their three minutes.
#[derive(Clone, Copy, Debug, Default)]
struct CappedRateLimit(RateLimitRetryPolicy);

impl RetryPolicy for CappedRateLimit {
    fn should_retry(&self, e: &TransportError) -> bool {
        self.0.should_retry(e)
    }

    fn backoff_hint(&self, e: &TransportError) -> Option<Duration> {
        self.0
            .backoff_hint(e)
            .map(|h| h.min(Duration::from_millis(RATE_LIMIT_BACKOFF_MS)))
    }
}

/// An HTTP RPC client sending `user_agent` that backs off and retries when
/// the RPC rate-limits (public RPCs answer bursts with 429 or -32005).
pub fn rpc_client(url: &str, user_agent: &str) -> Result<RpcClient> {
    let url: reqwest::Url = url
        .parse()
        .map_err(|_| Error::Invalid(format!("bad RPC URL {url:.60}")))?;
    let http = reqwest::Client::builder()
        .user_agent(user_agent)
        .build()
        .unwrap_or_default();
    let is_local = Http::with_client(http.clone(), url.clone()).guess_local();
    Ok(ClientBuilder::default()
        .layer(RetryBackoffLayer::new_with_policy(
            RATE_LIMIT_RETRIES,
            RATE_LIMIT_BACKOFF_MS,
            10_000,
            CappedRateLimit::default(),
        ))
        .transport(Http::with_client(http, url), is_local))
}

/// Read-only provider sending [`USER_AGENT`].
fn read_provider(url: &str) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .connect_client(rpc_client(url, USER_AGENT)?)
        .erased())
}

/// `DAVINCITypes.ProcessStatus` READY, ENDED, CANCELED and PAUSED.
const STATUS_READY: u8 = 0;
const STATUS_ENDED: u8 = 1;
const STATUS_CANCELED: u8 = 2;
const STATUS_PAUSED: u8 = 3;

/// Where a new process's election key comes from (`DAVINCITypes.KeyMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    /// A sequencer's key for [`NewProcess::process_id`] (`POST /processes/keys`);
    /// that sequencer decrypts the results.
    Sequencer(Point),
    /// A davinci-dkg committee pool key; only the committee decrypts, and only
    /// the final tally.
    DkgAutomatic,
    /// A pool key plus an organizer key drawn here: results stay locked until
    /// [`Organizer::reveal_process_key`].
    DkgLocked,
}

impl KeyMode {
    /// The `DAVINCITypes.KeyMode` value.
    pub fn id(&self) -> u8 {
        match self {
            KeyMode::Sequencer(_) => 0,
            KeyMode::DkgAutomatic => 1,
            KeyMode::DkgLocked => 2,
        }
    }
}

/// The organizer secret of a `DKG_LOCKED` process, in `[1, L)`. Nothing
/// stores it: lose it and the results never unlock. `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct OrganizerSecret(SdkU256);

impl std::fmt::Debug for OrganizerSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OrganizerSecret(<redacted>)")
    }
}

impl OrganizerSecret {
    /// Parses a 32-byte BE scalar; refuses anything outside `[1, L)`.
    pub fn from_be_bytes(b: &[u8; 32]) -> Result<Self> {
        let v = u256_from_be(b);
        if v == SdkU256::from(0u64) || v >= SUBGROUP_ORDER {
            return Err(Error::Invalid("organizer secret not in [1, L)".into()));
        }
        Ok(OrganizerSecret(v))
    }

    pub fn to_be_bytes(&self) -> [u8; 32] {
        u256_to_be(&self.0)
    }

    /// `sk·B8` in circomlib form.
    pub fn public_key(&self) -> Point {
        Point::generator().mul(&self.0)
    }
}

/// What [`Organizer::create_process`] made.
#[derive(Clone, Debug)]
pub struct CreatedProcess {
    pub pid: [u8; 31],
    /// `DkgLocked` only: the secret [`Organizer::reveal_process_key`] needs.
    pub organizer_secret: Option<OrganizerSecret>,
}

/// DKG side of an on-chain process (`keyMode` != SEQUENCER).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DkgProcess {
    /// `DKG_LOCKED` (organizer key) rather than `DKG_AUTOMATIC`.
    pub locked: bool,
    pub epoch_id: [u8; 12],
    pub aid: [u8; 32],
    /// Set by `requestResultsDecryption`.
    pub results_requested: bool,
    /// DKG index of the first submitted ciphertext.
    pub first_index: u16,
    /// Ciphertexts submitted (identity fields are skipped).
    pub count: u8,
    /// Fields recorded as 0 without the DKG, bit i = field i.
    pub zero_skipped: u16,
}

/// davinci-node census file for `participants` (address, weight), in order.
pub fn census_file(participants: &[([u8; 20], u128)]) -> CensusFile {
    CensusFile {
        participants: participants
            .iter()
            .map(|(key, weight)| CensusParticipant {
                key: *key,
                weight: *weight,
            })
            .collect(),
    }
}

/// The lean-IMT a Merkle census file commits to (leaves in file order).
pub fn merkle_census(file: &CensusFile) -> Result<LeanImt> {
    let mut t = LeanImt::new();
    for p in &file.participants {
        t.insert(census_leaf(&p.key, p.weight)?);
    }
    Ok(t)
}

/// SHA-256 of a metadata document: the hash of the exact bytes served at its
/// URI, with no JSON canonicalisation.
pub fn metadata_hash(document: &[u8]) -> [u8; 32] {
    Sha256::digest(document).into()
}

/// `newProcess` parameters. `start_time = 0` means now; the process starts READY.
#[derive(Clone, Debug)]
pub struct NewProcess {
    /// The id the registry will assign ([`Organizer::next_process_id`]), which
    /// the sequencer's `encryption_key` is bound to.
    pub process_id: [u8; 31],
    pub start_time: u64,
    pub duration: u64,
    pub max_voters: u64,
    pub ballot_mode: BallotMode,
    /// 1 (Merkle static), 2 (Merkle, organizer-updatable), 3 (on-chain
    /// census contract) or 4 (CSP).
    pub census_origin: u8,
    /// lean-IMT root, or the CSP address as an integer. For origin 3 the
    /// registry reads the root from the contract instead.
    pub census_root: Fr,
    /// The census contract for origin 3; zero for every other origin (the
    /// registry reverts `InvalidCensusAddress` otherwise).
    pub census_contract: [u8; 20],
    pub census_uri: String,
    /// Where the metadata document is served; must not be empty.
    pub metadata: String,
    /// [`metadata_hash`] of the document at `metadata`.
    pub metadata_hash: [u8; 32],
    pub key_mode: KeyMode,
}

/// A process as stored by the registry. This, not a sequencer's
/// `ProcessView`, is what a voter builds ballots from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OnchainProcess {
    pub id: [u8; 31],
    pub status: ProcessStatus,
    pub organization_id: [u8; 20],
    pub encryption_key: Point,
    /// Raw arbo root.
    pub state_root: [u8; 32],
    pub result: Vec<u64>,
    pub start_time: u64,
    pub duration: u64,
    pub max_voters: u64,
    pub voters_count: u64,
    pub overwritten_votes_count: u64,
    pub ballot_mode: BallotMode,
    pub census_origin: u8,
    pub census_root: Fr,
    pub census_contract: [u8; 20],
    pub census_uri: String,
    pub metadata_uri: String,
    /// SHA-256 of the exact bytes served at `metadata_uri`.
    pub metadata_hash: [u8; 32],
    /// `None` for a sequencer-key process.
    pub dkg: Option<DkgProcess>,
    /// Idle seconds that close the grace window after the end.
    pub grace: u64,
    /// Block time of the latest transition; 0 before the first.
    pub last_vote_at: u64,
}

fn u64_of(what: &str, v: U256) -> Result<u64> {
    u64::try_from(v).map_err(|_| Error::Chain(format!("{what} does not fit in u64")))
}

fn fr_of(what: &str, v: U256) -> Result<Fr> {
    fr_from_be(&v.to_be_bytes::<32>()).map_err(|_| Error::Chain(format!("{what} is not below p")))
}

// First word of a decoded error's Debug: its ABI name.
fn err_name(d: impl std::fmt::Debug) -> String {
    let d = format!("{d:?}");
    d.split(['(', ' ', '{'])
        .next()
        .unwrap_or_default()
        .to_string()
}

// Contract errors come back decoded by name where the ABIs know them: the
// registry's, then the DKG adapter's and the DKG's it bubbles up.
fn chain_err(e: alloy::contract::Error) -> Error {
    if let Some(d) = e.as_decoded_interface_error::<ProcessRegistryErrors>() {
        return Error::Reverted(err_name(d));
    }
    if let Some(d) = e.as_decoded_interface_error::<DavinciDKGAdapterErrors>() {
        return Error::Reverted(err_name(d));
    }
    if let Some(d) = e.as_decoded_interface_error::<IDKG::IDKGErrors>() {
        return Error::Reverted(err_name(d));
    }
    Error::Chain(e.to_string())
}

// A send refused because this nonce is taken: by an earlier copy of the same
// tx (geth "already known", anvil/reth "already imported", Nethermind
// "AlreadyKnown", Besu "Known transaction") or by any mined one.
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

fn check_receipt(r: &TransactionReceipt) -> Result<()> {
    if !r.status() {
        return Err(Error::Chain(format!(
            "transaction {} reverted",
            r.transaction_hash
        )));
    }
    Ok(())
}

// DKGParams with only the mode set (SEQUENCER and DKG_AUTOMATIC).
fn dkg_params(mode: u8) -> DAVINCITypes::DKGParams {
    DAVINCITypes::DKGParams {
        mode,
        epochId: FixedBytes::ZERO,
        orgPKx: U256::ZERO,
        orgPKy: U256::ZERO,
        popAx: U256::ZERO,
        popAy: U256::ZERO,
        popZ: U256::ZERO,
    }
}

// Recommended fillers (cached nonce) plus the organizer's wallet.
type Signing =
    FillProvider<JoinFill<JoinedRecommendedFillers, WalletFiller<EthereumWallet>>, RootProvider>;

/// Sends registry transactions from the organizer account.
#[derive(Clone, Debug)]
pub struct Organizer {
    registry: ProcessRegistryInstance<DynProvider>,
    /// The registry's provider, unerased: fills and signs before sending.
    signing: Signing,
    sender: Address,
    /// Every mined receipt, in order (gas accounting).
    receipts: Arc<Mutex<Vec<TransactionReceipt>>>,
    /// Every process this organizer created, `WrongProcessId` ones included.
    created: Arc<Mutex<Vec<[u8; 31]>>>,
    receipt_timeout: Duration,
}

impl Organizer {
    pub fn connect(url: &str, signer: PrivateKeySigner, registry: Address) -> Result<Self> {
        let sender = signer.address();
        let signing = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_client(rpc_client(url, USER_AGENT)?);
        Ok(Organizer {
            registry: ProcessRegistry::new(registry, signing.clone().erased()),
            signing,
            sender,
            receipts: Arc::default(),
            created: Arc::default(),
            receipt_timeout: RECEIPT_TIMEOUT,
        })
    }

    /// Longest wait for each transaction receipt (default 180 s).
    pub fn with_receipt_timeout(mut self, d: Duration) -> Self {
        self.receipt_timeout = d;
        self
    }

    pub fn address(&self) -> Address {
        self.sender
    }

    /// The signing provider. Other transactions from this account must go
    /// through it: it caches the nonce.
    pub fn provider(&self) -> DynProvider {
        self.registry.provider().clone()
    }

    /// The receipts of every transaction this organizer (or a clone) got
    /// mined, reverted ones included.
    pub fn receipts(&self) -> Vec<TransactionReceipt> {
        self.receipts.lock().map(|r| r.clone()).unwrap_or_default()
    }

    // Fills, signs and sends `tx`, then waits in `mined`. The RPC client
    // retries a send whose answer got lost; the copy is refused once the
    // first one is in, and the tx is ours if the chain knows its hash.
    async fn send(&self, tx: TransactionRequest) -> Result<TransactionReceipt> {
        let env = self
            .signing
            .fill(tx)
            .await
            .map_err(|e| chain_err(e.into()))?
            .try_into_envelope()
            .map_err(|e| Error::Chain(format!("transaction not signed: {e}")))?;
        let hash = *env.tx_hash();
        let pending = match self.signing.send_tx_envelope(env).await {
            Ok(p) => p,
            Err(e)
                if raced(&e.to_string())
                    && matches!(
                        self.signing.get_transaction_by_hash(hash).await,
                        Ok(Some(_))
                    ) =>
            {
                tracing::warn!(%hash, error = %e, "resend refused, the chain has the tx");
                PendingTransactionBuilder::new(self.signing.root().clone(), hash)
            }
            Err(e) => return Err(chain_err(e.into())),
        };
        self.mined(pending).await
    }

    // Waits for the receipt, records it and fails on a revert.
    async fn mined(
        &self,
        pending: PendingTransactionBuilder<Ethereum>,
    ) -> Result<TransactionReceipt> {
        let receipt = pending
            .with_timeout(Some(self.receipt_timeout))
            .get_receipt()
            .await
            .map_err(|e| Error::Chain(e.to_string()))?;
        if let Ok(mut r) = self.receipts.lock() {
            r.push(receipt.clone());
        }
        if !receipt.status()
            && let Some(name) = self.mined_revert(receipt.transaction_hash).await
        {
            return Err(Error::Reverted(name));
        }
        check_receipt(&receipt)?;
        Ok(receipt)
    }

    // Names a mined revert by replaying the tx on the latest state (usually
    // a lost race, which replays the same way); `None` if it does not.
    async fn mined_revert(&self, hash: B256) -> Option<String> {
        use alloy::consensus::Transaction as _;
        use alloy::network::TransactionResponse as _;
        let p = self.registry.provider();
        let tx = p.get_transaction_by_hash(hash).await.ok()??;
        let req = TransactionRequest::default()
            .from(tx.from())
            .to(tx.to()?)
            .input(tx.input().clone().into())
            .value(tx.value());
        let err = p.call(req).await.err()?;
        let data = err.as_error_resp()?.as_revert_data()?;
        let name = revert_name(&data);
        (!name.starts_with("0x") && name != "empty revert").then_some(name)
    }

    /// The id `newProcess` from this account gets next (`getNextProcessId`).
    /// Ask the sequencer for the election key of this id.
    pub async fn next_process_id(&self) -> Result<[u8; 31]> {
        let pid = self
            .registry
            .getNextProcessId(self.sender)
            .call()
            .await
            .map_err(chain_err)?;
        Ok(pid.0)
    }

    /// Creates a READY process; returns its id from the `ProcessCreated` event.
    /// Fails unless the registry assigns `p.process_id` next.
    ///
    /// `KeyMode::Sequencer`: a key issued for another id is not one the
    /// sequencer will decrypt with, so if another `newProcess` from this
    /// account lands first, the process is created anyway and
    /// [`Error::WrongProcessId`] carries its id; the organizer must cancel it
    /// (`setProcessStatus` CANCELED), since no node can finalize it.
    ///
    /// DKG modes: the registry takes the key from the DKG committee. `DkgLocked`
    /// draws the organizer secret from `OsRng`, proves possession for the
    /// registration epoch and the process's aid, and returns the secret.
    /// Either DKG mode retries once on `PoolExhausted` (simulated or mined),
    /// locked mode also when the registration epoch moved meanwhile.
    pub async fn create_process(&self, p: &NewProcess) -> Result<CreatedProcess> {
        let next = self.next_process_id().await?;
        if next != p.process_id {
            return Err(Error::Invalid(format!(
                "the registry assigns {} next, the params are for {}",
                ProcessId(next),
                ProcessId(p.process_id)
            )));
        }
        p.ballot_mode.pack()?;
        let adapter = match p.key_mode {
            KeyMode::Sequencer(_) => None,
            _ => Some(self.dkg_adapter().await?),
        };
        let mut retried = false;
        loop {
            let (dkg, secret) = match (p.key_mode, &adapter) {
                (KeyMode::DkgLocked, Some(a)) => {
                    let (params, sk) = self.locked_params(a, &next).await?;
                    (params, Some(sk))
                }
                (mode, _) => (dkg_params(mode.id()), None),
            };
            match self.send_new_process(p, dkg.clone()).await {
                Ok(pid) => {
                    return self.created_process(p, pid, secret);
                }
                // The pool emptied or a new epoch went Live under us.
                Err(e) if !retried && adapter.is_some() => {
                    let Some(a) = &adapter else { return Err(e) };
                    let pool = matches!(&e, Error::Reverted(n) if n == "PoolExhausted");
                    // Automatic mode picks its epoch on-chain: nothing moves.
                    let moved = p.key_mode == KeyMode::DkgLocked
                        && match a.registrationEpoch().call().await {
                            Ok(eid) => eid != dkg.epochId,
                            Err(_) => false,
                        };
                    if !(pool || moved) {
                        return Err(e);
                    }
                    tracing::warn!(error = %e, "DKG epoch changed or pool exhausted; retrying newProcess once");
                    retried = true;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // Simulates, sends and waits for `newProcess`; the created id.
    async fn send_new_process(
        &self,
        p: &NewProcess,
        dkg: DAVINCITypes::DKGParams,
    ) -> Result<[u8; 31]> {
        let m = &p.ballot_mode;
        let mode = DAVINCITypes::BallotMode {
            uniqueValues: m.unique_values,
            numFields: m.num_fields,
            groupSize: m.group_size,
            costExponent: m.cost_exponent,
            maxValue: U256::from(m.max_value),
            minValue: U256::from(m.min_value),
            maxValueSum: U256::from(m.max_value_sum),
            minValueSum: U256::from(m.min_value_sum),
        };
        let census = DAVINCITypes::Census {
            censusOrigin: p.census_origin,
            censusRoot: B256::from(fr_to_be(&p.census_root)),
            contractAddress: Address::from(p.census_contract),
            censusURI: p.census_uri.clone(),
            onchainAllowAnyValidRoot: false,
        };
        // DKG modes pass (0, 0): the registry takes the committee's key.
        let key = match p.key_mode {
            KeyMode::Sequencer(k) => DAVINCITypes::EncryptionKey {
                x: U256::from_be_bytes(fr_to_be(&k.x)),
                y: U256::from_be_bytes(fr_to_be(&k.y)),
            },
            _ => DAVINCITypes::EncryptionKey {
                x: U256::ZERO,
                y: U256::ZERO,
            },
        };
        let call = self.registry.newProcess(
            0,
            U256::from(p.start_time),
            U256::from(p.duration),
            U256::from(p.max_voters),
            mode,
            census,
            p.metadata.clone(),
            B256::from(p.metadata_hash),
            key,
            dkg,
        );
        // Simulate first so a revert comes back with its name.
        call.call().await.map_err(chain_err)?;
        let receipt = self.send(call.into_transaction_request()).await?;
        let registry = *self.registry.address();
        let pid = receipt
            .logs()
            .iter()
            .filter(|l| l.address() == registry)
            .filter_map(|l| l.log_decode::<ProcessCreated>().ok())
            .find(|l| l.inner.data.creator == self.sender)
            .map(|l| l.inner.data.processId.0)
            .ok_or_else(|| Error::Chain("no ProcessCreated event in the receipt".into()))?;
        if let Ok(mut c) = self.created.lock() {
            c.push(pid);
        }
        Ok(pid)
    }

    fn created_process(
        &self,
        p: &NewProcess,
        pid: [u8; 31],
        organizer_secret: Option<OrganizerSecret>,
    ) -> Result<CreatedProcess> {
        // Another transaction from this account can land in between. Only a
        // sequencer key is bound to the id; a locked PoP binds it too, so a
        // shifted locked process could not have been created.
        if pid != p.process_id && matches!(p.key_mode, KeyMode::Sequencer(_)) {
            return Err(Error::WrongProcessId {
                created: ProcessId(pid),
                expected: ProcessId(p.process_id),
            });
        }
        Ok(CreatedProcess {
            pid,
            organizer_secret,
        })
    }

    // DKGParams of a locked process: the registration epoch, a fresh
    // organizer secret and its PoP over (epoch, aid of `pid`).
    async fn locked_params(
        &self,
        adapter: &DavinciDKGAdapter::DavinciDKGAdapterInstance<DynProvider>,
        pid: &[u8; 31],
    ) -> Result<(DAVINCITypes::DKGParams, OrganizerSecret)> {
        let eid = adapter
            .registrationEpoch()
            .call()
            .await
            .map_err(chain_err)?;
        let aid = self
            .registry
            .aidFor(FixedBytes(*pid))
            .call()
            .await
            .map_err(chain_err)?;
        let sk = dkg::sample_organizer_sk(&mut OsRng);
        let (pkx, pky, ax, ay, z) = dkg::prove_organizer(eid.0, aid.0, sk, &mut OsRng)?;
        let params = DAVINCITypes::DKGParams {
            mode: KeyMode::DkgLocked.id(),
            epochId: eid,
            orgPKx: U256::from_be_bytes(pkx),
            orgPKy: U256::from_be_bytes(pky),
            popAx: U256::from_be_bytes(ax),
            popAy: U256::from_be_bytes(ay),
            popZ: U256::from_be_bytes(z),
        };
        Ok((params, OrganizerSecret(sk)))
    }

    /// The registry's DKG adapter; [`Error::DkgDisabled`] when it has none.
    pub async fn dkg_adapter(
        &self,
    ) -> Result<DavinciDKGAdapter::DavinciDKGAdapterInstance<DynProvider>> {
        let a = self.registry.dkgAdapter().call().await.map_err(chain_err)?;
        if a == Address::ZERO {
            return Err(Error::DkgDisabled);
        }
        Ok(DavinciDKGAdapter::new(a, self.provider()))
    }

    /// Publishes the organizer secret of a `DKG_LOCKED` process
    /// (`revealProcessKey`), after which the committee decrypts the tally.
    /// A wrong secret reverts `InvalidOrganizerSecret`.
    pub async fn reveal_process_key(&self, pid: &[u8; 31], sk: &OrganizerSecret) -> Result<()> {
        let call = self
            .registry
            .revealProcessKey(FixedBytes(*pid), U256::from_be_bytes(sk.to_be_bytes()));
        call.call().await.map_err(chain_err)?;
        self.send(call.into_transaction_request()).await?;
        Ok(())
    }

    /// Sets the process status to ENDED (only the organizer can, from the
    /// start on). Before the end it moves the end to now: votes already
    /// admitted settle through the grace window, and results follow it.
    pub async fn end_process(&self, pid: &[u8; 31]) -> Result<()> {
        self.set_status(pid, STATUS_ENDED).await
    }

    /// Sets the process status to CANCELED: no results will be set.
    pub async fn cancel_process(&self, pid: &[u8; 31]) -> Result<()> {
        self.set_status(pid, STATUS_CANCELED).await
    }

    /// Pauses a READY process before its end: nodes still take votes but
    /// settle nothing until it resumes. A process still paused at its end
    /// settles through the grace window.
    pub async fn pause_process(&self, pid: &[u8; 31]) -> Result<()> {
        self.set_status(pid, STATUS_PAUSED).await
    }

    /// Resumes a PAUSED process.
    pub async fn resume_process(&self, pid: &[u8; 31]) -> Result<()> {
        self.set_status(pid, STATUS_READY).await
    }

    /// Sets the duration, from the start time (`setProcessDuration`,
    /// organizer only, while READY or PAUSED and before the end). Extending is
    /// free; a shorter one must still end in the future and no earlier than
    /// now + `noticeMin` (see [`GraceParams`]), else `InvalidDuration`. The
    /// nodes flush during the notice and results follow the grace window.
    pub async fn set_process_duration(&self, pid: &[u8; 31], duration: u64) -> Result<()> {
        let call = self
            .registry
            .setProcessDuration(FixedBytes(*pid), U256::from(duration));
        call.call().await.map_err(chain_err)?;
        self.send(call.into_transaction_request()).await?;
        Ok(())
    }

    /// Sets the idle grace window after the end (`setProcessGrace`, organizer
    /// only, while READY or PAUSED and before the end), within the registry's
    /// `graceFloor..=graceCeil`, else `InvalidGrace`.
    pub async fn set_process_grace(&self, pid: &[u8; 31], secs: u32) -> Result<()> {
        let call = self.registry.setProcessGrace(FixedBytes(*pid), secs);
        call.call().await.map_err(chain_err)?;
        self.send(call.into_transaction_request()).await?;
        Ok(())
    }

    /// Sets max voters (`setProcessMaxVoters`, organizer only, while READY
    /// or PAUSED and before the end), never below the voters already counted.
    pub async fn set_process_max_voters(&self, pid: &[u8; 31], max_voters: u64) -> Result<()> {
        let call = self
            .registry
            .setProcessMaxVoters(FixedBytes(*pid), U256::from(max_voters));
        call.call().await.map_err(chain_err)?;
        self.send(call.into_transaction_request()).await?;
        Ok(())
    }

    /// The processes this organizer (or a clone) created, in order.
    pub fn created(&self) -> Vec<[u8; 31]> {
        self.created.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Cancels every created process still READY or PAUSED; returns those
    /// canceled. Tries them all; the first error is returned after.
    pub async fn cancel_open(&self) -> Result<Vec<[u8; 31]>> {
        let (mut done, mut first_err) = (Vec::new(), None);
        for pid in self.created() {
            let r = match self.process(&pid).await {
                Ok(p) if matches!(p.status, ProcessStatus::Ready | ProcessStatus::Paused) => {
                    self.cancel_process(&pid).await.map(|()| done.push(pid))
                }
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(done), Err)
    }

    async fn set_status(&self, pid: &[u8; 31], status: u8) -> Result<()> {
        let call = self.registry.setProcessStatus(FixedBytes(*pid), status);
        call.call().await.map_err(chain_err)?;
        self.send(call.into_transaction_request()).await?;
        Ok(())
    }

    /// Replaces an origin-2 census (`setProcessCensus`, organizer only) and
    /// waits for the receipt. The sequencers pick it up from `CensusUpdated`.
    pub async fn set_process_census(&self, pid: &[u8; 31], root: Fr, uri: &str) -> Result<()> {
        let census = DAVINCITypes::Census {
            censusOrigin: 2,
            censusRoot: B256::from(fr_to_be(&root)),
            contractAddress: Address::ZERO,
            censusURI: uri.to_string(),
            onchainAllowAnyValidRoot: false,
        };
        let call = self.registry.setProcessCensus(FixedBytes(*pid), census);
        call.call().await.map_err(chain_err)?;
        self.send(call.into_transaction_request()).await?;
        Ok(())
    }

    /// Replaces the metadata URI and hash (`setProcessMetadata`, organizer
    /// only, while READY or PAUSED and before the end) and waits for the
    /// receipt. `hash` is [`metadata_hash`] of the document at `uri`.
    pub async fn set_process_metadata(
        &self,
        pid: &[u8; 31],
        uri: &str,
        hash: [u8; 32],
    ) -> Result<()> {
        let (pid, hash) = (FixedBytes(*pid), B256::from(hash));
        let call = self.registry.setProcessMetadata(pid, uri.to_string(), hash);
        call.call().await.map_err(chain_err)?;
        self.send(call.into_transaction_request()).await?;
        Ok(())
    }

    pub async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess> {
        read_process(&self.registry, pid).await
    }

    /// The tally once the results are on-chain (status RESULTS), else `None`.
    /// Results land only after the grace window ([`Organizer::grace_end`]).
    pub async fn results(&self, pid: &[u8; 31]) -> Result<Option<Vec<u64>>> {
        let p = self.process(pid).await?;
        Ok((p.status == ProcessStatus::Results).then_some(p.result))
    }

    pub async fn grace_end(&self, pid: &[u8; 31]) -> Result<u64> {
        read_grace_end(&self.registry, pid).await
    }

    pub async fn grace_params(&self) -> Result<GraceParams> {
        read_grace_params(&self.registry).await
    }
}

/// The registry's grace immutables, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraceParams {
    /// Grace window of a new process.
    pub default_grace: u32,
    /// `setProcessGrace` bounds.
    pub grace_floor: u32,
    pub grace_ceil: u32,
    /// The window never closes later than end + this.
    pub grace_max_total: u32,
    /// Least notice a shortened end gives.
    pub notice_min: u32,
}

// Block time the window closes at: min(end + graceMaxTotal,
// max(end, lastVoteAt) + grace). It moves forward with every landing.
async fn read_grace_end(
    registry: &ProcessRegistryInstance<DynProvider>,
    pid: &[u8; 31],
) -> Result<u64> {
    let end = registry
        .getProcessGraceEnd(FixedBytes(*pid))
        .call()
        .await
        .map_err(chain_err)?;
    u64_of("graceEnd", end)
}

async fn read_grace_params(registry: &ProcessRegistryInstance<DynProvider>) -> Result<GraceParams> {
    Ok(GraceParams {
        default_grace: registry.defaultGrace().call().await.map_err(chain_err)?,
        grace_floor: registry.graceFloor().call().await.map_err(chain_err)?,
        grace_ceil: registry.graceCeil().call().await.map_err(chain_err)?,
        grace_max_total: registry.graceMaxTotal().call().await.map_err(chain_err)?,
        notice_min: registry.noticeMin().call().await.map_err(chain_err)?,
    })
}

/// What [`verify_registry`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistryInfo {
    pub chain_id: u64,
    pub verifier: Address,
    /// The DKG adapter the registry created, `None` when the DKG key modes
    /// are disabled. Callers creating DKG processes must require it.
    pub dkg_adapter: Option<Address>,
}

fn pin(
    field: &'static str,
    expected: impl std::fmt::Display,
    got: impl std::fmt::Display,
) -> Error {
    Error::Pin {
        field,
        expected: expected.to_string(),
        got: got.to_string(),
    }
}

/// Checks that `registry` settles what this release proves: the batch and
/// results program vks, `rootCVadcopFinal`, the ballot VK hash of the embedded
/// VK, `chainID` equal to the RPC's, and a verifier whose root and runtime code
/// are the pinned ones. A DKG adapter, if any, must name this registry. The
/// first mismatch comes back as [`Error::Pin`].
pub async fn verify_registry(rpc_url: &str, registry: Address) -> Result<RegistryInfo> {
    let provider = read_provider(rpc_url)?;
    let rpc = |e: alloy::transports::TransportError| Error::Chain(e.to_string());
    let r = ProcessRegistry::new(registry, &provider);
    let vk_hash = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?.vk_hash();
    let pins = [
        (
            "batchProgramVK",
            r.batchProgramVK().call().await,
            release::BATCH_PROGRAM_VK,
        ),
        (
            "resultsProgramVK",
            r.resultsProgramVK().call().await,
            release::RESULTS_PROGRAM_VK,
        ),
        (
            "rootCVadcopFinal",
            r.rootCVadcopFinal().call().await,
            release::ROOT_C_VADCOP_FINAL,
        ),
        ("ballotVKHash", r.ballotVKHash().call().await, vk_hash),
    ];
    for (field, got, expected) in pins {
        let got = got.map_err(chain_err)?;
        if got != B256::from(expected) {
            return Err(pin(field, B256::from(expected), got));
        }
    }
    let chain_id = provider.get_chain_id().await.map_err(rpc)?;
    let pinned = r.chainID().call().await.map_err(chain_err)?;
    if u64::from(pinned) != chain_id {
        return Err(pin("chainID", chain_id, pinned));
    }
    let verifier = r.ziskVerifier().call().await.map_err(chain_err)?;
    let code = provider.get_code_at(verifier).await.map_err(rpc)?;
    let hash = keccak256(&code);
    if hash != B256::from(release::ZISK_VERIFIER_CODEHASH) {
        return Err(pin(
            "verifier code hash",
            B256::from(release::ZISK_VERIFIER_CODEHASH),
            hash,
        ));
    }
    let root_c = IZiskVerifier::new(verifier, &provider)
        .getRootCVadcopFinal()
        .call()
        .await
        .map_err(|e| Error::Chain(e.to_string()))?;
    if root_c != B256::from(release::ROOT_C_VADCOP_FINAL) {
        return Err(pin(
            "verifier rootCVadcopFinal",
            B256::from(release::ROOT_C_VADCOP_FINAL),
            root_c,
        ));
    }
    let adapter = r.dkgAdapter().call().await.map_err(chain_err)?;
    let dkg_adapter = if adapter == Address::ZERO {
        None
    } else {
        let back = DavinciDKGAdapter::new(adapter, &provider)
            .registry()
            .call()
            .await
            .map_err(chain_err)?;
        if back != registry {
            return Err(pin("dkgAdapter.registry", registry, back));
        }
        Some(adapter)
    };
    Ok(RegistryInfo {
        chain_id,
        verifier,
        dkg_adapter,
    })
}

/// Name of a registry, verifier, DKG adapter or DKG custom error in `data`
/// (revert data), or its selector in hex; `"empty revert"` for none.
pub fn revert_name(data: &[u8]) -> String {
    use alloy::sol_types::{SolError, SolInterface};
    if data.is_empty() {
        return "empty revert".into();
    }
    if let Ok(e) = ProcessRegistryErrors::abi_decode(data) {
        return err_name(e);
    }
    if let Ok(e) = DavinciDKGAdapterErrors::abi_decode(data) {
        return err_name(e);
    }
    if let Ok(e) = IDKG::IDKGErrors::abi_decode(data) {
        return err_name(e);
    }
    if data.starts_with(&IZiskVerifier::InvalidProof::SELECTOR) {
        return "InvalidProof".into();
    }
    format!("0x{}", hex::encode(&data[..data.len().min(4)]))
}

/// Registry reads with no account: what a voter uses to learn the election
/// parameters instead of trusting a sequencer.
#[derive(Clone, Debug)]
pub struct RegistryReader {
    registry: ProcessRegistryInstance<DynProvider>,
}

impl RegistryReader {
    pub fn connect(rpc_url: &str, registry: Address) -> Result<Self> {
        let provider = read_provider(rpc_url)?;
        Ok(RegistryReader {
            registry: ProcessRegistry::new(registry, provider),
        })
    }

    pub async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess> {
        read_process(&self.registry, pid).await
    }

    pub async fn grace_end(&self, pid: &[u8; 31]) -> Result<u64> {
        read_grace_end(&self.registry, pid).await
    }

    pub async fn grace_params(&self) -> Result<GraceParams> {
        read_grace_params(&self.registry).await
    }
}

async fn read_process(
    registry: &ProcessRegistryInstance<DynProvider>,
    pid: &[u8; 31],
) -> Result<OnchainProcess> {
    let p = registry
        .getProcess(FixedBytes(*pid))
        .call()
        .await
        .map_err(chain_err)?;
    if p.organizationId == Address::ZERO {
        return Err(Error::Chain("process not found".into()));
    }
    let status = ProcessStatus::from_onchain(p.status)
        .ok_or_else(|| Error::Chain(format!("unknown status {}", p.status)))?;
    let encryption_key = Point::from_be(
        &p.encryptionKey.x.to_be_bytes::<32>(),
        &p.encryptionKey.y.to_be_bytes::<32>(),
    )?;
    let bm = &p.ballotMode;
    let ballot_mode = BallotMode {
        num_fields: bm.numFields,
        group_size: bm.groupSize,
        unique_values: bm.uniqueValues,
        cost_exponent: bm.costExponent,
        max_value: u64_of("maxValue", bm.maxValue)?,
        min_value: u64_of("minValue", bm.minValue)?,
        max_value_sum: u64_of("maxValueSum", bm.maxValueSum)?,
        min_value_sum: u64_of("minValueSum", bm.minValueSum)?,
    };
    let dkg = match p.keyMode {
        0 => None,
        m @ (1 | 2) => Some(DkgProcess {
            locked: m == 2,
            epoch_id: p.dkgEpochId.0,
            aid: p.dkgAid.0,
            results_requested: p.dkgResultsRequested,
            first_index: p.dkgFirstIndex,
            count: p.dkgCount,
            zero_skipped: p.dkgZeroSkipped,
        }),
        m => return Err(Error::Chain(format!("unknown key mode {m}"))),
    };
    Ok(OnchainProcess {
        id: *pid,
        status,
        organization_id: p.organizationId.0.0,
        encryption_key,
        state_root: p.latestStateRoot.0,
        result: p
            .result
            .iter()
            .map(|r| u64_of("result", *r))
            .collect::<Result<_>>()?,
        start_time: u64_of("startTime", p.startTime)?,
        duration: u64_of("duration", p.duration)?,
        max_voters: u64_of("maxVoters", p.maxVoters)?,
        voters_count: u64_of("votersCount", p.votersCount)?,
        overwritten_votes_count: u64_of("overwrittenVotesCount", p.overwrittenVotesCount)?,
        ballot_mode,
        census_origin: p.census.censusOrigin,
        census_root: fr_of("censusRoot", p.census.censusRoot.into())?,
        census_contract: p.census.contractAddress.0.0,
        census_uri: p.census.censusURI.clone(),
        metadata_uri: p.metadataURI.clone(),
        metadata_hash: p.metadataHash.0,
        dkg,
        grace: p.window.grace.into(),
        last_vote_at: p.window.lastVoteAt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn rate_limit_retries_outlast_a_public_rpc_refusal() {
        assert!(u64::from(RATE_LIMIT_RETRIES) * RATE_LIMIT_BACKOFF_MS >= 150_000);
    }

    // One HTTP request off `s`: its JSON-RPC id.
    async fn read_id(s: &mut tokio::net::TcpStream) -> serde_json::Value {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed mid-request");
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            let Some((head, body)) = text.split_once("\r\n\r\n") else {
                continue;
            };
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if body.len() >= len {
                let req: serde_json::Value = serde_json::from_str(&body[..len]).unwrap();
                return req["id"].clone();
            }
        }
    }

    #[tokio::test]
    async fn rides_out_a_rate_limit() {
        // Refuses twice the way a busy public RPC does, asking for an hour's
        // pause, then answers.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let seen = Arc::new(AtomicU32::new(0));
        let count = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let id = read_id(&mut s).await;
                let (status, body) = if count.fetch_add(1, Ordering::SeqCst) < 2 {
                    (
                        "429 Too Many Requests",
                        "<title>429</title>429 Too Many Requests".to_string(),
                    )
                } else {
                    let r = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": "0x10"});
                    ("200 OK", r.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nretry-after: 3600\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        let p = ProviderBuilder::new().connect_client(rpc_client(&url, "test").unwrap());
        let n = tokio::time::timeout(Duration::from_secs(20), p.get_block_number())
            .await
            .expect("the retries honoured the hour-long Retry-After");
        assert_eq!(n.unwrap(), 16);
        assert_eq!(seen.load(Ordering::SeqCst), 3);
    }
}
