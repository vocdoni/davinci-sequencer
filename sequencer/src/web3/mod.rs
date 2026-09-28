//! Chain layer: ProcessRegistry reads, event polling, blob-carrying
//! settlement transactions with a pre-flight `eth_call`, and blob retrieval.

mod blobs;
mod contracts;
mod failover;
mod release;
mod tx;

use std::fmt;

use alloy::primitives::{Address, B256, Bytes};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use serde::{Deserialize, Serialize};

pub use blobs::{
    AnvilBlobs, BeaconBlobs, BlobSource, blob_source, match_blobs, match_blobs_blocking,
};
pub use contracts::{
    Contracts, MAX_LOG_RANGE, endpoints_chain_id as rpc_chain_id, probe_osaka, resolve_blob_cap,
};
use failover::Failover;
pub(crate) use failover::host;
pub use release::{DeployedRelease, Mismatch};

/// `User-Agent` of every outgoing HTTP request: some public RPCs answer 403
/// without one.
pub const USER_AGENT: &str = concat!("davinci-sequencer/", env!("CARGO_PKG_VERSION"));

/// Provider over `urls` with sticky failover ([`Failover`]), sending
/// [`USER_AGENT`]. Transactions are signed by the caller.
pub fn rpc_provider(urls: &[url::Url]) -> alloy::providers::DynProvider {
    failover_provider(urls).0
}

/// [`rpc_provider`] with its failover switch counter.
pub(crate) fn failover_provider(
    urls: &[url::Url],
) -> (
    alloy::providers::DynProvider,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use alloy::providers::{Provider, ProviderBuilder};
    let local = urls
        .iter()
        .all(|u| alloy::transports::utils::guess_local_url(u.as_str()));
    let f = Failover::new(rpc_http_client(), urls);
    let epoch = f.epoch();
    let client = alloy::rpc::client::RpcClient::new(f, local);
    let p = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_client(client)
        .erased();
    (p, epoch)
}

/// The reqwest client under [`Failover`], with [`USER_AGENT`] and timeouts,
/// so a hung endpoint fails over instead of stalling.
pub(crate) fn rpc_http_client() -> alloy::transports::http::reqwest::Client {
    alloy::transports::http::reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| alloy::transports::http::reqwest::Client::new())
}

alloy::sol!(
    #[allow(missing_docs, clippy::too_many_arguments)]
    #[derive(Debug)]
    ProcessRegistry,
    "abi/ProcessRegistry.json"
);

alloy::sol!(
    #[allow(missing_docs)]
    #[derive(Debug)]
    ZiskVerifier,
    "abi/ZiskVerifier.json"
);

/// Own module: the adapter ABI declares its own `DAVINCITypes`.
pub(crate) mod adapter {
    alloy::sol!(
        #[allow(missing_docs)]
        #[derive(Debug)]
        DavinciDKGAdapter,
        "abi/DavinciDKGAdapter.json"
    );
}

/// Where a process's election key comes from (`DAVINCITypes.KeyMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    /// A sequencer holds the key and proves the results (results guest).
    #[default]
    Sequencer = 0,
    /// A davinci-dkg pool key; the committee decrypts the tally.
    DkgAutomatic = 1,
    /// Pool key plus organizer key; decryption waits for the reveal.
    DkgLocked = 2,
}

impl TryFrom<u8> for KeyMode {
    type Error = Web3Error;
    fn try_from(v: u8) -> Result<Self, Web3Error> {
        Ok(match v {
            0 => KeyMode::Sequencer,
            1 => KeyMode::DkgAutomatic,
            2 => KeyMode::DkgLocked,
            _ => return Err(Web3Error::Data(format!("key mode {v}"))),
        })
    }
}

/// The registry's DKG bookkeeping of a process; zero in SEQUENCER mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkgState {
    #[serde(with = "hex::serde")]
    pub epoch_id: [u8; 12],
    #[serde(with = "hex::serde")]
    pub aid: [u8; 32],
    /// `requestResultsDecryption` ran (also when every field was identity).
    pub requested: bool,
    /// DKG index of the first submitted ciphertext.
    pub first_index: u16,
    /// Ciphertexts submitted; identity fields are skipped.
    pub count: u8,
}

/// Registry process status (`DAVINCITypes.ProcessStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessStatus {
    Ready = 0,
    Ended = 1,
    Canceled = 2,
    Paused = 3,
    Results = 4,
    /// Never read from chain; placeholder for a status we could not fetch.
    Unknown = 255,
}

impl TryFrom<u8> for ProcessStatus {
    type Error = Web3Error;
    fn try_from(v: u8) -> Result<Self, Web3Error> {
        Ok(match v {
            0 => ProcessStatus::Ready,
            1 => ProcessStatus::Ended,
            2 => ProcessStatus::Canceled,
            3 => ProcessStatus::Paused,
            4 => ProcessStatus::Results,
            _ => return Err(Web3Error::Data(format!("process status {v}"))),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnchainCensus {
    /// 1 = static Merkle (lean-IMT), 2 = off-chain dynamic, 3 = on-chain
    /// dynamic, 4 = CSP.
    pub origin: u8,
    /// `censusRoot` as stored: the BE root integer, or the CSP address as uint160.
    #[serde(with = "hex::serde")]
    pub root: [u8; 32],
    pub uri: String,
    /// The census contract (origin 3); zero otherwise.
    #[serde(with = "hex::serde")]
    pub contract_address: [u8; 20],
}

/// `getProcess` in node types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnchainProcess {
    pub status: ProcessStatus,
    #[serde(with = "hex::serde")]
    pub organizer: [u8; 20],
    pub enc_key: Point,
    /// Raw arbo digest of the latest settled root.
    #[serde(with = "hex::serde")]
    pub state_root: [u8; 32],
    pub results: Vec<u64>,
    /// Times and counters saturate at `u64::MAX`.
    pub start_time: u64,
    pub duration: u64,
    pub max_voters: u64,
    pub voters_count: u64,
    pub overwritten_count: u64,
    pub creation_block: u64,
    pub batch_number: u64,
    pub metadata_uri: String,
    pub ballot_mode: BallotMode,
    pub census: OnchainCensus,
    pub key_mode: KeyMode,
    pub dkg: DkgState,
}

impl OnchainProcess {
    /// `start + duration`, saturating.
    pub fn end_time(&self) -> u64 {
        self.start_time.saturating_add(self.duration)
    }
}

/// Parameters of `newProcess`.
#[derive(Clone, Debug)]
pub struct NewProcess {
    pub status: ProcessStatus,
    /// 0 = the block timestamp.
    pub start_time: u64,
    pub duration: u64,
    pub max_voters: u64,
    pub ballot_mode: BallotMode,
    pub census: OnchainCensus,
    pub metadata: String,
    pub enc_key: Point,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    ProcessCreated {
        pid: [u8; 31],
        creator: Address,
    },
    StatusChanged {
        pid: [u8; 31],
        old: ProcessStatus,
        new: ProcessStatus,
    },
    DurationChanged {
        pid: [u8; 31],
        duration: u64,
    },
    MaxVotersChanged {
        pid: [u8; 31],
        max_voters: u64,
    },
    /// `voters` and `overwrites` are the process totals after the transition.
    StateTransitioned {
        pid: [u8; 31],
        sender: Address,
        old_root: [u8; 32],
        new_root: [u8; 32],
        voters: u64,
        overwrites: u64,
        n_blobs: u64,
    },
    ResultsSet {
        pid: [u8; 31],
        sender: Address,
        results: Vec<u64>,
    },
    /// Origin 2: the organizer replaced the census (`root` as stored, BE).
    CensusUpdated {
        pid: [u8; 31],
        root: [u8; 32],
        uri: String,
    },
    /// DKG modes: the final accumulator went to the committee.
    ResultsDecryptionRequested {
        pid: [u8; 31],
        epoch_id: [u8; 12],
        aid: [u8; 32],
        first_index: u16,
        count: u8,
    },
}

impl EventKind {
    pub fn pid(&self) -> &[u8; 31] {
        match self {
            EventKind::ProcessCreated { pid, .. }
            | EventKind::StatusChanged { pid, .. }
            | EventKind::DurationChanged { pid, .. }
            | EventKind::MaxVotersChanged { pid, .. }
            | EventKind::StateTransitioned { pid, .. }
            | EventKind::ResultsSet { pid, .. }
            | EventKind::CensusUpdated { pid, .. }
            | EventKind::ResultsDecryptionRequested { pid, .. } => pid,
        }
    }
}

/// A registry event with where it happened, in chain order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryEvent {
    pub block: u64,
    pub tx_hash: B256,
    pub log_index: u64,
    pub kind: EventKind,
}

/// A mined, successful transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxReceipt {
    pub tx_hash: B256,
    pub block: u64,
    pub gas_used: u64,
    /// Fee-bumped resends before this one was mined.
    pub replacements: u32,
}

/// Why a call or transaction did not go through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevertReason {
    /// The EVM reverted. `name` is the registry custom error (`InvalidStateRoot`,
    /// ...), `Error`/`Panic` for the builtin ones, or `unknown`.
    Revert { name: String, data: Bytes },
    /// No revert data: a node or transport failure.
    Rpc(String),
}

impl RevertReason {
    /// The error name, when the EVM reverted.
    pub fn name(&self) -> Option<&str> {
        match self {
            RevertReason::Revert { name, .. } => Some(name),
            RevertReason::Rpc(_) => None,
        }
    }
}

impl fmt::Display for RevertReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RevertReason::Revert { name, data } => write!(f, "reverted with {name} ({data})"),
            RevertReason::Rpc(e) => write!(f, "rpc: {e}"),
        }
    }
}

impl std::error::Error for RevertReason {}

#[derive(Debug, thiserror::Error)]
pub enum Web3Error {
    #[error("rpc: {0}")]
    Rpc(String),
    /// The EVM reverted (at estimation or mined); `RevertReason::name()`
    /// says why. Retrying the same call will not help.
    #[error("{0}")]
    Revert(RevertReason),
    /// The node refused the fees even after bumping; retry later.
    #[error("underpriced: {0}")]
    Underpriced(String),
    /// Another send held the node's key for too long; retry later.
    #[error("sender busy")]
    Busy,
    /// No receipt after every replacement. The tx may still land; the next
    /// send replaces it at the same nonce.
    #[error("tx at nonce {nonce} not mined after {attempts} attempts")]
    Stuck { nonce: u64, attempts: u32 },
    #[error("no signing key: the node is an observer")]
    NoSigner,
    #[error("unexpected chain data: {0}")]
    Data(String),
    #[error("blobs: {0}")]
    Blob(String),
    #[error("config: {0}")]
    Config(String),
    /// The registry or its verifier is not the pinned release.
    #[error("registry is not the pinned release: {}", release::describe(.0))]
    Release(Vec<Mismatch>),
}

pub type Result<T, E = Web3Error> = std::result::Result<T, E>;

pub(crate) fn rpc_err(e: impl fmt::Display) -> Web3Error {
    Web3Error::Rpc(e.to_string())
}
