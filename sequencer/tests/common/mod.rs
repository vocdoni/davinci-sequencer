//! Shared test harness: scripted fakes for the node's dependencies: a fake chain arbitrating
//! the state root under a mutex, a fake prover returning the expected
//! publics, and blob/clock fakes sharing the chain's state.

// Each test binary compiles this module separately and uses a subset.
#![allow(dead_code, unused_imports)]

pub mod anvil;

pub use std::collections::HashMap;
pub use std::path::{Path, PathBuf};
pub use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
pub use std::sync::{Arc, Mutex};
pub use std::time::Duration;

pub use alloy::primitives::{Address, B256};
pub use async_trait::async_trait;
pub use clap::Parser;
pub use davinci_sequencer::actor::{
    ActorError, ActorHandle, Chain, Clock, Deps, Prover, ProverError,
};
pub use davinci_sequencer::census::{CensusOptions, CensusStore};
pub use davinci_sequencer::config::Config;
pub use davinci_sequencer::keys::KeyStore;
pub use davinci_sequencer::monitor::{Node, spawn_node};
pub use davinci_sequencer::storage::{Db, LocalStatus, VoteStatus};
pub use davinci_sequencer::web3::{
    BlobSource, DkgState, EventKind, KeyMode, OnchainCensus, OnchainProcess, ProcessStatus,
    RegistryEvent, RevertReason, TxReceipt, Web3Error,
};
pub use davinci_state::{CensusOrigin, ProcessConfig, VerifiedVote, VotePackage, genesis_root};
pub use davinci_zkvm_sdk::ballot::{
    BallotMode, address_to_fr, encrypt_ballot, inputs_hash, vote_id,
};
pub use davinci_zkvm_sdk::blob::{Blob, TransitionBlobs};
pub use davinci_zkvm_sdk::census::{
    CensusWitness, LeanImt, census_leaf, eth_address, slot_key_address, vote_id_sign,
};
pub use davinci_zkvm_sdk::client::PlonkSnark;
pub use davinci_zkvm_sdk::crypto::babyjubjub::Point;
pub use davinci_zkvm_sdk::crypto::elgamal::keygen;
pub use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be};
pub use davinci_zkvm_sdk::groth16::BallotVerifier;
pub use davinci_zkvm_sdk::publics::{BatchPublics, ResultsPublics};
pub use davinci_zkvm_sdk::release;
pub use davinci_zkvm_sdk::types::{ProveRequest, ResultsRequest, SnarkJsProof};
pub use k256::ecdsa::SigningKey;
pub use rand::SeedableRng;
pub use rand::rngs::StdRng;
pub use tempfile::TempDir;
pub use tokio::sync::Semaphore;
pub use tokio_util::sync::CancellationToken;

pub const T0: u64 = 1_700_000_000;
pub const DURATION: u64 = 100_000;

// ---------------------------------------------------------------- harness

pub fn ballot_mode(nf: u8) -> BallotMode {
    BallotMode {
        num_fields: nf,
        group_size: 1,
        unique_values: false,
        cost_exponent: 1,
        max_value: 5,
        min_value: 0,
        max_value_sum: 5 * nf as u64,
        min_value_sum: 0,
    }
}

pub fn pid31() -> [u8; 31] {
    let mut b = [0u8; 31];
    b[..20].copy_from_slice(&[0xda; 20]);
    b[23..].copy_from_slice(&1234u64.to_be_bytes());
    b
}

/// Registry address `test_config` passes; FakeChain's chain id is 0.
pub const TEST_REGISTRY: [u8; 20] = {
    let mut r = [0u8; 20];
    r[19] = 1;
    r
};

/// The election key a node on `db` (under `test_config`) derives for `pid31()`.
pub fn node_key(db: &Db) -> Point {
    KeyStore::open(db, 0, TEST_REGISTRY)
        .unwrap()
        .public_key(&pid31())
}

pub fn pid_fr() -> Fr {
    let mut b = [0u8; 32];
    b[1..].copy_from_slice(&pid31());
    fr_from_be(&b).unwrap()
}

pub fn voter_key(i: usize) -> SigningKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(0x5eed_0000_0000_0000u64 + i as u64).to_be_bytes());
    b[31] = 1;
    SigningKey::from_bytes(&b.into()).unwrap()
}

pub fn voter_address(i: usize) -> [u8; 20] {
    eth_address(voter_key(i).verifying_key())
}

pub fn vk_hash() -> [u8; 32] {
    BallotVerifier::from_snarkjs_json(release::ballot_vk_json())
        .unwrap()
        .vk_hash()
}

pub struct Env {
    pub cfg: ProcessConfig,
    pub imt: LeanImt,
}

/// One election; `pk` overrides the encryption key (finalize tests use the
/// node's derived key, `node_key`).
pub fn env(nf: u8, n_voters: usize, pk: Option<Point>) -> Env {
    let (_, gen_pk) = keygen(&mut StdRng::seed_from_u64(7));
    let leaves: Vec<Fr> = (0..n_voters)
        .map(|i| census_leaf(&voter_address(i), 1).unwrap())
        .collect();
    let imt = LeanImt::from_leaves(leaves);
    let cfg = ProcessConfig {
        process_id: pid_fr(),
        ballot_mode: ballot_mode(nf),
        enc_key: pk.unwrap_or(gen_pk),
        census_origin: CensusOrigin::MerkleStatic,
        census_root: imt.root(),
        ballot_vk_hash: vk_hash(),
    };
    Env { cfg, imt }
}

pub fn dummy_proof() -> SnarkJsProof {
    let z = || "0".to_string();
    let o = || "1".to_string();
    SnarkJsProof {
        pi_a: [z(), o(), o()],
        pi_b: [[z(), z()], [o(), z()], [o(), z()]],
        pi_c: [z(), o(), o()],
        protocol: "groth16".into(),
        curve: "bn128".into(),
    }
}

/// A verified vote by census member `idx` with secret `kseed`.
pub fn fake_vote(env: &Env, idx: usize, fields: &[u64], kseed: u64) -> VerifiedVote {
    let cfg = &env.cfg;
    let key = voter_key(idx);
    let address = eth_address(key.verifying_key());
    let addr = address_to_fr(&address);
    let k = Fr::from(kseed);
    let vid = vote_id(&cfg.process_id, &addr, &k);
    let ballot = encrypt_ballot(&cfg.enc_key, fields, &k, cfg.ballot_mode.num_fields);
    let ih = inputs_hash(
        &cfg.process_id,
        &cfg.ballot_mode,
        &cfg.enc_key,
        &addr,
        vid,
        &ballot,
        &Fr::from(1u64),
    )
    .unwrap();
    let census = env.imt.proof(idx).unwrap();
    let slot = slot_key_address(&address);
    VerifiedVote {
        pkg: VotePackage {
            process_id: cfg.process_id,
            vote_id: vid,
            address,
            ballot,
            proof: dummy_proof(),
            inputs_hash: ih,
            signature: vote_id_sign(&key, vid),
            census: CensusWitness::Merkle(census),
            weight: 1,
        },
        slot,
    }
}

/// Writes the census document and returns its `file://` URI.
pub fn write_census(dir: &Path, n_voters: usize) -> String {
    write_census_of(dir, 0..n_voters)
}

/// [`write_census`] for the given voters, in order (repeats allowed).
pub fn write_census_of(dir: &Path, voters: impl IntoIterator<Item = usize>) -> String {
    let participants: Vec<serde_json::Value> = voters
        .into_iter()
        .map(|i| {
            serde_json::json!({
                "key": format!("0x{}", hex::encode(voter_address(i))),
                "weight": 1,
            })
        })
        .collect();
    let path = dir.join("census.json");
    std::fs::write(
        &path,
        serde_json::json!({ "participants": participants }).to_string(),
    )
    .unwrap();
    format!("file://{}", path.display())
}

// ------------------------------------------------------------ fake chain

/// How a scripted second ("bad") process misbehaves.
pub enum BadKind {
    /// Its `process()` read fails deterministically, forever.
    FetchFails,
    /// Its on-chain record carries this census URI (and genesis root).
    CensusUri { uri: String, state_root: [u8; 32] },
}

pub fn bad_pid31() -> [u8; 31] {
    let mut b = pid31();
    b[0] = 0xbb;
    b
}

pub fn bad_pid_fr() -> Fr {
    let mut b = [0u8; 32];
    b[1..].copy_from_slice(&bad_pid31());
    fr_from_be(&b).unwrap()
}

/// A recorded `requestResultsDecryption`: the accumulator and the siblings.
type DkgRequest = ([[u8; 32]; 64], Vec<[u8; 32]>);

pub struct Inner {
    proc: OnchainProcess,
    events: Vec<RegistryEvent>,
    block: u64,
    time: u64,
    blobs: HashMap<B256, Vec<Blob>>,
    submits: u64,
    /// Scripted failures: next N `process()` calls fail with an RPC error.
    fail_process: u32,
    /// Next N blob fetches fail.
    fail_blobs: u32,
    /// Next N `submit_transition` calls fail without landing.
    fail_submits: u32,
    /// Next N `submit_transition` calls land but report a receipt timeout.
    timeout_submits: u32,
    /// Events in this block are hidden from `events()` until cleared.
    hide_block: Option<u64>,
    /// A second, unservable process (bootstrap-failure tests).
    bad: Option<BadKind>,
    /// `events()` ignores `from` and re-delivers everything when set.
    replay_all: bool,
    /// When set, transitions must commit to this census root (register-20
    /// LE bytes) or revert `InvalidCensusRoot`, like the real contract.
    census_root_check: Option<[u8; 32]>,
    /// The census root of the last landed transition.
    last_census_root: Option<[u8; 32]>,
    /// The next `submit_results` reverts with this name.
    revert_results: Option<String>,
    /// How many times `submit_results` was called (landed or not).
    results_submits: u64,
    /// Every `events()` range asked for, in order.
    event_calls: Vec<(u64, u64)>,
    /// DKG modes: the committee's plaintexts, once it combined them all.
    dkg_plaintexts: Option<Vec<u64>>,
    /// Landed `requestResultsDecryption` calls, and every call made.
    dkg_requests: u64,
    dkg_request_calls: u64,
    /// Landed `finalizeResultsFromDKG` calls, and every call made.
    dkg_finalizes: u64,
    dkg_finalize_calls: u64,
    /// The next request loses a race: another node's lands first.
    dkg_request_race: bool,
    /// The accumulator and siblings of the landed request.
    dkg_request: Option<DkgRequest>,
    /// The next finalize reverts `InvalidStatus` and changes nothing: a
    /// lost race whose winner the chain does not show yet.
    dkg_finalize_lost: bool,
    /// The next `events()` call starting at this block fails, once.
    fail_events_at: Option<u64>,
    /// `events()` stalls this long first (a slow `eth_getLogs`).
    events_delay: Option<Duration>,
    /// The next N `simulate_transition` calls revert with this name.
    revert_simulate: Option<(String, u32)>,
    /// Another sequencer's batch lands first in the next submit's block.
    race_submit: Option<davinci_state::PreparedBatch>,
    /// Another node's DKG request lands before the n-th `process()` read.
    dkg_request_on_read: Option<u32>,
    /// Another node's finalize lands before the first `process()` read
    /// after `dkg_results_ready` says ready (`armed` once it did).
    dkg_finalize_after_ready: bool,
    dkg_finalize_armed: bool,
}

#[derive(Clone)]
pub struct FakeChain {
    inner: Arc<Mutex<Inner>>,
    signer: Option<Address>,
}

impl FakeChain {
    pub fn new(env: &Env, census_uri: String) -> Self {
        let proc = OnchainProcess {
            status: ProcessStatus::Ready,
            organizer: [0u8; 20],
            enc_key: env.cfg.enc_key,
            state_root: genesis_root(&env.cfg).unwrap(),
            results: vec![],
            start_time: T0,
            duration: DURATION,
            max_voters: 1000,
            voters_count: 0,
            overwritten_count: 0,
            creation_block: 1,
            batch_number: 0,
            metadata_uri: String::new(),
            metadata_hash: [0u8; 32],
            ballot_mode: env.cfg.ballot_mode,
            census: OnchainCensus {
                origin: env.cfg.census_origin as u8,
                root: fr_to_be(&env.cfg.census_root),
                uri: census_uri,
                contract_address: [0u8; 20],
            },
            key_mode: KeyMode::Sequencer,
            dkg: DkgState::default(),
        };
        let inner = Inner {
            proc,
            events: vec![RegistryEvent {
                block: 2,
                tx_hash: B256::with_last_byte(1),
                log_index: 0,
                kind: EventKind::ProcessCreated {
                    pid: pid31(),
                    creator: Address::ZERO,
                },
            }],
            block: 2,
            time: T0,
            blobs: HashMap::new(),
            submits: 0,
            fail_process: 0,
            fail_blobs: 0,
            fail_submits: 0,
            timeout_submits: 0,
            hide_block: None,
            bad: None,
            replay_all: false,
            census_root_check: None,
            last_census_root: None,
            revert_results: None,
            results_submits: 0,
            event_calls: Vec::new(),
            dkg_plaintexts: None,
            dkg_requests: 0,
            dkg_request_calls: 0,
            dkg_finalizes: 0,
            dkg_finalize_calls: 0,
            dkg_request_race: false,
            dkg_request: None,
            dkg_finalize_lost: false,
            fail_events_at: None,
            events_delay: None,
            revert_simulate: None,
            race_submit: None,
            dkg_request_on_read: None,
            dkg_finalize_after_ready: false,
            dkg_finalize_armed: false,
        };
        FakeChain {
            inner: Arc::new(Mutex::new(inner)),
            signer: Some(Address::repeat_byte(0xa1)),
        }
    }

    pub fn with_signer(&self, byte: u8) -> FakeChain {
        FakeChain {
            inner: self.inner.clone(),
            signer: Some(Address::repeat_byte(byte)),
        }
    }

    /// The same chain seen by a node without a signing key.
    pub fn observer(&self) -> FakeChain {
        FakeChain {
            inner: self.inner.clone(),
            signer: None,
        }
    }

    pub fn root(&self) -> [u8; 32] {
        self.inner.lock().unwrap().proc.state_root
    }

    /// The appended-blob attack: append a junk blob to every settled transition's
    /// stored sidecar, past the `n_blobs` the registry checked.
    pub fn append_junk_blob(&self) {
        let mut i = self.inner.lock().unwrap();
        for b in i.blobs.values_mut() {
            let junk: Blob = vec![0u8; 131072].into_boxed_slice().try_into().unwrap();
            b.push(junk);
        }
    }

    /// Script the next `submit_results` to revert with `name`.
    pub fn revert_results_once(&self, name: &str) {
        self.inner.lock().unwrap().revert_results = Some(name.to_string());
    }

    /// How many times any node called `submit_results` on this chain.
    pub fn results_submits(&self) -> u64 {
        self.inner.lock().unwrap().results_submits
    }

    /// The organizer extends the voting window (emits `DurationChanged`).
    pub fn extend_duration(&self, extra: u64) {
        let mut i = self.inner.lock().unwrap();
        i.proc.duration += extra;
        i.block += 1;
        let ev = RegistryEvent {
            block: i.block,
            tx_hash: B256::with_last_byte(4),
            log_index: 0,
            kind: EventKind::DurationChanged {
                pid: pid31(),
                duration: i.proc.duration,
            },
        };
        i.events.push(ev);
    }

    pub fn voters(&self) -> u64 {
        self.inner.lock().unwrap().proc.voters_count
    }

    pub fn results(&self) -> Vec<u64> {
        self.inner.lock().unwrap().proc.results.clone()
    }

    pub fn set_status(&self, s: ProcessStatus) {
        let mut i = self.inner.lock().unwrap();
        let old = i.proc.status;
        i.proc.status = s;
        i.block += 1;
        let ev = RegistryEvent {
            block: i.block,
            tx_hash: B256::with_last_byte(2),
            log_index: 0,
            kind: EventKind::StatusChanged {
                pid: pid31(),
                old,
                new: s,
            },
        };
        i.events.push(ev);
    }

    pub fn advance_time(&self, dt: u64) {
        let mut i = self.inner.lock().unwrap();
        i.time += dt;
        i.block += 1;
    }

    /// Sets the head time, backwards too (a lagging endpoint's head).
    pub fn set_time(&self, t: u64) {
        self.inner.lock().unwrap().time = t;
    }

    /// The process opens for votes at `t` (before the node boots).
    pub fn set_start_time(&self, t: u64) {
        self.inner.lock().unwrap().proc.start_time = t;
    }

    pub fn fail_process(&self, n: u32) {
        self.inner.lock().unwrap().fail_process = n;
    }

    pub fn fail_blobs(&self, n: u32) {
        self.inner.lock().unwrap().fail_blobs = n;
    }

    pub fn fail_submits(&self, n: u32) {
        self.inner.lock().unwrap().fail_submits = n;
    }

    pub fn timeout_submits(&self, n: u32) {
        self.inner.lock().unwrap().timeout_submits = n;
    }

    pub fn hide_block(&self, b: Option<u64>) {
        self.inner.lock().unwrap().hide_block = b;
    }

    /// Jumps the head to `block` (a long catch-up).
    pub fn set_head_block(&self, block: u64) {
        self.inner.lock().unwrap().block = block;
    }

    pub fn event_calls(&self) -> Vec<(u64, u64)> {
        self.inner.lock().unwrap().event_calls.clone()
    }

    pub fn fail_events_at(&self, from: u64) {
        self.inner.lock().unwrap().fail_events_at = Some(from);
    }

    pub fn events_delay(&self, d: Duration) {
        self.inner.lock().unwrap().events_delay = Some(d);
    }

    pub fn revert_simulates(&self, name: &str, n: u32) {
        self.inner.lock().unwrap().revert_simulate = Some((name.into(), n));
    }

    /// Another sequencer's `b` lands in the next submit's block, ahead of
    /// it: ours reverts on-chain, and a lagging endpoint's replay of it
    /// passes, so the node sees `Lost`.
    pub fn race_next_submit(&self, b: davinci_state::PreparedBatch) {
        self.inner.lock().unwrap().race_submit = Some(b);
    }

    pub fn head_block(&self) -> u64 {
        self.inner.lock().unwrap().block
    }

    pub fn set_census_contract(&self, c: [u8; 20]) {
        self.inner.lock().unwrap().proc.census.contract_address = c;
    }

    pub fn set_max_voters(&self, n: u64) {
        self.inner.lock().unwrap().proc.max_voters = n;
    }

    pub fn submit_count(&self) -> u64 {
        self.inner.lock().unwrap().submits
    }

    /// Registers the bad process, created in the same block as the good one
    /// and routed first (its failure must not shadow the good bootstrap).
    pub fn add_bad_process(&self, kind: BadKind) {
        let mut i = self.inner.lock().unwrap();
        i.bad = Some(kind);
        let ev = RegistryEvent {
            block: 2,
            tx_hash: B256::with_last_byte(9),
            log_index: 1,
            kind: EventKind::ProcessCreated {
                pid: bad_pid31(),
                creator: Address::ZERO,
            },
        };
        i.events.insert(0, ev);
    }

    /// Appends a raw registry event at the current head block.
    pub fn push_event(&self, kind: EventKind) {
        let mut i = self.inner.lock().unwrap();
        let block = i.block;
        let log_index = i.events.len() as u64;
        i.events.push(RegistryEvent {
            block,
            tx_hash: B256::with_last_byte(7),
            log_index,
            kind,
        });
    }

    /// Another party set the results on-chain: `StatusChanged(→RESULTS)`
    /// and `ResultsSet` land in the same block/tx, in that order (the
    /// registry emits both from one transaction).
    pub fn set_results_externally(&self, results: Vec<u64>) {
        let mut i = self.inner.lock().unwrap();
        let old = i.proc.status;
        i.proc.status = ProcessStatus::Results;
        i.proc.results = results.clone();
        i.block += 1;
        let block = i.block;
        i.events.push(RegistryEvent {
            block,
            tx_hash: B256::with_last_byte(4),
            log_index: 0,
            kind: EventKind::StatusChanged {
                pid: pid31(),
                old,
                new: ProcessStatus::Results,
            },
        });
        i.events.push(RegistryEvent {
            block,
            tx_hash: B256::with_last_byte(4),
            log_index: 1,
            kind: EventKind::ResultsSet {
                pid: pid31(),
                sender: self.signer.unwrap_or(Address::repeat_byte(0xEE)),
                results,
            },
        });
    }

    pub fn set_replay_all(&self, on: bool) {
        self.inner.lock().unwrap().replay_all = on;
    }

    /// Enforce the census root (register-20 LE bytes) on transitions.
    pub fn set_census_root_check(&self, root: Option<[u8; 32]>) {
        self.inner.lock().unwrap().census_root_check = root;
    }

    pub fn last_census_root(&self) -> Option<[u8; 32]> {
        self.inner.lock().unwrap().last_census_root
    }

    /// Turns the process into a DKG-mode one (before the node boots).
    pub fn set_key_mode(&self, m: KeyMode) {
        let mut i = self.inner.lock().unwrap();
        i.proc.key_mode = m;
        i.proc.dkg.epoch_id = [0xe1; 12];
        i.proc.dkg.aid = [0xa1; 32];
    }

    /// The committee combined every submitted ciphertext into `results`.
    pub fn dkg_combine(&self, results: Vec<u64>) {
        self.inner.lock().unwrap().dkg_plaintexts = Some(results);
    }

    /// `(landed, called)` for the request and the finalize.
    pub fn dkg_calls(&self) -> ((u64, u64), (u64, u64)) {
        let i = self.inner.lock().unwrap();
        (
            (i.dkg_requests, i.dkg_request_calls),
            (i.dkg_finalizes, i.dkg_finalize_calls),
        )
    }

    pub fn dkg_request(&self) -> Option<DkgRequest> {
        self.inner.lock().unwrap().dkg_request.clone()
    }

    pub fn dkg_state(&self) -> DkgState {
        self.inner.lock().unwrap().proc.dkg
    }

    /// Another node's request lands between our read and our send.
    pub fn race_dkg_request(&self) {
        self.inner.lock().unwrap().dkg_request_race = true;
    }

    /// The next finalize loses a race the chain does not show yet.
    pub fn lose_dkg_finalize(&self) {
        self.inner.lock().unwrap().dkg_finalize_lost = true;
    }

    /// Another node's request (every field active) lands just before the
    /// `n`-th `process()` read from now.
    pub fn race_dkg_request_on_read(&self, n: u32) {
        self.inner.lock().unwrap().dkg_request_on_read = Some(n);
    }

    /// Another node's finalize lands between our readiness check and the
    /// next `process()` read.
    pub fn race_dkg_finalize_after_ready(&self) {
        self.inner.lock().unwrap().dkg_finalize_after_ready = true;
    }

    pub fn status(&self) -> ProcessStatus {
        self.inner.lock().unwrap().proc.status
    }

    /// Another sequencer settles `b` (built on this chain's root); returns
    /// the block its `StateTransitioned` lands in.
    pub fn settle_externally(&self, b: &davinci_state::PreparedBatch) -> u64 {
        settle_foreign(&mut self.inner.lock().unwrap(), b)
    }
}

// Another sequencer's transition of `b` lands; returns its block.
fn settle_foreign(i: &mut Inner, b: &davinci_state::PreparedBatch) -> u64 {
    assert_eq!(b.old_root, i.proc.state_root);
    i.proc.state_root = b.new_root;
    i.proc.voters_count += b.expected.voters as u64;
    i.proc.overwritten_count += b.expected.overwrites as u64;
    i.proc.batch_number += 1;
    i.block += 1;
    let tx = B256::repeat_byte(0x5e);
    i.blobs.insert(tx, b.blobs.blobs.clone());
    let (block, voters, overwrites) = (i.block, i.proc.voters_count, i.proc.overwritten_count);
    i.events.push(RegistryEvent {
        block,
        tx_hash: tx,
        log_index: 0,
        kind: EventKind::StateTransitioned {
            pid: pid31(),
            sender: Address::repeat_byte(0xEE),
            old_root: b.old_root,
            new_root: b.new_root,
            voters,
            overwrites,
            n_blobs: b.blobs.blobs.len() as u64,
        },
    });
    block
}

/// `Sha256SmtLib.verifyInclusion`, as the registry runs it.
pub fn registry_verify(root: &[u8; 32], key: u64, value_be: &[u8; 32], sibs: &[[u8; 32]]) -> bool {
    use sha2::{Digest, Sha256};
    let n = sibs.len();
    if n == 0 || n > 64 {
        return false;
    }
    let d = sibs
        .iter()
        .rposition(|s| *s != [0u8; 32])
        .map_or(0, |i| i + 1);
    if d == n {
        return false;
    }
    let mut value_le = *value_be;
    value_le.reverse();
    let mut h: [u8; 32] = Sha256::new()
        .chain_update(key.to_le_bytes())
        .chain_update(value_le)
        .chain_update([1u8])
        .finalize()
        .into();
    for i in (0..d).rev() {
        let (l, r) = if (key >> i) & 1 == 0 {
            (h, sibs[i])
        } else {
            (sibs[i], h)
        };
        h = Sha256::new()
            .chain_update(l)
            .chain_update(r)
            .finalize()
            .into();
    }
    h == *root
}

// The registry's end rule: ENDED, or READY/PAUSED past the end.
fn ended(i: &Inner) -> bool {
    i.proc.status == ProcessStatus::Ended
        || (matches!(i.proc.status, ProcessStatus::Ready | ProcessStatus::Paused)
            && i.time >= i.proc.start_time + i.proc.duration)
}

// DKG finalize: RESULTS, `StatusChanged` then `ResultsSet` in one tx.
fn dkg_set_results(i: &mut Inner, sender: Address, results: Vec<u64>) {
    let old = i.proc.status;
    i.proc.status = ProcessStatus::Results;
    i.proc.results = results.clone();
    i.block += 1;
    let block = i.block;
    for (log_index, kind) in [
        EventKind::StatusChanged {
            pid: pid31(),
            old,
            new: ProcessStatus::Results,
        },
        EventKind::ResultsSet {
            pid: pid31(),
            sender,
            results,
        },
    ]
    .into_iter()
    .enumerate()
    {
        i.events.push(RegistryEvent {
            block,
            tx_hash: B256::with_last_byte(5),
            log_index: log_index as u64,
            kind,
        });
    }
}

// `requestResultsDecryption` landing: a READY/PAUSED process moves to
// ENDED (`ProcessStatusChanged` first), the flags and event, and the
// immediate finalize of an all-identity accumulator.
fn dkg_mark_requested(i: &mut Inner, sender: Address, count: u8) {
    i.proc.dkg.requested = true;
    i.proc.dkg.first_index = if count > 0 { 1 } else { 0 };
    i.proc.dkg.count = count;
    i.block += 1;
    let (block, d, old) = (i.block, i.proc.dkg, i.proc.status);
    let mut log_index = 0;
    if old != ProcessStatus::Ended {
        i.proc.status = ProcessStatus::Ended;
        i.events.push(RegistryEvent {
            block,
            tx_hash: B256::with_last_byte(6),
            log_index,
            kind: EventKind::StatusChanged {
                pid: pid31(),
                old,
                new: ProcessStatus::Ended,
            },
        });
        log_index += 1;
    }
    i.events.push(RegistryEvent {
        block,
        tx_hash: B256::with_last_byte(6),
        log_index,
        kind: EventKind::ResultsDecryptionRequested {
            pid: pid31(),
            epoch_id: d.epoch_id,
            aid: d.aid,
            first_index: d.first_index,
            count,
        },
    });
    if count == 0 {
        let nf = i.proc.ballot_mode.num_fields as usize;
        dkg_set_results(i, sender, vec![0; nf]);
    }
}

pub fn revert(name: &str) -> RevertReason {
    RevertReason::Revert {
        name: name.into(),
        data: Default::default(),
    }
}

/// The contract checks: window open, status READY and root continuity.
pub fn check_transition(i: &Inner, p: &BatchPublics) -> Result<(), RevertReason> {
    if i.proc.status != ProcessStatus::Ready || i.time >= i.proc.start_time + i.proc.duration {
        return Err(revert("InvalidStatus"));
    }
    if i.time < i.proc.start_time {
        return Err(revert("InvalidTimeBounds"));
    }
    if p.root_before != i.proc.state_root {
        return Err(revert("InvalidStateRoot"));
    }
    if let Some(want) = i.census_root_check
        && p.census_root != want
    {
        return Err(revert("InvalidCensusRoot"));
    }
    Ok(())
}

#[async_trait]
impl Chain for FakeChain {
    fn signer(&self) -> Option<Address> {
        self.signer
    }

    async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess, Web3Error> {
        let mut i = self.inner.lock().unwrap();
        if *pid == bad_pid31() {
            return match &i.bad {
                Some(BadKind::FetchFails) | None => {
                    Err(Web3Error::Rpc("scripted bad-process fetch".into()))
                }
                Some(BadKind::CensusUri { uri, state_root }) => {
                    let mut p = i.proc.clone();
                    p.census.uri = uri.clone();
                    p.state_root = *state_root;
                    Ok(p)
                }
            };
        }
        assert_eq!(*pid, pid31());
        if i.fail_process > 0 {
            i.fail_process -= 1;
            return Err(Web3Error::Rpc("scripted process failure".into()));
        }
        let other = Address::repeat_byte(0xEE);
        if let Some(n) = i.dkg_request_on_read {
            i.dkg_request_on_read = (n > 1).then_some(n - 1);
            if n == 1 {
                let count = i.proc.ballot_mode.num_fields;
                dkg_mark_requested(&mut i, other, count);
            }
        }
        if std::mem::take(&mut i.dkg_finalize_armed) {
            let mut results = i.dkg_plaintexts.clone().unwrap_or_default();
            results.resize(i.proc.ballot_mode.num_fields as usize, 0);
            dkg_set_results(&mut i, other, results);
        }
        Ok(i.proc.clone())
    }

    async fn events(&self, from: u64, to: u64) -> Result<Vec<RegistryEvent>, Web3Error> {
        let delay = {
            let mut i = self.inner.lock().unwrap();
            i.event_calls.push((from, to));
            if i.fail_events_at == Some(from) {
                i.fail_events_at = None;
                return Err(Web3Error::Rpc("scripted get_logs failure".into()));
            }
            i.events_delay
        };
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let i = self.inner.lock().unwrap();
        let lo = if i.replay_all { 0 } else { from };
        Ok(i.events
            .iter()
            .filter(|e| e.block >= lo && e.block <= to && Some(e.block) != i.hide_block)
            .cloned()
            .collect())
    }

    async fn head(&self) -> Result<(u64, u64), Web3Error> {
        let i = self.inner.lock().unwrap();
        Ok((i.block, i.time))
    }

    async fn confirmed_head(&self, confirmations: u64) -> Result<u64, Web3Error> {
        let i = self.inner.lock().unwrap();
        Ok(i.block.saturating_sub(confirmations))
    }

    async fn simulate_transition(
        &self,
        _pid: &[u8; 31],
        snark: &PlonkSnark,
        _blobs: &TransitionBlobs,
    ) -> Result<(), RevertReason> {
        let p = BatchPublics::from_public_values(&snark.public_values)
            .map_err(|e| RevertReason::Rpc(e.to_string()))?;
        let mut i = self.inner.lock().unwrap();
        if let Some((name, n)) = &mut i.revert_simulate
            && *n > 0
        {
            *n -= 1;
            return Err(revert(name));
        }
        check_transition(&i, &p)
    }

    async fn submit_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<TxReceipt, Web3Error> {
        let Some(sender) = self.signer else {
            return Err(Web3Error::NoSigner);
        };
        let p = BatchPublics::from_public_values(&snark.public_values)
            .map_err(|e| Web3Error::Data(e.to_string()))?;
        let mut i = self.inner.lock().unwrap();
        if i.fail_submits > 0 {
            i.fail_submits -= 1;
            return Err(Web3Error::Rpc("scripted submit failure".into()));
        }
        if let Some(b) = i.race_submit.take() {
            let block = settle_foreign(&mut i, &b);
            if check_transition(&i, &p).is_err() {
                return Err(Web3Error::Lost {
                    tx_hash: B256::repeat_byte(0x10),
                    block,
                });
            }
        }
        check_transition(&i, &p).map_err(Web3Error::Revert)?;
        i.last_census_root = Some(p.census_root);
        i.proc.state_root = p.root_after;
        i.proc.voters_count += p.voters as u64;
        i.proc.overwritten_count += p.overwrites as u64;
        i.proc.batch_number += 1;
        i.block += 1;
        i.submits += 1;
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&(0x7788_0000 + i.submits).to_be_bytes());
        let tx = B256::from(h);
        i.blobs.insert(tx, blobs.blobs.clone());
        let ev = RegistryEvent {
            block: i.block,
            tx_hash: tx,
            log_index: 0,
            kind: EventKind::StateTransitioned {
                pid: *pid,
                sender,
                old_root: p.root_before,
                new_root: p.root_after,
                voters: i.proc.voters_count,
                overwrites: i.proc.overwritten_count,
                n_blobs: blobs.blobs.len() as u64,
            },
        };
        i.events.push(ev);
        if i.timeout_submits > 0 {
            // The tx landed (root moved, event queued) but the receipt
            // never came back to the sender.
            i.timeout_submits -= 1;
            return Err(Web3Error::Stuck {
                nonce: 0,
                attempts: 1,
            });
        }
        Ok(TxReceipt {
            tx_hash: tx,
            block: i.block,
            gas_used: 0,
            replacements: 0,
        })
    }

    async fn submit_results(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
    ) -> Result<TxReceipt, Web3Error> {
        let Some(sender) = self.signer else {
            return Err(Web3Error::NoSigner);
        };
        let p = ResultsPublics::from_public_values(&snark.public_values)
            .map_err(|e| Web3Error::Data(e.to_string()))?;
        let mut i = self.inner.lock().unwrap();
        i.results_submits += 1;
        if let Some(name) = i.revert_results.take() {
            return Err(Web3Error::Revert(revert(&name)));
        }
        // Like the registry: results are accepted once ended, explicitly
        // or by the clock (a paused process past its end included).
        let ended_by_time = i.time >= i.proc.start_time + i.proc.duration
            && matches!(i.proc.status, ProcessStatus::Ready | ProcessStatus::Paused);
        if i.proc.status != ProcessStatus::Ended && !ended_by_time {
            return Err(Web3Error::Revert(revert("InvalidStatus")));
        }
        if p.state_root != i.proc.state_root {
            return Err(Web3Error::Revert(revert("InvalidStateRoot")));
        }
        i.proc.results = p.results.to_vec();
        i.proc.status = ProcessStatus::Results;
        i.block += 1;
        let ev = RegistryEvent {
            block: i.block,
            tx_hash: B256::with_last_byte(3),
            log_index: 0,
            kind: EventKind::ResultsSet {
                pid: *pid,
                sender,
                results: p.results.to_vec(),
            },
        };
        i.events.push(ev);
        Ok(TxReceipt {
            tx_hash: B256::with_last_byte(3),
            block: i.block,
            gas_used: 0,
            replacements: 0,
        })
    }

    // Like the registry, down to the SMT inclusion check under the root.
    async fn request_results_decryption(
        &self,
        _pid: &[u8; 31],
        accumulator: &[[u8; 32]; 64],
        siblings: &[[u8; 32]],
    ) -> Result<TxReceipt, Web3Error> {
        use sha2::{Digest, Sha256};
        let Some(sender) = self.signer else {
            return Err(Web3Error::NoSigner);
        };
        let mut i = self.inner.lock().unwrap();
        i.dkg_request_calls += 1;
        let r = |n: &str| Err(Web3Error::Revert(revert(n)));
        let identity =
            |x: &[u8; 32], y: &[u8; 32]| *x == [0u8; 32] && *y == fr_to_be(&Fr::from(1u64));
        let active = accumulator
            .chunks_exact(4)
            .take(i.proc.ballot_mode.num_fields as usize)
            .filter(|c| !identity(&c[0], &c[1]))
            .count() as u8;
        if std::mem::take(&mut i.dkg_request_race) {
            dkg_mark_requested(&mut i, Address::repeat_byte(0xEE), active);
        }
        if i.proc.key_mode == KeyMode::Sequencer {
            return r("InvalidKeyMode");
        }
        if i.proc.dkg.requested {
            return r("ResultsAlreadyRequested");
        }
        if matches!(
            i.proc.status,
            ProcessStatus::Canceled | ProcessStatus::Results
        ) {
            return r("InvalidStatus");
        }
        if !ended(&i) {
            return r("InvalidTimeBounds");
        }
        let mut h = Sha256::new();
        for c in accumulator {
            h.update(c);
        }
        let value: [u8; 32] = h.finalize().into();
        if !registry_verify(&i.proc.state_root, 4, &value, siblings) {
            return r("InvalidInclusionProof");
        }
        i.dkg_requests += 1;
        i.dkg_request = Some((*accumulator, siblings.to_vec()));
        dkg_mark_requested(&mut i, sender, active);
        Ok(TxReceipt {
            tx_hash: B256::with_last_byte(6),
            block: i.block,
            gas_used: 0,
            replacements: 0,
        })
    }

    async fn dkg_results_ready(&self, _pid: &[u8; 31]) -> Result<bool, Web3Error> {
        let mut i = self.inner.lock().unwrap();
        let ready = i.proc.dkg.requested && (i.proc.dkg.count == 0 || i.dkg_plaintexts.is_some());
        if ready && std::mem::take(&mut i.dkg_finalize_after_ready) {
            i.dkg_finalize_armed = true;
        }
        Ok(ready)
    }

    async fn finalize_results_from_dkg(&self, _pid: &[u8; 31]) -> Result<TxReceipt, Web3Error> {
        let Some(sender) = self.signer else {
            return Err(Web3Error::NoSigner);
        };
        let mut i = self.inner.lock().unwrap();
        i.dkg_finalize_calls += 1;
        let r = |n: &str| Err(Web3Error::Revert(revert(n)));
        if i.proc.key_mode == KeyMode::Sequencer {
            return r("InvalidKeyMode");
        }
        if std::mem::take(&mut i.dkg_finalize_lost)
            || matches!(
                i.proc.status,
                ProcessStatus::Canceled | ProcessStatus::Results
            )
        {
            return r("InvalidStatus");
        }
        if !i.proc.dkg.requested {
            return r("ResultsNotReady");
        }
        let Some(mut results) = i.dkg_plaintexts.clone() else {
            return r("ResultsNotReady");
        };
        results.resize(i.proc.ballot_mode.num_fields as usize, 0);
        i.dkg_finalizes += 1;
        dkg_set_results(&mut i, sender, results);
        Ok(TxReceipt {
            tx_hash: B256::with_last_byte(5),
            block: i.block,
            gas_used: 0,
            replacements: 0,
        })
    }
}

pub struct FakeBlobs(Arc<Mutex<Inner>>);

#[async_trait]
impl BlobSource for FakeBlobs {
    async fn blobs_for_tx(
        &self,
        tx: B256,
        _block: u64,
        n_blobs: u64,
    ) -> Result<Vec<Blob>, Web3Error> {
        let mut i = self.0.lock().unwrap();
        if i.fail_blobs > 0 {
            i.fail_blobs -= 1;
            return Err(Web3Error::Blob("scripted blob failure".into()));
        }
        let mut b = i
            .blobs
            .get(&tx)
            .cloned()
            .ok_or_else(|| Web3Error::Blob(format!("no blobs for {tx}")))?;
        // Like the real sources: the first n_blobs blobs, no fewer.
        if n_blobs == 0 || (b.len() as u64) < n_blobs {
            return Err(Web3Error::Blob(format!(
                "tx carries {} blobs, the transition names {n_blobs}",
                b.len()
            )));
        }
        b.truncate(n_blobs as usize);
        Ok(b)
    }
}

pub struct FakeClock(Arc<Mutex<Inner>>);

impl Clock for FakeClock {
    fn now(&self) -> u64 {
        self.0.lock().unwrap().time
    }
}

// ----------------------------------------------------------- fake prover

pub fn put_reg32(regs: &mut [u32; 64], base: usize, b: &[u8; 32]) {
    for (i, c) in b.chunks_exact(4).enumerate() {
        regs[base + i] = u32::from_le_bytes(c.try_into().unwrap());
    }
}

pub fn regs_to_pv(regs: &[u32; 64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(512);
    for r in regs {
        out.extend_from_slice(&(*r as u64).to_le_bytes());
    }
    out
}

pub fn encode_batch(p: &BatchPublics) -> Vec<u8> {
    let mut r = [0u32; 64];
    r[0] = p.ok as u32;
    r[1] = p.fail_mask;
    put_reg32(&mut r, 2, &p.root_before);
    put_reg32(&mut r, 10, &p.root_after);
    r[18] = p.voters;
    r[19] = p.overwrites;
    put_reg32(&mut r, 20, &p.census_root);
    put_reg32(&mut r, 28, &p.blobs_digest);
    r[36] = p.n_blobs;
    r[42] = p.occupied_before;
    r[43] = p.nproofs;
    r[44] = p.n_public;
    r[45] = p.log_n;
    regs_to_pv(&r)
}

pub fn encode_results(p: &ResultsPublics) -> Vec<u8> {
    let mut r = [0u32; 64];
    r[0] = p.ok as u32;
    r[1] = p.fail_mask;
    put_reg32(&mut r, 2, &p.state_root);
    for (i, v) in p.results.iter().enumerate() {
        r[10 + 2 * i] = *v as u32;
        r[11 + 2 * i] = (*v >> 32) as u32;
    }
    r[42] = p.cp_fail_index;
    regs_to_pv(&r)
}

pub struct FakeProver {
    gated: bool,
    hold: Arc<Semaphore>,
    /// When set, `prove_results` blocks until `release_results`.
    gate_results: AtomicBool,
    hold_results: Semaphore,
    calls: AtomicU64,
    results_calls: AtomicU64,
    /// Next batch proof reports `ok = 0` with this mask.
    pub fail_mask: Mutex<Option<u32>>,
    /// Next batch snark carries a zeroed program vk.
    pub tamper_vk: Mutex<bool>,
    /// Next N results proofs fail with a transient prover error.
    pub fail_results: Mutex<u32>,
    /// Next N batch proofs fail with a transient prover error.
    pub fail_batches: Mutex<u32>,
    /// Next N batch proofs fail permanently (the prover refused the input).
    pub fail_batches_permanent: Mutex<u32>,
    /// When each batch proof was requested (virtual time under a paused
    /// runtime), for backoff tests.
    pub call_times: Mutex<Vec<tokio::time::Instant>>,
    /// Vote count of each batch proof requested, in order.
    pub sizes: Mutex<Vec<usize>>,
}

impl FakeProver {
    pub fn new(gated: bool) -> Arc<Self> {
        Arc::new(FakeProver {
            gated,
            hold: Arc::new(Semaphore::new(0)),
            gate_results: AtomicBool::new(false),
            hold_results: Semaphore::new(0),
            calls: AtomicU64::new(0),
            results_calls: AtomicU64::new(0),
            fail_mask: Mutex::new(None),
            tamper_vk: Mutex::new(false),
            fail_results: Mutex::new(0),
            fail_batches: Mutex::new(0),
            fail_batches_permanent: Mutex::new(0),
            call_times: Mutex::new(Vec::new()),
            sizes: Mutex::new(Vec::new()),
        })
    }

    pub fn open() -> Arc<Self> {
        Self::new(false)
    }

    pub fn gated() -> Arc<Self> {
        Self::new(true)
    }

    pub fn release(&self, n: usize) {
        self.hold.add_permits(n);
    }

    pub fn gate_results(&self) {
        self.gate_results.store(true, Ordering::SeqCst);
    }

    pub fn release_results(&self, n: usize) {
        self.hold_results.add_permits(n);
    }

    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn sizes(&self) -> Vec<usize> {
        self.sizes.lock().unwrap().clone()
    }

    pub fn results_calls(&self) -> u64 {
        self.results_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Prover for FakeProver {
    async fn prove_batch(
        &self,
        request: &ProveRequest,
        expected: &BatchPublics,
    ) -> Result<(BatchPublics, PlonkSnark), ProverError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sizes.lock().unwrap().push(request.proofs.len());
        self.call_times
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        if self.gated {
            self.hold.acquire().await.unwrap().forget();
        }
        {
            let mut n = self.fail_batches.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                return Err(ProverError::transient("scripted transient batch failure"));
            }
        }
        {
            let mut n = self.fail_batches_permanent.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                return Err(ProverError::permanent("prover returned 400: bad input"));
            }
        }
        let mut p = *expected;
        if let Some(mask) = self.fail_mask.lock().unwrap().take() {
            p.ok = false;
            p.fail_mask = mask;
        }
        let mut vk = release::BATCH_PROGRAM_VK;
        if std::mem::take(&mut *self.tamper_vk.lock().unwrap()) {
            vk = [0u8; 32];
        }
        Ok((
            p,
            PlonkSnark {
                program_vk: vk,
                root_c_vadcop_final: release::ROOT_C_VADCOP_FINAL,
                public_values: encode_batch(&p),
                proof_bytes: vec![0u8; 768],
            },
        ))
    }

    async fn prove_results(
        &self,
        request: &ResultsRequest,
    ) -> Result<(ResultsPublics, PlonkSnark), ProverError> {
        self.results_calls.fetch_add(1, Ordering::SeqCst);
        if self.gate_results.load(Ordering::SeqCst) {
            self.hold_results.acquire().await.unwrap().forget();
        }
        {
            let mut n = self.fail_results.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                return Err(ProverError::transient("scripted transient results failure"));
            }
        }
        let mut root = [0u8; 32];
        hex::decode_to_slice(&request.state_root, &mut root)
            .map_err(|e| ProverError::transient(e.to_string()))?;
        let mut results = [0u64; 16];
        results[..request.results.len()].copy_from_slice(&request.results);
        let p = ResultsPublics {
            ok: true,
            fail_mask: 0,
            state_root: root,
            results,
            cp_fail_index: u32::MAX,
        };
        Ok((
            p,
            PlonkSnark {
                program_vk: release::RESULTS_PROGRAM_VK,
                root_c_vadcop_final: release::ROOT_C_VADCOP_FINAL,
                public_values: encode_results(&p),
                proof_bytes: vec![0u8; 768],
            },
        ))
    }
}

// ------------------------------------------------------------ node setup

pub struct TestSetup {
    pub env: Env,
    pub chain: FakeChain,
    pub _census_dir: TempDir,
    pub census_dir: PathBuf,
}

pub fn setup(nf: u8, n_voters: usize, pk: Option<Point>) -> TestSetup {
    let env = env(nf, n_voters, pk);
    let census_dir = TempDir::new().unwrap();
    let uri = write_census(census_dir.path(), n_voters);
    let chain = FakeChain::new(&env, uri);
    let dir = census_dir.path().to_path_buf();
    TestSetup {
        env,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    }
}

pub fn test_config(datadir: &Path, census_dir: &Path, batch_max: usize, margin: &str) -> Config {
    test_config_with(datadir, census_dir, batch_max, margin, &[])
}

/// [`test_config`] plus extra CLI flags (the API tests tune the key rate).
pub fn test_config_with(
    datadir: &Path,
    census_dir: &Path,
    batch_max: usize,
    margin: &str,
    extra: &[&str],
) -> Config {
    let bm = batch_max.to_string();
    // The fake chain's RPC, unless the test brings its own.
    let rpc: &[&str] = if extra.contains(&"--rpc-url") {
        &[]
    } else {
        &["--rpc-url", "http://127.0.0.1:8545"]
    };
    let bt: &[&str] = if extra.contains(&"--batch-time") {
        &[]
    } else {
        &["--batch-time", "1h"]
    };
    let base = [
        "davinci-sequencer",
        "--datadir",
        datadir.to_str().unwrap(),
        "--network",
        "custom",
        "--registry",
        "0x0000000000000000000000000000000000000001",
        "--blob-source",
        "anvil",
        "--batch-max",
        &bm,
        "--poll-interval",
        "20ms",
        "--confirmations",
        "0",
        "--settle-margin",
        margin,
        "--census-dir",
        census_dir.to_str().unwrap(),
    ];
    Config::parse_args(base.iter().chain(rpc).chain(bt).chain(extra).copied()).unwrap()
}

#[allow(clippy::too_many_arguments)]
pub async fn start_node(
    db: Db,
    datadir: &Path,
    s: &TestSetup,
    chain: FakeChain,
    prover: Arc<FakeProver>,
    batch_max: usize,
    margin: &str,
    shutdown: CancellationToken,
) -> Node {
    let cfg = test_config(datadir, &s.census_dir, batch_max, margin);
    start_node_cfg(db, chain, prover, cfg, shutdown).await
}

/// [`start_node`] with a caller-built [`Config`].
pub async fn start_node_cfg(
    db: Db,
    chain: FakeChain,
    prover: Arc<FakeProver>,
    cfg: Config,
    shutdown: CancellationToken,
) -> Node {
    start_node_tracked(db, chain, prover, cfg, shutdown).await.0
}

/// [`start_node_cfg`] plus the tracker of its actor and monitor tasks.
pub async fn start_node_tracked(
    db: Db,
    chain: FakeChain,
    prover: Arc<FakeProver>,
    cfg: Config,
    shutdown: CancellationToken,
) -> (Node, tokio_util::task::TaskTracker) {
    let tasks = tokio_util::task::TaskTracker::new();
    let deps = Deps {
        db,
        contracts: Arc::new(chain.clone()),
        prover,
        blobs: Arc::new(FakeBlobs(chain.inner.clone())),
        clock: Arc::new(FakeClock(chain.inner.clone())),
        tasks: tasks.clone(),
    };
    (spawn_node(cfg, deps, shutdown).await.unwrap(), tasks)
}

pub async fn wait_until(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..1500 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timeout waiting for {what}");
}

pub async fn handle(node: &Node) -> ActorHandle {
    let pid = pid_fr();
    wait_until("actor spawn", async || {
        node.processes.read().await.contains_key(&pid)
    })
    .await;
    node.processes.read().await.get(&pid).cloned().unwrap()
}

pub async fn vote_status(h: &ActorHandle, vid: u64) -> Option<VoteStatus> {
    h.status(vid).await.unwrap().map(|v| v.status)
}

pub async fn all_settled(h: &ActorHandle, vids: &[u64]) -> bool {
    for vid in vids {
        if vote_status(h, *vid).await != Some(VoteStatus::Settled) {
            return false;
        }
    }
    true
}

/// Reopens the database once the stopped node's tasks released their handles.
pub async fn reopen_db(dir: &Path) -> Db {
    for _ in 0..300 {
        if let Ok(db) = Db::open_in(dir) {
            return db;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("db lock never released");
}
