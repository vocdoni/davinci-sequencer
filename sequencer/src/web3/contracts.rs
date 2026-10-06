//! ProcessRegistry client: reads, event polling and the organizer helpers.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::json_abi::JsonAbi;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::{RpcError, TransportErrorKind};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::fr_to_be;
use davinci_zkvm_sdk::limits::TX_BLOB_CAP;
use url::Url;

use super::DAVINCITypes as T;
use super::ProcessRegistry as PR;
use super::adapter::DavinciDKGAdapter as AD;
use super::council::CouncilAdapter as CA;
use super::tx::SendState;
use super::{
    DkgState, EventKind, GraceParams, KeyMode, NewProcess, OnchainCensus, OnchainProcess,
    ProcessStatus, RegistryEvent, Result, TxReceipt, Web3Error, rpc_err, rpc_provider,
};
use crate::config::{Config, SecretString};

const ABI_JSON: &str = include_str!("../../abi/ProcessRegistry.json");
/// The registry bubbles up the verifier's errors (`InvalidProof`).
const VERIFIER_ABI_JSON: &str = include_str!("../../abi/ZiskVerifier.json");
/// And the DKG adapter's (`NoLiveEpoch`).
const ADAPTER_ABI_JSON: &str = include_str!("../../abi/DavinciDKGAdapter.json");
/// And the Council adapter's (`InvalidFieldRange`).
const COUNCIL_ABI_JSON: &str = include_str!("../../abi/CouncilAdapter.json");
/// Council manager errors (all argument-less) the Council adapter bubbles
/// up through the registry: binding at creation, request admission.
const COUNCIL_ERRORS: [&str; 12] = [
    "UnknownCeremony",
    "WrongPhase",
    "NotAllowedAdapter",
    "NotAuthorizedCreator",
    "AlreadyBound",
    "UnknownBinding",
    "AlreadyRequested",
    "BadFieldCount",
    "NonCanonical",
    "InvalidPoint",
    "NotInSubgroup",
    "UnknownRequest",
];
/// davinci-dkg manager and app-manager errors (all argument-less) the
/// adapter bubbles up through the registry.
const DKG_ERRORS: [&str; 15] = [
    "InvalidApplication",
    "ApplicationAlreadyExists",
    "InvalidSchnorrProof",
    "PointNotInSubgroup",
    "InvalidEpoch",
    "InvalidPhase",
    "InvalidOrganizerSecret",
    "InvalidPolicy",
    "AlreadyRevealed",
    "PoolExhausted",
    "InvalidProofInput",
    "InvalidCiphertext",
    "CiphertextAlreadySubmitted",
    "DecryptionLimitReached",
    "Unauthorized",
];
/// Widest block range per `eth_getLogs` request.
pub const MAX_LOG_RANGE: u64 = 5_000;
/// P256VERIFY (EIP-7951) is new in Osaka; its presence in `eth_config` marks the fork.
const OSAKA_PRECOMPILE: &str = "0x0000000000000000000000000000000000000100";

/// ProcessRegistry on one chain, with an optional signing key.
#[derive(Clone)]
pub struct Contracts {
    pub(super) provider: DynProvider,
    /// `provider`'s failover switch counter, see [`Contracts::events`].
    pub(super) epoch: Arc<AtomicU64>,
    pub(super) registry: Address,
    pub(super) signer: Option<Address>,
    /// Signs locally, so a send knows its hash before the RPC answers.
    pub(super) wallet: Option<EthereumWallet>,
    pub(super) chain_id: u64,
    pub(super) cell_proofs: Arc<AtomicBool>,
    /// Chain blob limit per transaction from `eth_config`, if it answered.
    pub(super) blob_cap: Option<usize>,
    pub(super) errors: Arc<HashMap<[u8; 4], String>>,
    pub(super) send_lock: Arc<tokio::sync::Mutex<SendState>>,
    pub(super) receipt_timeout: Duration,
    /// `dkgAdapter()`, an immutable: read once, on first DKG use.
    pub(super) dkg_adapter: Arc<tokio::sync::OnceCell<Address>>,
    /// `councilAdapter()`, likewise.
    pub(super) council_adapter: Arc<tokio::sync::OnceCell<Address>>,
}

impl std::fmt::Debug for Contracts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Contracts")
            .field("registry", &self.registry)
            .field("signer", &self.signer)
            .field("chain_id", &self.chain_id)
            .field("cell_proofs", &self.cell_proofs())
            .field("blob_cap", &self.blob_cap)
            .finish_non_exhaustive()
    }
}

pub(super) fn sat_u64(v: U256) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

fn exact_u64(what: &str, v: U256) -> Result<u64> {
    u64::try_from(v).map_err(|_| Web3Error::Data(format!("{what} above 64 bits")))
}

/// Custom error selectors of the registry, the ZisK verifier, the DKG and
/// Council adapters, and the DKG and Council contracts.
pub(super) fn error_names() -> Result<HashMap<[u8; 4], String>> {
    let mut out = HashMap::new();
    for name in DKG_ERRORS.iter().chain(&COUNCIL_ERRORS) {
        let h = alloy::primitives::keccak256(format!("{name}()"));
        out.insert([h[0], h[1], h[2], h[3]], name.to_string());
    }
    for (what, json) in [
        ("adapter", ADAPTER_ABI_JSON),
        ("council adapter", COUNCIL_ABI_JSON),
        ("registry", ABI_JSON),
        ("verifier", VERIFIER_ABI_JSON),
    ] {
        let abi: JsonAbi = serde_json::from_str(json)
            .map_err(|e| Web3Error::Config(format!("{what} ABI: {e}")))?;
        out.extend(abi.errors().map(|e| (e.selector().0, e.name.clone())));
    }
    Ok(out)
}

/// Sidecar version from an `eth_config` answer: v1 (cell proofs) iff the
/// current fork has Osaka's P256 precompile.
pub(super) fn eth_config_osaka(v: &serde_json::Value) -> bool {
    v["current"]["precompiles"].as_object().is_some_and(|m| {
        m.values().any(|a| {
            a.as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(OSAKA_PRECOMPILE))
        })
    })
}

/// A valid P-256 signature (EIP-7951 / RIP-7212 test vector): hash, r, s, x, y.
const P256_VECTOR: [u8; 160] = alloy::primitives::hex!(
    "4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4d"
    "a73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac"
    "36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d60"
    "4aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff3"
    "7618b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e"
);

/// Osaka from a P256VERIFY answer on [`P256_VECTOR`]: the precompile returns
/// `0x…01`; before Osaka the address is empty and returns nothing.
pub(super) fn p256_probe_mode(out: &[u8]) -> bool {
    out.len() == 32 && out[..31].iter().all(|b| *b == 0) && out[31] == 1
}

/// P256VERIFY probe attempts; public RPCs throw transient 429s and timeouts.
const PROBE_TRIES: u32 = 4;

/// Probes the chain for Osaka without `eth_config`; see [`p256_probe_mode`].
/// Retries with backoff and returns the last error rather than guess v0:
/// a wrong sidecar version fails every settlement after a full re-prove.
pub async fn probe_osaka<P: Provider>(p: &P) -> Result<bool> {
    let tx = TransactionRequest::default()
        .with_to(alloy::primitives::address!(
            "0000000000000000000000000000000000000100"
        ))
        .with_input(P256_VECTOR);
    let mut delay = Duration::from_millis(250);
    let mut attempt = 1;
    loop {
        match p
            .call(tx.clone())
            .block(alloy::eips::BlockId::latest())
            .await
        {
            Ok(out) => return Ok(p256_probe_mode(&out)),
            Err(e) if attempt < PROBE_TRIES => {
                tracing::warn!(error = %e, attempt, "P256VERIFY probe failed; retrying");
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempt += 1;
            }
            Err(e) => return Err(rpc_err(e)),
        }
    }
}

/// Sidecar version of chains whose RPCs rarely serve `eth_config`, used when
/// the probe fails too: true for EIP-7594 cell proofs.
fn known_chain_cell_proofs(chain_id: u64) -> Option<bool> {
    match chain_id {
        100 | 10200 => Some(true), // Gnosis, Chiado: Osaka
        _ => None,
    }
}

/// `eth_chainId` of every endpoint: another chain is fatal and named, an
/// unreachable endpoint is kept with a warning, and one must answer.
pub async fn endpoints_chain_id(rpcs: &[Url]) -> Result<u64> {
    let mut first: Option<(u64, String)> = None;
    for u in rpcs {
        let h = super::failover::host(u);
        match rpc_provider(std::slice::from_ref(u)).get_chain_id().await {
            Err(e) => tracing::warn!(
                endpoint = %h,
                error = %e,
                "RPC endpoint unreachable at boot; kept for failover"
            ),
            Ok(id) => match &first {
                None => first = Some((id, h)),
                Some((want, fh)) if *want != id => {
                    return Err(Web3Error::Config(format!(
                        "RPC endpoint {h} is on chain {id}, {fh} on chain {want}"
                    )));
                }
                Some(_) => {}
            },
        }
    }
    first
        .map(|(id, _)| id)
        .ok_or_else(|| Web3Error::Rpc("no RPC endpoint answered eth_chainId".into()))
}

/// Known per-tx blob limits for chains whose RPCs rarely serve `eth_config`.
fn known_chain_blob_cap(chain_id: u64) -> Option<usize> {
    match chain_id {
        100 | 10200 => Some(2), // Gnosis, Chiado
        _ => None,
    }
}

/// Blobs per settlement transaction: `--max-blobs-per-tx` (warned when above
/// what `eth_config` advertises), else `eth_config`, else the known-chain
/// table, else `TX_BLOB_CAP`.
pub fn resolve_blob_cap(flag: Option<usize>, advertised: Option<usize>, chain_id: u64) -> usize {
    match (flag, advertised) {
        (Some(n), Some(a)) if n > a => {
            tracing::warn!(
                flag = n,
                eth_config = a,
                "--max-blobs-per-tx is above the chain's eth_config blob limit; transactions may be rejected"
            );
            n
        }
        (Some(n), _) => n,
        (None, Some(a)) => a,
        (None, None) => {
            let (n, from) = match known_chain_blob_cap(chain_id) {
                Some(n) => (n, "known-chain table"),
                None => (TX_BLOB_CAP, "protocol default"),
            };
            tracing::warn!(
                chain_id,
                cap = n,
                from,
                "eth_config unavailable: blob cap not read from the chain; set --max-blobs-per-tx to override"
            );
            n
        }
    }
}

/// Blobs per transaction the chain takes, from an `eth_config` answer (EIP-7910):
/// `current.blobSchedule.max`, at most `TX_BLOB_CAP`. `None` when the method is
/// missing or the answer has no positive `max`.
pub fn eth_config_blob_cap(
    res: &std::result::Result<serde_json::Value, RpcError<TransportErrorKind>>,
) -> Option<usize> {
    let max = res.as_ref().ok()?["current"]["blobSchedule"]["max"].as_u64()?;
    let max = usize::try_from(max).ok()?;
    (max > 0).then(|| max.min(TX_BLOB_CAP))
}

fn point_to_abi(p: &Point) -> T::EncryptionKey {
    T::EncryptionKey {
        x: U256::from_be_bytes(fr_to_be(&p.x)),
        y: U256::from_be_bytes(fr_to_be(&p.y)),
    }
}

fn mode_to_abi(m: &BallotMode) -> T::BallotMode {
    T::BallotMode {
        uniqueValues: m.unique_values,
        numFields: m.num_fields,
        groupSize: m.group_size,
        costExponent: m.cost_exponent,
        maxValue: U256::from(m.max_value),
        minValue: U256::from(m.min_value),
        maxValueSum: U256::from(m.max_value_sum),
        minValueSum: U256::from(m.min_value_sum),
    }
}

fn mode_from_abi(m: &T::BallotMode) -> Result<BallotMode> {
    Ok(BallotMode {
        num_fields: m.numFields,
        group_size: m.groupSize,
        unique_values: m.uniqueValues,
        cost_exponent: m.costExponent,
        max_value: exact_u64("maxValue", m.maxValue)?,
        min_value: exact_u64("minValue", m.minValue)?,
        max_value_sum: exact_u64("maxValueSum", m.maxValueSum)?,
        min_value_sum: exact_u64("minValueSum", m.minValueSum)?,
    })
}

fn process_from_abi(p: &T::Process) -> Result<OnchainProcess> {
    let enc_key = Point::from_be(
        &p.encryptionKey.x.to_be_bytes(),
        &p.encryptionKey.y.to_be_bytes(),
    )
    .map_err(|e| Web3Error::Data(format!("encryption key: {e}")))?;
    let results = p
        .result
        .iter()
        .map(|r| exact_u64("result", *r))
        .collect::<Result<Vec<_>>>()?;
    let key_mode = KeyMode::try_from(p.keyMode)?;
    Ok(OnchainProcess {
        status: ProcessStatus::try_from(p.status)?,
        organizer: p.organizationId.0.0,
        enc_key,
        state_root: p.latestStateRoot.0,
        results,
        start_time: sat_u64(p.startTime),
        duration: sat_u64(p.duration),
        max_voters: sat_u64(p.maxVoters),
        voters_count: sat_u64(p.votersCount),
        overwritten_count: sat_u64(p.overwrittenVotesCount),
        creation_block: sat_u64(p.creationBlock),
        batch_number: sat_u64(p.batchNumber),
        metadata_uri: p.metadataURI.clone(),
        metadata_hash: p.metadataHash.0,
        ballot_mode: mode_from_abi(&p.ballotMode)?,
        census: OnchainCensus {
            origin: p.census.censusOrigin,
            root: p.census.censusRoot.0,
            uri: p.census.censusURI.clone(),
            contract_address: p.census.contractAddress.0.0,
        },
        key_mode,
        dkg: DkgState {
            epoch_id: p.dkgEpochId.0,
            aid: p.dkgAid.0,
            requested: p.dkgResultsRequested,
            first_index: p.dkgFirstIndex,
            count: p.dkgCount,
        },
        grace: p.window.grace.into(),
        last_vote_at: p.window.lastVoteAt,
    })
}

fn event_from_log(log: &Log) -> Result<Option<RegistryEvent>> {
    if log.removed {
        return Ok(None);
    }
    let (Some(block), Some(tx_hash), Some(log_index)) =
        (log.block_number, log.transaction_hash, log.log_index)
    else {
        return Err(Web3Error::Data("log without block, tx or index".into()));
    };
    let Some(topic0) = log.topic0() else {
        return Ok(None);
    };
    let bad = |e: alloy::sol_types::Error| Web3Error::Data(format!("event decode: {e}"));
    let kind = match *topic0 {
        PR::ProcessCreated::SIGNATURE_HASH => {
            let e = PR::ProcessCreated::decode_log(&log.inner).map_err(bad)?;
            EventKind::ProcessCreated {
                pid: e.processId.0,
                creator: e.creator,
            }
        }
        PR::ProcessStatusChanged::SIGNATURE_HASH => {
            let e = PR::ProcessStatusChanged::decode_log(&log.inner).map_err(bad)?;
            EventKind::StatusChanged {
                pid: e.processId.0,
                old: ProcessStatus::try_from(e.oldStatus)?,
                new: ProcessStatus::try_from(e.newStatus)?,
            }
        }
        PR::ProcessDurationChanged::SIGNATURE_HASH => {
            let e = PR::ProcessDurationChanged::decode_log(&log.inner).map_err(bad)?;
            EventKind::DurationChanged {
                pid: e.processId.0,
                duration: sat_u64(e.duration),
            }
        }
        PR::ProcessGraceChanged::SIGNATURE_HASH => {
            let e = PR::ProcessGraceChanged::decode_log(&log.inner).map_err(bad)?;
            EventKind::GraceChanged {
                pid: e.processId.0,
                grace: e.grace.into(),
            }
        }
        PR::ProcessMaxVotersChanged::SIGNATURE_HASH => {
            let e = PR::ProcessMaxVotersChanged::decode_log(&log.inner).map_err(bad)?;
            EventKind::MaxVotersChanged {
                pid: e.processId.0,
                max_voters: sat_u64(e.maxVoters),
            }
        }
        PR::ProcessStateTransitioned::SIGNATURE_HASH => {
            let e = PR::ProcessStateTransitioned::decode_log(&log.inner).map_err(bad)?;
            EventKind::StateTransitioned {
                pid: e.processId.0,
                sender: e.sender,
                old_root: e.oldStateRoot.0,
                new_root: e.newStateRoot.0,
                voters: sat_u64(e.newVotersCount),
                overwrites: sat_u64(e.newOverwrittenVotesCount),
                n_blobs: sat_u64(e.nBlobs),
            }
        }
        PR::ProcessResultsSet::SIGNATURE_HASH => {
            let e = PR::ProcessResultsSet::decode_log(&log.inner).map_err(bad)?;
            EventKind::ResultsSet {
                pid: e.processId.0,
                sender: e.sender,
                results: e
                    .result
                    .iter()
                    .map(|r| exact_u64("result", *r))
                    .collect::<Result<_>>()?,
            }
        }
        PR::CensusUpdated::SIGNATURE_HASH => {
            let e = PR::CensusUpdated::decode_log(&log.inner).map_err(bad)?;
            EventKind::CensusUpdated {
                pid: e.processId.0,
                root: e.censusRoot.0,
                uri: e.censusURI.clone(),
            }
        }
        PR::ProcessMetadataUpdated::SIGNATURE_HASH => {
            let e = PR::ProcessMetadataUpdated::decode_log(&log.inner).map_err(bad)?;
            EventKind::MetadataUpdated {
                pid: e.processId.0,
                uri: e.metadataURI.clone(),
                hash: e.metadataHash.0,
            }
        }
        PR::ResultsDecryptionRequested::SIGNATURE_HASH => {
            let e = PR::ResultsDecryptionRequested::decode_log(&log.inner).map_err(bad)?;
            EventKind::ResultsDecryptionRequested {
                pid: e.processId.0,
                epoch_id: e.epochId.0,
                aid: e.aid.0,
                first_index: e.firstIndex,
                count: e.count,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(RegistryEvent {
        block,
        tx_hash,
        log_index,
        kind,
    }))
}

impl Contracts {
    /// Connects with the node configuration.
    pub async fn connect(cfg: &Config) -> Result<Self> {
        Self::new(&cfg.rpc_url, cfg.registry, cfg.privkey.as_ref()).await
    }

    /// Connects to `rpcs` (in order of preference, with sticky failover).
    /// Without `privkey` the handle is read-only. Fails if an endpoint is on
    /// another chain, none answers, or the registry has no code. The sidecar
    /// version is fixed here.
    pub async fn new(
        rpcs: &[Url],
        registry: Address,
        privkey: Option<&SecretString>,
    ) -> Result<Self> {
        let (wallet, signer) = match privkey {
            Some(k) => {
                // The parse error is not echoed: it could quote the key.
                let s = PrivateKeySigner::from_str(k.expose().trim())
                    .map_err(|_| Web3Error::Config("the private key is not 32-byte hex".into()))?;
                let addr = s.address();
                (Some(EthereumWallet::from(s)), Some(addr))
            }
            None => (None, None),
        };
        let chain_id = endpoints_chain_id(rpcs).await?;
        let (provider, epoch) = super::failover_provider(rpcs);
        let code = provider.get_code_at(registry).await.map_err(rpc_err)?;
        if code.is_empty() {
            return Err(Web3Error::Config(format!("no contract at {registry}")));
        }
        // Endpoints lacking eth_config are common: ask each before the probe.
        let mut eth_config = Err(TransportErrorKind::custom_str("no RPC endpoint"));
        for u in rpcs {
            eth_config = rpc_provider(std::slice::from_ref(u))
                .raw_request("eth_config".into(), ())
                .await;
            if eth_config.is_ok() {
                break;
            }
        }
        let blob_cap = eth_config_blob_cap(&eth_config);
        let cell_proofs = match &eth_config {
            Ok(v) => eth_config_osaka(v),
            Err(e) => {
                tracing::warn!(error = %e, "eth_config unavailable: probing P256VERIFY for Osaka");
                match (
                    probe_osaka(&provider).await,
                    known_chain_cell_proofs(chain_id),
                ) {
                    (Ok(v), _) => v,
                    (Err(e), Some(v)) => {
                        tracing::warn!(error = %e, chain_id, cell_proofs = v,
                            "P256VERIFY probe failed: sidecar version from the known-chain table");
                        v
                    }
                    (Err(e), None) => {
                        return Err(Web3Error::Config(format!(
                            "blob sidecar version unknown: no eth_config and the P256VERIFY probe failed ({e})"
                        )));
                    }
                }
            }
        };
        if !cell_proofs {
            tracing::info!("pre-Osaka chain: blob transactions carry v0 sidecars");
        }
        Ok(Contracts {
            provider,
            epoch,
            registry,
            signer,
            wallet,
            chain_id,
            cell_proofs: Arc::new(AtomicBool::new(cell_proofs)),
            blob_cap,
            errors: Arc::new(error_names()?),
            send_lock: Arc::new(tokio::sync::Mutex::new(SendState::default())),
            receipt_timeout: Duration::from_secs(120),
            dkg_adapter: Arc::default(),
            council_adapter: Arc::default(),
        })
    }

    pub fn registry(&self) -> Address {
        self.registry
    }

    /// The sender address; `None` in observer mode.
    pub fn signer(&self) -> Option<Address> {
        self.signer
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Whether blob transactions carry EIP-7594 cell proofs (v1 sidecars).
    pub fn cell_proofs(&self) -> bool {
        self.cell_proofs.load(Ordering::Relaxed)
    }

    /// Blobs per transaction the chain advertised in `eth_config`, capped at
    /// `TX_BLOB_CAP`; `None` if the RPC does not serve it.
    pub fn chain_blob_cap(&self) -> Option<usize> {
        self.blob_cap
    }

    pub fn provider(&self) -> &DynProvider {
        &self.provider
    }

    /// How long a send waits for a receipt before it replaces the tx with
    /// higher fees.
    pub fn set_receipt_timeout(&mut self, d: Duration) {
        self.receipt_timeout = d;
    }

    /// `getProcess`; an unknown pid is an error.
    pub async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess> {
        let call = PR::getProcessCall {
            processId: FixedBytes(*pid),
        };
        let p = self.view(self.registry, call).await?;
        if p.organizationId == Address::ZERO {
            return Err(Web3Error::Data(format!(
                "unknown process 0x{}",
                hex::encode(pid)
            )));
        }
        process_from_abi(&p)
    }

    /// `getProcessGraceEnd`, saturating.
    pub async fn grace_end(&self, pid: &[u8; 31]) -> Result<u64> {
        let call = PR::getProcessGraceEndCall {
            processId: FixedBytes(*pid),
        };
        Ok(sat_u64(self.view(self.registry, call).await?))
    }

    /// The registry's grace immutables.
    pub async fn grace_params(&self) -> Result<GraceParams> {
        let r = self.registry;
        Ok(GraceParams {
            default_grace: self.view(r, PR::defaultGraceCall {}).await?.into(),
            grace_floor: self.view(r, PR::graceFloorCall {}).await?.into(),
            grace_ceil: self.view(r, PR::graceCeilCall {}).await?.into(),
            grace_max_total: self.view(r, PR::graceMaxTotalCall {}).await?.into(),
            notice_min: self.view(r, PR::noticeMinCall {}).await?.into(),
        })
    }

    /// Registry events in `[from, to]`, in chain order. Blocks near head can
    /// be reorged away: poll up to [`Contracts::confirmed_head`] with the
    /// configured `confirmations` (0 on dev chains).
    pub async fn events(&self, from: u64, to: u64) -> Result<Vec<RegistryEvent>> {
        let sigs = vec![
            PR::ProcessCreated::SIGNATURE_HASH,
            PR::ProcessStatusChanged::SIGNATURE_HASH,
            PR::ProcessDurationChanged::SIGNATURE_HASH,
            PR::ProcessGraceChanged::SIGNATURE_HASH,
            PR::ProcessMaxVotersChanged::SIGNATURE_HASH,
            PR::ProcessStateTransitioned::SIGNATURE_HASH,
            PR::ProcessResultsSet::SIGNATURE_HASH,
            PR::CensusUpdated::SIGNATURE_HASH,
            PR::ProcessMetadataUpdated::SIGNATURE_HASH,
            PR::ResultsDecryptionRequested::SIGNATURE_HASH,
        ];
        // A failover mid-page could land on a lagging endpoint that answers an
        // empty range above its head: read the head first, and refuse the
        // page if the endpoint changed or its head is short of `to`.
        // Load-balanced endpoints whose backends lag each other are covered
        // by the confirmation margin instead.
        let e0 = self.epoch.load(Ordering::SeqCst);
        let head = self.provider.get_block_number().await.map_err(rpc_err)?;
        let mut out = Vec::new();
        let mut start = from;
        while start <= to {
            let end = start.saturating_add(MAX_LOG_RANGE - 1).min(to);
            let filter = Filter::new()
                .address(self.registry)
                .event_signature(sigs.clone())
                .from_block(start)
                .to_block(end);
            for log in self.provider.get_logs(&filter).await.map_err(rpc_err)? {
                if log.address() != self.registry {
                    continue;
                }
                if let Some(ev) = event_from_log(&log)? {
                    out.push(ev);
                }
            }
            if end == u64::MAX {
                break;
            }
            start = end + 1;
        }
        if self.epoch.load(Ordering::SeqCst) != e0 {
            return Err(Web3Error::Rpc(format!(
                "RPC failover during logs {from}..={to}: page retried"
            )));
        }
        if head < to {
            return Err(Web3Error::Rpc(format!(
                "RPC head {head} is behind the log range {from}..={to}: page retried"
            )));
        }
        out.sort_by_key(|e| (e.block, e.log_index));
        Ok(out)
    }

    /// Latest block number and its timestamp.
    pub async fn head(&self) -> Result<(u64, u64)> {
        self.head_at(BlockNumberOrTag::Latest).await
    }

    /// Number and timestamp of a block by tag (`Latest`, `Safe`, `Finalized`, ...).
    pub async fn head_at(&self, tag: BlockNumberOrTag) -> Result<(u64, u64)> {
        let b = self
            .provider
            .get_block_by_number(tag)
            .await
            .map_err(rpc_err)?
            .ok_or_else(|| Web3Error::Data(format!("no {tag} block")))?;
        Ok((b.header.number, b.header.timestamp))
    }

    /// The highest block `confirmations` behind head.
    pub async fn confirmed_head(&self, confirmations: u64) -> Result<u64> {
        Ok(self.head().await?.0.saturating_sub(confirmations))
    }

    /// `newProcess`; returns the pid from the `ProcessCreated` log.
    pub async fn create_process(&self, p: &NewProcess) -> Result<([u8; 31], TxReceipt)> {
        let call = PR::newProcessCall {
            status: p.status as u8,
            startTime: U256::from(p.start_time),
            duration: U256::from(p.duration),
            maxVoters: U256::from(p.max_voters),
            ballotMode: mode_to_abi(&p.ballot_mode),
            census: T::Census {
                censusOrigin: p.census.origin,
                censusRoot: FixedBytes(p.census.root),
                contractAddress: Address::from(p.census.contract_address),
                censusURI: p.census.uri.clone(),
                onchainAllowAnyValidRoot: false,
            },
            metadataURI: p.metadata.clone(),
            metadataHash: FixedBytes(p.metadata_hash),
            encryptionKey: point_to_abi(&p.enc_key),
            // SEQUENCER mode: every DKG field zero.
            dkg: T::DKGParams {
                mode: KeyMode::Sequencer as u8,
                epochId: FixedBytes::ZERO,
                orgPKx: U256::ZERO,
                orgPKy: U256::ZERO,
                popAx: U256::ZERO,
                popAy: U256::ZERO,
                popZ: U256::ZERO,
            },
        };
        let tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(call.abi_encode());
        let (receipt, logs) = self.send(tx, None).await?;
        let pid = logs
            .iter()
            .filter(|l| l.address() == self.registry)
            .find_map(|l| PR::ProcessCreated::decode_log(&l.inner).ok())
            .map(|e| e.processId.0)
            .ok_or_else(|| Web3Error::Data("no ProcessCreated log".into()))?;
        Ok((pid, receipt))
    }

    /// The registry's DKG adapter; a config error when the DKG modes are
    /// disabled (zero address).
    pub async fn dkg_adapter(&self) -> Result<Address> {
        let a = self
            .dkg_adapter
            .get_or_try_init(|| self.view(self.registry, PR::dkgAdapterCall {}))
            .await?;
        if *a == Address::ZERO {
            return Err(Web3Error::Config("the registry has no DKG adapter".into()));
        }
        Ok(*a)
    }

    /// The registry's Council adapter; a config error when the COUNCIL
    /// mode is disabled (zero address).
    pub async fn council_adapter(&self) -> Result<Address> {
        let a = self
            .council_adapter
            .get_or_try_init(|| self.view(self.registry, PR::councilAdapterCall {}))
            .await?;
        if *a == Address::ZERO {
            return Err(Web3Error::Config(
                "the registry has no Council adapter".into(),
            ));
        }
        Ok(*a)
    }

    /// Whether `finalizeResultsFromDKG` can run: the decryption was requested
    /// and the committee combined every submitted ciphertext (the `plaintexts`
    /// view of the adapter the process's key mode routes to, as the registry
    /// does). A request with no active field finalizes at once, except for a
    /// COUNCIL process whose ceremony has not opened decryption: the registry
    /// publishes nothing before the gate opens, not even all-zero results.
    pub async fn dkg_results_ready(&self, pid: &[u8; 31]) -> Result<bool> {
        let p = self.process(pid).await?;
        let (eid, aid) = (FixedBytes(p.dkg.epoch_id), FixedBytes(p.dkg.aid));
        let (first, count) = (p.dkg.first_index, p.dkg.count.into());
        let pending = !p.dkg.requested;
        match p.key_mode {
            KeyMode::Sequencer => Err(Web3Error::Data("not a DKG process".into())),
            _ if pending => Ok(false),
            KeyMode::Council if !self.council_decryption_open(&p.dkg.epoch_id).await? => Ok(false),
            _ if p.dkg.count == 0 => Ok(true),
            KeyMode::DkgAutomatic | KeyMode::DkgLocked => {
                let call = AD::plaintextsCall {
                    eid,
                    aid,
                    first,
                    count,
                };
                Ok(self.view(self.dkg_adapter().await?, call).await?.ready)
            }
            // The ceremony id and the request id; the whole request at once.
            KeyMode::Council => {
                let call = CA::plaintextsCall {
                    cid: eid,
                    requestId: aid,
                    first,
                    count,
                };
                Ok(self.view(self.council_adapter().await?, call).await?.ready)
            }
        }
    }

    /// The decryption gate of Council ceremony `cid` (`isDecryptionOpen` on
    /// the registry's Council adapter, which reads the manager): closed until
    /// the ceremony's scheduled date, or its organizer's opening or fallback
    /// date. Once open it stays open.
    pub async fn council_decryption_open(&self, cid: &[u8; 12]) -> Result<bool> {
        let call = CA::isDecryptionOpenCall {
            cid: FixedBytes(*cid),
        };
        self.view(self.council_adapter().await?, call).await
    }

    /// `setProcessStatus(pid, ENDED)`.
    pub async fn end_process(&self, pid: &[u8; 31]) -> Result<TxReceipt> {
        let call = PR::setProcessStatusCall {
            processId: FixedBytes(*pid),
            newStatus: ProcessStatus::Ended as u8,
        };
        let tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(call.abi_encode());
        Ok(self.send(tx, None).await?.0)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{B256, keccak256};
    use alloy::rpc::json_rpc::ErrorPayload;

    use super::*;

    fn err_resp(code: i64, msg: &'static str) -> RpcError<TransportErrorKind> {
        RpcError::ErrorResp(ErrorPayload {
            code,
            message: msg.into(),
            data: None,
        })
    }

    #[test]
    fn sidecar_mode_decision() {
        let osaka = serde_json::json!({"current": {"precompiles": {
            "SHA256": "0x0000000000000000000000000000000000000002",
            "P256VERIFY": "0x0000000000000000000000000000000000000100"}}});
        let prague = serde_json::json!({"current": {"precompiles": {
            "SHA256": "0x0000000000000000000000000000000000000002"}}});
        assert!(eth_config_osaka(&osaka));
        assert!(!eth_config_osaka(&prague));
        assert!(!eth_config_osaka(&serde_json::json!(null)));
    }

    #[test]
    fn p256_probe_decision() {
        let mut one = [0u8; 32];
        one[31] = 1;
        // 0x…01: Osaka, v1.
        assert!(p256_probe_mode(&one));
        // Empty: no precompile, v0.
        assert!(!p256_probe_mode(&[]));
        // Anything else is not a verified signature.
        assert!(!p256_probe_mode(&[0u8; 32]));
        assert!(!p256_probe_mode(&[1u8]));
        let mut high = one;
        high[0] = 1;
        assert!(!p256_probe_mode(&high));
        assert_eq!(known_chain_cell_proofs(100), Some(true));
        assert_eq!(known_chain_cell_proofs(10200), Some(true));
        assert_eq!(known_chain_cell_proofs(1), None);
    }

    #[test]
    fn blob_cap_resolution() {
        // eth_config wins over the table.
        assert_eq!(resolve_blob_cap(None, Some(3), 100), 3);
        assert_eq!(resolve_blob_cap(None, Some(6), 1), 6);
        // Without eth_config: Gnosis and Chiado take 2, other chains six.
        assert_eq!(resolve_blob_cap(None, None, 100), 2);
        assert_eq!(resolve_blob_cap(None, None, 10200), 2);
        assert_eq!(resolve_blob_cap(None, None, 1), TX_BLOB_CAP);
        assert_eq!(resolve_blob_cap(None, None, 31337), TX_BLOB_CAP);
        // The flag always wins, even above what the chain advertises.
        assert_eq!(resolve_blob_cap(Some(4), None, 100), 4);
        assert_eq!(resolve_blob_cap(Some(1), Some(2), 100), 1);
        assert_eq!(resolve_blob_cap(Some(6), Some(2), 100), 6);
    }

    // Trimmed `eth_config` of Gnosis mainnet (gnosis-rpc.publicnode.com,
    // 2026-09-27): two blobs per block, and chainId is a hex string.
    const GNOSIS_ETH_CONFIG: &str = r#"{"current":{"activationTime":1776168380,
        "blobSchedule":{"baseFeeUpdateFraction":1112826,"max":2,"target":1},
        "chainId":"0x64","forkId":"0xcfca387c",
        "precompiles":{"KZG_POINT_EVALUATION":"0x000000000000000000000000000000000000000a",
        "P256VERIFY":"0x0000000000000000000000000000000000000100"}},
        "next":null,"last":null}"#;

    #[test]
    fn eth_config_blob_cap_decision() {
        let gnosis: serde_json::Value = serde_json::from_str(GNOSIS_ETH_CONFIG).unwrap();
        assert_eq!(gnosis["current"]["chainId"], "0x64");
        assert_eq!(eth_config_blob_cap(&Ok(gnosis.clone())), Some(2));
        assert!(eth_config_osaka(&gnosis));
        // Above the protocol maximum: capped.
        let mainnet = serde_json::json!({"current": {"blobSchedule": {"max": 9, "target": 6}}});
        assert_eq!(eth_config_blob_cap(&Ok(mainnet)), Some(TX_BLOB_CAP));
        // Missing method, or no usable max: the caller falls back.
        for missing in [
            Err(err_resp(-32601, "method not found")),
            Err(err_resp(-32601, "method is not available")),
        ] {
            assert_eq!(eth_config_blob_cap(&missing), None);
        }
        for bad in [
            serde_json::json!({"current": {"blobSchedule": {"max": 0}}}),
            serde_json::json!({"current": {"blobSchedule": {"max": "0x2"}}}),
            serde_json::json!({"current": {"blobSchedule": {"max": -1}}}),
            serde_json::json!({"current": {}}),
            serde_json::json!(null),
        ] {
            assert_eq!(eth_config_blob_cap(&Ok(bad.clone())), None, "{bad}");
        }
    }

    #[test]
    fn verifier_errors_are_named() {
        let names = error_names().unwrap();
        let sel = |sig: &str| <[u8; 4]>::try_from(&keccak256(sig)[..4]).unwrap();
        assert_eq!(
            names.get(&sel("InvalidProof()")).map(String::as_str),
            Some("InvalidProof")
        );
        assert_eq!(
            names.get(&sel("InvalidStateRoot()")).map(String::as_str),
            Some("InvalidStateRoot")
        );
        assert_eq!(
            names.get(&sel("MissingBlob(uint256)")).map(String::as_str),
            Some("MissingBlob")
        );
        for e in ["GraceOpen", "EmptyTransition", "InvalidGrace"] {
            assert_eq!(
                names.get(&sel(&format!("{e}()"))).map(String::as_str),
                Some(e)
            );
        }
    }

    #[test]
    fn council_mode_is_named_and_read_like_the_dkg() {
        assert_eq!(KeyMode::try_from(3).unwrap(), KeyMode::Council);
        assert!(KeyMode::try_from(4).is_err());
        assert_eq!(
            serde_json::to_string(&KeyMode::Council).unwrap(),
            "\"council\""
        );
        // Both adapters answer the registry's one `plaintexts` signature.
        assert_eq!(AD::plaintextsCall::SELECTOR, CA::plaintextsCall::SELECTOR);
        let names = error_names().unwrap();
        let sel = |sig: &str| <[u8; 4]>::try_from(&keccak256(sig)[..4]).unwrap();
        for e in [
            "CouncilDisabled",
            "InvalidFieldRange",
            "UnsupportedKeyMode",
            "NotAuthorizedCreator",
            "NotInSubgroup",
            "AlreadyRequested",
            "DecryptionNotOpen",
        ] {
            assert_eq!(
                names.get(&sel(&format!("{e}()"))).map(String::as_str),
                Some(e)
            );
        }
    }

    // An EL stub: `head` blocks, one ProcessCreated log at block 95 that it
    // serves once it has the block, and a switch to fail `eth_getLogs`.
    #[derive(Default)]
    struct Node {
        head: AtomicU64,
        fail_logs: AtomicBool,
    }

    const REGISTRY: Address = Address::repeat_byte(0x11);
    const PID: [u8; 31] = [7; 31];

    async fn node(head: u64, fail_logs: bool) -> (Url, Arc<Node>) {
        use axum::http::StatusCode;
        use serde_json::{Value, json};
        let n = Arc::new(Node {
            head: AtomicU64::new(head),
            fail_logs: AtomicBool::new(fail_logs),
        });
        let st = n.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| async move {
                let head = st.head.load(Ordering::SeqCst);
                let ok = |v: Value| json!({"jsonrpc":"2.0","id":req["id"],"result":v});
                let out = match req["method"].as_str().unwrap_or("") {
                    "eth_chainId" => ok("0x64".into()),
                    "eth_getCode" => ok("0x6001".into()),
                    "eth_config" => ok(json!({"current":{"blobSchedule":{"max":6}}})),
                    "eth_blockNumber" => ok(format!("{head:#x}").into()),
                    "eth_getLogs" if st.fail_logs.load(Ordering::SeqCst) => {
                        return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!("down")));
                    }
                    "eth_getLogs" => {
                        let mut pid = [0u8; 32];
                        pid[..31].copy_from_slice(&PID);
                        let log = json!({
                            "address": REGISTRY,
                            "topics": [PR::ProcessCreated::SIGNATURE_HASH, B256::from(pid),
                                B256::left_padding_from(&[0x22; 20])],
                            "data": "0x",
                            "blockNumber": "0x5f",
                            "blockHash": B256::repeat_byte(1),
                            "transactionHash": B256::repeat_byte(2),
                            "transactionIndex": "0x0",
                            "logIndex": "0x0",
                            "removed": false,
                        });
                        // A lagging node does not have block 95 yet.
                        ok(json!(if head >= 95 { vec![log] } else { vec![] }))
                    }
                    m => panic!("unexpected method {m}"),
                };
                (StatusCode::OK, axum::Json(out))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        (url, n)
    }

    // The first endpoint dies mid-page and the failover lands on one that
    // lags: the page is refused until the laggard has `to`, then the event
    // comes through.
    #[tokio::test]
    async fn a_failover_mid_page_does_not_skip_events() {
        let (a, _) = node(100, true).await;
        let (b, lag) = node(50, false).await;
        let c = Contracts::new(&[a, b], REGISTRY, None).await.unwrap();
        // Head from A, logs fail over to B, which answers an empty range.
        let err = c.events(90, 100).await.unwrap_err();
        assert!(err.to_string().contains("failover"), "{err}");
        // Retried on B, still behind.
        let err = c.events(90, 100).await.unwrap_err();
        assert!(err.to_string().contains("behind"), "{err}");
        // B catches up: the event is delivered.
        lag.head.store(100, Ordering::SeqCst);
        let evs = c.events(90, 100).await.unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].block, 95);
        assert!(matches!(evs[0].kind, EventKind::ProcessCreated { pid, .. } if pid == PID));
    }
}
