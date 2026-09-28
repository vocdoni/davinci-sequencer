//! Organizer helper: the census file and the process lifecycle on the zkVM
//! ProcessRegistry (create in any key mode, end, reveal a DKG organizer key,
//! read results), and the registry pin check.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy::providers::{DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy::rpc::client::{ClientBuilder, RpcClient};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::transports::http::{Http, reqwest};
use alloy::transports::layers::RetryBackoffLayer;
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{LeanImt, census_leaf};
use davinci_zkvm_sdk::crypto::babyjubjub::{Point, SUBGROUP_ORDER};
use davinci_zkvm_sdk::crypto::field::{
    U256 as SdkU256, fr_from_be, fr_to_be, u256_from_be, u256_to_be,
};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::{dkg, release};
use rand::rngs::OsRng;

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
        error NotRegistrar();
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
        .layer(RetryBackoffLayer::new(10, 1000, 10_000))
        .transport(Http::with_client(http, url), is_local))
}

/// Read-only provider sending [`USER_AGENT`].
fn read_provider(url: &str) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .connect_client(rpc_client(url, USER_AGENT)?)
        .erased())
}

/// `DAVINCITypes.ProcessStatus` ENDED and CANCELED.
const STATUS_ENDED: u8 = 1;
const STATUS_CANCELED: u8 = 2;

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
    pub metadata: String,
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
    /// `None` for a sequencer-key process.
    pub dkg: Option<DkgProcess>,
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

/// Sends registry transactions from the organizer account.
#[derive(Clone, Debug)]
pub struct Organizer {
    registry: ProcessRegistryInstance<DynProvider>,
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
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_client(rpc_client(url, USER_AGENT)?)
            .erased();
        Ok(Organizer {
            registry: ProcessRegistry::new(registry, provider),
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
        use alloy::rpc::types::TransactionRequest;
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
            key,
            dkg,
        );
        // Simulate first so a revert comes back with its name.
        call.call().await.map_err(chain_err)?;
        let receipt = self.mined(call.send().await.map_err(chain_err)?).await?;
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
        self.mined(call.send().await.map_err(chain_err)?).await?;
        Ok(())
    }

    /// Sets the process status to ENDED (only the organizer can).
    pub async fn end_process(&self, pid: &[u8; 31]) -> Result<()> {
        self.set_status(pid, STATUS_ENDED).await
    }

    /// Sets the process status to CANCELED: no results will be set.
    pub async fn cancel_process(&self, pid: &[u8; 31]) -> Result<()> {
        self.set_status(pid, STATUS_CANCELED).await
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
        self.mined(call.send().await.map_err(chain_err)?).await?;
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
        self.mined(call.send().await.map_err(chain_err)?).await?;
        Ok(())
    }

    pub async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess> {
        read_process(&self.registry, pid).await
    }

    /// The tally once the results are on-chain (status RESULTS), else `None`.
    pub async fn results(&self, pid: &[u8; 31]) -> Result<Option<Vec<u64>>> {
        let p = self.process(pid).await?;
        Ok((p.status == ProcessStatus::Results).then_some(p.result))
    }
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
        dkg,
    })
}
