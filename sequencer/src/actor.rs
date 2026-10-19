//! One actor per election: the single writer over its `ProcessState`.
//! It admits votes, seals batches, runs the prove→check→settle job, syncs
//! transitions other sequencers settled, and hands the finalizer its
//! results request. Everything mutating goes through the actor's mailbox.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::Address;
use async_trait::async_trait;
use davinci_state::{
    CensusOrigin, Error as StateError, ProcessConfig, ProcessState, VerifiedVote, VotePackage,
    genesis_root, vote_still_valid,
};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{TransitionBlobs, decode_blobs};
use davinci_zkvm_sdk::census::{CensusProof, CensusWitness, CspProof, EcdsaSignature};
use davinci_zkvm_sdk::client::PlonkSnark;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_to_be};
use davinci_zkvm_sdk::limits::{MAX_BATCH_SIZE, MAX_REFRESH, refresh_target};
use davinci_zkvm_sdk::publics::{BatchPublics, ResultsPublics, fail_bits};
use davinci_zkvm_sdk::types::{ProveRequest, ResultsRequest, SnarkJsProof};
use davinci_zkvm_sdk::{blob, release};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::keys::KeyStore;
use crate::metrics::Metrics;
use crate::monitor::Anchor;
use crate::storage::{Db, LocalStatus, ProcessRecord, StoredVote, VoteStatus, fr_hex};
use crate::web3::l1proof::L1BlobProof;
use crate::web3::{
    BlobSource, EventKind, GraceParams, KeyMode, L1Blobs, L1Origin, OnchainProcess, ProcessStatus,
    RegistryEvent, RevertReason, TxReceipt, Web3Error,
};

/// Votes an actor holds in memory before sealing; beyond it submits are refused.
const PENDING_CAP: usize = 16 * 1024;

/// How often a DKG-mode finalizer checks for the committee's plaintexts.
const DKG_POLL: std::time::Duration = std::time::Duration::from_secs(15);
/// Slowest pace of the check on a closed Council decryption gate, which can
/// stay closed for months: from [`DKG_POLL`], doubling up to this.
const GATE_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(300);

/// The wait after the `n`-th consecutive closed-gate read (1-based).
fn gate_backoff(n: u32) -> std::time::Duration {
    DKG_POLL
        .saturating_mul(1 << n.saturating_sub(1).min(16))
        .min(GATE_POLL_MAX)
}

fn not_l1() -> Web3Error {
    Web3Error::Config("the registry is not in L1-blob mode".into())
}

// ------------------------------------------------------------------ traits

/// The chain operations the node needs; `Contracts` implements it, tests
/// script it.
#[async_trait]
pub trait Chain: Send + Sync {
    fn signer(&self) -> Option<Address>;
    /// EVM chain id; 0 when unknown (test doubles).
    fn chain_id(&self) -> u64 {
        0
    }
    async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess, Web3Error>;
    /// `getProcess` at `block`. Required, so a wrapper cannot fall back to
    /// the unpinned read.
    async fn process_at(&self, pid: &[u8; 31], block: u64) -> Result<OnchainProcess, Web3Error>;
    /// `getProcess` at the block with this hash (EIP-1898): a backend that
    /// does not have that block errors instead of answering from its own.
    async fn process_at_hash(
        &self,
        pid: &[u8; 31],
        hash: [u8; 32],
    ) -> Result<OnchainProcess, Web3Error>;
    async fn events(&self, from: u64, to: u64) -> Result<Vec<RegistryEvent>, Web3Error>;
    /// `(block, timestamp)` of the head.
    async fn head(&self) -> Result<(u64, u64), Web3Error>;
    async fn confirmed_head(&self, confirmations: u64) -> Result<u64, Web3Error>;
    /// Hash and timestamp of block `n`; `None` when unknown (past the
    /// endpoint's head). The monitor counts it as a failed read: after a few
    /// in a row it routes on without the reorg check.
    async fn block_ref(&self, n: u64) -> Result<Option<([u8; 32], u64)>, Web3Error>;
    async fn simulate_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<(), RevertReason>;
    async fn submit_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<TxReceipt, Web3Error>;
    async fn submit_results(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
    ) -> Result<TxReceipt, Web3Error>;
    /// DKG modes: `requestResultsDecryption` (64 BE coordinates, siblings
    /// root to leaf).
    async fn request_results_decryption(
        &self,
        pid: &[u8; 31],
        accumulator: &[[u8; 32]; 64],
        siblings: &[[u8; 32]],
    ) -> Result<TxReceipt, Web3Error>;
    /// DKG modes: every submitted ciphertext has a combined plaintext (and,
    /// for COUNCIL, the ceremony's decryption gate is open).
    async fn dkg_results_ready(&self, pid: &[u8; 31]) -> Result<bool, Web3Error>;
    /// COUNCIL: whether ceremony `cid` has opened decryption (the Council
    /// adapter's `isDecryptionOpen`).
    async fn council_decryption_open(&self, cid: &[u8; 12]) -> Result<bool, Web3Error>;
    /// DKG modes: `finalizeResultsFromDKG`.
    async fn finalize_results_from_dkg(&self, pid: &[u8; 31]) -> Result<TxReceipt, Web3Error>;
    /// L1-blob mode: OP's L1 origin (`L1Block` read at one block).
    async fn l1_origin(&self) -> Result<L1Origin, Web3Error> {
        Err(not_l1())
    }
    /// L1-blob mode: `record()` on the verifier; the origin it stored.
    async fn record_l1_origin(&self) -> Result<L1Origin, Web3Error> {
        Err(not_l1())
    }
    /// L1-blob mode: the pre-flight `eth_call` of `submitStateTransitionL1`.
    async fn simulate_transition_l1(
        &self,
        _pid: &[u8; 31],
        _snark: &PlonkSnark,
        _blobs: &TransitionBlobs,
        _proof: &L1BlobProof,
    ) -> Result<(), RevertReason> {
        Err(RevertReason::Rpc(not_l1().to_string()))
    }
    /// L1-blob mode: `submitStateTransitionL1`.
    async fn submit_transition_l1(
        &self,
        _pid: &[u8; 31],
        _snark: &PlonkSnark,
        _blobs: &TransitionBlobs,
        _proof: &L1BlobProof,
    ) -> Result<TxReceipt, Web3Error> {
        Err(not_l1())
    }
}

/// `permanent` marks failures retrying cannot fix: the proving job itself
/// reported `failed`, or a done job returned malformed publics. Transport
/// and queue trouble stays transient.
#[derive(Debug, thiserror::Error)]
#[error("prover: {msg}")]
pub struct ProverError {
    pub msg: String,
    pub permanent: bool,
}

impl ProverError {
    pub fn transient(msg: impl Into<String>) -> Self {
        ProverError {
            msg: msg.into(),
            permanent: false,
        }
    }

    pub fn permanent(msg: impl Into<String>) -> Self {
        ProverError {
            msg: msg.into(),
            permanent: true,
        }
    }
}

/// The proving backend; the HTTP `ProverClient` in production, a fake in
/// tests. `expected` is advisory (fakes echo it), the real prover ignores it.
#[async_trait]
pub trait Prover: Send + Sync {
    /// Also returns the job's own proving seconds (queue wait excluded)
    /// when the prover reports them.
    async fn prove_batch(
        &self,
        request: &ProveRequest,
        expected: &BatchPublics,
    ) -> Result<(BatchPublics, PlonkSnark, Option<f64>), ProverError>;
    async fn prove_results(
        &self,
        request: &ResultsRequest,
    ) -> Result<(ResultsPublics, PlonkSnark), ProverError>;
}

/// Chain time source (the fake clock follows the scripted chain).
pub trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Census lookups the actor needs at seal time (dynamic origins).
#[derive(Clone)]
pub struct CensusAccess {
    /// Origin-1/2 fetched censuses.
    pub store: Arc<crate::census::CensusStore>,
    /// Origin-3 census contracts.
    pub onchain: Arc<crate::census::onchain::OnchainIndex>,
    /// Origin-2 downloads (the monitor's queue).
    pub queue: crate::monitor::CensusQueue,
}

/// Everything an actor talks to.
#[derive(Clone)]
pub struct Deps {
    pub db: Db,
    pub contracts: Arc<dyn Chain>,
    pub prover: Arc<dyn Prover>,
    pub blobs: Arc<dyn BlobSource>,
    pub clock: Arc<dyn Clock>,
    /// The registry's grace immutables.
    pub grace: GraceParams,
    /// Actor and monitor tasks; `api::run` awaits it on shutdown so the
    /// database handle is released before the process exits.
    pub tasks: tokio_util::task::TaskTracker,
    /// L1-blob mode: the Ethereum side (poster, proofs); `None` in blob mode.
    pub l1: Option<Arc<crate::l1da::L1Da>>,
}

#[async_trait]
impl Chain for crate::web3::Contracts {
    fn signer(&self) -> Option<Address> {
        crate::web3::Contracts::signer(self)
    }
    fn chain_id(&self) -> u64 {
        crate::web3::Contracts::chain_id(self)
    }
    async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess, Web3Error> {
        crate::web3::Contracts::process(self, pid).await
    }
    async fn process_at(&self, pid: &[u8; 31], block: u64) -> Result<OnchainProcess, Web3Error> {
        crate::web3::Contracts::process_at(self, pid, block).await
    }
    async fn process_at_hash(
        &self,
        pid: &[u8; 31],
        hash: [u8; 32],
    ) -> Result<OnchainProcess, Web3Error> {
        crate::web3::Contracts::process_at_hash(self, pid, hash).await
    }
    async fn events(&self, from: u64, to: u64) -> Result<Vec<RegistryEvent>, Web3Error> {
        crate::web3::Contracts::events(self, from, to).await
    }
    async fn head(&self) -> Result<(u64, u64), Web3Error> {
        crate::web3::Contracts::head(self).await
    }
    async fn confirmed_head(&self, confirmations: u64) -> Result<u64, Web3Error> {
        crate::web3::Contracts::confirmed_head(self, confirmations).await
    }
    async fn block_ref(&self, n: u64) -> Result<Option<([u8; 32], u64)>, Web3Error> {
        crate::web3::Contracts::block_ref(self, n).await
    }
    async fn simulate_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<(), RevertReason> {
        crate::web3::Contracts::simulate_transition(self, pid, snark, blobs).await
    }
    async fn submit_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<TxReceipt, Web3Error> {
        crate::web3::Contracts::submit_transition(self, pid, snark, blobs).await
    }
    async fn submit_results(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
    ) -> Result<TxReceipt, Web3Error> {
        crate::web3::Contracts::submit_results(self, pid, snark).await
    }
    async fn request_results_decryption(
        &self,
        pid: &[u8; 31],
        accumulator: &[[u8; 32]; 64],
        siblings: &[[u8; 32]],
    ) -> Result<TxReceipt, Web3Error> {
        crate::web3::Contracts::request_results_decryption(self, pid, accumulator, siblings).await
    }
    async fn dkg_results_ready(&self, pid: &[u8; 31]) -> Result<bool, Web3Error> {
        crate::web3::Contracts::dkg_results_ready(self, pid).await
    }
    async fn council_decryption_open(&self, cid: &[u8; 12]) -> Result<bool, Web3Error> {
        crate::web3::Contracts::council_decryption_open(self, cid).await
    }
    async fn finalize_results_from_dkg(&self, pid: &[u8; 31]) -> Result<TxReceipt, Web3Error> {
        crate::web3::Contracts::finalize_results_from_dkg(self, pid).await
    }
    async fn l1_origin(&self) -> Result<L1Origin, Web3Error> {
        crate::web3::Contracts::l1_origin(self).await
    }
    async fn record_l1_origin(&self) -> Result<L1Origin, Web3Error> {
        crate::web3::Contracts::record_l1_origin(self).await
    }
    async fn simulate_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<(), RevertReason> {
        crate::web3::Contracts::simulate_transition_l1(self, pid, snark, blobs, proof).await
    }
    async fn submit_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<TxReceipt, Web3Error> {
        crate::web3::Contracts::submit_transition_l1(self, pid, snark, blobs, proof).await
    }
}

/// The davinci-zkvm HTTP prover as a [`Prover`].
pub struct ProverService {
    client: davinci_zkvm_sdk::client::ProverClient,
    /// For `DELETE /jobs/{id}`, which the pinned SDK has no call for.
    base: String,
    http: reqwest::Client,
    poll: std::time::Duration,
    timeout: std::time::Duration,
}

impl ProverService {
    pub fn new(base_url: &str, poll: std::time::Duration, timeout: std::time::Duration) -> Self {
        let http = prover_http();
        ProverService {
            client: davinci_zkvm_sdk::client::ProverClient::with_http(base_url, http.clone()),
            base: base_url.trim_end_matches('/').to_string(),
            http,
            poll,
            timeout,
        }
    }

    fn cancel_on_drop(&self, id: &davinci_zkvm_sdk::types::JobId) -> CancelJob {
        CancelJob {
            http: self.http.clone(),
            url: Some(format!("{}/jobs/{id}", self.base)),
            id: id.to_string(),
        }
    }
}

/// Bound on the best-effort cancel of a stale prover job.
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Cancels a prover job when dropped still armed: the flight waiting on it
/// was aborted (a lost race, a rollback, shutdown), and its job would hold
/// the prover ahead of the next batch. Spawned, so the actor never waits.
struct CancelJob {
    http: reqwest::Client,
    url: Option<String>,
    id: String,
}

impl CancelJob {
    fn disarm(mut self) {
        self.url = None;
    }
}

impl Drop for CancelJob {
    fn drop(&mut self) {
        let Some(url) = self.url.take() else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (http, id) = (self.http.clone(), std::mem::take(&mut self.id));
        rt.spawn(async move {
            // 404 and 405: a prover that forgot the job or has no cancel route.
            match http.delete(url).timeout(CANCEL_TIMEOUT).send().await {
                Ok(r) if r.status().is_success() => info!(job = %id, "cancelled stale prover job"),
                Ok(r) => debug!(job = %id, status = %r.status(), "stale prover job not cancelled"),
                Err(e) => debug!(job = %id, e = %e.without_url(), "stale prover job cancel failed"),
            }
        });
    }
}

// The SDK client's timeouts, plus our User-Agent.
fn prover_http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::web3::USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn perr(e: impl std::fmt::Display) -> ProverError {
    ProverError::transient(e.to_string())
}

// A job that reached `failed` will fail again on the same input.
fn werr(e: davinci_zkvm_sdk::Error) -> ProverError {
    ProverError {
        permanent: matches!(e, davinci_zkvm_sdk::Error::JobFailed(_)),
        msg: e.to_string(),
    }
}

/// Submit errors: a 4xx means the prover refused this request itself, so
/// resubmitting the same input cannot succeed — except 408 (timeout) and
/// 429 (rate limit), which describe the moment, not the input. Those,
/// queue-full (503) and transport trouble stay transient.
fn serr(e: davinci_zkvm_sdk::Error) -> ProverError {
    use davinci_zkvm_sdk::Error as E;
    let permanent = matches!(&e, E::Status { status, .. }
            if (400..500).contains(status) && !matches!(*status, 408 | 429))
        || matches!(e, E::Input(_) | E::Json(_) | E::Field(_) | E::JobFailed(_));
    ProverError {
        msg: e.to_string(),
        permanent,
    }
}

/// Extra wait windows on the same job before giving up transiently.
const WAIT_WINDOWS: u32 = 3;

impl ProverService {
    /// Waits for a job, re-entering the wait on a poll timeout: the job is
    /// still ours, and resubmitting would double-queue the batch.
    async fn wait_done(
        &self,
        id: &davinci_zkvm_sdk::types::JobId,
    ) -> Result<davinci_zkvm_sdk::types::Job, ProverError> {
        // The first window plus WAIT_WINDOWS extra ones.
        for _ in 0..=WAIT_WINDOWS {
            match self.client.wait(id, self.poll, self.timeout).await {
                Ok(job) => return Ok(job),
                Err(davinci_zkvm_sdk::Error::Timeout) => continue,
                Err(e) => return Err(werr(e)),
            }
        }
        Err(ProverError::transient("job outlived the wait windows"))
    }
}

#[async_trait]
impl Prover for ProverService {
    async fn prove_batch(
        &self,
        request: &ProveRequest,
        _expected: &BatchPublics,
    ) -> Result<(BatchPublics, PlonkSnark, Option<f64>), ProverError> {
        let id = self.client.prove(request).await.map_err(serr)?;
        // Armed until the job is done: an aborted flight or an abandoned wait
        // (the windows ran out) must not leave the job ahead of the next one.
        let cancel = self.cancel_on_drop(&id);
        let job = self.wait_done(&id).await?;
        cancel.disarm();
        let publics = self.client.publics(&id).await.map_err(perr)?;
        let got =
            BatchPublics::parse(&publics).map_err(|e| ProverError::permanent(e.to_string()))?;
        let snark = self.client.snark(&id).await.map_err(perr)?;
        Ok((got, snark, job.elapsed_ms.map(|ms| ms as f64 / 1000.0)))
    }

    async fn prove_results(
        &self,
        request: &ResultsRequest,
    ) -> Result<(ResultsPublics, PlonkSnark), ProverError> {
        let id = self.client.results(request).await.map_err(serr)?;
        self.wait_done(&id).await?;
        let publics = self.client.publics(&id).await.map_err(perr)?;
        let got =
            ResultsPublics::parse(&publics).map_err(|e| ProverError::permanent(e.to_string()))?;
        let snark = self.client.snark(&id).await.map_err(perr)?;
        Ok((got, snark))
    }
}

// ------------------------------------------------------------------ errors

#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    /// The process no longer accepts votes.
    #[error("process closed: {0}")]
    Closed(String),
    /// Voting opens at this unix time.
    #[error("not open yet: voting starts at {0}")]
    NotStarted(u64),
    /// The process is at `max_voters`; only overwrites of occupied slots fit.
    #[error("max voters reached")]
    MaxVoters,
    #[error("vote {0} already submitted")]
    Duplicate(u64),
    /// The slot holds `slot_depth` pending or in-flight votes; retry once one settles.
    #[error("slot {0} has too many queued votes")]
    SlotBusy(u64),
    #[error("vote is for another process")]
    WrongProcess,
    #[error("not found")]
    NotFound,
    /// The actor is gone (node shutting down).
    #[error("actor stopped")]
    Stopped,
    /// A capacity limit; retry shortly.
    #[error("busy: {0}")]
    Busy(String),
    #[error("internal: {0}")]
    Internal(String),
}

// ------------------------------------------------------------------ handle

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSnapshot {
    pub pid: Fr,
    pub root: [u8; 32],
    pub voters: u64,
    pub overwrites: u64,
    pub occupied: usize,
    pub pending: usize,
    /// `next_seal_not_before` of the open batch; None when nothing is
    /// pending.
    pub next_seal_not_before: Option<u64>,
    pub in_flight: bool,
    pub status: ProcessStatus,
    pub local: LocalStatus,
    /// Whether `submit` would be admitted right now.
    pub accepting: bool,
    /// The committed root is the on-chain root, and no newer one was seen.
    pub synced: bool,
}

pub(crate) enum Msg {
    Submit(Box<VerifiedVote>, oneshot::Sender<Result<(), ActorError>>),
    Status(u64, oneshot::Sender<Result<Option<StoredVote>, ActorError>>),
    Proof(
        u64,
        oneshot::Sender<Result<(arbo::Proof, [u8; 32]), ActorError>>,
    ),
    Ballot(u64, oneshot::Sender<Result<Option<Ballot>, ActorError>>),
    Snapshot(oneshot::Sender<ProcessSnapshot>),
    Event(RegistryEvent),
    Head {
        block: u64,
        time: u64,
        /// The monitor's anchor; `None` before it stored a block hash.
        anchor: Option<Anchor>,
    },
    JobProved {
        generation: u64,
    },
    JobDone {
        generation: u64,
        outcome: JobOutcome,
        /// L1-blob mode: the job mined at least one blob post.
        posted: bool,
    },
    FinalizeDone {
        generation: u64,
        result: Result<(), FinalizeFail>,
    },
    /// Eager results: a checked snark at `root`, submitted once the grace ends.
    ResultsHeld {
        generation: u64,
        root: [u8; 32],
        snark: Box<PlonkSnark>,
    },
    /// Test hook: the next `prepare` reports a refresh overflow (an arm a
    /// sized selection never reaches).
    ForceSealOverflow,
    /// Test hook: the next prepared batch trips the blob-cap belt.
    ForceBlobBelt,
}

/// How a finalize attempt failed: transient re-arms after a short cooldown,
/// permanent after a longer one (never latched).
/// `wait` is not a failure: the DKG has not decrypted yet, poll again.
/// `gate` is not one either: a Council ceremony has not opened decryption.
/// `dkg_requested` is the request flag the attempt last saw on-chain.
pub(crate) struct FinalizeFail {
    pub permanent: bool,
    pub wait: bool,
    pub gate: bool,
    pub msg: String,
    pub dkg_requested: Option<bool>,
}

/// Client side of an actor's mailbox.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<Msg>,
    /// The actor's `chain_time`, mirrored for the API's early refusals.
    head: Arc<AtomicU64>,
    clock: Arc<dyn Clock>,
}

impl ActorHandle {
    /// A handle whose mailbox nobody drains: an actor stuck mid-message.
    #[cfg(feature = "test-hooks")]
    pub fn stalled(capacity: usize, head: u64, clock: Arc<dyn Clock>) -> ActorHandle {
        let (tx, rx) = mpsc::channel(capacity);
        std::mem::forget(rx);
        ActorHandle {
            tx,
            head: Arc::new(AtomicU64::new(head)),
            clock,
        }
    }

    /// The refusal the actor would give `rec`'s process right now by status
    /// and clock, from the head time it last saw; `None` = it would admit.
    pub fn admission_error(&self, rec: &ProcessRecord) -> Option<ActorError> {
        admission_error(
            rec.local,
            &rec.onchain,
            self.head.load(Ordering::Relaxed),
            self.clock.now(),
        )
    }

    /// [`ActorHandle::submit`] that never waits for room in the mailbox: a
    /// full one is `Busy` for this process alone.
    pub async fn submit_nowait(&self, v: VerifiedVote) -> Result<(), ActorError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(Msg::Submit(Box::new(v), tx))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => ActorError::Busy("process queue busy".into()),
                mpsc::error::TrySendError::Closed(_) => ActorError::Stopped,
            })?;
        rx.await.map_err(|_| ActorError::Stopped)?
    }

    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Msg) -> Result<T, ActorError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .await
            .map_err(|_| ActorError::Stopped)?;
        rx.await.map_err(|_| ActorError::Stopped)
    }

    /// Admits a validated vote into the pending queue.
    pub async fn submit(&self, v: VerifiedVote) -> Result<(), ActorError> {
        self.ask(|tx| Msg::Submit(Box::new(v), tx)).await?
    }

    /// The stored record of a vote, if this node ever saw it.
    pub async fn status(&self, vid: u64) -> Result<Option<StoredVote>, ActorError> {
        self.ask(|tx| Msg::Status(vid, tx)).await?
    }

    /// SMT inclusion proof of a settled vote id and the committed root it
    /// verifies under, read atomically from the actor.
    pub async fn vote_id_proof(&self, vid: u64) -> Result<(arbo::Proof, [u8; 32]), ActorError> {
        self.ask(|tx| Msg::Proof(vid, tx)).await?
    }

    /// The committed (re-encrypted) ballot at `slot`, if any.
    pub async fn slot_ballot(&self, slot: u64) -> Result<Option<Ballot>, ActorError> {
        self.ask(|tx| Msg::Ballot(slot, tx)).await?
    }

    pub async fn snapshot(&self) -> Result<ProcessSnapshot, ActorError> {
        self.ask(Msg::Snapshot).await
    }

    pub(crate) async fn event(&self, ev: RegistryEvent) {
        let _ = self.tx.send(Msg::Event(ev)).await;
    }

    /// A Head never waits on a full mailbox (one busy actor must not hold
    /// up the others' heads): it is dropped, and the next carries a newer
    /// block. Events, which must not be lost, still wait.
    pub(crate) async fn head(&self, block: u64, time: u64, anchor: Option<Anchor>) {
        let _ = self.tx.try_send(Msg::Head {
            block,
            time,
            anchor,
        });
    }

    /// Whether the actor's task ended (a panic unwinds only its task).
    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Test hook: the next `prepare` reports a refresh overflow.
    #[doc(hidden)]
    pub async fn force_seal_overflow(&self) {
        let _ = self.tx.send(Msg::ForceSealOverflow).await;
    }

    /// Test hook: the next prepared batch trips the blob-cap belt.
    #[doc(hidden)]
    pub async fn force_blob_belt(&self) {
        let _ = self.tx.send(Msg::ForceBlobBelt).await;
    }
}

// ------------------------------------------------------------- vote packing

/// Serialized form of a `VerifiedVote` in `StoredVote.package`.
#[derive(Serialize, Deserialize)]
struct PackedVote {
    #[serde(with = "fr_hex")]
    process_id: Fr,
    vote_id: u64,
    #[serde(with = "hex::serde")]
    address: [u8; 20],
    ballot: Ballot,
    proof: SnarkJsProof,
    signature: EcdsaSignature,
    census: PackedCensus,
    weight: u128,
    slot: u64,
}

#[derive(Serialize, Deserialize)]
enum PackedCensus {
    /// 256-bit Keccak lean-IMT values as BE hex, never field elements.
    Merkle {
        #[serde(with = "hex::serde")]
        root: [u8; 32],
        #[serde(with = "hex::serde")]
        leaf: [u8; 32],
        path_bits: u64,
        siblings: Vec<String>,
    },
    Csp(CspProof),
}

fn pack_vote(v: &VerifiedVote) -> Result<Vec<u8>, ActorError> {
    let p = &v.pkg;
    let census = match &p.census {
        CensusWitness::Merkle(m) => PackedCensus::Merkle {
            root: m.root,
            leaf: m.leaf,
            path_bits: m.path_bits,
            siblings: m.siblings.iter().map(hex::encode).collect(),
        },
        CensusWitness::Csp(c) => PackedCensus::Csp(c.clone()),
    };
    serde_json::to_vec(&PackedVote {
        process_id: p.process_id,
        vote_id: p.vote_id,
        address: p.address,
        ballot: p.ballot,
        proof: p.proof.clone(),
        signature: p.signature,
        census,
        weight: p.weight,
        slot: v.slot,
    })
    .map_err(|e| ActorError::Internal(e.to_string()))
}

fn unpack_vote(bytes: &[u8]) -> Result<VerifiedVote, String> {
    let p: PackedVote = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let census = match p.census {
        PackedCensus::Merkle {
            root,
            leaf,
            path_bits,
            siblings,
        } => {
            let siblings = siblings
                .iter()
                .map(|s| {
                    let mut b = [0u8; 32];
                    hex::decode_to_slice(s, &mut b).map_err(|e| e.to_string())?;
                    Ok(b)
                })
                .collect::<Result<Vec<[u8; 32]>, String>>()?;
            CensusWitness::Merkle(CensusProof {
                root,
                leaf,
                path_bits,
                siblings,
            })
        }
        PackedCensus::Csp(c) => CensusWitness::Csp(c),
    };
    Ok(VerifiedVote {
        pkg: VotePackage {
            process_id: p.process_id,
            vote_id: p.vote_id,
            address: p.address,
            ballot: p.ballot,
            proof: p.proof,
            signature: p.signature,
            census,
            weight: p.weight,
        },
        slot: p.slot,
    })
}

// ------------------------------------------------------------------- actor

pub(crate) enum JobOutcome {
    /// Settled on-chain.
    Landed(TxReceipt),
    /// The guest published `ok = 0` with this fail mask.
    Guest(u32),
    /// The prover failed the job for good (a `failed` job, a refused
    /// input, undecodable publics): requeued and bisected.
    Failed(String),
    /// The node's own checks refused the proof or the L1 settlement (a
    /// publics or pin mismatch): every batch fails alike, so the votes are
    /// held pending, never bisected.
    Held(String),
    /// The settlement reverted with this error name.
    Reverted(String),
    /// The settlement reverted on-chain (in this block) and no replay
    /// names why: a race.
    Lost(String, u64),
    /// RPC trouble; the votes go back to pending.
    Transient(String),
}

/// How long a landed-but-unconfirmed settlement may wait for its event
/// before the flight is rolled back (covers a reorg dropping the tx).
const LANDED_EVENT_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// Longest an L1-blob job waits for Ethereum and OP's origin when the grace
/// end is far or unbounded.
const L1_JOB_CAP: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// Consecutive ticks the landing-time read may stay stale (failing, or a
/// root behind the events) before the node warns and counts a stall; it
/// warns again every as many ticks.
const STALE_WARN_TICKS: u32 = 30;
/// Degraded mode probes reads by hash again every this many heads.
const DEGRADED_PROBE: u32 = 10;
/// What an anchored read was applied on: committed root, pinned root, log
/// root, floor, sync latch, check flag.
type AppliedOn = ([u8; 32], [u8; 32], [u8; 32], u64, bool, bool);
/// Wait before a failed anchored read is tried again at the same anchor.
const READ_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a pause read on-chain, but not yet routed as an event, holds
/// sealing back.
const PAUSE_HOLD: std::time::Duration = std::time::Duration::from_secs(90);

/// L1-blob mode: consecutive failed cycles (blobs posted on Ethereum, no
/// landing) before sealing pauses; nothing bounds the spend otherwise.
const FAILED_CYCLES_BEFORE_PAUSE: u32 = 3;
/// The first pause; each further failed cycle doubles it, up to [`MAX_PAUSE`].
const FIRST_PAUSE: std::time::Duration = std::time::Duration::from_secs(600);
const MAX_PAUSE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Longest cooldown between bisection cycles, seconds.
const BISECT_BACKOFF_MAX: u64 = 30;
/// Single-vote batches errored in a row before the actor stops erroring
/// and holds every vote pending (the prover fails every input).
const ISOLATED_BEFORE_HOLD: u32 = 3;
/// Results: consecutive failed attempts before the retries slow to
/// [`RESULTS_HOLD_RETRY`]; until then they back off up to
/// [`BISECT_BACKOFF_MAX`].
const RESULTS_FAILS_BEFORE_HOLD: u32 = 3;
const RESULTS_HOLD_RETRY: u64 = 300;

/// Set after a prover-side `Failed`: the votes of the last failed batch, the
/// cap of the next seal and the prover's reason. In memory only (a restart
/// retries at full size).
struct Bisect {
    suspect: BTreeSet<u64>,
    cap: usize,
    reason: String,
}

struct Flight {
    generation: u64,
    prepared: davinci_state::PreparedBatch,
    votes: Vec<VerifiedVote>,
    /// Admission times of `votes`, for an age-preserving requeue.
    votes_at: Vec<u64>,
    /// Set when the settlement tx landed but the receipt read failed: if the
    /// confirmed chain never shows our root by this instant, roll back.
    landed_deadline: Option<tokio::time::Instant>,
    /// The proof passed its checks (`Msg::JobProved`): what fails after
    /// this is the settlement.
    proved: bool,
    /// L1-blob mode, past `landed_deadline`: a read by number showed our
    /// root at this block, so the flight waits for its event until the
    /// anchor passes it.
    hint_block: Option<u64>,
    /// Cancels the job when the flight is dropped (rollback, requeue,
    /// shutdown): it stops waiting on the prover, which cancels the prover
    /// job, and an L1-blob job stops waiting on Ethereum.
    _abort: tokio_util::sync::DropGuard,
}

struct Actor {
    pid: Fr,
    pid31: [u8; 31],
    deps: Deps,
    census: CensusAccess,
    keys: KeyStore,
    metrics: Arc<Metrics>,
    batch_max: usize,
    batch_time: u64,
    min_mix: usize,
    solo_wait: u64,
    flush_horizon: u64,
    /// Landing margin reserved after proving.
    settle_margin: u64,
    /// L1-blob mode: the Ethereum leg, which runs during the proof.
    l1_settle_lag: u64,
    slot_depth: usize,
    prove_base: f64,
    /// Seal jitter of the current window, drawn once per window.
    jitter: Option<f64>,
    /// Pending size below which no transaction can be full, keyed by the
    /// (occupied, exposed) counts it was computed for.
    cap_est: Option<((usize, usize), usize)>,
    state: ProcessState<crate::storage::ArboStore>,
    record: ProcessRecord,
    pending: Vec<VerifiedVote>,
    pending_at: Vec<u64>,
    chain_time: u64,
    /// Newest chain block a Head message reported; upper bound for gap replay.
    last_head_block: u64,
    /// The monitor's anchor from the last Head that had one: every state
    /// decision not made from an event is made from a read at its hash.
    anchor: Option<Anchor>,
    /// The last anchored read applied: its anchor and the process there.
    checked: Option<(Anchor, Box<OnchainProcess>)>,
    /// A failed anchored read: not retried at that anchor before this.
    read_retry: Option<(Anchor, tokio::time::Instant)>,
    /// [`Self::applied_on`] when the last anchored read was applied.
    applied: Option<AppliedOn>,
    /// Block of the newest applied transition; lower bound for gap replay.
    last_tx_block: u64,
    /// Newest block an event or an own commit showed: an anchored read
    /// below it merges nothing and leaves `needs_check` set.
    check_at: u64,
    /// Newest (block, log index) the record's fields came from (a field
    /// event, a merged anchored read): a field event at or below it, routed
    /// again after a rewind, would rewind the record (and error votes on an
    /// older cap), so it is skipped; the anchored read at or above
    /// `check_at` merges the branch's values.
    fields_at: (u64, u64),
    /// `new_root` of the newest transition record (genesis before the
    /// first). Behind the committed root when a record write was lost; the
    /// resync writes the missing records from the chain.
    log_root: [u8; 32],
    in_flight: Option<Flight>,
    generation: u64,
    next_index: u64,
    /// Set on a lost race; suppresses sealing until a sync applies.
    await_sync: bool,
    /// The latch is a lost race's: only a read made after it clears it.
    race_latch: bool,
    /// Consecutive transient batch-job failures; drives the prove cooldown.
    prove_attempts: u32,
    /// Prove cooldown after transient job trouble: no seal before this.
    prove_after: Option<tokio::time::Instant>,
    /// Resync backoff: no replay attempt before this instant.
    resync_after: Option<tokio::time::Instant>,
    resync_backoff_ms: u64,
    finalizing: bool,
    /// Consecutive finalize attempts that failed for good (a failed results
    /// proof, a refused check, a revert): retried, never latched.
    results_failures: u32,
    finalize_attempts: u32,
    /// Consecutive attempts that found the Council decryption gate closed.
    gate_polls: u32,
    /// Transient finalize cooldown: no new attempt before this instant.
    finalize_after: Option<tokio::time::Instant>,
    /// Prove the results during the grace (sequencer-key processes).
    eager: bool,
    /// The election key is not this node's. It never changes, so the
    /// derivation (a scalar multiplication) runs once, not every tick.
    foreign_key: bool,
    /// Eager results: the checked snark and the root it proves.
    held: Option<([u8; 32], Box<PlonkSnark>)>,
    /// Our results transaction has a receipt, which may be depth 0: no new
    /// attempt before its event, and past this instant a chain read decides
    /// (RESULTS finalizes, anything else resubmits).
    results_sent: Option<tokio::time::Instant>,
    /// Lowest block the results were confirmed at (their event, or an
    /// anchored read showing them): an anchored read at or above it that
    /// lacks them undoes the finalization.
    results_at: Option<u64>,
    /// The same for a cancel: undone, admission reopens.
    cancel_at: Option<u64>,
    /// The last anchored read was below a confirmation it does not show:
    /// close-out and results wait for one at or above it.
    suspended: bool,
    /// Since when reads by hash keep failing (none answered since).
    hash_fail_since: Option<tokio::time::Instant>,
    /// Degraded mode (an endpoint without EIP-1898 calls): heads since the
    /// last probe by hash.
    degraded: Option<u32>,
    /// `--degraded-number-reads`: degraded mode is allowed at all.
    number_reads: bool,
    /// Reads by hash failed for [`LANDED_EVENT_WAIT`] with degraded reads
    /// off (warned once until they answer).
    hash_stalled: bool,
    /// The first tick reads the head by number once: a root there the log
    /// does not know (a transition whose event was lost) holds sealing
    /// until an anchored read or the sync clears it.
    boot_hint: bool,
    /// A cycle posted and failed: retries are proof-first until our own
    /// landing (a peer's landing lowers `failed_cycles`, not this).
    proof_first_until_landing: bool,
    /// Why the last anchored read left the record stale.
    stale_why: String,
    /// Something moved since the last anchored read (an event, an own
    /// landing, a failure): the grace end, the status and the root may be
    /// behind, so nothing closes or finalizes until an anchored read at or
    /// above `check_at` shows the pinned root.
    needs_check: bool,
    /// Consecutive ticks the stale read has not cleared.
    stale_ticks: u32,
    /// The exposed set: every slot a sealed batch changed (writes and
    /// refreshes as one set) and the vote ids of those attempts. Mirrors
    /// the persisted `ExposedRecord`; cleared once every vote is final.
    exposed: BTreeSet<u64>,
    exposed_vids: BTreeSet<u64>,
    /// Latched when the origin-3 census contract turned unusable: the
    /// pending votes were errored once and no more are admitted.
    census_broken: Option<String>,
    /// Origin 2: re-check pending votes once the updated census loads.
    census_recheck: bool,
    /// Consecutive `InvalidCensusRoot` reverts of one census root (LE).
    census_reverts: ([u8; 32], u32),
    /// Set when a settlement met a pause the events have not shown yet: no
    /// seal before this instant or the next status event.
    pause_hold: Option<tokio::time::Instant>,
    /// L1-blob mode: consecutive cycles that posted blobs but did not land.
    /// Our landing resets it, a synced one takes one off; both end the
    /// pause. In memory only, so a restart resets it too.
    failed_cycles: u32,
    /// No seal before this instant after [`FAILED_CYCLES_BEFORE_PAUSE`]
    /// failed cycles.
    failed_pause: Option<tokio::time::Instant>,
    /// A `DurationChanged` since the last head. An END emits it before its
    /// `StatusChanged`, at a time no head shows yet: no seal until a head
    /// does, so the budget never sees the new end with a stale clock.
    end_moved: bool,
    /// Bisection of a batch the prover failed for good; `None` otherwise.
    bisect: Option<Bisect>,
    /// Single-vote batches the prover failed and the actor errored since our
    /// last landing; at [`ISOLATED_BEFORE_HOLD`] it sets `prover_hold`. Only
    /// our own landing (or a restart) clears both: another sequencer's says
    /// nothing about our prover.
    isolated: u32,
    /// The prover fails every input: retry full batches, error nothing.
    /// Holds the prover's latest reason, which the close reason names.
    prover_hold: Option<String>,
    /// The last node-side check failure that held votes pending, named in
    /// their close reason. Our landing clears it.
    held_reason: Option<String>,
    /// Errors of a failed batch's votes (out of the queue) whose write
    /// failed: retried every tick, and nothing seals until they are stored,
    /// or a restart would requeue those votes behind later ones of theirs.
    unwritten: Vec<(Vec<u64>, String)>,
    /// A requeue write failed: the stored queue lags the one here (a vote
    /// out of it, or behind one requeued after it). Rewritten from this one
    /// every tick, and nothing seals until then, or a restart would put a
    /// voter's later vote first.
    queue_lost: bool,
    /// The chain left our log at transition `.0`: it sits at that
    /// transition's old root, or forked from it (since `.1`, seen up to
    /// block `.2`). If that holds for [`LANDED_EVENT_WAIT`], a reorg took
    /// transitions we committed and the process is rebuilt from the chain;
    /// only an anchored read at or above `.2` on our root clears it.
    diverged: Option<(u64, tokio::time::Instant, u64)>,
    /// The state must be reopened at its committed record (a rebuild whose
    /// genesis write failed, a tree reset that failed): retried before
    /// anything touches the state.
    state_lost: bool,
    /// A rebuild kept votes Settled that its replay may not bring back:
    /// check them once the replay converges (`Db::rebuild_check`).
    rebuild_check: bool,
    /// Test hook: the next `prepare` reports a refresh overflow.
    force_overflow: bool,
    /// Test hook: the next prepared batch trips the blob-cap belt.
    force_belt: bool,
    tx: mpsc::Sender<Msg>,
    head: Arc<AtomicU64>,
}

/// Seal jitter factor, uniform in [0.9, 1.1]: one draw per batch window
/// scales the timer and solo deadlines.
pub fn seal_jitter(rng: &mut impl rand::Rng) -> f64 {
    rng.gen_range(0.9..=1.1)
}

pub(crate) fn pid31_of(pid: &Fr) -> [u8; 31] {
    let be = fr_to_be(pid);
    let mut out = [0u8; 31];
    out.copy_from_slice(&be[1..]);
    out
}

fn ierr(e: impl std::fmt::Display) -> ActorError {
    ActorError::Internal(e.to_string())
}

/// Runs CPU-heavy state work (KZG, point decompression, tree hashing)
/// without starving the async workers: on the multi-thread runtime the
/// worker is handed back via `block_in_place`; on a current-thread
/// runtime (tests) it runs inline.
fn cpu<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Opens the process state at the committed record its tree storage holds
/// (written with the slot store in one batch, so no crash can split them),
/// or creates it at genesis on the first start.
fn open_state(
    db: &Db,
    pid: &Fr,
    cfg: &ProcessConfig,
) -> Result<ProcessState<crate::storage::ArboStore>, ActorError> {
    let storage = db.arbo_storage(pid).map_err(ierr)?;
    match ProcessState::open_stored(cfg.clone(), storage) {
        Ok(s) => Ok(s),
        Err(StateError::Invalid(m)) if m.contains("no process") => {
            let storage = db.arbo_storage(pid).map_err(ierr)?;
            ProcessState::create(cfg.clone(), storage).map_err(ierr)
        }
        Err(e) => Err(ierr(e)),
    }
}

/// Copies a `getProcess` read over the stored record: every field the
/// events move, except the pin (`state_root`, moved only by applied
/// transitions) and the counters, which follow the chain only at the pin.
/// `last_vote_at` only grows.
pub(crate) fn merge_onchain(o: &mut OnchainProcess, p: &OnchainProcess) {
    o.status = p.status;
    o.duration = p.duration;
    o.grace = p.grace;
    o.max_voters = p.max_voters;
    o.metadata_uri.clone_from(&p.metadata_uri);
    o.metadata_hash = p.metadata_hash;
    o.census.root = p.census.root;
    o.census.uri.clone_from(&p.census.uri);
    o.dkg = p.dkg;
    o.results.clone_from(&p.results);
    o.last_vote_at = o.last_vote_at.max(p.last_vote_at);
    if p.state_root == o.state_root {
        o.voters_count = p.voters_count;
        o.overwritten_count = p.overwritten_count;
    }
}

/// Why a vote is refused by the clock and status right now, `None` when
/// admission is open. The one rule the actor admits by and the API refuses
/// ahead of validation by. Paused still accepts: sealing gates on Ready, so
/// the votes queue and settle on resume. `chain_time` 0 means "unknown":
/// refuse until the first Head rather than read a failed head as the epoch.
/// The end also follows the local clock: a Head can be dropped (a full
/// mailbox), and a stale `chain_time` must not keep admission open past it.
pub(crate) fn admission_error(
    local: LocalStatus,
    p: &OnchainProcess,
    chain_time: u64,
    now: u64,
) -> Option<ActorError> {
    let not_closed = local == LocalStatus::Active
        && matches!(p.status, ProcessStatus::Ready | ProcessStatus::Paused)
        && chain_time != 0;
    if not_closed && chain_time >= p.start_time && chain_time.max(now) < p.end_time() {
        return None;
    }
    if not_closed && chain_time < p.start_time {
        return Some(ActorError::NotStarted(p.start_time));
    }
    Some(ActorError::Closed("not accepting votes".into()))
}

/// Spawns the actor of one process and returns its handle. `chain_time` is
/// a head-timestamp read so the actor knows the window before its first Head.
#[allow(clippy::too_many_arguments)] // one internal call site
pub(crate) fn spawn_actor(
    cfg: &Config,
    deps: &Deps,
    census: CensusAccess,
    keys: KeyStore,
    metrics: Arc<Metrics>,
    record: ProcessRecord,
    pcfg: ProcessConfig,
    chain_time: u64,
    shutdown: CancellationToken,
) -> Result<ActorHandle, ActorError> {
    let pid = pcfg.process_id;
    let mut state = open_state(&deps.db, &pid, &pcfg)?;
    state
        .set_blob_cap(crate::l1da::batch_blob_cap(
            deps.l1.is_some(),
            cfg.max_blobs_per_tx,
        ))
        .map_err(ierr)?;
    let transitions = deps.db.transitions(&pid).map_err(ierr)?;
    let next_index = transitions.last().map(|t| t.index + 1).unwrap_or(0);
    let last_tx_block = transitions.last().map(|t| t.block).unwrap_or(0);
    let log_root = match transitions.last() {
        Some(t) => t.new_root,
        None => genesis_root(&pcfg).map_err(ierr)?,
    };
    let results_at = (record.local == LocalStatus::Finalized).then_some(0);
    let cancel_at = (record.onchain.status == ProcessStatus::Canceled).then_some(0);
    let (tx, rx) = mpsc::channel(1024);
    let head = Arc::new(AtomicU64::new(chain_time));
    let mut actor = Actor {
        pid,
        pid31: pid31_of(&pid),
        deps: deps.clone(),
        census,
        keys,
        metrics,
        batch_max: cfg.batch_max,
        batch_time: cfg.batch_time.as_secs(),
        min_mix: cfg.min_mix as usize,
        solo_wait: cfg.solo_wait.as_secs(),
        flush_horizon: cfg.flush_horizon.as_secs(),
        settle_margin: cfg.settle_margin.as_secs(),
        l1_settle_lag: cfg.l1_settle_lag.as_secs(),
        slot_depth: cfg.slot_depth as usize,
        prove_base: cfg.prove_base.as_secs_f64(),
        jitter: None,
        cap_est: None,
        state,
        record,
        pending: Vec::new(),
        pending_at: Vec::new(),
        chain_time,
        head: head.clone(),
        last_head_block: 0,
        anchor: None,
        checked: None,
        read_retry: None,
        applied: None,
        last_tx_block,
        check_at: last_tx_block,
        // The stored record's fields came from at or below the cursor.
        fields_at: (
            deps.db
                .meta_u64(crate::monitor::LAST_BLOCK_KEY)
                .map_err(ierr)?
                .unwrap_or(0),
            u64::MAX,
        ),
        log_root,
        in_flight: None,
        generation: 0,
        next_index,
        await_sync: false,
        race_latch: false,
        prove_attempts: 0,
        prove_after: None,
        resync_after: None,
        resync_backoff_ms: 0,
        finalizing: false,
        results_failures: 0,
        finalize_attempts: 0,
        gate_polls: 0,
        finalize_after: None,
        eager: cfg.eager_results,
        foreign_key: false,
        held: None,
        results_sent: None,
        // Stored as confirmed: the first anchored read lacking it undoes it.
        results_at,
        cancel_at,
        suspended: false,
        boot_hint: true,
        hash_fail_since: None,
        degraded: None,
        number_reads: cfg.degraded_number_reads,
        hash_stalled: false,
        proof_first_until_landing: false,
        stale_why: String::new(),
        // The record may predate the last landing: re-read before closing.
        needs_check: true,
        stale_ticks: 0,
        exposed: BTreeSet::new(),
        exposed_vids: BTreeSet::new(),
        census_broken: None,
        census_recheck: false,
        census_reverts: ([0u8; 32], 0),
        pause_hold: None,
        failed_cycles: 0,
        failed_pause: None,
        end_moved: false,
        bisect: None,
        isolated: 0,
        prover_hold: None,
        held_reason: None,
        unwritten: Vec::new(),
        queue_lost: false,
        diverged: None,
        state_lost: false,
        rebuild_check: deps.db.rebuild_check(&pid).map_err(ierr)?,
        force_overflow: false,
        force_belt: false,
        tx: tx.clone(),
    };
    actor.recover()?;
    let tracker = deps.tasks.clone();
    tracker.spawn(async move { actor.run(rx, shutdown).await });
    Ok(ActorHandle {
        tx,
        head,
        clock: deps.clock.clone(),
    })
}

impl Actor {
    /// Restart recovery: every vote left aggregated/processed goes back to
    /// pending (the batch seed died with the process, by design), and the
    /// pending queue is reloaded in order. Votes whose id already reached
    /// the tree (crash between commit and settle) are settled instead.
    fn recover(&mut self) -> Result<(), ActorError> {
        let db = &self.deps.db;
        // The exposed set survives restarts (a sealed attempt may have
        // broadcast before the crash).
        if let Some(r) = db.exposed(&self.pid).map_err(ierr)? {
            self.exposed = r.slots.iter().copied().collect();
            self.exposed_vids = r.vote_ids.iter().copied().collect();
        }
        let stale = db
            .vote_ids_with_status(&self.pid, &[VoteStatus::Aggregated, VoteStatus::Processed])
            .map_err(ierr)?;
        if !stale.is_empty() {
            db.requeue(&self.pid, &stale).map_err(ierr)?;
        }
        let mut queue = Vec::new();
        for vid in db.pending(&self.pid).map_err(ierr)? {
            if let Some(sv) = db.vote(&self.pid, vid).map_err(ierr)? {
                let landed = self.state.has_vote_id(vid).map_err(ierr)?;
                queue.push((sv, landed));
            }
        }
        // In the tree (a crash between the tree's commit and the record):
        // settled, and the votes of their voters queued before them are
        // superseded, in one write, before anything is dropped. The stored
        // queue is in cast order.
        let (mut settled, mut superseded) = (BTreeSet::new(), BTreeSet::new());
        let mut later = std::collections::HashSet::new();
        for (sv, landed) in queue.iter().rev() {
            if *landed {
                settled.insert(sv.vote_id);
                later.insert(sv.slot);
            } else if later.contains(&sv.slot) {
                superseded.insert(sv.vote_id);
            }
        }
        if !settled.is_empty() {
            let (a, b): (Vec<u64>, Vec<u64>) = (
                settled.iter().copied().collect(),
                superseded.iter().copied().collect(),
            );
            db.settle_votes(&self.pid, &a, &b).map_err(ierr)?;
        }
        for (sv, _) in queue {
            let vid = sv.vote_id;
            if settled.contains(&vid) || superseded.contains(&vid) {
                continue;
            }
            match unpack_vote(&sv.package) {
                Ok(v) => {
                    self.pending.push(v);
                    self.pending_at.push(sv.created_at);
                }
                Err(e) => {
                    warn!(pid = %hex::encode(self.pid31), vid, %e, "unreadable stored vote");
                    db.set_status(
                        &self.pid,
                        &[vid],
                        VoteStatus::Error,
                        Some("unreadable package"),
                    )
                    .map_err(ierr)?;
                    db.remove_pending(&self.pid, &[vid]).map_err(ierr)?;
                }
            }
        }
        self.sort_pending();
        self.error_new_slots_if_full();
        Ok(())
    }

    /// The queue once `back` goes back at its head: the order the stored
    /// queue is rewritten to in the same write, so a requeue write that
    /// failed before leaves no vote of it behind a later one there.
    fn queue_with(&self, back: &[(u64, VerifiedVote)]) -> Vec<u64> {
        let mut q: Vec<(u64, u64)> = back
            .iter()
            .map(|(at, v)| (*at, v.pkg.vote_id))
            .chain(
                self.pending_at
                    .iter()
                    .copied()
                    .zip(self.pending.iter().map(|v| v.pkg.vote_id)),
            )
            .collect();
        q.sort_by_key(|(at, _)| *at);
        q.into_iter().map(|(_, v)| v).collect()
    }

    /// Restores cast order: a stable sort by admission time over the queue
    /// order, where requeued votes sit ahead of the later votes of their slot.
    fn sort_pending(&mut self) {
        let mut q: Vec<(u64, VerifiedVote)> = std::mem::take(&mut self.pending_at)
            .into_iter()
            .zip(std::mem::take(&mut self.pending))
            .collect();
        q.sort_by_key(|(at, _)| *at);
        (self.pending_at, self.pending) = q.into_iter().unzip();
    }

    async fn run(&mut self, mut rx: mpsc::Receiver<Msg>, shutdown: CancellationToken) {
        loop {
            // No tree snapshot outlives a message: an idle actor's would pin
            // every later commit's pages in the shared file.
            self.state.release_reads();
            tokio::select! {
                _ = shutdown.cancelled() => return,
                msg = rx.recv() => match msg {
                    None => return,
                    // A handler can wait on RPC or census I/O; shutdown cuts
                    // it like a crash would, and recovery replays from the db.
                    Some(m) => tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = self.handle(m) => {}
                    },
                },
            }
        }
    }

    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Submit(v, reply) => {
                let _ = reply.send(self.admit(*v));
            }
            Msg::Status(vid, reply) => {
                let _ = reply.send(self.deps.db.vote(&self.pid, vid).map_err(ierr));
            }
            Msg::Proof(vid, reply) => {
                let root = self.state.committed().root;
                let _ = reply.send(
                    self.state
                        .vote_id_proof(vid)
                        .map(|p| (p, root))
                        .map_err(|_| ActorError::NotFound),
                );
            }
            Msg::Ballot(slot, reply) => {
                let _ = reply.send(self.state.slot_ballot(slot).map_err(ierr));
            }
            Msg::Snapshot(reply) => {
                let c = self.state.committed();
                let _ = reply.send(ProcessSnapshot {
                    pid: self.pid,
                    root: c.root,
                    voters: c.voters,
                    overwrites: c.overwrites,
                    occupied: self.state.occupied(),
                    pending: self.pending.len(),
                    next_seal_not_before: self.next_seal_not_before(),
                    in_flight: self.in_flight.is_some(),
                    status: self.record.onchain.status,
                    local: self.record.local,
                    accepting: self.accepting(),
                    // Finalized: the final root needs a chain read; the
                    // events may never have shown the transitions before it.
                    synced: c.root == self.record.onchain.state_root
                        && !self.await_sync
                        && (self.record.local != LocalStatus::Finalized || !self.needs_check),
                });
            }
            Msg::Event(ev) => {
                // Decisions wait for an anchored read that covers it.
                self.check_at = self.check_at.max(ev.block);
                self.needs_check = true;
                self.on_event(ev).await
            }
            Msg::Head {
                block,
                time,
                anchor,
            } => {
                // A reorg the monitor saw (or a lower anchor, another hash at
                // the same block): re-read even with nothing else waiting.
                if let (Some(a), Some(old)) = (anchor, self.anchor)
                    && (a.reorgs != old.reorgs || (!a.same(&old) && a.block <= old.block))
                {
                    self.needs_check = true;
                }
                self.anchor = anchor.or(self.anchor);
                if let Some(n) = &mut self.degraded {
                    *n = n.saturating_add(1);
                }
                self.chain_time = time;
                self.head.store(time, Ordering::Relaxed);
                // One uncorroborated `latest` read: time and pacing only. A
                // bogus block here would refuse every later read.
                self.last_head_block = block;
                self.end_moved = false;
            }
            Msg::JobProved { generation } => {
                if let Some(f) = self.in_flight.as_mut()
                    && f.generation == generation
                {
                    f.proved = true;
                    let vids = f.prepared.vote_ids.clone();
                    self.set_status(&vids, VoteStatus::Processed, None);
                }
            }
            Msg::JobDone {
                generation,
                outcome,
                posted,
            } => self.on_job_done(generation, outcome, posted).await,
            Msg::ForceSealOverflow => self.force_overflow = true,
            Msg::ForceBlobBelt => self.force_belt = true,
            Msg::ResultsHeld {
                generation,
                root,
                snark,
            } => {
                if generation == self.generation {
                    self.finalizing = false;
                    self.finalize_attempts = 0;
                    self.results_failures = 0;
                    self.held = Some((root, snark));
                }
            }
            Msg::FinalizeDone { generation, result } => {
                if generation != self.generation {
                    return;
                }
                self.finalizing = false;
                if let Err(FinalizeFail {
                    dkg_requested: Some(r),
                    ..
                }) = &result
                    && self.record.onchain.dkg.requested != *r
                {
                    // Our request landed (or a reorg dropped one): later polls
                    // skip rebuilding the inputs, and the backoff starts over.
                    self.record.onchain.dkg.requested = *r;
                    self.finalize_attempts = 0;
                    self.put_record();
                }
                match result {
                    // Commit at the event, like an L1 landing: the receipt
                    // may be depth 0 and reorg out.
                    Ok(()) => {
                        self.finalize_attempts = 0;
                        self.results_failures = 0;
                        if self.record.local == LocalStatus::Active {
                            self.results_sent =
                                Some(tokio::time::Instant::now() + LANDED_EVENT_WAIT);
                        }
                    }
                    Err(f) if f.permanent => self.results_failed(&f.msg),
                    Err(f) if f.gate => {
                        // A Council ceremony opens decryption on its own
                        // schedule, possibly months after the end: back off
                        // to GATE_POLL_MAX and keep polling, never latch.
                        self.finalize_attempts = 0;
                        self.gate_polls = self.gate_polls.saturating_add(1);
                        let wait = gate_backoff(self.gate_polls);
                        self.finalize_after = Some(tokio::time::Instant::now() + wait);
                        if self.gate_polls == 1 {
                            info!(
                                pid = %hex::encode(self.pid31),
                                "results wait for the Council ceremony to open decryption"
                            );
                        } else {
                            debug!(
                                pid = %hex::encode(self.pid31),
                                e = ?f.msg,
                                polls = self.gate_polls,
                                retry_in = wait.as_secs(),
                                "Council decryption still closed"
                            );
                        }
                    }
                    Err(f) if f.wait => {
                        // A locked process can wait days for its reveal: poll
                        // at a fixed pace, not an error.
                        self.finalize_attempts = 0;
                        self.gate_polls = 0;
                        self.finalize_after = Some(tokio::time::Instant::now() + DKG_POLL);
                        debug!(pid = %hex::encode(self.pid31), e = ?f.msg, "waiting for the DKG");
                    }
                    Err(f) => {
                        // A moved root: drop the held proof. Re-read the chain
                        // on any failure: one that names another root (or none:
                        // a DKG request off ours) may be a reorg we are past.
                        if f.msg.contains("InvalidStateRoot") {
                            self.held = None;
                        }
                        self.needs_check = true;
                        // Transient trouble re-arms after a cooldown.
                        self.finalize_attempts += 1;
                        let secs = (1u64 << self.finalize_attempts.min(6)).min(60);
                        self.finalize_after = Some(
                            tokio::time::Instant::now() + std::time::Duration::from_secs(secs),
                        );
                        warn!(
                            pid = %hex::encode(self.pid31),
                            e = ?f.msg,
                            attempts = self.finalize_attempts,
                            retry_in = secs,
                            "finalize attempt failed, will retry"
                        );
                    }
                }
            }
        }
        self.tick().await;
    }

    fn end_time(&self) -> u64 {
        self.record.onchain.end_time()
    }

    /// The registry's grace end: transitions land before it, results at or
    /// after it. Moves with every landing (`last_vote_at`), up to the cap.
    fn grace_end(&self) -> u64 {
        self.record
            .onchain
            .grace_end(self.deps.grace.grace_max_total)
    }

    /// Flush mode: seal whatever is pending, from `flush_horizon` before the
    /// end through the grace window.
    fn flush(&self) -> bool {
        self.chain_time != 0
            && self.chain_time >= self.end_time().saturating_sub(self.flush_horizon)
    }

    /// Earliest the open batch can seal, for the queue as it stands: a
    /// queue at --batch-max seals now, otherwise the timer (enough
    /// distinct slots) or the solo expiry at the jitter floor, clamped
    /// by the flush. New votes only bring it forward — company upgrades
    /// a lone batch to the shorter wait — and it is not a deadline: the
    /// jitter is private and gates (an in-flight batch, the proving
    /// budget) can hold a seal past it. None when nothing is pending.
    fn next_seal_not_before(&self) -> Option<u64> {
        let oldest = self.pending_at.first().copied()?;
        let now = self.deps.clock.now();
        let distinct = self
            .pending
            .iter()
            .map(|v| v.slot)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let due = if self.pending.len() >= self.seal_max() {
            now
        } else if distinct < self.min_mix {
            oldest + (self.solo_wait as f64 * 0.9).ceil() as u64
        } else {
            oldest + (self.batch_time as f64 * 0.9).ceil() as u64
        };
        let flush_at = self.end_time().saturating_sub(self.flush_horizon);
        // Paused after failed L1 cycles, or cooling down after a transient
        // failure: not before the pause ends.
        let paused = self.failed_pause.max(self.prove_after).map_or(0, |t| {
            let left = t.saturating_duration_since(tokio::time::Instant::now());
            now.saturating_add(left.as_secs())
        });
        Some(due.min(flush_at).max(paused))
    }

    /// Most votes the next seal takes: `--batch-max`, or less while a
    /// bisection halves a failing batch. While held nothing can land, so
    /// only the pending exposed votes ride the cycles: fresh ones would just
    /// widen the exposed set.
    fn seal_max(&self) -> usize {
        let mut max = self.batch_max;
        if let Some(b) = &self.bisect
            && self.prover_hold.is_none()
        {
            max = max.min(b.cap);
        }
        if self.prover_hold.is_some() || self.held_reason.is_some() {
            let exposed = self
                .pending
                .iter()
                .filter(|v| self.exposed_vids.contains(&v.pkg.vote_id))
                .count();
            max = max.min(exposed.max(1));
        }
        max
    }

    /// A vote whose own coverage history no transition can refresh: errored
    /// to recast (a new vote id is a fresh transition). Only this vote.
    fn recast_overflow(&mut self, vid: u64) {
        error!(pid = %hex::encode(self.pid31), "a vote's coverage history is too large to refresh; it must recast");
        self.finish(
            &[vid],
            VoteStatus::Error,
            Some("exposed refresh set too large, recast"),
        );
    }

    /// While held nothing can land, so only the pending exposed votes ride
    /// the cycles, picked by membership: a vote skipped by an earlier
    /// selection must not take an exposed one's place. `None` (the whole
    /// queue) when not held, or when none is pending (one vote probes).
    fn held_votes(&self) -> Option<Vec<VerifiedVote>> {
        if self.prover_hold.is_none() && self.held_reason.is_none() {
            return None;
        }
        let v: Vec<VerifiedVote> = self
            .pending
            .iter()
            .filter(|v| self.exposed_vids.contains(&v.pkg.vote_id))
            .cloned()
            .collect();
        (!v.is_empty()).then_some(v)
    }

    /// The end check also follows the local clock: a Head can be dropped
    /// (a full mailbox), and a stale `chain_time` must not keep admission
    /// open past the end.
    fn admission_error(&self) -> Option<ActorError> {
        admission_error(
            self.record.local,
            &self.record.onchain,
            self.chain_time,
            self.deps.clock.now(),
        )
    }

    fn accepting(&self) -> bool {
        self.admission_error().is_none()
    }

    fn admit(&mut self, v: VerifiedVote) -> Result<(), ActorError> {
        if v.pkg.process_id != self.pid {
            return Err(ActorError::WrongProcess);
        }
        if let Some(e) = self.admission_error() {
            return Err(e);
        }
        if let Some(m) = &self.census_broken {
            return Err(ActorError::Closed(m.clone()));
        }
        // Not proven bad: queued votes wait, new ones are turned away.
        if self.state.config().census_origin == CensusOrigin::MerkleOnchainDynamic
            && let Err(e @ crate::census::onchain::UsableError::Suspect(_)) = self
                .census
                .onchain
                .usable(&self.record.onchain.census.contract_address)
        {
            return Err(ActorError::Busy(e.to_string()));
        }
        if self.pending.len() >= PENDING_CAP {
            return Err(ActorError::Busy("pending queue full".into()));
        }
        // Settled by another sequencer: a duplicate even without a local record.
        if self.state.has_vote_id(v.pkg.vote_id).map_err(ierr)? {
            return Err(ActorError::Duplicate(v.pkg.vote_id));
        }
        // A resend of a queued vote is a duplicate whatever its slot's depth.
        let flight = self.in_flight.as_ref().map_or(&[][..], |f| &f.votes[..]);
        let vid = v.pkg.vote_id;
        if self
            .pending
            .iter()
            .chain(flight)
            .any(|q| q.pkg.vote_id == vid)
        {
            return Err(ActorError::Duplicate(vid));
        }
        // At most slot_depth queued (pending or in-flight) votes per slot;
        // they settle in admission order.
        let queued = self
            .pending
            .iter()
            .chain(flight)
            .filter(|q| q.slot == v.slot)
            .count();
        if queued >= self.slot_depth {
            return Err(ActorError::SlotBusy(v.slot));
        }
        // A new-slot vote must fit under max_voters with the queued new
        // slots. A revote of a queued slot reserves nothing new.
        if queued == 0 && self.state.slot_ballot(v.slot).map_err(ierr)?.is_none() {
            let occupied = self.state.occupied() as u64;
            if occupied + self.queued_new_slots()? >= self.record.onchain.max_voters {
                return Err(ActorError::MaxVoters);
            }
        }
        // Admission times never step back within the queue (clock steps),
        // so sorting by them keeps each slot's order.
        let floor = self
            .in_flight
            .iter()
            .flat_map(|f| f.votes_at.iter().copied())
            .chain(self.pending_at.last().copied())
            .max()
            .unwrap_or(0);
        let now = self.deps.clock.now().max(floor);
        let sv = StoredVote {
            pid: self.pid,
            vote_id: v.pkg.vote_id,
            address: v.pkg.address,
            slot: v.slot,
            package: pack_vote(&v)?,
            status: VoteStatus::Pending,
            error: None,
            created_at: now,
            updated_at: now,
        };
        match self.deps.db.admit_vote(&sv) {
            Ok(()) => {}
            Err(crate::storage::StorageError::VoteExists(vid)) => {
                return Err(ActorError::Duplicate(vid));
            }
            Err(e) => return Err(ierr(e)),
        }
        self.pending.push(v);
        self.pending_at.push(now);
        info!(pid = %hex::encode(self.pid31), vid, "vote admitted");
        if self.pending.len() < self.min_mix {
            // Say so, or a lone voter looks stuck.
            info!(
                pid = %hex::encode(self.pid31),
                queued = self.pending.len(),
                min_mix = self.min_mix,
                earliest = self.next_seal_not_before().unwrap_or(0),
                "queued below min mix"
            );
        }
        Ok(())
    }

    /// Queued slots (pending + in flight) not yet occupied in the tree.
    // O(queue) db gets per new-slot admit; index it if admission rate hurts.
    fn queued_new_slots(&self) -> Result<u64, ActorError> {
        let mut slots: std::collections::HashSet<u64> =
            self.pending.iter().map(|q| q.slot).collect();
        if let Some(f) = &self.in_flight {
            slots.extend(f.votes.iter().map(|q| q.slot));
        }
        let mut n = 0;
        for s in slots {
            if self.state.slot_ballot(s).map_err(ierr)?.is_none() {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Runs after every message: reconcile with the chain (resync), close
    /// out a closed window, seal a due batch, kick finalization. The
    /// monitor's Head messages are the heartbeat that drives this.
    async fn tick(&mut self) {
        // An Ignored actor never runs. A Finalized one never seals or
        // finalizes, but follows the chain to its final root (resync, then
        // the close-out of what is left).
        if self.record.local == LocalStatus::Ignored || !self.reopen_state() {
            return;
        }
        if !self.unwritten.is_empty() {
            self.retry_unwritten();
        }
        if self.queue_lost {
            let vids: Vec<u64> = self.pending.iter().map(|v| v.pkg.vote_id).collect();
            match self.deps.db.set_queue(&self.pid, &vids) {
                Ok(()) => self.queue_lost = false,
                Err(e) => error!(pid = %hex::encode(self.pid31), %e, "queue rewrite; retrying"),
            }
        }
        // Our results receipt saw no event in time: an anchored read decides.
        let results_due = self
            .results_sent
            .is_some_and(|t| tokio::time::Instant::now() >= t);
        if results_due {
            self.needs_check = true;
        }
        self.anchored_read().await;
        // Close-out and results wait on the anchored read, so one that keeps
        // failing (or an anchor stuck behind the events) is logged and counted.
        if self.needs_check {
            self.stale_ticks += 1;
            if self.stale_ticks.is_multiple_of(STALE_WARN_TICKS) {
                self.metrics.stalled_reads.fetch_add(1, Ordering::SeqCst);
                warn!(
                    pid = %hex::encode(self.pid31),
                    ticks = self.stale_ticks,
                    why = %self.stale_why,
                    "no anchored read covers the latest landing; close-out and results wait"
                );
            }
        } else {
            self.stale_ticks = 0;
        }
        if results_due && !self.needs_check {
            self.results_sent = None;
            if self.record.local == LocalStatus::Active
                && self.record.onchain.status != ProcessStatus::Results
            {
                warn!(pid = %hex::encode(self.pid31), "results transaction never confirmed; resubmitting");
            }
        }
        self.maybe_rebuild().await;
        if self.state_lost {
            return;
        }
        self.landed_deadline().await;
        // Reconcile before anything closes: a vote a synced transition
        // settled must never be errored.
        self.maybe_resync().await;
        self.maybe_close_out().await;
        if self.record.local != LocalStatus::Active {
            return;
        }
        self.maybe_clear_exposed();
        self.maybe_register_census().await;
        self.maybe_recheck_census().await;
        if std::mem::take(&mut self.boot_hint) {
            self.unknown_root_hint().await;
        }
        self.maybe_seal().await;
        self.maybe_finalize();
    }

    /// The one chain read that decides anything besides events: the process
    /// at the monitor's anchor (EIP-1898), applied by
    /// [`Self::apply_anchored`]. Once per anchor while something waits on
    /// the chain; a failed read, or one that left the record unchecked, is
    /// tried again after [`READ_RETRY`] at the same anchor. Otherwise a
    /// check at an anchor already read (something moved, nothing above the
    /// anchor) applies that read again: the block's state is what it was.
    async fn anchored_read(&mut self) {
        let Some(a) = self.anchor else {
            if self.needs_check {
                self.stale_why = "no anchor yet: the monitor has stored no block hash".into();
            }
            return;
        };
        let now = tokio::time::Instant::now();
        if self.read_retry.is_some_and(|(r, t)| r.same(&a) && now < t) || !self.read_wanted() {
            return;
        }
        // A block's state at its hash does not change: at an anchor already
        // read, the read is applied again only when something here moved.
        let stored = match &self.checked {
            Some((c, p)) if c.same(&a) => {
                if !self.needs_check && !self.await_sync || self.applied == Some(self.applied_on())
                {
                    return;
                }
                Some((**p).clone())
            }
            _ => None,
        };
        let fresh = stored.is_none();
        let p = match stored {
            // Still canonical: a reorg the monitor has not seen yet replaced
            // the block that read was made at.
            Some(_) if !self.at_anchor(a).await => {
                self.read_retry = Some((a, now + READ_RETRY));
                self.stale_why = format!("anchor block {} is no longer canonical", a.block);
                return;
            }
            Some(p) => p,
            None => match self.read_at(a).await {
                Ok(p) => p,
                Err(e) => {
                    self.read_retry = Some((a, now + READ_RETRY));
                    self.stale_why = format!("read at anchor block {}: {e}", a.block);
                    return;
                }
            },
        };
        self.apply_anchored(&p, a, fresh);
        self.checked = Some((a, Box::new(p)));
        self.read_retry = None;
        self.applied = Some(self.applied_on());
    }

    /// What an anchored read was applied on: the roots, the floor and the
    /// flags it decides. While they stand, applying it again changes
    /// nothing.
    fn applied_on(&self) -> AppliedOn {
        (
            self.state.committed().root,
            self.record.onchain.state_root,
            self.log_root,
            self.check_at,
            self.await_sync,
            self.needs_check,
        )
    }

    /// `getProcess` at the anchor's hash. After reads by hash failed for
    /// [`LANDED_EVENT_WAIT`] while the anchor's block hash still reads (an
    /// endpoint without EIP-1898 calls), degraded mode reads at the
    /// anchor's number between two checks that the block there is still
    /// the anchor's; it probes by hash again every [`DEGRADED_PROBE`] heads
    /// and leaves at the first success.
    async fn read_at(&mut self, a: Anchor) -> Result<OnchainProcess, Web3Error> {
        let now = tokio::time::Instant::now();
        if self.degraded.is_none_or(|n| n >= DEGRADED_PROBE) {
            let r = self
                .deps
                .contracts
                .process_at_hash(&self.pid31, a.hash)
                .await;
            match &r {
                Ok(_) => {
                    if self.degraded.take().is_some() || std::mem::take(&mut self.hash_stalled) {
                        info!(pid = %hex::encode(self.pid31), "reads by block hash answer again");
                    }
                    self.hash_fail_since = None;
                    return r;
                }
                Err(e) => match &mut self.degraded {
                    Some(n) => *n = 0,
                    None => {
                        let since = *self.hash_fail_since.get_or_insert(now);
                        if now < since + LANDED_EVENT_WAIT {
                            return r;
                        }
                        // A read by number is not bound to the anchor's
                        // branch behind a balancer: without the flag, wait.
                        if !self.number_reads {
                            if !std::mem::replace(&mut self.hash_stalled, true) {
                                self.metrics.hash_read_stalls.fetch_add(1, Ordering::SeqCst);
                                warn!(pid = %hex::encode(self.pid31), %e, "reads by block hash keep failing; nothing is decided from the chain until they answer (--degraded-number-reads, for a single-backend endpoint)");
                            }
                            return r;
                        }
                        if !self.at_anchor(a).await {
                            return r;
                        }
                        warn!(pid = %hex::encode(self.pid31), %e, "reads by block hash keep failing while block hashes read; degraded mode: reading at the anchor's number, checked against its hash");
                        self.degraded = Some(0);
                    }
                },
            }
        }
        if !self.at_anchor(a).await {
            return Err(Web3Error::Rpc(format!(
                "block {} is no longer the anchor's",
                a.block
            )));
        }
        let p = self.deps.contracts.process_at(&self.pid31, a.block).await?;
        if !self.at_anchor(a).await {
            return Err(Web3Error::Rpc(format!(
                "block {} moved during the read",
                a.block
            )));
        }
        Ok(p)
    }

    /// Logs a state decision made from the read at anchor `a` (the chaos
    /// harness checks each was on the canonical chain).
    fn decided(&self, what: &'static str, a: Anchor) {
        debug!(
            target: "davinci_sequencer::decision",
            pid = %hex::encode(self.pid31),
            decision = what,
            block = a.block,
            hash = %hex::encode(a.hash),
            "anchored decision"
        );
    }

    /// The endpoint's block at the anchor's number has the anchor's hash.
    async fn at_anchor(&self, a: Anchor) -> bool {
        matches!(self.deps.contracts.block_ref(a.block).await, Ok(Some((h, _))) if h == a.hash)
    }

    /// Something waits on an anchored read: a landing or event it has not
    /// covered, a sync latch, a divergence, a suspended confirmation, or
    /// pending votes (whose close-out a lost event must not hold forever),
    /// or a landed flight waiting for its event.
    fn read_wanted(&self) -> bool {
        self.needs_check
            || self.await_sync
            || self.diverged.is_some()
            || self.suspended
            || !self.pending.is_empty()
            || self
                .in_flight
                .as_ref()
                .is_some_and(|f| f.landed_deadline.is_some())
    }

    /// Applies a read at anchor `a`; it decides only what lies at or below
    /// `a.block`. A transition of our log recorded there that the chain has
    /// left diverges; results or a cancel it shows are confirmed there, and
    /// one confirmed at or below `a.block` it lacks is undone (above, it is
    /// suspended: close-out and results wait). Below an event or own landing
    /// already applied (`check_at`) it merges no field. Otherwise it heals a
    /// pin left behind the committed tree, latches an unknown chain root and
    /// merges every field the events move, with their handlers' side effects
    /// (an event lost in the mailbox at a crash is never routed again).
    /// `fresh`: a read just made, not one applied again.
    fn apply_anchored(&mut self, p: &OnchainProcess, a: Anchor, fresh: bool) {
        let committed = self.state.committed().root;
        let flight = self.in_flight.as_ref().map(|f| f.prepared.new_root);
        let ours = |r: &[u8; 32]| *r == committed || flight == Some(*r);
        let passed = self.passed_at(&p.state_root, a.block);
        // At a root we passed: a reorg took our later transitions (or the
        // chain lags). Back at our root, at or above every block the
        // divergence was seen at, the chain came back.
        match passed {
            Some(i) => {
                if self.diverged.is_none() {
                    self.decided("diverge", a);
                }
                self.diverge(i, true, a.block)
            }
            None if ours(&p.state_root)
                && self.diverged.is_some_and(|(_, _, seen)| a.block >= seen) =>
            {
                info!(pid = %hex::encode(self.pid31), block = a.block, "the chain is back on our transition log");
                self.decided("converge", a);
                self.diverged = None;
            }
            None => {}
        }
        // Below an applied event or landing, or the snapshot the fields
        // came from: an older state, decided on for divergence only.
        let below = a.block < self.check_at.max(self.fields_at.0);
        let before = self.record.onchain.clone();
        let reopened = self.confirm(p, a, below);
        if below {
            self.needs_check = true;
            self.stale_why = format!(
                "anchor block {} below block {} of an applied event, landing or merge",
                a.block,
                self.check_at.max(self.fields_at.0)
            );
            if self.record.onchain != before || reopened {
                self.put_record();
            }
            return;
        }
        // The chain is at our tree's root and the pin is not: the record
        // write after a commit was lost, or our landing reorged out.
        if p.state_root == committed && before.state_root != committed {
            warn!(pid = %hex::encode(self.pid31), "pin behind the committed tree; re-pinning at the chain root");
            self.decided("repin", a);
            self.record.onchain.state_root = committed;
        }
        // Canceled and RESULTS freeze the root: the read's root is the final
        // target, even when the events never showed the transitions before
        // it. A root we passed would be a reorg, not a target.
        if matches!(p.status, ProcessStatus::Canceled | ProcessStatus::Results)
            && p.state_root != self.record.onchain.state_root
            && passed.is_none()
        {
            info!(pid = %hex::encode(self.pid31), root = %format!("0x{}", hex::encode(p.state_root)), "process closed at a root not reached yet; syncing to it");
            self.decided("final_root", a);
            self.record.onchain.state_root = p.state_root;
        }
        let pinned = p.state_root == self.record.onchain.state_root;
        // A root that is not ours, not one we passed and not our flight's:
        // the chain went ahead with a transition the events have not shown
        // (or lost). Sync before sealing on a root that can only revert.
        if !pinned
            && !ours(&p.state_root)
            && !self.await_sync
            && passed.is_none()
            && self.passed_index(&p.state_root).is_none()
        {
            info!(pid = %hex::encode(self.pid31), root = %format!("0x{}", hex::encode(p.state_root)), "chain at an unknown root; syncing before the next seal");
            self.decided("sync_latch", a);
            self.await_sync = true;
        }
        // A sync latch with no event to clear it (a revert answered by a
        // lagging endpoint): the anchor shows our root; after a lost race,
        // in a read made since (not the one before the revert, which a reorg
        // may have replaced).
        if (fresh || !self.race_latch) && self.await_sync && pinned && p.state_root == committed {
            self.decided("sync_clear", a);
            self.clear_sync();
        }
        // While a confirmation is suspended its status and results stay.
        let mut q = p.clone();
        if self.suspended {
            q.status = self.record.onchain.status;
            q.results.clone_from(&self.record.onchain.results);
        }
        let merged = self.record.onchain.clone();
        merge_onchain(&mut self.record.onchain, &q);
        self.fields_at = self.fields_at.max((a.block, u64::MAX));
        if self.record.onchain != merged {
            self.decided("merge", a);
        }
        let o = self.record.onchain.clone();
        if o.duration != before.duration {
            self.end_moved = true;
        }
        if o.max_voters != before.max_voters {
            self.error_new_slots_if_full();
        }
        if o.census.root != before.census.root {
            self.census_moved();
            // Its event, which queues the download, may be lost.
            if self.state.config().census_origin == CensusOrigin::MerkleOffchainDynamic {
                self.census
                    .queue
                    .queue(self.pid, o.census.uri.clone(), o.census.root);
            }
        }
        if o.status != before.status {
            self.status_moved(o.status);
        }
        if self.record.onchain != before || reopened {
            self.put_record();
        }
        self.needs_check = !pinned || self.suspended;
        if !pinned {
            self.stale_why = format!(
                "chain root 0x{} at anchor block {} is not the pinned one",
                hex::encode(p.state_root),
                a.block
            );
        } else if self.suspended {
            self.stale_why = format!(
                "anchor block {} below a confirmation it does not show",
                a.block
            );
        }
    }

    /// Results and a cancel at an anchored read at `block`: present, they
    /// are confirmed there (finalized, admission closed); absent, one
    /// confirmed at or below `block` is undone (the results go in again, a
    /// cancel reopens), unless the read is `below` an applied event; one
    /// confirmed above `block` is suspended. Whether one was undone.
    fn confirm(&mut self, p: &OnchainProcess, a: Anchor, below: bool) -> bool {
        let block = a.block;
        let lower = |c: Option<u64>| Some(c.map_or(block, |c| c.min(block)));
        let mut undone = false;
        self.suspended = false;
        match p.status {
            ProcessStatus::Results => {
                self.results_at = lower(self.results_at);
                if self.record.local == LocalStatus::Active {
                    self.decided("finalize", a);
                }
                if self.record.onchain.status != ProcessStatus::Results {
                    self.record.onchain.results.clone_from(&p.results);
                    self.status_moved(ProcessStatus::Results);
                }
                self.mark_finalized();
            }
            _ => match self.results_at {
                Some(c) if block >= c && !below => {
                    warn!(pid = %hex::encode(self.pid31), status = ?p.status, block, "results gone from the chain; finalizing again");
                    self.decided("undo_results", a);
                    self.results_at = None;
                    self.record.local = LocalStatus::Active;
                    self.record.onchain.status = p.status;
                    self.record.onchain.results.clone_from(&p.results);
                    self.held = None;
                    self.results_sent = None;
                    undone = true;
                }
                Some(_) => self.suspended = true,
                None => {}
            },
        }
        match p.status {
            ProcessStatus::Canceled => {
                self.cancel_at = lower(self.cancel_at);
                if self.record.onchain.status != ProcessStatus::Canceled {
                    self.decided("cancel", a);
                    self.status_moved(ProcessStatus::Canceled);
                }
            }
            _ => match self.cancel_at {
                Some(c) if block >= c && !below => {
                    warn!(pid = %hex::encode(self.pid31), status = ?p.status, block, "cancel gone from the chain; reopening");
                    self.decided("undo_cancel", a);
                    self.cancel_at = None;
                    self.record.onchain.status = p.status;
                    undone = true;
                }
                Some(_) => self.suspended = true,
                None => {}
            },
        }
        undone
    }

    /// The index of the transition in our log leaving `root`, a root we
    /// passed: a chain read showing it lags or lost our later transitions.
    /// The log's last root counts too while the tree is past it (a landing
    /// whose record was never written): index `next_index`. A failed read
    /// counts as not passed.
    fn passed_index(&self, root: &[u8; 32]) -> Option<u64> {
        if *root == self.log_root && self.state.committed().root != self.log_root {
            return Some(self.next_index);
        }
        match self.deps.db.transitions(&self.pid) {
            Ok(ts) => ts.iter().find(|t| t.old_root == *root).map(|t| t.index),
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "transitions read");
                None
            }
        }
    }

    /// [`Self::passed_index`] for a read at `block`: only a transition our log
    /// records at or below it (a landing of ours above it, committed at its
    /// receipt, is not there yet); the unrecorded landing only at or above
    /// every block applied (`check_at`).
    fn passed_at(&self, root: &[u8; 32], block: u64) -> Option<u64> {
        if *root == self.log_root && self.state.committed().root != self.log_root {
            return (block >= self.check_at).then_some(self.next_index);
        }
        match self.deps.db.transitions(&self.pid) {
            Ok(ts) => ts
                .iter()
                .find(|t| t.old_root == *root && t.block <= block)
                .map(|t| t.index),
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "transitions read");
                None
            }
        }
    }

    /// The chain left our log at transition `index`, seen at `block`: it
    /// sits at that transition's old root (`at_root`), or forked from it. A
    /// read further along our log restarts the wait (the chain is taking it
    /// back).
    fn diverge(&mut self, index: u64, at_root: bool, block: u64) {
        let now = tokio::time::Instant::now();
        self.diverged = match self.diverged {
            Some((d, _, b)) if at_root && index > d => Some((index, now, b.max(block))),
            Some((d, t, b)) => Some((d.min(index), t, b.max(block))),
            None => {
                warn!(pid = %hex::encode(self.pid31), index, at_root, block, "the chain left our transition log; rebuilding from it unless it comes back");
                Some((index, now, block))
            }
        };
    }

    /// The chain left our log for [`LANDED_EVENT_WAIT`]: the anchored read
    /// at the current anchor, still off our root, rebuilds the process from
    /// the chain. An older read never decides it.
    async fn maybe_rebuild(&mut self) {
        let Some((d, since, _)) = self.diverged else {
            return;
        };
        if tokio::time::Instant::now() < since + LANDED_EVENT_WAIT {
            return;
        }
        let Some((a, p)) = self
            .checked
            .as_ref()
            .filter(|(a, _)| self.anchor.is_some_and(|b| b.same(a)))
            .map(|(a, p)| (*a, (**p).clone()))
        else {
            return;
        };
        // Back on our root below where it was seen: wait for the anchor.
        if p.state_root == self.state.committed().root
            || self
                .in_flight
                .as_ref()
                .is_some_and(|f| f.prepared.new_root == p.state_root)
        {
            return;
        }
        let d = match self.passed_at(&p.state_root, a.block) {
            Some(i) if i > d => return self.diverge(i, true, a.block),
            Some(i) => i,
            None => d,
        };
        // The read may be from before a reorg the monitor has not seen yet.
        if !self.at_anchor(a).await {
            return;
        }
        self.decided("rebuild", a);
        self.rebuild_from_chain(d, &p);
        // Failed (storage): wait as long again before the next try.
        if let Some((d, _, b)) = self.diverged {
            self.diverged = Some((d, tokio::time::Instant::now(), b));
        }
    }

    /// A landed-but-unconfirmed flight past its deadline (the tx reorged
    /// out, or its event is late): rolled back and requeued. In L1-blob
    /// mode, which commits at the event, it waits on while the anchored
    /// read shows our root, or while the anchor is below block B where a
    /// read by number showed it: a hint, read once, that only delays.
    async fn landed_deadline(&mut self) {
        if !self.in_flight.as_ref().is_some_and(|f| {
            f.landed_deadline
                .is_some_and(|d| tokio::time::Instant::now() >= d)
        }) {
            return;
        }
        let Some(mut f) = self.in_flight.take() else {
            return;
        };
        if self.deps.l1.is_some() {
            if f.hint_block.is_none()
                && let Ok((b, _)) = self.deps.contracts.head().await
                && let Ok(p) = self.deps.contracts.process_at(&self.pid31, b).await
                && p.state_root == f.prepared.new_root
            {
                f.hint_block = Some(b);
            }
            let anchor = self.anchor.map_or(0, |a| a.block);
            // On the anchored chain (its event late): keep waiting for it.
            let anchored = self.checked_at_anchor()
                && self
                    .checked
                    .as_ref()
                    .is_some_and(|(_, p)| p.state_root == f.prepared.new_root);
            if anchored || f.hint_block.is_some_and(|b| anchor < b) {
                debug!(pid = %hex::encode(self.pid31), "settlement on-chain, its event still pending");
                f.landed_deadline = Some(tokio::time::Instant::now() + LANDED_EVENT_WAIT);
                self.in_flight = Some(f);
                return;
            }
        }
        warn!(
            pid = %hex::encode(self.pid31),
            "landed settlement never confirmed; rolling back and requeueing"
        );
        self.rollback_flight(&f);
        self.requeue_flight(f);
    }

    /// Settled votes whose id is not in the tree at `root`: those the
    /// transitions after it settled. Every past root stays readable (tree
    /// nodes are never deleted).
    fn settled_after(&self, root: &[u8; 32]) -> Result<Vec<u64>, String> {
        let e = |e: &dyn std::fmt::Display| e.to_string();
        let ids = self
            .deps
            .db
            .vote_ids_with_status(&self.pid, &[VoteStatus::Settled])
            .map_err(|x| e(&x))?;
        let storage = self.deps.db.arbo_storage(&self.pid).map_err(|x| e(&x))?;
        let tree = arbo::Tree::new(storage, davinci_zkvm_sdk::limits::SMT_LEVELS, arbo::Sha256)
            .map_err(|x| e(&x))?;
        let mut out = Vec::new();
        for vid in ids {
            // The state tree's vote-id key: 8 bytes little-endian.
            if !tree
                .gen_proof_at(root, &vid.to_le_bytes())
                .map_err(|x| e(&x))?
                .exists
            {
                out.push(vid);
            }
        }
        Ok(out)
    }

    /// A reorg took transitions from index `d` on, which we committed. In
    /// one write: drop the tree and the log, put the votes those transitions
    /// settled back in the queue (the close-out errors them after the
    /// resync if the window closed), pin the chain's root. Then re-create
    /// the genesis and replay the chain from its blobs.
    fn rebuild_from_chain(&mut self, d: u64, p: &OnchainProcess) {
        let pid = hex::encode(self.pid31);
        let log = match self.deps.db.transitions(&self.pid) {
            Ok(l) => l,
            Err(e) => {
                error!(%pid, %e, "rebuild: transitions read");
                return;
            }
        };
        // Past the log's end: the tree's unrecorded landing left its last root.
        let fork = match d == self.next_index {
            true => Some(self.log_root),
            false => log.iter().find(|t| t.index == d).map(|t| t.old_root),
        };
        let Some(fork) = fork else {
            error!(%pid, d, "rebuild: no transition at the divergence");
            self.diverged = None;
            return;
        };
        let genesis = match genesis_root(self.state.config()) {
            Ok(g) => g,
            Err(e) => {
                error!(%pid, %e, "rebuild: genesis root");
                return;
            }
        };
        // A flight on our root can never land now.
        if let Some(f) = self.in_flight.take() {
            self.rollback_flight(&f);
            self.requeue_flight(f);
        }
        let mut unsettle = match cpu(|| self.settled_after(&fork)) {
            Ok(v) => v,
            Err(e) => {
                error!(%pid, %e, "rebuild: settled votes");
                return;
            }
        };
        // A landing of ours whose record write was lost left its votes
        // aggregated or processed, out of the queue (the flight is back in
        // it by now): they go back too.
        match self
            .deps
            .db
            .vote_ids_with_status(&self.pid, &[VoteStatus::Aggregated, VoteStatus::Processed])
        {
            Ok(ids) => {
                // Not those queued here, nor errored ones whose write retries.
                let known: std::collections::HashSet<u64> = self
                    .pending
                    .iter()
                    .map(|v| v.pkg.vote_id)
                    .chain(self.unwritten.iter().flat_map(|(v, _)| v.iter().copied()))
                    .collect();
                unsettle.extend(ids.into_iter().filter(|v| !known.contains(v)));
            }
            Err(e) => {
                error!(%pid, %e, "rebuild: unrecorded votes");
                return;
            }
        }
        let mut back: Vec<(u64, VerifiedVote)> = Vec::new();
        for vid in &unsettle {
            match self.deps.db.vote(&self.pid, *vid) {
                Ok(Some(sv)) => match unpack_vote(&sv.package) {
                    Ok(v) => back.push((sv.created_at, v)),
                    // Pending in storage: recover() errors it at a restart.
                    Err(e) => warn!(%pid, vid, %e, "unreadable stored vote"),
                },
                Ok(None) => {}
                Err(e) => {
                    error!(%pid, %e, "rebuild: vote read");
                    return;
                }
            }
        }
        // Cast order: by admission second, then by landing order in the lost
        // transitions (a voter's votes of one second sit in different ones,
        // in cast order); vote ids say nothing.
        let nf = self.state.config().ballot_mode.num_fields;
        // Where an earlier rebuild, whose check is still pending, stored
        // each kept vote landing (the replay may not have reached them).
        // Unreadable, the rebuild waits: without it, that order is lost.
        let stored = match self.rebuild_check {
            true => match self.deps.db.rebuild_check_order(&self.pid) {
                Ok(o) => o,
                Err(e) => {
                    error!(%pid, %e, "rebuild: the pending check's order is unreadable; holding");
                    return;
                }
            },
            false => Vec::new(),
        };
        let mut landed: std::collections::HashMap<u64, (u64, usize)> = stored
            .iter()
            .map(|&(v, i, p)| (v, (i, p as usize)))
            .collect();
        for t in log.iter().filter(|t| t.index >= d) {
            let td = match self.deps.db.blobs(&self.pid, t.index) {
                Ok(b) => cpu(|| decode_blobs(&b, nf)).map_err(|e| format!("{e:?}")),
                Err(e) => Err(e.to_string()),
            };
            match td {
                Ok(td) => {
                    for (i, v) in td.vote_ids.iter().enumerate() {
                        landed.insert(*v, (t.index, i));
                    }
                }
                // Only a tie-break: order by admission time alone.
                Err(e) => warn!(%pid, index = t.index, %e, "rebuild: lost transition's blobs"),
            }
        }
        back.sort_by_key(|(at, v)| {
            (
                *at,
                landed.get(&v.pkg.vote_id).copied().unwrap_or((u64::MAX, 0)),
            )
        });
        // Votes the rebuild keeps Settled (in the tree at the fork root) are
        // checked once the replay converges, as are those an earlier rebuild
        // kept whose check is still pending. Any requeued vote's reseal
        // covers every batch that carried it through the coverage history.
        let keep = log.iter().any(|t| t.index < d) || self.rebuild_check;
        // Where each kept vote landed, `(vid, index, pos)`: the check that
        // requeues those the replay does not bring back has no other way to
        // order a voter's votes of one second. The earlier rebuild's entries,
        // overwritten by those of the current log.
        let kept: Option<Vec<(u64, u64, u64)>> = keep.then(|| {
            let mut at: std::collections::HashMap<u64, (u64, u64)> =
                stored.iter().map(|&(v, i, p)| (v, (i, p))).collect();
            for t in log.iter().filter(|t| t.index < d) {
                let Ok(b) = self.deps.db.blobs(&self.pid, t.index) else {
                    continue;
                };
                if let Ok(td) = cpu(|| decode_blobs(&b, nf)) {
                    for (p, v) in td.vote_ids.iter().enumerate() {
                        at.insert(*v, (t.index, p as u64));
                    }
                }
            }
            let mut out: Vec<(u64, u64, u64)> =
                at.into_iter().map(|(v, (i, p))| (v, i, p)).collect();
            out.sort_by_key(|&(v, i, p)| (i, p, v));
            out
        });
        let mut vids = self.exposed_vids.clone();
        vids.extend(back.iter().map(|(_, v)| v.pkg.vote_id));
        let exposed = crate::storage::ExposedRecord {
            slots: self.exposed.iter().copied().collect(),
            vote_ids: vids.iter().copied().collect(),
        };
        // The tree's unrecorded landing is lost too.
        let lost = log.iter().filter(|t| t.index >= d).count();
        let depth = lost + usize::from(self.state.committed().root != self.log_root);
        let mut record = self.record.clone();
        record.onchain.state_root = p.state_root;
        record.onchain.voters_count = p.voters_count;
        record.onchain.overwritten_count = p.overwritten_count;
        record.note = Some(format!(
            "rebuilt from the chain: a reorg took {depth} committed transitions"
        ));
        let order: Vec<u64> = back.iter().map(|(_, v)| v.pkg.vote_id).collect();
        self.state.release_reads();
        if let Err(e) = self.deps.db.reset_process(
            &self.pid,
            &self.queue_with(&back),
            &exposed,
            kept.as_deref(),
            &record,
        ) {
            error!(%pid, %e, "rebuild write failed; retrying");
            return;
        }
        // The stored queue is this one again.
        self.queue_lost = false;
        error!(
            %pid,
            depth,
            index = d,
            unsettled = order.len(),
            root = %format!("0x{}", hex::encode(p.state_root)),
            "a reorg took transitions we committed; rebuilt the process from the chain"
        );
        self.metrics.rebuilds.fetch_add(1, Ordering::SeqCst);
        self.record = record;
        self.exposed_vids = vids;
        // At the head, like the store's queue: ahead of later votes of
        // their slots cast in the same second.
        let (ats, vs): (Vec<u64>, Vec<VerifiedVote>) = back.into_iter().unzip();
        self.pending.splice(0..0, vs);
        self.pending_at.splice(0..0, ats);
        self.sort_pending();
        self.next_index = 0;
        self.last_tx_block = 0;
        self.log_root = genesis;
        // Its own latch: the read it decided on may release it.
        self.await_sync = true;
        self.race_latch = false;
        self.resync_after = None;
        self.resync_backoff_ms = 0;
        self.diverged = None;
        self.cap_est = None;
        self.held = None;
        self.needs_check = true;
        self.rebuild_check = keep;
        self.state_lost = true;
        self.reopen_state();
    }

    /// After a rebuild's replay converged (the tree at the chain's root, the
    /// log complete): a vote still Settled but not in the tree was kept by
    /// the rebuild and then lost with its transition. Back to pending (its
    /// re-seal covers the batches that carried it through the history), and
    /// the check marker dropped, in one write. Whether the check is done.
    fn check_kept_settled(&mut self) -> bool {
        let pid = hex::encode(self.pid31);
        // A scan of the process's votes and a tree walk per Settled one.
        let missing: Result<Vec<u64>, String> = cpu(|| {
            let settled = self
                .deps
                .db
                .vote_ids_with_status(&self.pid, &[VoteStatus::Settled])
                .map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            for vid in settled {
                if !self.state.has_vote_id(vid).map_err(|e| e.to_string())? {
                    out.push(vid);
                }
            }
            Ok(out)
        });
        let missing = match missing {
            Ok(m) => m,
            Err(e) => {
                error!(%pid, %e, "settled votes check");
                return false;
            }
        };
        let mut back: Vec<(u64, VerifiedVote)> = Vec::new();
        for vid in &missing {
            match self.deps.db.vote(&self.pid, *vid) {
                Ok(Some(sv)) => match unpack_vote(&sv.package) {
                    Ok(v) => back.push((sv.created_at, v)),
                    // Pending in storage: recover() errors it at a restart.
                    Err(e) => warn!(%pid, vid, %e, "unreadable stored vote"),
                },
                Ok(None) => {}
                Err(e) => {
                    error!(%pid, %e, "vote read");
                    return false;
                }
            }
        }
        // Ties by where the rebuilds stored each vote landed (a voter's
        // votes of one second); without an entry (a marker with no order),
        // last. An unreadable order holds the check, and sealing with it.
        let cast: std::collections::HashMap<u64, (u64, u64)> =
            match self.deps.db.rebuild_check_order(&self.pid) {
                Ok(o) => o.into_iter().map(|(v, i, p)| (v, (i, p))).collect(),
                Err(e) => {
                    error!(%pid, %e, "rebuild check: the stored order is unreadable; holding");
                    return false;
                }
            };
        back.sort_by_key(|(at, v)| {
            (
                *at,
                cast.get(&v.pkg.vote_id)
                    .copied()
                    .unwrap_or((u64::MAX, u64::MAX)),
            )
        });
        let order: Vec<u64> = back.iter().map(|(_, v)| v.pkg.vote_id).collect();
        if let Err(e) = self
            .deps
            .db
            .finish_rebuild_check(&self.pid, &self.queue_with(&back))
        {
            error!(%pid, %e, "rebuild check write");
            return false;
        }
        self.queue_lost = false;
        if !order.is_empty() {
            warn!(%pid, votes = order.len(), "votes the rebuild kept settled are gone from the chain; re-sealing them");
            let (ats, vs): (Vec<u64>, Vec<VerifiedVote>) = back.into_iter().unzip();
            self.pending.splice(0..0, vs);
            self.pending_at.splice(0..0, ats);
            self.sort_pending();
        }
        self.rebuild_check = false;
        true
    }

    /// Reopens the state at its committed record (re-creating the genesis a
    /// rebuild left to do); false while that fails.
    fn reopen_state(&mut self) -> bool {
        if !self.state_lost {
            return true;
        }
        let cfg = self.state.config().clone();
        let cap = self.state.blob_cap();
        match open_state(&self.deps.db, &self.pid, &cfg)
            .and_then(|mut s| s.set_blob_cap(cap).map(|()| s).map_err(ierr))
        {
            Ok(s) => {
                self.state = s;
                self.state_lost = false;
                // Events may have been dropped meanwhile: re-read the chain.
                self.needs_check = true;
                true
            }
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "re-creating the rebuilt process failed; retrying");
                false
            }
        }
    }

    /// Repairs falling behind the chain (lost race, failed blob sync, a lost
    /// transition record): on every heartbeat, if the committed root is not
    /// the on-chain root or the log is behind the tree, replay the gap, with
    /// capped exponential backoff between attempts. The target root lives in
    /// `record.onchain.state_root` and is never dropped on a failed attempt.
    async fn maybe_resync(&mut self) {
        let behind = |a: &Self| {
            let root = a.state.committed().root;
            a.await_sync || root != a.record.onchain.state_root || a.log_root != root
        };
        if !behind(self) {
            // Converged after a rebuild: check the votes it kept Settled,
            // once; a failed check retries under the resync backoff.
            if self.rebuild_check && self.diverged.is_none() {
                // A failed check waits out its backoff, which only a
                // successful one resets.
                if self
                    .resync_after
                    .is_some_and(|t| tokio::time::Instant::now() < t)
                {
                    return;
                }
                if !self.check_kept_settled() {
                    let ms = (self.resync_backoff_ms * 2).clamp(100, 60_000);
                    self.resync_backoff_ms = ms;
                    self.resync_after =
                        Some(tokio::time::Instant::now() + std::time::Duration::from_millis(ms));
                    return;
                }
            }
            self.resync_after = None;
            self.resync_backoff_ms = 0;
            return;
        }
        if self.in_flight.is_some() {
            if self.record.local != LocalStatus::Finalized {
                return; // an own settlement or its event will resolve first
            }
            // Past RESULTS a flight can only revert: drop it so the tree can
            // follow the chain (its votes settle from the sync or close).
            if let Some(f) = self.in_flight.take() {
                self.rollback_flight(&f);
                self.requeue_flight(f);
            }
            if self.state_lost {
                return;
            }
        }
        if let Some(t) = self.resync_after
            && tokio::time::Instant::now() < t
        {
            return;
        }
        let upto = self.last_head_block.max(self.record.onchain.creation_block);
        self.replay_gap(upto).await;
        if behind(self) {
            // Re-read the chain too: the target may be gone (a pinned
            // transition reorged out before it applied), which only a read
            // shows and heals.
            self.needs_check = true;
            let ms = (self.resync_backoff_ms * 2).clamp(100, 60_000);
            self.resync_backoff_ms = ms;
            self.resync_after =
                Some(tokio::time::Instant::now() + std::time::Duration::from_millis(ms));
        } else {
            self.resync_after = None;
            self.resync_backoff_ms = 0;
        }
    }

    /// A cancel or the results closed the process for good: no grace end
    /// to wait for.
    fn close_requested(&self) -> bool {
        self.record.local == LocalStatus::Finalized
            || matches!(
                self.record.onchain.status,
                ProcessStatus::Canceled | ProcessStatus::Results
            )
    }

    /// A read by number at the head showing a root that is neither ours nor
    /// one we passed: hold sealing (the sync latch) until an anchored read
    /// at or above that block shows our root, or the sync applies. A hint:
    /// it only delays.
    async fn unknown_root_hint(&mut self) {
        let Ok((block, _)) = self.deps.contracts.head().await else {
            return;
        };
        let Ok(p) = self.deps.contracts.process_at(&self.pid31, block).await else {
            return;
        };
        let committed = self.state.committed().root;
        if p.state_root != committed
            && p.state_root != self.record.onchain.state_root
            && self.passed_index(&p.state_root).is_none()
        {
            info!(pid = %hex::encode(self.pid31), root = %format!("0x{}", hex::encode(p.state_root)), "chain at an unknown root; syncing before the next seal");
            self.await_sync = true;
            self.needs_check = true;
            self.hint_floor(block).await;
        }
    }

    /// Raises the anchored-read floor to a block a hint named (a revert's,
    /// the head's), once the endpoint knows that block: a bogus `latest`
    /// must not hold every later anchored read below it.
    async fn hint_floor(&mut self, block: u64) {
        if matches!(self.deps.contracts.block_ref(block).await, Ok(Some(_))) {
            self.check_at = self.check_at.max(block);
        }
    }

    /// The sync latch is released (a sync applied, or the chain shows our
    /// root).
    fn clear_sync(&mut self) {
        self.await_sync = false;
        self.race_latch = false;
    }

    /// The last anchored read is at the current anchor.
    fn checked_at_anchor(&self) -> bool {
        matches!((&self.checked, self.anchor), (Some((c, _)), Some(a)) if c.same(&a))
    }

    /// Errors the pending votes once the chain closed the window: nothing in
    /// flight, the tree at the pinned root, the read at the current anchor
    /// applied with nothing newer since and no confirmation suspended, and
    /// the anchor block's time past the record's grace end (a later block
    /// can neither land nor extend), or the process canceled or with
    /// results. Anything else waits for the next anchor.
    async fn maybe_close_out(&mut self) {
        let committed = self.state.committed().root;
        if self.pending.is_empty()
            || self.in_flight.is_some()
            || self.await_sync
            || committed != self.record.onchain.state_root
            || self.needs_check
            || self.suspended
            || !self.checked_at_anchor()
            || !(self.close_requested()
                || self
                    .anchor
                    .is_some_and(|a| a.time != 0 && a.time >= self.grace_end()))
        {
            return;
        }
        let Some(a) = self.anchor else {
            return;
        };
        // The read may be from before a reorg the monitor has not seen yet.
        if !self.at_anchor(a).await || !self.settle_pending_in_tree() {
            return;
        }
        self.decided("close_out", a);
        let reason = self.closed_reason();
        self.close_out(&reason);
    }

    /// The close reason of a pending vote: held votes name why (a node-side
    /// check, else the prover's reason under a hold).
    fn closed_reason(&self) -> String {
        match self.held_reason.as_ref().or(self.prover_hold.as_ref()) {
            Some(m) => format!("process closed (held: {m})"),
            None => "process closed".into(),
        }
    }

    /// Errors out every pending vote; they can never settle now.
    fn close_out(&mut self, reason: &str) {
        self.bisect = None;
        if self.pending.is_empty() {
            return;
        }
        let vids: Vec<u64> = self.pending.iter().map(|v| v.pkg.vote_id).collect();
        self.finish(&vids, VoteStatus::Error, Some(reason));
    }

    fn set_status(&self, vids: &[u64], s: VoteStatus, err: Option<&str>) {
        if let Err(e) = self.deps.db.set_status(&self.pid, vids, s, err) {
            error!(pid = %hex::encode(self.pid31), %e, "set_status");
        }
    }

    /// Errors the votes of a failed batch (out of the queue); a failed write
    /// is retried every tick.
    fn error_batch(&mut self, vids: &[u64], msg: &str) {
        if let Err(e) = self
            .deps
            .db
            .set_status(&self.pid, vids, VoteStatus::Error, Some(msg))
        {
            error!(pid = %hex::encode(self.pid31), %e, "vote status write; retrying");
            self.unwritten.push((vids.to_vec(), msg.to_string()));
        }
    }

    /// Retries the failed error writes; a vote another sequencer landed
    /// meanwhile stays settled.
    fn retry_unwritten(&mut self) {
        for (vids, msg) in std::mem::take(&mut self.unwritten) {
            let open: Vec<u64> = vids
                .into_iter()
                .filter(|v| {
                    !matches!(self.deps.db.vote(&self.pid, *v),
                        Ok(Some(sv)) if sv.status == VoteStatus::Settled)
                })
                .collect();
            if !open.is_empty()
                && self
                    .deps
                    .db
                    .set_status(&self.pid, &open, VoteStatus::Error, Some(&msg))
                    .is_err()
            {
                self.unwritten.push((open, msg));
            }
        }
    }

    /// Queued votes take a final status and leave the queue, in one write;
    /// a failed write keeps them queued here for the next pass to retry.
    fn finish(&mut self, vids: &[u64], s: VoteStatus, err: Option<&str>) -> bool {
        if let Err(e) = self.deps.db.finish_votes(&self.pid, vids, s, err) {
            error!(pid = %hex::encode(self.pid31), %e, "vote status write; retrying");
            return false;
        }
        self.drop_pending(vids);
        true
    }

    /// Drops (settles) pending votes whose id already reached the tree —
    /// synced from another sequencer or left over from a crash mid-settle —
    /// and errors the earlier pending votes of their slots: the queue is in
    /// cast order, and sealing one of those would overwrite the later vote.
    /// False while that is not stored: nothing may seal meanwhile.
    fn settle_pending_in_tree(&mut self) -> bool {
        let (settled, superseded) = match self.supersede_split() {
            Ok(x) => x,
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "has_vote_id");
                return false;
            }
        };
        if settled.is_empty() {
            return true;
        }
        // One write, so no failure or crash keeps a superseded vote queued.
        if let Err(e) = self.deps.db.settle_votes(&self.pid, &settled, &superseded) {
            error!(pid = %hex::encode(self.pid31), %e, "vote status write; retrying");
            return false;
        }
        if !superseded.is_empty() {
            info!(pid = %hex::encode(self.pid31), n = superseded.len(), "queued votes superseded by a later vote of their voter");
        }
        self.drop_pending(&settled);
        self.drop_pending(&superseded);
        true
    }

    /// The queued votes whose id is in the tree, and the queued votes cast
    /// before one of those by the same voter (the queue is in cast order).
    fn supersede_split(&self) -> Result<(Vec<u64>, Vec<u64>), StateError> {
        let (mut settled, mut superseded) = (Vec::new(), Vec::new());
        // Slots with a later queued vote in the tree, walking back.
        let mut later = std::collections::HashSet::new();
        for v in self.pending.iter().rev() {
            if self.state.has_vote_id(v.pkg.vote_id)? {
                settled.push(v.pkg.vote_id);
                later.insert(v.slot);
            } else if later.contains(&v.slot) {
                superseded.push(v.pkg.vote_id);
            }
        }
        Ok((settled, superseded))
    }

    /// Smallest selection that could fill a transaction, taking every vote
    /// as an overwrite and the whole exposed set as live. A shorter queue
    /// can never trip the blob cap, so its selection is skipped. In memory only.
    fn capacity_estimate(&mut self) -> usize {
        let key = (self.state.occupied(), self.exposed.len());
        if let Some((k, n)) = self.cap_est
            && k == key
        {
            return n;
        }
        let (occ, e) = key;
        let nf = self.state.config().ballot_mode.num_fields;
        let cap = self.state.blob_cap();
        let n = (1..=MAX_BATCH_SIZE)
            .find(|&n| {
                let r = refresh_target(n, n).min(occ).max(e);
                r > MAX_REFRESH || blob::blob_count(n, n + r, nf) > cap
            })
            .unwrap_or(MAX_BATCH_SIZE + 1);
        self.cap_est = Some((key, n));
        n
    }

    async fn maybe_seal(&mut self) {
        if self.pending.is_empty() {
            self.jitter = None;
        }
        if self.in_flight.is_some()
            || self.finalizing
            || self.await_sync
            || self.end_moved
            // A failed batch's errors are not stored yet.
            || !self.unwritten.is_empty()
            // Nor the queue's order.
            || self.queue_lost
            || self.pending.is_empty()
            || self.record.local != LocalStatus::Active
            || self.deps.contracts.signer().is_none()
            // The chain left our log: our root may never land.
            || self.diverged.is_some()
            // Votes a rebuild kept Settled are not checked yet: one gone
            // from the chain goes back to pending first.
            || self.rebuild_check
            // Never seal off a stale root: wait until the committed tree
            // matches the pinned on-chain root (resync closes the gap).
            || self.state.committed().root != self.record.onchain.state_root
            // Nor before the log has the tree's last transition: the next
            // landing would take its index and it would never be recorded.
            || self.log_root != self.state.committed().root
        {
            return;
        }
        // Ready seals; Ended flushes; Paused blocks only before the end (a
        // pause cannot outlive it, the registry settles it like Ended).
        let flush = self.flush();
        match self.record.onchain.status {
            ProcessStatus::Ready => {}
            ProcessStatus::Ended if flush => {}
            ProcessStatus::Paused if self.chain_time >= self.end_time() => {}
            _ => return,
        }
        // Cooling down after transient prover/RPC trouble, holding for a
        // pause the events have not shown yet, or after failed L1 cycles?
        if let Some(t) = self.prove_after.max(self.pause_hold).max(self.failed_pause)
            && tokio::time::Instant::now() < t
        {
            return;
        }
        // Nor before the start, where the settlement reverts: admission
        // waits for it, but a lagging head read can step back past it.
        if self.chain_time < self.record.onchain.start_time {
            return;
        }
        // Triggers. The selection reads storage, so it runs only when
        // a trigger can fire: the queue could fill a transaction, a
        // deadline passed, or flush mode.
        let j = *self.jitter.get_or_insert_with(|| seal_jitter(&mut OsRng));
        let age = self
            .deps
            .clock
            .now()
            .saturating_sub(self.pending_at.first().copied().unwrap_or(u64::MAX))
            as f64;
        let timer_due = age >= self.batch_time as f64 * j;
        let solo_due = age >= self.solo_wait as f64 * j;
        let at_max = self.pending.len() >= self.seal_max();
        if !(at_max || timer_due || flush || self.pending.len() >= self.capacity_estimate()) {
            return;
        }
        // Unwritten, a superseded vote could seal over its voter's later one.
        if !self.settle_pending_in_tree() || self.pending.is_empty() {
            return;
        }
        // Without the votes that just settled.
        let at_max = self.pending.len() >= self.seal_max();
        // The batch census root (guest register 20), by origin.
        let Some(census_root) = self.batch_census_root() else {
            return;
        };
        if self.state.config().census_root != census_root {
            self.state.set_census_root(census_root);
        }
        let max_voters = self.record.onchain.max_voters;
        let held = self.held_votes();
        // Each candidate's coverage history, read once for this attempt: a
        // vote is charged the histories of the votes its batch carries,
        // never those of votes left out.
        let cov = {
            let cands = held.as_deref().unwrap_or(&self.pending);
            let ids: Vec<u64> = cands.iter().map(|v| v.pkg.vote_id).collect();
            // One index scan per candidate: off the async worker.
            match cpu(|| self.deps.db.coverage(&self.pid, &ids)) {
                Ok(c) => c,
                Err(e) => {
                    let secs = self.prove_backoff();
                    error!(pid = %hex::encode(self.pid31), %e, retry_in = secs, "coverage history read");
                    return;
                }
            }
        };
        let none = BTreeSet::new();
        // Time budget: the largest batch whose estimated proof plus the
        // landing margin ends before the window closes. While open, the
        // earliest legal close is an END now under a grace lowered to the
        // floor; past the end neither is possible and the grace end binds.
        // `Ended` is past the end even before a head shows the END block's
        // time (a shortened end stays `Ready` until it).
        let nf = self.state.config().ballot_mode.num_fields;
        let now = self.chain_time.max(self.deps.clock.now());
        let mut deadline = self.grace_end();
        if self.chain_time < self.end_time() && self.record.onchain.status != ProcessStatus::Ended {
            deadline = deadline.min(now.saturating_add(self.deps.grace.grace_floor));
        }
        let window = deadline.saturating_sub(now);
        let spv = self.metrics.prove.spv(nf);
        // L1-blob mode posts a batch in chunks of `l1.chunk` blobs. The leg
        // normally runs during the proof and must fit the window on its
        // own. While bisecting or holding, and after any cycle that posted
        // and failed (until our next landing), the job is proof-first: a
        // flaky prover spends no post per cycle, the leg follows the proof
        // and each chunk takes proof time.
        let proof_first = self.deps.l1.is_some()
            && (self.bisect.is_some()
                || self.prover_hold.is_some()
                || self.proof_first_until_landing);
        let chunk = self.deps.l1.as_ref().map(|l1| l1.chunk);
        let blob_cap = self.state.blob_cap();
        let max_chunks = chunk.map_or(1, |c| blob_cap.div_ceil(c.max(1)));
        let seal_max = self.seal_max();
        // (cap, max_blobs) for a batch posted in `chunks` chunks; `None`:
        // no batch fits (nor does one of more chunks).
        let budget = |chunks: usize| -> Option<(usize, usize)> {
            let extra = crate::l1da::POST_TIME.saturating_mul(chunks as u64 - 1);
            let leg = self.l1_settle_lag.saturating_add(extra);
            let serial = match chunk {
                None => 0,
                Some(_) if proof_first => leg,
                Some(_) => {
                    if window < leg.saturating_add(self.settle_margin) {
                        return None;
                    }
                    0
                }
            };
            let left = window as f64 - self.prove_base - self.settle_margin as f64 - serial as f64;
            let n_max = (left / spv).floor();
            if n_max < 1.0 {
                return None;
            }
            let cap = seal_max
                .min(n_max.min(MAX_BATCH_SIZE as f64) as usize)
                .max(1);
            Some((cap, chunk.map_or(blob_cap, |c| (chunks * c).min(blob_cap))))
        };
        // One selection per feasible chunk count: the largest batch wins,
        // the fewest chunks on a tie. A vote is charged only the histories
        // its batch carries, and a vote that does not fit is skipped, so a
        // vote no budget can carry holds up nothing else.
        let chosen = cpu(|| {
            choose_chunks(
                max_chunks,
                |chunks| {
                    let (cap, max_blobs) = budget(chunks)?;
                    Some(
                        self.state
                            .select_covered(
                                held.as_deref().unwrap_or(&self.pending),
                                max_voters,
                                &none,
                                &cov,
                                cap,
                                max_blobs,
                            )
                            .map(|sel| (cap, max_blobs, sel)),
                    )
                },
                |(cap, _, sel)| (sel.votes.len(), sel.full && sel.votes.len() == *cap),
            )
        });
        let (cap, max_blobs, k, full, mut sized) = match chosen {
            Ok(Some((_, (cap, max_blobs, sel)))) => (
                cap,
                max_blobs,
                sel.votes.len(),
                sel.full,
                sel.votes
                    .into_iter()
                    .cloned()
                    .collect::<Vec<VerifiedVote>>(),
            ),
            Ok(None) => {
                debug!(pid = %hex::encode(self.pid31), window, "no batch fits before the window closes");
                return; // the window closes on these votes
            }
            // This vote's own history fits no transition: it recasts.
            Err(StateError::CoverageOverflow { vote_id, .. }) => {
                self.recast_overflow(vote_id);
                return;
            }
            Err(e) => {
                let secs = self.prove_backoff();
                error!(pid = %hex::encode(self.pid31), %e, retry_in = secs, "select_batch; votes stay pending");
                return;
            }
        };
        // Company is the batch size (distinct admissible slots), not the
        // queue length. A vote kept out by its own history does not make
        // the batch full: it waits for the timer.
        let seal = k >= 1
            && (full
                || at_max
                || (k >= self.min_mix && timer_due)
                || (k < self.min_mix && solo_due)
                || flush);
        if !seal {
            return;
        }
        // Several can hold at once; log the dominant one.
        let reason = if flush {
            "flush"
        } else if at_max {
            "at_max"
        } else if full {
            "full"
        } else if k >= self.min_mix {
            "timer"
        } else {
            "solo"
        };
        debug!(
            pid = %hex::encode(self.pid31),
            k,
            queued = self.pending.len(),
            reason = %reason,
            age,
            "seal attempt"
        );
        // Seal-time re-proof (origins 2/3) under the chosen budget: changed
        // voters are errored, and what is left is selected again.
        let selected: Vec<VerifiedVote> = loop {
            let n_sized = sized.len();
            let Some(keep) = self.reproof(census_root, sized).await else {
                return; // census layer trouble; retry next tick
            };
            if keep.len() == n_sized {
                break keep; // nothing dropped, sizing holds
            }
            let now_held = self.held_votes();
            // The re-proof errored the last held vote: the queue is not the
            // set the coverage was read for. The next tick reads it afresh.
            if now_held.is_some() != held.is_some() {
                debug!(pid = %hex::encode(self.pid31), "the held votes are gone; reselecting next tick");
                return;
            }
            let cands = now_held.as_deref().unwrap_or(&self.pending);
            debug_assert!(cands.iter().all(|v| cov.covers(v.pkg.vote_id)));
            sized = match cpu(|| {
                self.state
                    .select_covered(cands, max_voters, &none, &cov, cap, max_blobs)
            }) {
                Ok(sel) if !sel.votes.is_empty() => sel.votes.into_iter().cloned().collect(),
                Ok(_) => return,
                // Unreachable after the passes; never errors votes.
                Err(e) => {
                    let secs = self.prove_backoff();
                    error!(pid = %hex::encode(self.pid31), %e, retry_in = secs, "select_batch; votes stay pending");
                    return;
                }
            };
        };
        let vids: Vec<u64> = selected.iter().map(|v| v.pkg.vote_id).collect();
        // Exactly what these votes' seal must refresh: the cover the
        // selection sized.
        let must = cov.union(&vids);
        let prepared = if std::mem::take(&mut self.force_overflow) {
            Err(StateError::RefreshOverflow {
                exposed: must.len(),
                votes: selected.len(),
            })
        } else {
            cpu(|| self.state.prepare(&selected, &mut OsRng, &must))
        };
        let prepared = match prepared {
            Ok(p) => p,
            // select_batch sized the batch, so this is a node fault (a tree
            // write; a refresh overflow is unreachable): the votes stay
            // pending, never errored here, and the grace end closes them if
            // it never clears.
            Err(e) => {
                let secs = self.prove_backoff();
                error!(pid = %hex::encode(self.pid31), %e, retry_in = secs, "prepare failed; votes stay pending");
                // prepare's own reset of the root is best-effort. If ours
                // fails too, reopen at the committed record before any more
                // state work.
                if let Err(e) = self.state.reset_to_committed() {
                    error!(pid = %hex::encode(self.pid31), %e, "tree reset after a failed prepare; reopening");
                    self.state_lost = true;
                }
                return;
            }
        };
        // Belt: select_batch sized the batch for the cap, so this never
        // fires; a batch above it could never land.
        if prepared.blobs.blobs.len() > max_blobs || std::mem::take(&mut self.force_belt) {
            let secs = self.prove_backoff();
            error!(
                pid = %hex::encode(self.pid31),
                blobs = prepared.blobs.blobs.len(),
                cap = max_blobs,
                retry_in = secs,
                "prepared batch exceeds the blob cap; not sealing"
            );
            self.rollback_prepared(&prepared);
            return;
        }
        // One commit before the batch can reach the chain: the votes go
        // aggregated, the widened exposed set is stored, and the batch joins
        // the coverage history (a reseal of any of its votes refreshes its
        // slots, whatever happens to this batch).
        let mut slots = self.exposed.clone();
        slots.extend(prepared.updated_slots());
        let mut evids = self.exposed_vids.clone();
        evids.extend(vids.iter().copied());
        let rec = crate::storage::ExposedRecord {
            slots: slots.iter().copied().collect(),
            vote_ids: evids.iter().copied().collect(),
        };
        let entry = crate::storage::ExposedRecord {
            slots: prepared.updated_slots(),
            vote_ids: vids.clone(),
        };
        if let Err(e) =
            self.deps
                .db
                .seal_batch_covered(&self.pid, &vids, &rec, &prepared.new_root, &entry)
        {
            let secs = self.prove_backoff();
            error!(pid = %hex::encode(self.pid31), %e, retry_in = secs, "seal_batch");
            self.rollback_prepared(&prepared);
            return;
        }
        self.exposed = slots;
        self.exposed_vids = evids;
        let at: std::collections::HashMap<u64, u64> = self
            .pending
            .iter()
            .zip(&self.pending_at)
            .map(|(v, t)| (v.pkg.vote_id, *t))
            .collect();
        let votes_at: Vec<u64> = vids
            .iter()
            .map(|v| at.get(v).copied().unwrap_or(now))
            .collect();
        self.drop_pending(&vids);
        self.jitter = None;
        self.generation += 1;
        info!(
            pid = %hex::encode(self.pid31),
            votes = vids.len(),
            overwrites = prepared.overwrites,
            reason = %reason,
            "batch sealed"
        );
        let cancel = CancellationToken::new();
        // The grace end known now; it only moves later.
        let left = std::time::Duration::from_secs(self.grace_end().saturating_sub(now));
        let job = JobInput {
            generation: self.generation,
            pid31: self.pid31,
            request: Arc::clone(&prepared.request),
            expected: prepared.expected,
            blobs: prepared.blobs.clone(),
            prover: self.deps.prover.clone(),
            chain: self.deps.contracts.clone(),
            tx: self.tx.clone(),
            clock: self.deps.clock.clone(),
            metrics: self.metrics.clone(),
            nf,
            n: vids.len(),
            prove_base: self.prove_base,
            l1: self.deps.l1.clone(),
            proof_first,
            deadline: tokio::time::Instant::now() + left.min(L1_JOB_CAP),
            cancel: cancel.clone(),
            posted: Default::default(),
        };
        self.in_flight = Some(Flight {
            generation: self.generation,
            prepared,
            votes: selected,
            votes_at,
            landed_deadline: None,
            hint_block: None,
            proved: false,
            _abort: cancel.drop_guard(),
        });
        tokio::spawn(run_job(job));
    }

    /// The census root the next batch commits to. `None` = do not seal
    /// this tick.
    fn batch_census_root(&mut self) -> Option<[u8; 32]> {
        match self.state.config().census_origin {
            // Fixed at genesis (or the CSP address).
            CensusOrigin::MerkleStatic | CensusOrigin::Csp => Some(self.state.config().census_root),
            // The latest CensusUpdated root, once its tree is loaded. The
            // store only holds the newest root per process, so
            // waiting here never seals against a stale census.
            CensusOrigin::MerkleOffchainDynamic => {
                let root = self.record.onchain.census.root;
                match self.census.store.has(&root) {
                    Ok(true) => Some(root),
                    Ok(false) => None, // still downloading, or failed for good
                    Err(e) => {
                        error!(pid = %hex::encode(self.pid31), ?e, "census lookup");
                        None
                    }
                }
            }
            // The contract's latest confirmed root at seal time.
            CensusOrigin::MerkleOnchainDynamic => {
                let c = self.record.onchain.census.contract_address;
                match self.census.onchain.usable(&c) {
                    Ok(()) => {}
                    // A failed register at resume: transient, healed by
                    // maybe_register_census on the next tick.
                    Err(crate::census::onchain::UsableError::NotIndexed) => return None,
                    // Queued votes wait for the next good sync.
                    Err(crate::census::onchain::UsableError::Suspect(_)) => return None,
                    Err(crate::census::onchain::UsableError::Unusable(reason)) => {
                        self.census_unusable(&reason);
                        return None;
                    }
                }
                self.census.onchain.latest(&c).map(|(root, _, _)| root)
            }
        }
    }

    /// Origin 3, the contract can never validate again: error every
    /// pending vote once and stop admitting.
    fn census_unusable(&mut self, reason: &str) {
        if self.census_broken.is_some() {
            return;
        }
        error!(pid = %hex::encode(self.pid31), %reason, "census contract unusable; refusing votes");
        let msg = format!("census contract unusable: {reason}");
        self.close_out(&msg);
        self.census_broken = Some(msg);
    }

    /// Origin 3: a resume whose `register` failed leaves the contract
    /// unindexed, which is transient — re-register until it sticks.
    async fn maybe_register_census(&mut self) {
        if self.state.config().census_origin != CensusOrigin::MerkleOnchainDynamic {
            return;
        }
        let c = self.record.onchain.census.contract_address;
        if matches!(
            self.census.onchain.usable(&c),
            Err(crate::census::onchain::UsableError::NotIndexed)
        ) && let Err(e) = self.census.onchain.register(c).await
        {
            error!(pid = %hex::encode(self.pid31), ?e, "census contract register");
        }
    }

    /// Seal-time re-proof (origins 2/3): re-derives each selected vote's
    /// witness at the batch root. A voter missing at that root or with a
    /// changed leaf is errored ("census changed, recast") and dropped.
    /// `None` = census layer trouble; do not seal this tick.
    async fn reproof(
        &mut self,
        root: [u8; 32],
        selected: Vec<VerifiedVote>,
    ) -> Option<Vec<VerifiedVote>> {
        let origin = self.state.config().census_origin;
        if !matches!(
            origin,
            CensusOrigin::MerkleOffchainDynamic | CensusOrigin::MerkleOnchainDynamic
        ) {
            return Some(selected);
        }
        let contract = self.record.onchain.census.contract_address;
        let mut keep = Vec::with_capacity(selected.len());
        let mut changed: Vec<u64> = Vec::new();
        // Sequential lookups: a cache-miss tree rebuild on a large
        // batch stalls this mailbox — batch the proofs if seal latency bites.
        for mut v in selected {
            let res = if origin == CensusOrigin::MerkleOffchainDynamic {
                self.census.store.proof_async(&root, &v.pkg.address).await
            } else {
                self.census
                    .onchain
                    .proof(&contract, &root, &v.pkg.address)
                    .await
            };
            match res {
                // The leaf binds address and weight, so this covers both
                // "dropped from the census" and "weight changed".
                Ok(Some((p, _))) if vote_still_valid(self.state.config(), &v.pkg, Some(p.leaf)) => {
                    v.pkg.census = CensusWitness::Merkle(p);
                    keep.push(v);
                }
                Ok(_) => changed.push(v.pkg.vote_id),
                Err(e) => {
                    warn!(pid = %hex::encode(self.pid31), ?e, "census re-proof failed; not sealing");
                    return None;
                }
            }
        }
        if !changed.is_empty() {
            warn!(
                pid = %hex::encode(self.pid31),
                n = changed.len(),
                "votes dropped at seal: census changed"
            );
            // Kept queued, they would be selected and dropped again.
            if !self.finish(&changed, VoteStatus::Error, Some("census changed, recast")) {
                return None;
            }
        }
        Some(keep)
    }

    /// Origin 2, after a CensusUpdated: once the new tree is loaded, every
    /// pending vote is re-checked against it and changed ones are errored.
    async fn maybe_recheck_census(&mut self) {
        if !self.census_recheck {
            return;
        }
        if self.pending.is_empty() {
            self.census_recheck = false;
            return;
        }
        let root = self.record.onchain.census.root;
        if !self.census.store.has(&root).unwrap_or(false) {
            return; // still downloading; re-check next tick
        }
        let mut changed: Vec<u64> = Vec::new();
        for v in &self.pending {
            let leaf = match self.census.store.proof_async(&root, &v.pkg.address).await {
                Ok(p) => p.map(|(p, _)| p.leaf),
                Err(e) => {
                    warn!(pid = %hex::encode(self.pid31), ?e, "census re-check failed");
                    return; // retry next tick
                }
            };
            if !vote_still_valid(self.state.config(), &v.pkg, leaf) {
                changed.push(v.pkg.vote_id);
            }
        }
        self.census_recheck = false;
        if changed.is_empty() {
            return;
        }
        info!(
            pid = %hex::encode(self.pid31),
            n = changed.len(),
            "pending votes dropped: census changed"
        );
        if !self.finish(&changed, VoteStatus::Error, Some("census changed, recast")) {
            self.census_recheck = true;
        }
    }

    /// Drops the exposed set once every recorded vote is settled or
    /// errored — no later re-seal needs to cover those slots any more.
    /// An errored exposed vote recast later is a fresh transition, like any
    /// cross-batch revote; the exposed set only ties re-attempts of one batch.
    fn maybe_clear_exposed(&mut self) {
        if self.exposed_vids.is_empty() {
            return;
        }
        for vid in &self.exposed_vids {
            match self.deps.db.vote(&self.pid, *vid) {
                // A missing row can never become pending again.
                Ok(Some(v)) if !matches!(v.status, VoteStatus::Settled | VoteStatus::Error) => {
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    error!(pid = %hex::encode(self.pid31), %e, "vote read");
                    return;
                }
            }
        }
        // The coverage history keeps every batch's slots for a later reseal.
        if let Err(e) = self.deps.db.clear_exposed(&self.pid) {
            error!(pid = %hex::encode(self.pid31), %e, "clear exposed set");
            return;
        }
        self.exposed.clear();
        self.exposed_vids.clear();
    }

    fn drop_pending(&mut self, vids: &[u64]) {
        let set: std::collections::HashSet<u64> = vids.iter().copied().collect();
        let mut i = 0;
        while i < self.pending.len() {
            if set.contains(&self.pending[i].pkg.vote_id) {
                self.pending.remove(i);
                self.pending_at.remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn rollback_flight(&mut self, f: &Flight) {
        self.rollback_prepared(&f.prepared);
    }

    /// A failed rollback leaves the tree at the prepared root: reopen at
    /// the committed record before any more state work.
    fn rollback_prepared(&mut self, p: &davinci_state::PreparedBatch) {
        if let Err(e) = self.state.rollback(p) {
            error!(pid = %hex::encode(self.pid31), %e, "rollback failed; reopening");
            self.state_lost = true;
        }
    }

    /// Extends the transient-failure streak; returns the cooldown in seconds.
    fn prove_backoff(&mut self) -> u64 {
        self.prove_backoff_upto(300)
    }

    /// [`Self::prove_backoff`] with a ceiling of `max` seconds.
    fn prove_backoff_upto(&mut self, max: u64) -> u64 {
        self.prove_attempts += 1;
        let secs = (1u64 << self.prove_attempts.min(8)).min(max);
        self.prove_after = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(secs));
        secs
    }

    /// Counts a cycle that posted blobs but did not land; from the
    /// [`FAILED_CYCLES_BEFORE_PAUSE`]th in a row, pauses sealing: 10 min,
    /// doubling per further failure, at most 1 h. The votes stay pending.
    fn failed_cycle(&mut self, error: &str) {
        self.failed_cycles = self.failed_cycles.saturating_add(1);
        self.proof_first_until_landing = true;
        let Some(extra) = self.failed_cycles.checked_sub(FAILED_CYCLES_BEFORE_PAUSE) else {
            return;
        };
        let pause = FIRST_PAUSE.saturating_mul(1 << extra.min(8)).min(MAX_PAUSE);
        self.failed_pause = Some(tokio::time::Instant::now() + pause);
        error!(
            pid = %hex::encode(self.pid31),
            pause_secs = pause.as_secs(),
            failed_cycles = self.failed_cycles,
            %error,
            "L1 cycles keep posting blobs without landing; pausing this process's sealing (votes stay pending)"
        );
    }

    /// Puts a flight's votes back in the queue with their original age,
    /// ahead of the later votes of their slots.
    fn requeue_flight(&mut self, f: Flight) {
        if let Err(e) = self.deps.db.requeue(&self.pid, &f.prepared.vote_ids) {
            error!(pid = %hex::encode(self.pid31), %e, "requeue; rewriting the queue");
            self.queue_lost = true;
        }
        self.pending.splice(0..0, f.votes);
        self.pending_at.splice(0..0, f.votes_at);
        self.sort_pending();
    }

    /// A batch the prover failed for good: requeue it and bisect. The first
    /// failure retries at full size; each later one halves the next seal,
    /// until a single vote fails and is errored alone. After
    /// [`ISOLATED_BEFORE_HOLD`] of those the prover fails everything: hold,
    /// erroring nothing more.
    fn prover_failed(&mut self, f: Flight, reason: String) {
        let n = f.votes.len();
        let vids: BTreeSet<u64> = f.prepared.vote_ids.iter().copied().collect();
        self.metrics.bisections.fetch_add(1, Ordering::SeqCst);
        let secs = self.prove_backoff_upto(BISECT_BACKOFF_MAX);
        if self.prover_hold.is_some() {
            error!(
                pid = %hex::encode(self.pid31),
                votes = n,
                retry_in = secs,
                %reason,
                "batch proof failed; holding its votes pending"
            );
            self.prover_hold = Some(reason);
            self.requeue_flight(f);
            return;
        }
        let mut isolate = false;
        let mut b = match self.bisect.take() {
            None => Bisect {
                suspect: vids,
                cap: n,
                reason,
            },
            Some(mut b) => {
                b.suspect = vids;
                b.cap = n.div_ceil(2).max(1);
                b.reason = reason;
                isolate = n == 1;
                b
            }
        };
        if isolate {
            let msg = format!("{} (isolated by bisection)", b.reason);
            error!(pid = %hex::encode(self.pid31), vids = ?f.prepared.vote_ids, %msg, "vote isolated by bisection; errored");
            self.error_batch(&f.prepared.vote_ids, &msg);
            for vid in &f.prepared.vote_ids {
                b.suspect.remove(vid);
            }
            self.isolated += 1;
            self.metrics.isolated_votes.fetch_add(1, Ordering::SeqCst);
            if self.isolated >= ISOLATED_BEFORE_HOLD {
                self.prover_hold = Some(b.reason.clone());
                error!(pid = %hex::encode(self.pid31), "prover fails every batch; holding votes pending");
            }
        } else {
            error!(
                pid = %hex::encode(self.pid31),
                votes = n,
                cap = b.cap,
                retry_in = secs,
                reason = %b.reason,
                "batch proof failed; requeued for bisection"
            );
            self.requeue_flight(f);
        }
        self.bisect = Some(b);
    }

    /// A landing: suspects no longer pending are settled (or closed). None
    /// left ends the bisection; else the rest seals whole, up to the
    /// current cap. Only our own
    /// landing (`own`) clears the isolated count and the hold.
    fn bisect_landed(&mut self, own: bool) {
        if own {
            self.isolated = 0;
            self.prover_hold = None;
        }
        let Some(b) = self.bisect.as_mut() else {
            return;
        };
        let pending: std::collections::HashSet<u64> =
            self.pending.iter().map(|v| v.pkg.vote_id).collect();
        b.suspect.retain(|v| pending.contains(v));
        // A landing that settled none of them must not undo the halving.
        if b.suspect.is_empty() {
            self.bisect = None;
        } else {
            b.cap = b.cap.min(b.suspect.len()).max(1);
        }
    }

    /// maxVoters reached: queued new-slot votes can never settle, so they
    /// error now rather than at the end. Overwrites stay.
    fn error_new_slots_if_full(&mut self) {
        if (self.state.occupied() as u64) < self.record.onchain.max_voters {
            return;
        }
        let mut full = Vec::new();
        for v in &self.pending {
            match self.state.slot_ballot(v.slot) {
                Ok(None) => full.push(v.pkg.vote_id),
                Ok(Some(_)) => {}
                Err(e) => {
                    error!(pid = %hex::encode(self.pid31), %e, "slot_ballot");
                    return;
                }
            }
        }
        if full.is_empty() {
            return;
        }
        info!(pid = %hex::encode(self.pid31), n = full.len(), "max voters reached; erroring new-slot votes");
        self.finish(
            &full,
            VoteStatus::Error,
            Some(&ActorError::MaxVoters.to_string()),
        );
    }

    async fn on_job_done(&mut self, generation: u64, outcome: JobOutcome, posted: bool) {
        let Some(mut f) = self.in_flight.take_if(|f| f.generation == generation) else {
            return; // a stale job of an aborted flight
        };
        let vids = f.prepared.vote_ids.clone();
        // Blobs went out and the batch failed: a failed cycle. A settle
        // `Transient` counts in its arm (it may have landed after all), an
        // unhandled revert in the generic `Reverted` arm.
        if posted {
            match &outcome {
                JobOutcome::Guest(mask) => self.failed_cycle(&format!("guest mask {mask:#x}")),
                JobOutcome::Failed(e) | JobOutcome::Held(e) => self.failed_cycle(e),
                JobOutcome::Transient(e) if !f.proved => self.failed_cycle(e),
                _ => {}
            }
        }
        match outcome {
            JobOutcome::Landed(rc) if self.deps.l1.is_some() => {
                // L1-blob mode: OP's unsafe blocks reorg with Ethereum, so the
                // settlement commits at its confirmed event; one that never
                // shows is rolled back by tick() and requeued.
                info!(pid = %hex::encode(self.pid31), tx = %rc.tx_hash, block = rc.block,
                    "settlement mined; committing at its event");
                f.landed_deadline = Some(tokio::time::Instant::now() + LANDED_EVENT_WAIT);
                self.in_flight = Some(f);
            }
            JobOutcome::Landed(rc) => self.commit_flight(f, rc.tx_hash.0, rc.block),
            JobOutcome::Guest(mask) => {
                self.rollback_flight(&f);
                let bits = fail_bits(mask).join(", ");
                let msg = format!("guest rejected the batch: {bits} (mask {mask:#x})");
                error!(pid = %hex::encode(self.pid31), %msg, "batch failed in the guest");
                self.metrics.guest_rejections.fetch_add(1, Ordering::SeqCst);
                self.error_batch(&vids, &msg);
            }
            JobOutcome::Failed(reason) => {
                self.rollback_flight(&f);
                self.prover_failed(f, reason);
                return; // the backoff streak continues
            }
            JobOutcome::Held(msg) => {
                // Every batch fails this way until the node or the prover is
                // fixed: hold the votes pending, the close errors them.
                self.rollback_flight(&f);
                self.metrics.held_batches.fetch_add(1, Ordering::SeqCst);
                let secs = self.prove_backoff();
                error!(
                    pid = %hex::encode(self.pid31),
                    votes = vids.len(),
                    retry_in = secs,
                    %msg,
                    "batch failed a node-side check; votes held pending"
                );
                self.held_reason = Some(msg);
                self.requeue_flight(f);
                return; // the backoff streak continues
            }
            JobOutcome::Reverted(name) if name == "InvalidStateRoot" => {
                // Refused at the head: its block delays the anchored check.
                let block = match self.deps.contracts.head().await {
                    Ok((b, _)) => b,
                    Err(_) => self.last_head_block,
                };
                if self.race_lost(f, block).await {
                    return;
                }
            }
            JobOutcome::Lost(e, block) => {
                info!(pid = %hex::encode(self.pid31), %e, "settlement reverted in a race");
                if self.race_lost(f, block).await {
                    return;
                }
            }
            JobOutcome::Reverted(name) if name == "InvalidCensusRoot" => {
                // The census re-rooted under the flight. Requeue the
                // votes (never error them) and re-seal at the current root.
                self.rollback_flight(&f);
                let root = f.prepared.expected.census_root;
                self.census_reverts = if self.census_reverts.0 == root {
                    (root, self.census_reverts.1 + 1)
                } else {
                    (root, 1)
                };
                let streak = self.census_reverts.1;
                warn!(pid = %hex::encode(self.pid31), streak, "settlement reverted InvalidCensusRoot; re-rooting");
                if streak >= 3 {
                    // Same root keeps reverting: back off like a transient.
                    self.prove_backoff();
                }
                self.requeue_flight(f);
                return;
            }
            JobOutcome::Reverted(name) if name == "MaxVotersReached" => {
                // The organizer lowered maxVoters under the flight. Requeue
                // (never error the batch): the read merges the new cap, and
                // only new-slot votes that can never fit are errored.
                self.rollback_flight(&f);
                self.needs_check = true;
                let secs = self.prove_backoff();
                warn!(pid = %hex::encode(self.pid31), retry_in = secs, "settlement reverted MaxVotersReached; requeueing");
                self.requeue_flight(f);
                // A cap lowered under the flight (its event cleaned only the
                // queue): its new-slot votes meet the current cap now.
                self.error_new_slots_if_full();
                return;
            }
            JobOutcome::Reverted(name)
                if name == "InvalidStatus" || name == "InvalidTimeBounds" =>
            {
                self.rollback_flight(&f);
                // Paused under the flight, not started, or a budget overrun
                // inside the grace: the votes wait for the next seal. Only a
                // closed window errors them.
                if let Some(status) = self.may_reopen().await {
                    let secs = self.prove_backoff();
                    // Until the pause event arrives, the record still says
                    // Ready: re-sealing would only re-prove into the revert.
                    if status == ProcessStatus::Paused
                        && self.record.onchain.status == ProcessStatus::Ready
                    {
                        self.pause_hold = Some(tokio::time::Instant::now() + PAUSE_HOLD);
                    }
                    info!(pid = %hex::encode(self.pid31), %name, retry_in = secs, "settlement refused while the process is not open, requeueing");
                    self.requeue_flight(f);
                    return;
                }
                // The close-out errors what is left, once the tree is at the
                // chain's root: a peer may have settled these votes. It waits
                // on the record's grace end, which can be later than the
                // chain's (a reorg took the last landing): until then each
                // retry backs off, and the record is read again.
                self.needs_check = true;
                let secs = self.prove_backoff();
                warn!(pid = %hex::encode(self.pid31), retry_in = secs, "window closed while the batch was in flight; requeueing for the close-out");
                self.requeue_flight(f);
                return;
            }
            JobOutcome::Reverted(name) => {
                self.rollback_flight(&f);
                let msg = format!("settlement reverted: {name}");
                if posted {
                    self.failed_cycle(&msg);
                }
                error!(pid = %hex::encode(self.pid31), ?msg, "settlement reverted");
                self.error_batch(&vids, &msg);
            }
            JobOutcome::Transient(e) => {
                // A receipt timeout may hide a tx that actually landed: if
                // the chain already shows our root, keep the flight and let
                // its transition event commit it (with real tx hash/block)
                // instead of double-spending the votes or faking a lost race.
                if let Ok(p) = self.deps.contracts.process(&self.pid31).await
                    && p.state_root == f.prepared.new_root
                {
                    warn!(pid = %hex::encode(self.pid31), ?e, "transient failure but the tx landed; awaiting its event");
                    // Bounded wait: tick() rolls the flight back if the
                    // confirmed chain never delivers the event (reorg).
                    f.landed_deadline = Some(tokio::time::Instant::now() + LANDED_EVENT_WAIT);
                    self.in_flight = Some(f);
                    return;
                }
                self.rollback_flight(&f);
                // The settlement gave up after posting: a failed cycle.
                if posted && f.proved {
                    self.failed_cycle(&e);
                }
                // Hold the votes pending under a saturating cooldown (~4 min
                // cap) rather than re-sealing hot against a down prover or
                // RPC; the window close errors them out if it never returns.
                let secs = self.prove_backoff();
                warn!(
                    pid = %hex::encode(self.pid31),
                    ?e,
                    attempts = self.prove_attempts,
                    retry_in = secs,
                    "transient settlement failure, requeueing"
                );
                self.requeue_flight(f);
                return; // the failure streak continues
            }
        }
        // Only a conclusive outcome (settled, guest rejection, closed
        // window) ends the transient-failure streak; the Transient arm
        // above returns early so its backoff keeps growing.
        self.prove_attempts = 0;
        self.prove_after = None;
        self.census_reverts = ([0u8; 32], 0);
    }

    /// A settlement that lost a race: roll back and requeue its votes. When
    /// the chain still shows our root nobody settled (a lagging endpoint
    /// answered); that cools down and returns true, keeping the failure
    /// streak. Otherwise another sequencer settled first: sync, then retry.
    async fn race_lost(&mut self, f: Flight, block: u64) -> bool {
        self.rollback_flight(&f);
        if let Ok(p) = self.deps.contracts.process(&self.pid31).await
            && p.state_root == self.state.committed().root
        {
            let secs = self.prove_backoff();
            warn!(pid = %hex::encode(self.pid31), retry_in = secs, "settlement race at our own root, requeueing");
            self.requeue_flight(f);
            return true;
        }
        self.metrics.lost_races.fetch_add(1, Ordering::SeqCst);
        self.await_sync = true;
        self.race_latch = true;
        // Re-read at an anchor at or past the revert's block (its state
        // there is what refused ours): a chain at a root we passed is a
        // reorg, not a race.
        self.needs_check = true;
        self.hint_floor(block).await;
        warn!(pid = %hex::encode(self.pid31), "lost the settlement race, waiting for sync");
        self.requeue_flight(f);
        false
    }

    /// After a status or window revert: the process's status if it may
    /// still take the votes (paused, before its start, or in its grace), by
    /// the chain and by the events seen so far. `None` once canceled or
    /// past its grace end. A failed read keeps it open: the local grace end
    /// can miss a landing, and close-out errors the votes once it is known.
    async fn may_reopen(&self) -> Option<ProcessStatus> {
        let open = |s| {
            matches!(
                s,
                ProcessStatus::Ready | ProcessStatus::Paused | ProcessStatus::Ended
            )
        };
        let (status, grace_end) = match self.deps.contracts.process(&self.pid31).await {
            Ok(p) => (p.status, p.grace_end(self.deps.grace.grace_max_total)),
            Err(e) => {
                warn!(pid = %hex::encode(self.pid31), %e, "process read failed after a revert; requeueing");
                (self.record.onchain.status, u64::MAX)
            }
        };
        (open(status) && open(self.record.onchain.status) && self.chain_time < grace_end)
            .then_some(status)
    }

    fn commit_flight(&mut self, f: Flight, tx_hash: [u8; 32], block: u64) {
        // An anchored read below the landing's block cannot show it.
        self.check_at = self.check_at.max(block);
        // The registry's counters after this landing, from the publics it
        // checked: occupied_before is its votersCount and an overwrite is
        // no new voter. The landing's event raises them again (idempotent).
        let e = &f.prepared.expected;
        let voters =
            u64::from(e.occupied_before) + u64::from(e.voters.saturating_sub(e.overwrites));
        let overwrites = self
            .record
            .onchain
            .overwritten_count
            .saturating_add(u64::from(e.overwrites));
        self.follow_counts(voters, overwrites);
        if let Err(e) = self.state.commit(&f.prepared) {
            // The transition IS on-chain; record the target root so resync
            // repairs the local state from our own blobs, and requeue the
            // votes so settle_pending_in_tree settles them after the sync.
            error!(pid = %hex::encode(self.pid31), %e, "commit failed; resyncing from chain");
            // Roll the tree back to the committed root first: leaving it at
            // the prepared root would wedge every later apply/prepare.
            self.rollback_flight(&f);
            self.record.onchain.state_root = f.prepared.new_root;
            // The receipt may be depth 0: if a reorg drops the tx, the
            // anchored read at or above its block re-pins at our root.
            self.needs_check = true;
            self.put_record();
            self.requeue_flight(f);
            return;
        }
        let p = &f.prepared;
        let rec = crate::storage::TransitionRecord {
            index: self.next_index,
            old_root: p.old_root,
            new_root: p.new_root,
            tx_hash,
            block,
            sender: self
                .deps
                .contracts
                .signer()
                .map(|a| a.into_array())
                .unwrap_or_default(),
            n_votes: p.expected.voters as u64,
            n_overwrites: p.expected.overwrites as u64,
            n_blobs: p.blobs.blobs.len() as u64,
            by_self: true,
        };
        // Votes, transition, pin and counts in one write after the tree's.
        self.record.onchain.state_root = p.new_root;
        self.write_transition(&p.vote_ids, &[], &rec, &p.blobs.blobs);
        self.needs_check = true;
        self.clear_sync();
        self.failed_cycles = 0;
        self.proof_first_until_landing = false;
        self.failed_pause = None;
        self.held_reason = None;
        self.bisect_landed(true);
        self.metrics.settled_by_self.fetch_add(1, Ordering::SeqCst);
        info!(
            pid = %hex::encode(self.pid31),
            votes = p.expected.voters,
            overwrites = p.expected.overwrites,
            root = %format!("0x{}", hex::encode(p.new_root)),
            "transition settled"
        );
        self.error_new_slots_if_full();
    }

    async fn on_event(&mut self, ev: RegistryEvent) {
        // No tree to apply it to: dropped, and the chain re-read once the
        // state is back (the resync replays transitions, the read merges
        // the fields).
        if !self.reopen_state() {
            self.needs_check = true;
            return;
        }
        let field = !matches!(
            ev.kind,
            EventKind::ProcessCreated { .. } | EventKind::StateTransitioned { .. }
        );
        if field {
            if (ev.block, ev.log_index) <= self.fields_at {
                return;
            }
            self.fields_at = (ev.block, ev.log_index);
        }
        match ev.kind {
            EventKind::ProcessCreated { .. } => {}
            EventKind::StatusChanged { new, .. } => {
                // Events come from confirmed blocks.
                if new == ProcessStatus::Canceled {
                    self.cancel_at = Some(ev.block);
                }
                if new == ProcessStatus::Results {
                    self.results_at = Some(ev.block);
                }
                self.status_moved(new);
                self.put_record();
            }
            // Shortened or extended: admission, flush mode, the budget and
            // the grace end all read it.
            EventKind::DurationChanged { duration, .. } => {
                self.record.onchain.duration = duration;
                self.end_moved = true;
                self.put_record();
            }
            EventKind::GraceChanged { grace, .. } => {
                self.record.onchain.grace = grace;
                self.put_record();
            }
            EventKind::MaxVotersChanged { max_voters, .. } => {
                self.record.onchain.max_voters = max_voters;
                self.put_record();
                self.error_new_slots_if_full();
            }
            // Followed like the duration and the census, so the stored copy
            // catches up with getProcess; nothing here reads it.
            EventKind::MetadataUpdated { uri, hash, .. } => {
                self.record.onchain.metadata_uri = uri;
                self.record.onchain.metadata_hash = hash;
                self.put_record();
            }
            EventKind::ResultsDecryptionRequested {
                epoch_id,
                aid,
                first_index,
                count,
                ..
            } => {
                let d = &mut self.record.onchain.dkg;
                d.epoch_id = epoch_id;
                d.aid = aid;
                d.requested = true;
                d.first_index = first_index;
                d.count = count;
                self.put_record();
            }
            EventKind::ResultsSet { results, .. } => {
                self.results_at = Some(ev.block);
                self.record.onchain.results = results;
                self.status_moved(ProcessStatus::Results);
                // StatusChanged(→RESULTS) precedes this in the same tx, so
                // mark_finalized's early return must not swallow the
                // results: persist unconditionally (idempotent).
                self.put_record();
            }
            EventKind::StateTransitioned {
                sender,
                old_root,
                new_root,
                voters,
                overwrites,
                n_blobs,
                l1,
                ..
            } => {
                self.on_transition(
                    ev.block,
                    ev.tx_hash.0,
                    sender,
                    old_root,
                    new_root,
                    (voters, overwrites),
                    n_blobs,
                    l1,
                )
                .await;
            }
            EventKind::CensusUpdated { root, uri, .. } => {
                let moved = root != self.record.onchain.census.root;
                self.record.onchain.census.root = root;
                self.record.onchain.census.uri = uri;
                self.put_record();
                if moved {
                    self.census_moved();
                }
            }
        }
    }

    /// The process status moved (its event, or a chain read showing it).
    fn status_moved(&mut self, new: ProcessStatus) {
        self.record.onchain.status = new;
        // A confirmation never outlives its cancel.
        if new != ProcessStatus::Canceled {
            self.cancel_at = None;
        }
        self.pause_hold = None;
        // Ended flushes through the grace; the tick closes out at its end.
        // Canceled and RESULTS close out from the tick too, once a read
        // shows the tree at the chain's final root.
        if matches!(new, ProcessStatus::Canceled | ProcessStatus::Results) {
            self.needs_check = true;
        }
        if new == ProcessStatus::Results {
            self.mark_finalized();
        }
    }

    /// The census root moved (its event, or a chain read showing it).
    fn census_moved(&mut self) {
        if self.state.config().census_origin != CensusOrigin::MerkleOffchainDynamic {
            return;
        }
        // A flight built on the old root can only revert; taking it also
        // stales its JobDone via the generation guard.
        if let Some(f) = self.in_flight.take() {
            warn!(pid = %hex::encode(self.pid31), "census re-rooted under a flight; requeueing");
            self.rollback_flight(&f);
            self.requeue_flight(f);
        }
        // Pending votes are re-checked once the new tree loads.
        self.census_recheck = true;
    }

    /// Raises the stored registry counters to a landing's totals. Both only
    /// grow on-chain, so a replayed older event cannot rewind them.
    fn follow_counts(&mut self, voters: u64, overwrites: u64) -> bool {
        let o = &mut self.record.onchain;
        let moved = voters > o.voters_count || overwrites > o.overwritten_count;
        o.voters_count = o.voters_count.max(voters);
        o.overwritten_count = o.overwritten_count.max(overwrites);
        moved
    }

    fn put_record(&self) {
        if let Err(e) = self.deps.db.put_process(&self.record) {
            error!(pid = %hex::encode(self.pid31), %e, "put_process");
        }
    }

    /// The votes of a batch stored here and not settled: they settle with its
    /// record, queued or not (one this node errored or lost track of after a
    /// failed write may still land through another sequencer).
    fn unsettled_here(&self, vids: &[u64]) -> Vec<u64> {
        vids.iter()
            .copied()
            .filter(|v| {
                matches!(self.deps.db.vote(&self.pid, *v),
                    Ok(Some(sv)) if sv.status != VoteStatus::Settled)
            })
            .collect()
    }

    /// Records a transition the tree just applied: its settled votes, the
    /// record with its blobs, and the process record (pin, counts), in one
    /// write. A failure leaves the log behind the tree; the resync writes
    /// the record again from the chain.
    fn write_transition(
        &mut self,
        vids: &[u64],
        superseded: &[u64],
        rec: &crate::storage::TransitionRecord,
        blobs: &[davinci_zkvm_sdk::blob::Blob],
    ) {
        match self
            .deps
            .db
            .settle_superseding(&self.pid, vids, superseded, rec, blobs, &self.record)
        {
            Ok(()) => {
                self.next_index = rec.index + 1;
                self.last_tx_block = self.last_tx_block.max(rec.block);
                self.log_root = rec.new_root;
            }
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "transition write failed; resyncing the log")
            }
        }
    }

    /// The record of a transition the tree applied but whose record write
    /// was lost (a crash between the two, or a failed write): the next one
    /// after the log, met again as its event or in a gap replay. It waits
    /// for its blobs, its counts come from the event's totals, and batch
    /// votes the lost write left unsettled settle with it.
    #[allow(clippy::too_many_arguments)] // the event's fields, as routed
    async fn record_missing(
        &mut self,
        block: u64,
        tx_hash: [u8; 32],
        sender: Address,
        old_root: [u8; 32],
        new_root: [u8; 32],
        (voters, overwrites): (u64, u64),
        n_blobs: u64,
        l1: Option<L1Blobs>,
    ) {
        // Not ours: our tree never reached its root.
        if !matches!(self.state.has_root(&new_root), Ok(true)) {
            return;
        }
        let fetched = match &l1 {
            None => {
                self.deps
                    .blobs
                    .blobs_for_tx(alloy::primitives::B256::from(tx_hash), block, n_blobs)
                    .await
            }
            Some(l) => self.deps.blobs.blobs_for_l1(&l.blocks, &l.vhs).await,
        };
        // No record without its archive: a rebuild after a reorg replays
        // from it. The resync retries.
        let blobs = match fetched {
            Ok(b) => b,
            Err(e) => {
                warn!(pid = %hex::encode(self.pid31), ?e, "blobs of an unrecorded transition unavailable; retrying");
                return;
            }
        };
        let log = match self.deps.db.transitions(&self.pid) {
            Ok(log) => log,
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "log repair read");
                return;
            }
        };
        let nf = self.state.config().ballot_mode.num_fields;
        let td = match cpu(|| decode_blobs(&blobs, nf)) {
            Ok(t) => t,
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), ?e, "blob decode of an unrecorded transition");
                return;
            }
        };
        let vids = self.unsettled_here(&td.vote_ids);
        // The log's totals before it: distinct voters and overwrites.
        let (n_before, w_before) = log.iter().fold((0u64, 0u64), |(n, w), t| {
            (
                n.saturating_add(t.n_votes.saturating_sub(t.n_overwrites)),
                w.saturating_add(t.n_overwrites),
            )
        });
        let n_overwrites = overwrites.saturating_sub(w_before);
        let rec = crate::storage::TransitionRecord {
            index: self.next_index,
            old_root,
            new_root,
            tx_hash,
            block,
            sender: sender.into_array(),
            n_votes: voters.saturating_sub(n_before).saturating_add(n_overwrites),
            n_overwrites,
            // The registry's count, whether or not the blobs were fetched.
            n_blobs,
            by_self: self.deps.contracts.signer() == Some(sender),
        };
        warn!(pid = %hex::encode(self.pid31), index = rec.index, "writing a transition record lost at a crash");
        // As apply_sync: we were at its old root, so it is the target now.
        if self.record.onchain.state_root == old_root {
            self.record.onchain.state_root = new_root;
        }
        let Ok((_, superseded)) = self.supersede_split() else {
            return;
        };
        self.write_transition(&vids, &superseded, &rec, &blobs);
    }

    /// A finalize attempt that failed for good (a failed results proof or
    /// check, a revert): retried after a cooldown, never latched, and from
    /// the [`RESULTS_FAILS_BEFORE_HOLD`]th in a row held to one attempt per
    /// [`RESULTS_HOLD_RETRY`] seconds.
    fn results_failed(&mut self, msg: &str) {
        self.results_failures = self.results_failures.saturating_add(1);
        let n = self.results_failures;
        let secs = if n >= RESULTS_FAILS_BEFORE_HOLD {
            RESULTS_HOLD_RETRY
        } else {
            (1u64 << n).min(BISECT_BACKOFF_MAX)
        };
        self.finalize_after =
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(secs));
        error!(
            pid = %hex::encode(self.pid31),
            %msg,
            failures = n,
            retry_in = secs,
            "results failed; retrying"
        );
    }

    fn mark_finalized(&mut self) {
        self.results_failures = 0;
        if self.record.local == LocalStatus::Finalized {
            return;
        }
        self.record.local = LocalStatus::Finalized;
        self.held = None;
        self.results_sent = None;
        self.put_record();
        info!(pid = %hex::encode(self.pid31), "process finalized");
    }

    #[allow(clippy::too_many_arguments)] // the event's fields, as routed
    async fn on_transition(
        &mut self,
        block: u64,
        tx_hash: [u8; 32],
        sender: Address,
        old_root: [u8; 32],
        new_root: [u8; 32],
        (voters, overwrites): (u64, u64),
        n_blobs: u64,
        l1: Option<L1Blobs>,
    ) {
        // Every landing (replays included, harmlessly) moves the grace end.
        self.needs_check = true;
        // Replayed events must not abort a live flight or roll the pinned
        // root back, so the already-applied checks come first.
        let committed = self.state.committed().root;
        if new_root == committed {
            // Already applied (our own settled transition echoing back).
            // Forward-only pin: never let a replayed event rewind the target.
            let moved = self.follow_counts(voters, overwrites);
            if self.record.onchain.state_root == old_root {
                self.record.onchain.state_root = new_root;
                self.put_record();
            } else if moved {
                self.put_record();
            }
            // Clear the sync latch only at the pinned tip: a replayed old
            // event must not unblock sealing while we are still behind.
            if self.record.onchain.state_root == new_root {
                self.clear_sync();
            }
            // Its record was lost at a crash: write it now.
            if self.log_root == old_root && self.in_flight.is_none() {
                self.record_missing(
                    block,
                    tx_hash,
                    sender,
                    old_root,
                    new_root,
                    (voters, overwrites),
                    n_blobs,
                    l1,
                )
                .await;
            }
            return;
        }
        if self.seen_tx(&old_root, &new_root, block) {
            return; // replayed event of a transition already in the log
        }
        // Our own in-flight batch landing: its receipt handler commits.
        if let Some(f) = &self.in_flight {
            if f.prepared.new_root == new_root {
                let f = self.in_flight.take().unwrap();
                self.commit_flight(f, tx_hash, block);
                if self.follow_counts(voters, overwrites) {
                    self.put_record();
                }
                return;
            }
            // A foreign transition while ours is in flight: we lost.
            if let Some(f) = self.in_flight.take() {
                self.rollback_flight(&f);
                self.metrics.lost_races.fetch_add(1, Ordering::SeqCst);
                warn!(pid = %hex::encode(self.pid31), "foreign transition beat the in-flight batch");
                self.requeue_flight(f);
            }
        }
        // The chain tip moved: pin the target root now (and persist it),
        // so a failed sync below is retried instead of forgotten.
        self.record.onchain.state_root = new_root;
        self.follow_counts(voters, overwrites);
        self.put_record();
        // With a record missing, the gap replay keeps block order.
        if old_root == committed && self.log_root == committed {
            self.apply_sync(block, tx_hash, sender, old_root, new_root, n_blobs, l1)
                .await;
            return;
        }
        // A gap: replay this process's transitions from the registry.
        self.replay_gap(block).await;
    }

    /// Whether this transition is already in the local log (a replay), by
    /// its roots: a reorg branch can re-include the same transaction in a
    /// later block. Roots never repeat, so one leaving the committed root
    /// is new; one leaving a root we passed for another is the chain
    /// forking there.
    fn seen_tx(&mut self, old_root: &[u8; 32], new_root: &[u8; 32], block: u64) -> bool {
        if *old_root == self.state.committed().root {
            return false;
        }
        match self.deps.db.transitions(&self.pid) {
            Ok(ts) => match ts.iter().find(|t| t.old_root == *old_root) {
                Some(t) if t.new_root == *new_root => true,
                Some(t) => {
                    self.diverge(t.index, false, block);
                    false
                }
                None => false,
            },
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "transitions read");
                false
            }
        }
    }

    /// Applies a foreign transition from its DA blobs. Returns whether it
    /// applied; failures leave the target root in place for `maybe_resync`.
    #[allow(clippy::too_many_arguments)] // the event's fields, as routed
    async fn apply_sync(
        &mut self,
        block: u64,
        tx_hash: [u8; 32],
        sender: Address,
        old_root: [u8; 32],
        new_root: [u8; 32],
        n_blobs: u64,
        l1: Option<L1Blobs>,
    ) -> bool {
        // Only the first n_blobs blobs: the count the registry checked. An
        // appended junk blob would otherwise fail every decode forever. In
        // L1-blob mode the event names the Ethereum blobs themselves.
        let fetched = match &l1 {
            None => {
                self.deps
                    .blobs
                    .blobs_for_tx(alloy::primitives::B256::from(tx_hash), block, n_blobs)
                    .await
            }
            Some(l) => self.deps.blobs.blobs_for_l1(&l.blocks, &l.vhs).await,
        };
        let blobs = match fetched {
            Ok(b) => b,
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), ?e, "blob fetch failed");
                return false;
            }
        };
        let nf = self.state.config().ballot_mode.num_fields;
        let td = match cpu(|| decode_blobs(&blobs, nf)) {
            Ok(t) => t,
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), ?e, "blob decode failed");
                return false;
            }
        };
        // Every vote this public batch carries, stored here or not yet (a
        // voter may retry the same package here after a reorg takes it):
        // any re-seal of one must cover its slots. In the coverage history
        // before the transition becomes authoritative.
        if !td.vote_ids.is_empty() {
            let rec = crate::storage::ExposedRecord {
                slots: td.updates.iter().map(|(s, _)| *s).collect(),
                vote_ids: td.vote_ids.clone(),
            };
            if let Err(e) = self.deps.db.put_coverage(&self.pid, &new_root, &rec) {
                error!(pid = %hex::encode(self.pid31), %e, "persist the synced batch's coverage");
                return false;
            }
        }
        let before = self.state.committed();
        if let Err(e) = cpu(|| self.state.apply_synced(&td, new_root)) {
            error!(pid = %hex::encode(self.pid31), %e, "apply_synced failed");
            // Its own reset of the root is best-effort.
            if let Err(e) = self.state.reset_to_committed() {
                error!(pid = %hex::encode(self.pid31), %e, "tree reset after a failed sync; reopening");
                self.state_lost = true;
            }
            return false;
        }
        let after = self.state.committed();
        let rec = crate::storage::TransitionRecord {
            index: self.next_index,
            old_root,
            new_root,
            tx_hash,
            block,
            sender: sender.into_array(),
            n_votes: after.voters.saturating_sub(before.voters),
            n_overwrites: after.overwrites.saturating_sub(before.overwrites),
            n_blobs: blobs.len() as u64,
            by_self: false,
        };
        if self.record.onchain.state_root == old_root {
            // We were at the tip: advance it. A gap replay keeps the newer
            // target the events already told us about.
            self.record.onchain.state_root = new_root;
        }
        let vids = self.unsettled_here(&td.vote_ids);
        // With the record: the queued votes this batch's votes supersede. On
        // a read error the record waits for the log repair, which retries both.
        let superseded = match self.supersede_split() {
            Ok((_, s)) => s,
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "has_vote_id");
                return false;
            }
        };
        self.write_transition(&vids, &superseded, &rec, &blobs);
        self.metrics
            .synced_from_others
            .fetch_add(1, Ordering::SeqCst);
        self.clear_sync();
        // Another sequencer landing says nothing about our own cycles: it
        // ends the pause but takes only one off the streak, so a broken node
        // in a busy election still pauses.
        self.failed_cycles = self.failed_cycles.saturating_sub(1);
        self.failed_pause = None;
        // Pending votes another sequencer settled for us are done. Votes
        // whose slot got overwritten stay pending: their id is not in the tree.
        self.settle_pending_in_tree();
        self.bisect_landed(false);
        self.error_new_slots_if_full();
        info!(
            pid = %hex::encode(self.pid31),
            votes = rec.n_votes,
            root = %format!("0x{}", hex::encode(new_root)),
            "synced transition from another sequencer"
        );
        true
    }

    /// Re-fetches this process's registry events from the newest applied
    /// transition and applies every one that continues our committed root,
    /// in order; stops on the first failure (maybe_resync retries later).
    async fn replay_gap(&mut self, upto: u64) {
        let from = self.record.onchain.creation_block.max(self.last_tx_block);
        if upto < from {
            return;
        }
        let events = match self.deps.contracts.events(from, upto).await {
            Ok(e) => e,
            Err(e) => {
                error!(pid = %hex::encode(self.pid31), %e, "gap replay fetch failed");
                return;
            }
        };
        let mut log: Option<Vec<crate::storage::TransitionRecord>> = None;
        for ev in events {
            if *ev.kind.pid() != self.pid31 {
                continue;
            }
            let EventKind::StateTransitioned {
                sender,
                old_root,
                new_root,
                voters,
                overwrites,
                n_blobs,
                l1,
                ..
            } = ev.kind
            else {
                continue;
            };
            if self.follow_counts(voters, overwrites) {
                self.put_record();
            }
            let committed = self.state.committed().root;
            // The log first catches up with the tree (records lost at a
            // crash), then the tree with the chain. A transition leaving the
            // log's last root for a root our tree reached is a missing
            // record; for any other root it forks there (ours was reorged
            // out): rebuild from there.
            if old_root == self.log_root && self.log_root != committed {
                if !matches!(self.state.has_root(&new_root), Ok(true)) {
                    self.diverge(self.next_index, false, ev.block);
                    continue;
                }
                self.record_missing(
                    ev.block,
                    ev.tx_hash.0,
                    sender,
                    old_root,
                    new_root,
                    (voters, overwrites),
                    n_blobs,
                    l1,
                )
                .await;
                // Still waiting (its blobs): later records keep block order.
                if self.log_root != new_root {
                    return;
                }
                continue;
            }
            if old_root == committed {
                // Never record past a missing record.
                if self.log_root != committed {
                    return;
                }
                if !self
                    .apply_sync(
                        ev.block,
                        ev.tx_hash.0,
                        sender,
                        old_root,
                        new_root,
                        n_blobs,
                        l1,
                    )
                    .await
                {
                    return;
                }
                continue;
            }
            // Leaving a root we passed for another: the chain forked there.
            let ts = log.get_or_insert_with(|| {
                self.deps.db.transitions(&self.pid).unwrap_or_else(|e| {
                    error!(pid = %hex::encode(self.pid31), %e, "transitions read");
                    Vec::new()
                })
            });
            if let Some(i) = ts
                .iter()
                .find(|t| t.old_root == old_root && t.new_root != new_root)
                .map(|t| t.index)
            {
                self.diverge(i, false, ev.block);
            }
        }
    }

    fn maybe_finalize(&mut self) {
        if self.finalizing
            // Our results transaction awaits its event.
            || self.results_sent.is_some()
            || self.in_flight.is_some()
            || self.diverged.is_some()
            // A confirmation an anchored read does not show yet.
            || self.suspended
            || self.record.local != LocalStatus::Active
            // Observers never settle, even when they hold the election key.
            || self.deps.contracts.signer().is_none()
        {
            return;
        }
        // Cooling down after a transient finalize failure?
        if let Some(t) = self.finalize_after
            && tokio::time::Instant::now() < t
        {
            return;
        }
        // The contract also accepts results for a process paused past its
        // end, so Paused counts like Ready here.
        if !matches!(
            self.record.onchain.status,
            ProcessStatus::Ended | ProcessStatus::Ready | ProcessStatus::Paused
        ) {
            return;
        }
        // Synced to the on-chain root?
        let root = self.state.committed().root;
        if root != self.record.onchain.state_root {
            return;
        }
        // The task reads at the anchor too.
        let at = match (self.anchor, self.degraded) {
            (Some(a), None) => crate::finalize::At::Hash(a.hash),
            (Some(a), Some(_)) => crate::finalize::At::Number(a.block),
            (None, _) => return,
        };
        // Results land only once the grace window closed (`GraceOpen`).
        let closed = !self.needs_check && self.chain_time >= self.grace_end();
        if self.record.onchain.key_mode != KeyMode::Sequencer {
            if closed {
                self.finalize_dkg(at);
            }
            return;
        }
        let hold = !closed;
        if hold {
            // Eager results: once the end is seen and nothing is left to
            // settle here, prove at the current root during the grace. Not
            // before a chain read confirmed that root (fresh after recover).
            let end_seen = self.record.onchain.status == ProcessStatus::Ended
                || (self.chain_time != 0 && self.chain_time >= self.end_time());
            if !self.eager || !end_seen || !self.pending.is_empty() || self.needs_check {
                return;
            }
        }
        if let Some((r, snark)) = &self.held
            && *r == root
        {
            if hold {
                return; // already holding this root
            }
            // Proved at this root during the grace: submit it now. It stays
            // held for a retry after a transient failure.
            let snark = (**snark).clone();
            self.generation += 1;
            self.finalizing = true;
            self.finalize_after = None;
            info!(pid = %hex::encode(self.pid31), "finalizing: submitting held results");
            spawn_finalize(
                self.tx.clone(),
                self.generation,
                crate::finalize::run_submit_held(
                    self.deps.contracts.clone(),
                    self.pid31,
                    snark,
                    self.deps.grace.grace_max_total,
                    at,
                    self.generation,
                    self.tx.clone(),
                ),
            );
            return;
        }
        // Proved at a root that has moved since: prove again.
        self.held = None;
        if self.foreign_key {
            return;
        }
        let Some(sk) = self
            .keys
            .secret_for(&self.pid31, &self.state.config().enc_key)
        else {
            self.foreign_key = true;
            return; // not our election key; another node finalizes
        };
        self.metrics
            .finalize_attempts
            .fetch_add(1, Ordering::SeqCst);
        let (request, tally) = match cpu(|| self.state.results_request(&sk, &mut OsRng)) {
            Ok(r) => r,
            Err(e) => {
                self.results_failed(&format!("results request: {e}"));
                return;
            }
        };
        self.generation += 1;
        self.finalizing = true;
        self.finalize_after = None;
        info!(pid = %hex::encode(self.pid31), hold, "finalizing: proving results");
        spawn_finalize(
            self.tx.clone(),
            self.generation,
            crate::finalize::run_finalize(
                self.deps.prover.clone(),
                self.deps.contracts.clone(),
                self.pid31,
                request,
                root,
                tally,
                self.deps.grace.grace_max_total,
                hold,
                at,
                self.generation,
                self.tx.clone(),
            ),
        );
    }

    /// DKG modes: nobody holds the key. Any signing node binds the committed
    /// accumulator to the final root (`requestResultsDecryption`), then
    /// publishes the committee's plaintexts once they are all combined.
    fn finalize_dkg(&mut self, at: crate::finalize::At) {
        // Once the request is on-chain only readiness is polled: no inputs,
        // and the poll is not a finalize attempt.
        let inputs = if self.record.onchain.dkg.requested {
            None
        } else {
            self.metrics
                .finalize_attempts
                .fetch_add(1, Ordering::SeqCst);
            match cpu(|| self.state.dkg_results_inputs()) {
                Ok((acc, sibs)) => Some((
                    acc.map(|c| davinci_zkvm_sdk::crypto::field::u256_to_be(&c)),
                    sibs,
                )),
                Err(e) => {
                    self.results_failed(&format!("DKG results inputs: {e}"));
                    self.record.note = Some(format!("DKG results request: {e}"));
                    self.put_record();
                    return;
                }
            }
        };
        self.generation += 1;
        self.finalizing = true;
        self.finalize_after = None;
        spawn_finalize(
            self.tx.clone(),
            self.generation,
            crate::finalize::run_finalize_dkg(
                self.deps.contracts.clone(),
                self.pid31,
                inputs,
                self.state.committed().root,
                self.deps.grace.grace_max_total,
                at,
                self.generation,
                self.tx.clone(),
            ),
        );
    }
}

// -------------------------------------------------------------------- job

struct JobInput {
    generation: u64,
    pid31: [u8; 31],
    request: Arc<ProveRequest>,
    expected: BatchPublics,
    blobs: TransitionBlobs,
    prover: Arc<dyn Prover>,
    chain: Arc<dyn Chain>,
    tx: mpsc::Sender<Msg>,
    clock: Arc<dyn Clock>,
    metrics: Arc<Metrics>,
    nf: u8,
    n: usize,
    prove_base: f64,
    /// L1-blob mode: the Ethereum side; `None` in blob mode.
    l1: Option<Arc<crate::l1da::L1Da>>,
    /// L1-blob mode, bisecting: no blob post before the proof passed its
    /// checks, so a failing retry spends nothing on Ethereum.
    proof_first: bool,
    /// L1-blob mode: no settlement after this.
    deadline: tokio::time::Instant,
    /// Fired when the actor drops the flight.
    cancel: CancellationToken,
    /// L1-blob mode: set once a post of this job is mined.
    posted: Arc<std::sync::atomic::AtomicBool>,
}

/// Sends `msg` to the actor when dropped still armed: a task that ends
/// without its own report (a panic, which tokio swallows) does not strand
/// the flight or the finalize waiting on it.
struct ReportOnDrop {
    tx: mpsc::Sender<Msg>,
    msg: Option<Msg>,
    /// A job's L1 post flag, read when the report goes out.
    posted: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        let Some(mut m) = self.msg.take() else {
            return;
        };
        if let (Msg::JobDone { posted, .. }, Some(flag)) = (&mut m, &self.posted) {
            *posted = flag.load(Ordering::Relaxed);
        }
        // A full mailbox must not lose it: wait for room on the runtime.
        if let Err(mpsc::error::TrySendError::Full(m)) = self.tx.try_send(m)
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let tx = self.tx.clone();
            rt.spawn(async move {
                let _ = tx.send(m).await;
            });
        }
    }
}

/// Spawns a finalize task that reports a transient failure if it dies.
fn spawn_finalize(
    tx: mpsc::Sender<Msg>,
    generation: u64,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    tokio::spawn(async move {
        let mut guard = ReportOnDrop {
            tx,
            msg: Some(Msg::FinalizeDone {
                generation,
                result: Err(FinalizeFail {
                    permanent: false,
                    wait: false,
                    gate: false,
                    msg: "finalize task ended without a result".into(),
                    dkg_requested: None,
                }),
            }),
            posted: None,
        };
        task.await;
        guard.msg = None;
    });
}

async fn run_job(job: JobInput) {
    let generation = job.generation;
    let tx = job.tx.clone();
    let mut guard = ReportOnDrop {
        tx: tx.clone(),
        msg: Some(Msg::JobDone {
            generation,
            outcome: JobOutcome::Transient("job task ended without a result".into()),
            posted: false,
        }),
        posted: Some(job.posted.clone()),
    };
    let (outcome, posted) = job_inner(job).await;
    guard.msg = None;
    let _ = tx
        .send(Msg::JobDone {
            generation,
            outcome,
            posted,
        })
        .await;
}

/// The outcome, and whether the job mined any blob post on Ethereum
/// (L1-blob mode only).
async fn job_inner(job: JobInput) -> (JobOutcome, bool) {
    let Some(l1) = job.l1.clone() else {
        // Only the proof is abandoned: a settlement already under way runs
        // to its receipt.
        let snark = tokio::select! {
            _ = job.cancel.cancelled() => {
                return (JobOutcome::Transient("flight aborted".into()), false);
            }
            r = proved(&job) => match r {
                Ok(s) => s,
                Err(o) => return (o, false),
            },
        };
        return (settle_blobs(&job, &snark).await, false);
    };
    // L1-blob mode: the blobs go to Ethereum while the batch is proved (after
    // it, while bisecting); the flight's end (rollback, shutdown) cancels
    // the job.
    let lj = crate::l1da::L1Job {
        pid: job.pid31,
        root_before: job.expected.root_before,
        blobs: &job.blobs,
        deadline: job.deadline,
        cancel: job.cancel.clone(),
        posted: job.posted.clone(),
    };
    let work = async {
        let (proved, posting) = if job.proof_first {
            (proved(&job).await, crate::l1da::Posting::default())
        } else {
            crate::l1da::with_post(&l1, &*job.chain, &lj, proved(&job)).await
        };
        let snark = match proved {
            Ok(s) => s,
            Err(o) => return o,
        };
        crate::l1da::settle(&l1, &*job.chain, &lj, &snark, posting)
            .await
            .into()
    };
    let outcome = tokio::select! {
        _ = job.cancel.cancelled() => JobOutcome::Transient("flight aborted".into()),
        o = work => o,
    };
    (outcome, lj.posted.load(Ordering::Relaxed))
}

/// Proves the batch and checks the proof against the prepared batch and the
/// release pins.
async fn proved(job: &JobInput) -> Result<PlonkSnark, JobOutcome> {
    let t0 = job.clock.now();
    let (got, snark) = match job.prover.prove_batch(&job.request, &job.expected).await {
        Ok((got, snark, proving)) => {
            // Wall time counts queue wait: only when the prover reports none.
            let secs = proving.unwrap_or_else(|| job.clock.now().saturating_sub(t0) as f64);
            job.metrics
                .prove
                .observe(job.nf, job.n, secs, job.prove_base);
            (got, snark)
        }
        // Transport/queue trouble is retried as is; a failed job is bisected.
        Err(e) if !e.permanent => return Err(JobOutcome::Transient(e.to_string())),
        Err(e) => return Err(JobOutcome::Failed(e.to_string())),
    };
    if !got.ok || got.fail_mask != 0 {
        return Err(JobOutcome::Guest(got.fail_mask));
    }
    if got != job.expected {
        return Err(JobOutcome::Held(
            "proved publics do not match the prepared batch".into(),
        ));
    }
    match BatchPublics::from_public_values(&snark.public_values) {
        Ok(pv) if pv == job.expected => {}
        _ => {
            return Err(JobOutcome::Held(
                "snark public values do not match the batch".into(),
            ));
        }
    }
    if snark.program_vk != release::BATCH_PROGRAM_VK {
        return Err(JobOutcome::Held(
            "program vk is not the pinned batch circuit vk".into(),
        ));
    }
    if snark.root_c_vadcop_final != release::ROOT_C_VADCOP_FINAL {
        return Err(JobOutcome::Held(
            "root_c_vadcop_final is not the pinned setup".into(),
        ));
    }
    let _ = job
        .tx
        .send(Msg::JobProved {
            generation: job.generation,
        })
        .await;
    Ok(snark)
}

/// Blob mode: the pre-flight `eth_call`, then the blob transaction. A flight
/// the actor dropped while it proved (a peer landed, a rebuild) sends
/// nothing: the chain may be back at its root, with a new flight on it.
async fn settle_blobs(job: &JobInput, snark: &PlonkSnark) -> JobOutcome {
    let aborted = || JobOutcome::Transient("flight aborted".into());
    if job.cancel.is_cancelled() {
        return aborted();
    }
    if let Err(r) = job
        .chain
        .simulate_transition(&job.pid31, snark, &job.blobs)
        .await
    {
        return match r {
            RevertReason::Revert { name, .. } => JobOutcome::Reverted(name),
            RevertReason::Rpc(e) => JobOutcome::Transient(e),
        };
    }
    if job.cancel.is_cancelled() {
        return aborted();
    }
    match job
        .chain
        .submit_transition(&job.pid31, snark, &job.blobs)
        .await
    {
        Ok(rc) => JobOutcome::Landed(rc),
        Err(Web3Error::Revert(r)) => JobOutcome::Reverted(r.name().unwrap_or("revert").to_string()),
        Err(e @ Web3Error::Lost { block, .. }) => JobOutcome::Lost(e.to_string(), block),
        Err(e @ Web3Error::FeeCap { .. }) => JobOutcome::Transient(e.to_string()),
        Err(e) => JobOutcome::Transient(e.to_string()),
    }
}

impl From<crate::l1da::Settled> for JobOutcome {
    fn from(s: crate::l1da::Settled) -> Self {
        use crate::l1da::Settled as S;
        match s {
            S::Landed(rc) => JobOutcome::Landed(rc),
            S::Reverted(name) => JobOutcome::Reverted(name),
            S::Lost(e, block) => JobOutcome::Lost(e, block),
            S::Transient(e) => JobOutcome::Transient(e),
            S::Failed(e) => JobOutcome::Held(e),
        }
    }
}

/// Runs `pass` for 1, 2, … `max_chunks` chunks while it finds a budget
/// (`None`: neither this count nor a larger one fits) and keeps the largest
/// selection, the fewest chunks on a tie. A pass full at its cap ends the
/// scan: a larger count never raises the cap, so it cannot select more. A
/// pass error ends the scan with it. `size`: (votes, full at its cap).
fn choose_chunks<S, E>(
    max_chunks: usize,
    mut pass: impl FnMut(usize) -> Option<Result<S, E>>,
    size: impl Fn(&S) -> (usize, bool),
) -> Result<Option<(usize, S)>, E> {
    let mut best: Option<(usize, S)> = None;
    for chunks in 1..=max_chunks {
        let Some(r) = pass(chunks) else { break };
        let s = r?;
        let (n, saturated) = size(&s);
        if n > best.as_ref().map_or(0, |(_, b)| size(b).0) {
            best = Some((chunks, s));
        }
        if saturated {
            break;
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::{ProverService, WAIT_WINDOWS, choose_chunks, serr};
    use davinci_zkvm_sdk::Error as E;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // A full mailbox never holds up the heartbeat: a Head is dropped (the
    // next one carries the newer block), never awaited.
    #[tokio::test]
    async fn a_head_to_a_full_mailbox_does_not_wait() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let h = super::ActorHandle {
            tx,
            head: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            clock: Arc::new(crate::actor::SystemClock),
        };
        h.head(1, 1, None).await;
        let sent = tokio::time::timeout(Duration::from_millis(200), h.head(2, 2, None)).await;
        assert!(sent.is_ok(), "a head waited on a full mailbox");
    }

    // (votes, cap) per chunk count; `None` from the first infeasible count.
    fn choose(passes: &[Option<(usize, usize)>]) -> Option<(usize, usize)> {
        let r: Result<_, ()> = choose_chunks(
            passes.len(),
            |c| passes[c - 1].map(Ok),
            |&(n, cap)| (n, n == cap),
        );
        r.unwrap().map(|(c, (n, _))| (c, n))
    }

    // The largest batch wins, ties to the fewest chunks; an infeasible
    // count ends the scan; an empty pass is never chosen.
    #[test]
    fn chunk_choice_rule() {
        assert_eq!(choose(&[Some((3, 9)), Some((5, 9))]), Some((2, 5)));
        assert_eq!(choose(&[Some((4, 9)), Some((4, 8))]), Some((1, 4)));
        assert_eq!(choose(&[Some((0, 9)), Some((2, 9))]), Some((2, 2)));
        assert_eq!(choose(&[Some((0, 9)), Some((0, 9))]), None);
        assert_eq!(choose(&[Some((1, 9)), None, Some((7, 9))]), Some((1, 1)));
        // Full at its cap: no later pass is looked at.
        assert_eq!(choose(&[Some((9, 9)), Some((8, 9))]), Some((1, 9)));
    }

    // Over random inputs with caps that never grow (a larger chunk count
    // never raises the cap) the early stop equals the full scan.
    #[test]
    fn chunk_choice_early_stop_never_discards_a_larger_pass() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        for _ in 0..10_000 {
            let k = rng.gen_range(1..6);
            let mut cap = rng.gen_range(1..8);
            let mut passes = Vec::new();
            for _ in 0..k {
                cap = rng.gen_range(1..=cap);
                passes.push(Some((rng.gen_range(0..=cap), cap)));
            }
            let mut want: Option<(usize, usize)> = None;
            for (i, (n, _)) in passes.iter().flatten().enumerate() {
                if *n > want.map_or(0, |(_, m)| m) {
                    want = Some((i + 1, *n));
                }
            }
            assert_eq!(choose(&passes), want, "{passes:?}");
        }
    }

    // A 4xx on submit is a refusal of this input; server trouble is not,
    // and neither are 408/429, which describe the moment, not the input.
    #[test]
    fn submit_error_mapping() {
        let s = |status| E::Status {
            status,
            body: String::new(),
        };
        assert!(serr(s(400)).permanent);
        assert!(serr(s(422)).permanent);
        assert!(!serr(s(408)).permanent);
        assert!(!serr(s(429)).permanent);
        assert!(!serr(s(500)).permanent);
        assert!(!serr(s(502)).permanent);
        assert!(!serr(E::QueueFull).permanent);
        assert!(!serr(E::Http("connection refused".into())).permanent);
        assert!(!serr(E::Timeout).permanent);
        assert!(serr(E::Input("bad request".into())).permanent);
    }

    /// Stub prover: counts POST /prove submits and GET /jobs/{id} polls of
    /// job `j1`; the job reads `running` until `done_after` polls (0 =
    /// never), then `done`.
    async fn stub_prover(done_after: usize) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let submits = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let (sc, pc) = (submits.clone(), polls.clone());
        let app = axum::Router::new()
            .route(
                "/prove",
                axum::routing::post(move || {
                    sc.fetch_add(1, Ordering::SeqCst);
                    async {
                        (
                            axum::http::StatusCode::ACCEPTED,
                            axum::Json(serde_json::json!({"job_id": "j1"})),
                        )
                    }
                }),
            )
            .route(
                "/jobs/:id",
                axum::routing::get(
                    move |axum::extract::Path(id): axum::extract::Path<String>| {
                        assert_eq!(id, "j1", "polled a different job id");
                        let n = pc.fetch_add(1, Ordering::SeqCst) + 1;
                        let status = if done_after != 0 && n >= done_after {
                            "done"
                        } else {
                            "running"
                        };
                        async move {
                            axum::Json(serde_json::json!({
                                "job_id": "j1", "status": status, "elapsed_ms": 1500
                            }))
                        }
                    },
                ),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, submits, polls)
    }

    // A job that outlives one wait window is re-polled under the
    // same id — never resubmitted — and completes on a later window.
    #[tokio::test]
    async fn wait_done_repolls_the_same_job_across_windows() {
        // ~4 polls per 80 ms window; done on the 8th poll (second window).
        let (url, submits, polls) = stub_prover(8).await;
        let svc = ProverService::new(&url, Duration::from_millis(20), Duration::from_millis(80));
        let id = davinci_zkvm_sdk::types::JobId::parse("j1").unwrap();
        let job = svc.wait_done(&id).await.unwrap();
        // The job's own proving time, which the budget model observes.
        assert_eq!(job.elapsed_ms, Some(1500));
        assert_eq!(submits.load(Ordering::SeqCst), 0, "resubmitted the job");
        assert!(polls.load(Ordering::SeqCst) >= 8);
    }

    // A job that never finishes exhausts 1 + WAIT_WINDOWS windows
    // and gives up with a transient error, still without resubmitting.
    #[tokio::test]
    async fn wait_done_gives_up_transiently_after_all_windows() {
        let (url, submits, polls) = stub_prover(0).await;
        let svc = ProverService::new(&url, Duration::from_millis(20), Duration::from_millis(80));
        let id = davinci_zkvm_sdk::types::JobId::parse("j1").unwrap();
        let e = svc.wait_done(&id).await.unwrap_err();
        assert!(!e.permanent, "window exhaustion must stay transient");
        assert_eq!(submits.load(Ordering::SeqCst), 0);
        // Every window polled at least once.
        assert!(polls.load(Ordering::SeqCst) >= (1 + WAIT_WINDOWS) as usize);
    }
}
