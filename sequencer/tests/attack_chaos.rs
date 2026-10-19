//! Seeded chaos runs of one election's actor. Each seed interleaves votes,
//! revotes and resends with peer landings, lost races, prover failures,
//! slow and tampered proofs, reorgs of random depth (seen at the monitor's
//! cursor or not), lost receipts and transactions that land late, storage
//! faults, crashes and restarts on the same datadir, clock jumps, pauses, the
//! end and the grace window, then drains and checks, against an oracle
//! rebuilt from the canonical chain:
//!
//! - every admitted vote is settled on chain exactly once or errored with a
//!   reason, never both, never lost; per voter, the votes settle in
//!   admission order, so the last one counts;
//! - the node's root is the chain's, and the published tally is the sum of
//!   the plaintexts of the final ballots;
//! - no two batches of one node are in flight on one root, and no proof the
//!   node must refuse (pins, publics) reaches the chain;
//! - every batch the node published or synced is in its coverage history,
//!   and a re-sealed vote's landing refreshes every slot of the batches that
//!   carried it before (docs/security.md);
//! - no panic, no actor call left unanswered (virtual-time timeouts).
//!
//! `CHAOS_SEEDS=N` runs N seeds per shard (default 50, four shards),
//! `CHAOS_SEED=s` only seed `s`, `CHAOS_TRACE=1` prints every trace.

mod common;

use common::*;
use davinci_sequencer::web3::l1proof::L1BlobProof;
use davinci_zkvm_sdk::blob::decode_blobs;
use davinci_zkvm_sdk::crypto::elgamal::{Ciphertext, decrypt};
use davinci_zkvm_sdk::crypto::field::U256;
use rand::Rng;
use std::collections::{BTreeSet, HashSet};

/// What a run exercises beyond the blob-mode sequencer-key election.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Blob,
    /// L1-blob mode: blobs on a scripted Ethereum, settlement on OP.
    L1,
    /// A committee key: results through the DKG or Council path.
    Dkg(KeyMode),
    /// An origin-2 census the organizer re-roots.
    Census,
    /// An origin-3 census: a real `OwnedCensus` on anvil, members added
    /// while the election runs, its RPC behind a proxy that fails (ANVIL=1).
    Onchain,
}

/// Origin 3: the census contract, its RPC proxy, and what it holds.
struct Onchain {
    ch: common::anvil::CensusChain,
    proxy: common::anvil::RpcProxy,
    /// Members in insertion order.
    members: Vec<usize>,
    /// Each root the contract had (BE), with its member count.
    roots: Vec<([u8; 32], usize)>,
    /// The drain runs at full harness seconds.
    draining: bool,
}

const NF: u8 = 2;
/// Over the guest's 16-slot refresh floor, so a batch refreshes a sample
/// and the coverage rule has something to check.
const VOTERS: usize = 40;
/// Seeds per shard by default (200 in all, about 80 s); four shards run in
/// parallel.
const SEEDS: u64 = 50;
const SHARDS: u64 = 4;
/// Longest wait for one actor call, virtual seconds: past it is a deadlock.
const CALL_WAIT: u64 = 120;
/// Marks a tampered snark, which no settlement may carry.
const TAMPER_TAG: u8 = 0xEE;

// ------------------------------------------------------------- panics

/// Panics by thread: a seed runs on its shard's test thread.
static PANICS: Mutex<Vec<(Option<String>, String)>> = Mutex::new(Vec::new());

// Records every panic (tokio swallows task panics) on top of the default hook.
fn panic_hook() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current().name().map(str::to_string);
            PANICS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((thread, info.to_string()));
            prev(info);
        }));
    });
}

// ---------------------------------------------------------- decisions

thread_local! {
    /// The run whose node logs on this thread (a seed runs on its shard's
    /// test thread, on a current-thread runtime).
    static DECIDE: std::cell::RefCell<Option<Arc<Shared>>> = const { std::cell::RefCell::new(None) };
}

/// Hands every anchored decision the node logs to the run's checks.
struct Decisions;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Decisions {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if e.metadata().target() != "davinci_sequencer::decision" {
            return;
        }
        #[derive(Default)]
        struct F(String, u64, String);
        impl tracing::field::Visit for F {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                match f.name() {
                    "decision" => self.0 = format!("{v:?}").trim_matches('"').to_string(),
                    "hash" => self.2 = format!("{v:?}"),
                    _ => {}
                }
            }
            fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                if f.name() == "decision" {
                    self.0 = v.to_string();
                }
            }
            fn record_u64(&mut self, f: &tracing::field::Field, v: u64) {
                if f.name() == "block" {
                    self.1 = v;
                }
            }
        }
        let mut f = F::default();
        e.record(&mut f);
        DECIDE.with(|d| {
            if let Some(sh) = &*d.borrow() {
                sh.decision(&f.0, f.1, &f.2);
            }
        });
    }
}

/// Routes the node's decisions on this thread to `sh`, through a
/// process-wide subscriber.
fn watch_decisions(sh: &Arc<Shared>) {
    use tracing_subscriber::layer::SubscriberExt;
    static OURS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ours = *OURS.get_or_init(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Decisions))
            .is_ok()
    });
    // A `RUST_LOG` run shows the logs instead: no decision checks there.
    if ours {
        DECIDE.with(|d| *d.borrow_mut() = Some(sh.clone()));
    }
}

// ------------------------------------------------------------ controls

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tamper {
    /// Another program key.
    Vk,
    /// Another setup.
    RootC,
    /// Publics other than the prepared batch's, in both views.
    Publics,
    /// The prover's publics match; the snark's public values do not.
    Values,
}

/// A job: its node incarnation, root before and root after.
type JobKey = (u64, [u8; 32], [u8; 32]);

/// Faults the harness arms; jobs consume them.
#[derive(Default)]
struct Knobs {
    transient: u32,
    permanent: u32,
    guest: u32,
    tamper_once: Option<Tamper>,
    tamper_sticky: Option<Tamper>,
    slow_secs: u64,
    results_transient: u32,
    /// The next proof asks for a crash and never returns.
    crash_on_prove: bool,
    /// The next landing asks for a crash before its receipt returns.
    crash_after_land: bool,
    /// The next N submissions wait in a mempool and report a lost receipt.
    stash: u32,
}

/// What every node incarnation of one run shares with the harness.
#[derive(Default)]
struct Shared {
    knobs: Mutex<Knobs>,
    violations: Mutex<Vec<String>>,
    /// A hook asked the harness to crash the node now.
    crash: AtomicBool,
    /// Incarnation of the live node; calls from older ones are a dead
    /// process's and fail.
    epoch: AtomicU64,
    down: AtomicBool,
    /// Every job by start: (epoch, root before, root after).
    started: Mutex<Vec<JobKey>>,
    /// Submitted transitions not mined yet.
    mempool: Mutex<Vec<(PlonkSnark, TransitionBlobs)>>,
    /// Blocks from which a reorg seen at the cursor changed the hashes.
    forks: Mutex<Vec<u64>>,
    /// What the fakes saw, for the trace.
    notes: Mutex<Vec<String>>,
    /// The process's `maxVoters`, which the fake registry does not check.
    max_voters: AtomicU64,
    /// While reads freeze, the reorgs a frozen endpoint knows of; `None`
    /// when not frozen.
    frozen_forks: Mutex<Option<usize>>,
    /// Block hashes come from a backend that is not frozen (a load-balanced
    /// endpoint), `CHAOS_SPLIT_VIEW=1`.
    split_view: AtomicBool,
    /// Block hashes come from a backend at this head, behind the others.
    hash_lag: Mutex<Option<u64>>,
    /// The endpoint refuses `blockHash` calls (EIP-1898).
    no_hash_reads: AtomicBool,
    /// Reads by number come from a backend on an old branch that answers
    /// this snapshot; reads by hash, from a fresh one.
    stale_proc: Mutex<Option<OnchainProcess>>,
    /// A parked transaction's root after, and how many batches were out when
    /// it was sent: it was sealed then, however late it is mined.
    stashed_at: Mutex<HashMap<[u8; 32], usize>>,
    /// L1-blob mode: our settlements, which the fake registry does not list
    /// with its sends: (sends before it, tx, blobs); a reverted one's tx
    /// starts 0x10.
    l1_sent: Mutex<Vec<(usize, B256, Vec<Blob>)>>,
    /// The census root (register-20 LE) each of our batches committed to,
    /// by its root after.
    census_roots: Mutex<HashMap<[u8; 32], [u8; 32]>>,
    /// From the first reorg on, reads by number answer from the branch it
    /// replaced, forever (`CHAOS_OLD_BRANCH=1`, the `old` shards).
    old_branch: AtomicBool,
    /// Block hashes `process_at_hash` answered at.
    hash_served: Mutex<std::collections::HashSet<[u8; 32]>>,
    /// Reads by hash were refused at some point (degraded mode reads by
    /// number at the anchor).
    hash_refused: AtomicBool,
    /// Anchored decisions the node logged, and the reorgs that took some
    /// transitions for good (not all re-included).
    decisions: AtomicU64,
    rebuilds: AtomicU64,
    partial_reorgs: AtomicU64,
    full_reorgs: AtomicU64,
}

impl Shared {
    /// The canonical hash of block `n`: every reorg the chain went through
    /// (with `known`, only the first that many: a frozen endpoint's view).
    fn canonical(&self, n: u64, known: usize) -> [u8; 32] {
        let forks = self
            .forks
            .lock()
            .unwrap()
            .iter()
            .take(known)
            .filter(|f| **f <= n)
            .count();
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&n.to_be_bytes());
        h[8..16].copy_from_slice(&(forks as u64).to_be_bytes());
        h
    }

    /// An anchored decision: at a canonical block hash the node read by
    /// hash (or, an endpoint refusing those, in degraded mode).
    fn decision(&self, what: &str, block: u64, hash: &str) {
        self.decisions.fetch_add(1, Ordering::Relaxed);
        if what == "rebuild" {
            self.rebuilds.fetch_add(1, Ordering::Relaxed);
        }
        // Canonical, or what a frozen endpoint (one that has not seen the
        // newest reorgs) serves for both the anchor and the read.
        let frozen = *self.frozen_forks.lock().unwrap();
        let ok = hash == hex::encode(self.canonical(block, usize::MAX))
            || frozen.is_some_and(|k| hash == hex::encode(self.canonical(block, k)));
        if !ok {
            self.violation(format!(
                "decision {what} at block {block} hash {} off the canonical chain",
                &hash[..hash.len().min(32)]
            ));
        }
        let served = hex::decode(hash)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .is_some_and(|h| self.hash_served.lock().unwrap().contains(&h));
        if !served && !self.hash_refused.load(Ordering::SeqCst) {
            self.violation(format!(
                "decision {what} at block {block} without a read by hash there"
            ));
        }
        if what != "merge" {
            self.note(format!("decision {what} at block {block}"));
        }
    }

    fn alive(&self, epoch: u64) -> bool {
        !self.down.load(Ordering::SeqCst) && self.epoch.load(Ordering::SeqCst) == epoch
    }

    /// The registry's `MaxVotersReached`: the batch's new voters on top of
    /// the chain's count pass the cap.
    fn over_cap(&self, chain: &FakeChain, p: &BatchPublics) -> bool {
        let new = u64::from(p.voters.saturating_sub(p.overwrites));
        chain.voters() + new > self.max_voters.load(Ordering::SeqCst)
    }

    fn note(&self, m: String) {
        self.notes.lock().unwrap().push(m);
    }

    fn violation(&self, m: String) {
        self.violations.lock().unwrap().push(m);
    }

    /// A job calls the chain: none of its node started on its root after
    /// it, or two batches of one node were in flight on one root.
    fn sending(&self, key: JobKey) {
        let s = self.started.lock().unwrap();
        let Some(i) = s.iter().rposition(|k| *k == key) else {
            return;
        };
        if s[i + 1..].iter().any(|k| k.0 == key.0 && k.1 == key.1) {
            self.violation(format!(
                "a dropped batch on root {} reached the chain beside a newer one",
                short(&key.1)
            ));
        }
    }
}

fn short(b: &[u8; 32]) -> String {
    hex::encode(&b[..4])
}

/// [`fake_vote`] with its census proof from `imt` at `leaf` (origin 2: the
/// voter's place in the census it votes under).
fn census_vote(
    env: &Env,
    imt: &LeanImt,
    leaf: usize,
    voter: usize,
    fields: &[u64],
    kseed: u64,
) -> VerifiedVote {
    let cfg = &env.cfg;
    let key = voter_key(voter);
    let address = eth_address(key.verifying_key());
    let addr = address_to_fr(&address);
    let k = Fr::from(kseed);
    let vid = vote_id(&cfg.process_id, &addr, &k);
    let ballot = encrypt_ballot(&cfg.enc_key, fields, &k, cfg.ballot_mode.num_fields);
    VerifiedVote {
        pkg: VotePackage {
            process_id: cfg.process_id,
            vote_id: vid,
            address,
            ballot,
            proof: dummy_proof(),
            signature: vote_id_sign(&key, vid),
            census: CensusWitness::Merkle(imt.proof(leaf).unwrap()),
            weight: 1,
        },
        slot: slot_key_address(&address),
    }
}

/// A census document of `members` (voter, weight) in `dir`; its `file://`
/// URI and tree.
fn write_members(dir: &Path, members: &[(usize, u128)]) -> (String, LeanImt) {
    let participants: Vec<serde_json::Value> = members
        .iter()
        .map(|(v, w)| {
            serde_json::json!({
                "key": format!("0x{}", hex::encode(voter_address(*v))),
                "weight": *w as u64,
            })
        })
        .collect();
    let path = dir.join("census.json");
    std::fs::write(
        &path,
        serde_json::json!({ "participants": participants }).to_string(),
    )
    .unwrap();
    let imt = LeanImt::from_leaves(
        members
            .iter()
            .map(|(v, w)| census_leaf(&voter_address(*v), *w).unwrap())
            .collect(),
    );
    (format!("file://{}", path.display()), imt)
}

fn honest_snark(p: &BatchPublics) -> PlonkSnark {
    PlonkSnark {
        program_vk: release::BATCH_PROGRAM_VK,
        root_c_vadcop_final: release::ROOT_C_VADCOP_FINAL,
        public_values: encode_batch(p),
        proof_bytes: vec![0u8; 768],
    }
}

/// The prover service of one node incarnation.
struct ChaosProver {
    sh: Arc<Shared>,
    epoch: u64,
}

#[async_trait]
impl Prover for ChaosProver {
    async fn prove_batch(
        &self,
        request: &ProveRequest,
        expected: &BatchPublics,
    ) -> Result<(BatchPublics, PlonkSnark, Option<f64>), ProverError> {
        let sh = &self.sh;
        if !sh.alive(self.epoch) {
            return Err(ProverError::transient("node down"));
        }
        let vids: Vec<String> = request
            .sigs
            .iter()
            .map(|s| format!("{:#x}", s.vote_id))
            .collect();
        sh.note(format!(
            "prove {} -> {}: {vids:?}",
            short(&expected.root_before),
            short(&expected.root_after)
        ));
        sh.started
            .lock()
            .unwrap()
            .push((self.epoch, expected.root_before, expected.root_after));
        let (slow, crash, fault) = {
            let mut k = sh.knobs.lock().unwrap();
            let fault = if k.transient > 0 {
                k.transient -= 1;
                Some(Err(ProverError::transient("chaos: prover unreachable")))
            } else if k.permanent > 0 {
                k.permanent -= 1;
                Some(Err(ProverError::permanent("chaos: job failed")))
            } else {
                None
            };
            let guest = fault.is_none() && k.guest > 0;
            if guest {
                k.guest -= 1;
            }
            let tamper = if fault.is_none() && !guest {
                k.tamper_once.take().or(k.tamper_sticky)
            } else {
                None
            };
            (
                k.slow_secs,
                std::mem::take(&mut k.crash_on_prove),
                fault
                    .map(|f| (f, false, None))
                    .unwrap_or((Ok(()), guest, tamper)),
            )
        };
        if crash {
            sh.crash.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        if slow > 0 {
            tokio::time::sleep(Duration::from_secs(slow)).await;
        }
        let (fault, guest, tamper) = fault;
        if !sh.alive(self.epoch) {
            return Err(ProverError::transient("node down"));
        }
        fault?;
        if guest {
            let mut p = *expected;
            p.ok = false;
            p.fail_mask = 1;
            return Ok((p, honest_snark(&p), Some(2.0)));
        }
        let mut got = *expected;
        let mut snark = honest_snark(expected);
        if let Some(t) = tamper {
            snark.proof_bytes[0] = TAMPER_TAG;
            match t {
                Tamper::Vk => snark.program_vk = [0u8; 32],
                Tamper::RootC => snark.root_c_vadcop_final = [0u8; 32],
                Tamper::Publics => {
                    got.voters += 1;
                    snark.public_values = encode_batch(&got);
                }
                Tamper::Values => {
                    let mut p = *expected;
                    p.overwrites += 1;
                    snark.public_values = encode_batch(&p);
                }
            }
        }
        Ok((got, snark, Some(2.0)))
    }

    async fn prove_results(
        &self,
        request: &ResultsRequest,
    ) -> Result<(ResultsPublics, PlonkSnark), ProverError> {
        if !self.sh.alive(self.epoch) {
            return Err(ProverError::transient("node down"));
        }
        {
            let mut k = self.sh.knobs.lock().unwrap();
            if k.results_transient > 0 {
                k.results_transient -= 1;
                return Err(ProverError::transient("chaos: results prover unreachable"));
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

/// The registry as one node incarnation sees it: the fake chain, plus
/// block hashes that move with the reorgs the cursor can see, a mempool,
/// and the settlement-side invariant checks.
struct ChaosChain {
    inner: FakeChain,
    sh: Arc<Shared>,
    epoch: u64,
}

impl ChaosChain {
    fn down(&self) -> Option<Web3Error> {
        (!self.sh.alive(self.epoch)).then(|| Web3Error::Rpc("node down".into()))
    }

    fn check_snark(&self, what: &str, snark: &PlonkSnark) {
        if snark.proof_bytes.first() == Some(&TAMPER_TAG)
            || snark.program_vk != release::BATCH_PROGRAM_VK
            || snark.root_c_vadcop_final != release::ROOT_C_VADCOP_FINAL
        {
            self.sh
                .violation(format!("{what} of a proof the node had to refuse"));
        }
    }

    fn note_census(&self, snark: &PlonkSnark) {
        if let Ok(p) = BatchPublics::from_public_values(&snark.public_values) {
            self.sh
                .census_roots
                .lock()
                .unwrap()
                .insert(p.root_after, p.census_root);
        }
    }

    fn key(&self, snark: &PlonkSnark) -> Option<JobKey> {
        let p = BatchPublics::from_public_values(&snark.public_values).ok()?;
        Some((self.epoch, p.root_before, p.root_after))
    }
}

#[async_trait]
impl Chain for ChaosChain {
    fn signer(&self) -> Option<Address> {
        self.inner.signer()
    }
    async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess, Web3Error> {
        if let Some(p) = self.sh.stale_proc.lock().unwrap().clone() {
            return Ok(p);
        }
        self.inner.process(pid).await
    }
    async fn process_at(&self, pid: &[u8; 31], block: u64) -> Result<OnchainProcess, Web3Error> {
        if let Some(p) = self.sh.stale_proc.lock().unwrap().clone() {
            return Ok(p);
        }
        self.inner.process_at(pid, block).await
    }
    // The reads' backend (frozen or not) knows a block by the hash its own
    // view gives it; any other hash (another branch) is unknown to it.
    async fn process_at_hash(
        &self,
        pid: &[u8; 31],
        hash: [u8; 32],
    ) -> Result<OnchainProcess, Web3Error> {
        if self.sh.no_hash_reads.load(Ordering::SeqCst) {
            self.sh.hash_refused.store(true, Ordering::SeqCst);
            return Err(Web3Error::Rpc(
                "invalid params: blockHash not supported".into(),
            ));
        }
        let n = u64::from_be_bytes(hash[..8].try_into().unwrap());
        let known = self.sh.frozen_forks.lock().unwrap().unwrap_or(usize::MAX);
        let forks = self
            .sh
            .forks
            .lock()
            .unwrap()
            .iter()
            .take(known)
            .filter(|f| **f <= n)
            .count() as u64;
        let (head, _) = self.inner.head().await?;
        if n > head || hash[8..16] != forks.to_be_bytes() {
            return Err(Web3Error::Rpc(format!(
                "header not found: 0x{}",
                hex::encode(hash)
            )));
        }
        let p = self.inner.process_at(pid, n).await?;
        self.sh.hash_served.lock().unwrap().insert(hash);
        Ok(p)
    }
    async fn events(&self, from: u64, to: u64) -> Result<Vec<RegistryEvent>, Web3Error> {
        self.inner.events(from, to).await
    }
    async fn head(&self) -> Result<(u64, u64), Web3Error> {
        self.inner.head().await
    }
    // One endpoint: frozen reads freeze the confirmed head too (unless the
    // split view serves it from a fresh backend).
    async fn confirmed_head(&self, confirmations: u64) -> Result<u64, Web3Error> {
        if self.sh.split_view.load(Ordering::SeqCst) {
            return self.inner.confirmed_head(confirmations).await;
        }
        Ok(self.inner.head().await?.0.saturating_sub(confirmations))
    }
    async fn block_ref(&self, n: u64) -> Result<Option<([u8; 32], u64)>, Web3Error> {
        let head = if self.sh.split_view.load(Ordering::SeqCst) {
            self.inner.head_block()
        } else {
            self.inner.head().await?.0
        };
        if n > head || self.sh.hash_lag.lock().unwrap().is_some_and(|l| n > l) {
            return Ok(None);
        }
        let known = match *self.sh.frozen_forks.lock().unwrap() {
            Some(k) if !self.sh.split_view.load(Ordering::SeqCst) => k,
            _ => usize::MAX,
        };
        let forks = self
            .sh
            .forks
            .lock()
            .unwrap()
            .iter()
            .take(known)
            .filter(|f| **f <= n)
            .count();
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&n.to_be_bytes());
        h[8..16].copy_from_slice(&(forks as u64).to_be_bytes());
        Ok(Some((h, self.inner.block_time(n))))
    }
    async fn simulate_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<(), RevertReason> {
        if let Some(e) = self.down() {
            return Err(RevertReason::Rpc(e.to_string()));
        }
        self.check_snark("a simulation", snark);
        if let Some(k) = self.key(snark) {
            self.sh.sending(k);
        }
        if let Ok(p) = BatchPublics::from_public_values(&snark.public_values)
            && self.sh.over_cap(&self.inner, &p)
        {
            return Err(revert("MaxVotersReached"));
        }
        self.inner.simulate_transition(pid, snark, blobs).await
    }
    async fn submit_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<TxReceipt, Web3Error> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.check_snark("a settlement", snark);
        if let Some(k) = self.key(snark) {
            self.sh.sending(k);
        }
        if let Ok(p) = BatchPublics::from_public_values(&snark.public_values)
            && self.sh.over_cap(&self.inner, &p)
        {
            return Err(Web3Error::Revert(revert("MaxVotersReached")));
        }
        let stash = {
            let mut k = self.sh.knobs.lock().unwrap();
            let s = k.stash > 0;
            k.stash = k.stash.saturating_sub(1);
            s
        };
        if stash {
            if let Ok(p) = BatchPublics::from_public_values(&snark.public_values) {
                let out = self.inner.sent_batches().len();
                self.sh.stashed_at.lock().unwrap().insert(p.root_after, out);
            }
            self.sh
                .mempool
                .lock()
                .unwrap()
                .push((snark.clone(), blobs.clone()));
            return Err(Web3Error::Stuck {
                nonce: 0,
                attempts: 1,
            });
        }
        self.note_census(snark);
        let r = self.inner.submit_transition(pid, snark, blobs).await;
        if r.is_ok() && std::mem::take(&mut self.sh.knobs.lock().unwrap().crash_after_land) {
            self.sh.crash.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        r
    }
    async fn submit_results(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
    ) -> Result<TxReceipt, Web3Error> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.inner.submit_results(pid, snark).await
    }
    async fn request_results_decryption(
        &self,
        pid: &[u8; 31],
        accumulator: &[[u8; 32]; 64],
        siblings: &[[u8; 32]],
    ) -> Result<TxReceipt, Web3Error> {
        self.inner
            .request_results_decryption(pid, accumulator, siblings)
            .await
    }
    async fn dkg_results_ready(&self, pid: &[u8; 31]) -> Result<bool, Web3Error> {
        self.inner.dkg_results_ready(pid).await
    }
    async fn council_decryption_open(&self, cid: &[u8; 12]) -> Result<bool, Web3Error> {
        self.inner.council_decryption_open(cid).await
    }
    async fn finalize_results_from_dkg(&self, pid: &[u8; 31]) -> Result<TxReceipt, Web3Error> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.inner.finalize_results_from_dkg(pid).await
    }
    async fn l1_origin(&self) -> Result<L1Origin, Web3Error> {
        self.inner.l1_origin().await
    }
    async fn record_l1_origin(&self) -> Result<L1Origin, Web3Error> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.inner.record_l1_origin().await
    }
    async fn simulate_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<(), RevertReason> {
        if let Some(e) = self.down() {
            return Err(RevertReason::Rpc(e.to_string()));
        }
        self.check_snark("an L1 simulation", snark);
        if let Some(k) = self.key(snark) {
            self.sh.sending(k);
        }
        if let Ok(p) = BatchPublics::from_public_values(&snark.public_values)
            && self.sh.over_cap(&self.inner, &p)
        {
            return Err(revert("MaxVotersReached"));
        }
        self.inner
            .simulate_transition_l1(pid, snark, blobs, proof)
            .await
    }
    async fn submit_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<TxReceipt, Web3Error> {
        if let Some(e) = self.down() {
            return Err(e);
        }
        self.check_snark("an L1 settlement", snark);
        if let Some(k) = self.key(snark) {
            self.sh.sending(k);
        }
        if let Ok(p) = BatchPublics::from_public_values(&snark.public_values)
            && self.sh.over_cap(&self.inner, &p)
        {
            return Err(Web3Error::Revert(revert("MaxVotersReached")));
        }
        self.note_census(snark);
        let at = self.inner.sent_batches().len();
        let r = self
            .inner
            .submit_transition_l1(pid, snark, blobs, proof)
            .await;
        let tx = match &r {
            Ok(rc) => Some(rc.tx_hash),
            Err(Web3Error::Lost { .. }) => {
                let mut h = [0x10; 32];
                h[24..].copy_from_slice(&(at as u64).to_be_bytes());
                Some(B256::from(h))
            }
            Err(_) => None,
        };
        if let Some(tx) = tx {
            self.sh
                .l1_sent
                .lock()
                .unwrap()
                .push((at, tx, blobs.blobs.clone()));
        }
        if r.is_ok() && std::mem::take(&mut self.sh.knobs.lock().unwrap().crash_after_land) {
            self.sh.crash.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
        r
    }
}

// ---------------------------------------------------------------- run

/// One vote the harness built: for this node, or only for a peer.
struct Cast {
    v: VerifiedVote,
    voter: usize,
    fields: [u64; 2],
    /// Its place in this node's admission order, once admitted.
    admitted: Option<usize>,
    /// A peer batch carried it.
    by_peer: bool,
    /// The step it was admitted at.
    admitted_step: usize,
    /// Census mode: the version (index in `Run::census`) its proof is at.
    version: usize,
}

struct Live {
    node: Node,
    h: ActorHandle,
    db: Db,
    shutdown: CancellationToken,
    tasks: tokio_util::task::TaskTracker,
}

/// Per-seed node settings.
struct Tuning {
    batch_max: usize,
    min_mix: u32,
    batch_time: u64,
    solo_wait: u64,
    flush_horizon: u64,
    /// `--confirmations`: the monitor's cursor sits this far below the head.
    confirmations: u64,
}

/// One census the organizer announced (origin 2): its members by voter and
/// weight, in file order; `imt` is `None` when nothing serves it.
struct Version {
    members: Vec<(usize, u128)>,
    root: [u8; 32],
    imt: Option<LeanImt>,
}

struct Run {
    mode: Mode,
    /// L1-blob mode: the scripted Ethereum.
    l1: Option<FakeL1>,
    /// Committee modes: the election secret the committee shares.
    dkg_sk: Option<U256>,
    /// The accumulator the committee last combined.
    combined: Option<[[u8; 32]; 64]>,
    /// Locked mode: the organizer revealed its key.
    revealed: bool,
    /// Origin 2: every census announced, the first at genesis.
    census: Vec<Version>,
    /// No votes at all (a zero-vote election).
    no_votes: bool,
    rng: StdRng,
    s: TestSetup,
    dir: TempDir,
    sh: Arc<Shared>,
    tune: Tuning,
    node: Option<Live>,
    first_db: Option<Db>,
    votes: Vec<Cast>,
    admitted: Vec<usize>,
    kseed: u64,
    trace: Vec<String>,
    step: usize,
    /// Crash at this step (an armed storage crash takes effect).
    crash_at: Option<usize>,
    /// Resume a pause at this step.
    resume_at: Option<usize>,
    /// Unfreeze the chain's reads at this step.
    unfreeze_at: Option<usize>,
    /// Show the hidden block's events at this step.
    unhide_at: Option<usize>,
    /// Reads by number are fresh again at this step.
    unstale_at: Option<usize>,
    /// L1-blob mode: undo a held pool, pinned origin, proof fault, gated
    /// receipt or fee spike at these steps.
    l1_undo: Vec<(usize, &'static str)>,
    /// Stop failing transition writes at this step.
    unfail_tx_at: Option<usize>,
    restarts: u32,
    /// `--degraded-number-reads` (a single-backend endpoint).
    number_reads: bool,
    /// Origin 3: the census contract.
    onchain: Option<Onchain>,
}

/// An actor call that must answer within [`CALL_WAIT`] virtual seconds.
async fn within<T>(sh: &Shared, what: &str, f: impl std::future::Future<Output = T>) -> Option<T> {
    match tokio::time::timeout(Duration::from_secs(CALL_WAIT), f).await {
        Ok(t) => Some(t),
        Err(_) => {
            sh.violation(format!("deadlock: {what} unanswered for {CALL_WAIT} s"));
            None
        }
    }
}

impl Run {
    fn new(seed: u64) -> Run {
        Run::of(Mode::Blob, seed)
    }

    fn of(mode: Mode, seed: u64) -> Run {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        match mode {
            Mode::Blob => {
                let s = setup(NF, VOTERS, Some(node_key(&db)));
                Run::with(seed, s, dir, db)
            }
            Mode::L1 => {
                let s = setup(NF, VOTERS, Some(node_key(&db)));
                let mut run = Run::with(seed, s, dir, db);
                run.mode = Mode::L1;
                let l1 = FakeL1::new();
                l1.strict_nonces(true);
                run.l1 = Some(l1);
                run
            }
            Mode::Dkg(m) => {
                // The env's own key (`keygen` at seed 7): the committee's.
                let s = setup(NF, VOTERS, None);
                s.chain.set_key_mode(m);
                let mut run = Run::with(seed, s, dir, db);
                run.mode = mode;
                run.dkg_sk = Some(keygen(&mut StdRng::seed_from_u64(7)).0);
                run.no_votes = run.rng.gen_bool(0.15);
                if m == KeyMode::Council && run.rng.gen_bool(0.4) {
                    run.s.chain.set_council_gate(false);
                }
                run
            }
            Mode::Onchain => panic!("an origin-3 run starts with Run::onchain"),
            Mode::Census => {
                let n = 8 + (seed as usize % 24);
                let mut run = Run::offchain_with(seed, n, dir, db);
                run.mode = Mode::Census;
                run
            }
        }
    }

    /// An origin-2 election (a census the organizer can re-root) over
    /// voters `0..n`.
    fn offchain(seed: u64, n: usize) -> Run {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        Run::offchain_with(seed, n, dir, db)
    }

    fn offchain_with(seed: u64, n: usize, dir: TempDir, db: Db) -> Run {
        let mut e = env(NF, n, Some(node_key(&db)));
        e.cfg.census_origin = CensusOrigin::MerkleOffchainDynamic;
        let census_dir = TempDir::new().unwrap();
        let uri = write_census(census_dir.path(), n);
        let chain = FakeChain::new(&e, uri);
        let path = census_dir.path().to_path_buf();
        let first = Version {
            members: (0..n).map(|v| (v, 1)).collect(),
            root: e.cfg.census_root,
            imt: Some(e.imt.clone()),
        };
        let s = TestSetup {
            env: e,
            chain,
            _census_dir: census_dir,
            census_dir: path,
        };
        let mut run = Run::with(seed, s, dir, db);
        run.census.push(first);
        run
    }

    /// An origin-3 election over a real census contract on anvil, the first
    /// 20 voters members at its creation.
    async fn onchain(seed: u64) -> Run {
        let ch = common::anvil::CensusChain::start().await;
        let first: Vec<([u8; 20], u128)> = (0..20).map(|v| (voter_address(v), 1)).collect();
        ch.add(&first).await;
        let (root, _) = ch.state().await;
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let mut e = env(NF, VOTERS, Some(node_key(&db)));
        e.cfg.census_origin = CensusOrigin::MerkleOnchainDynamic;
        e.cfg.census_root = root;
        let census_dir = TempDir::new().unwrap();
        let chain = FakeChain::new(&e, String::new());
        chain.set_census_contract(ch.census.into_array());
        while chain.head_block() < ch.head().await {
            chain.advance_time(0);
        }
        let path = census_dir.path().to_path_buf();
        let s = TestSetup {
            env: e,
            chain,
            _census_dir: census_dir,
            census_dir: path,
        };
        let proxy = common::anvil::RpcProxy::start(ch.anvil.endpoint()).await;
        let mut run = Run::with(seed, s, dir, db);
        run.mode = Mode::Onchain;
        run.onchain = Some(Onchain {
            ch,
            proxy,
            members: (0..20).collect(),
            roots: vec![(root, 20)],
            draining: false,
        });
        run
    }

    fn with(seed: u64, s: TestSetup, dir: TempDir, db: Db) -> Run {
        let mut rng = StdRng::seed_from_u64(seed);
        let batch_time = rng.gen_range(20..200);
        let tune = Tuning {
            batch_max: [1, 2, 3, 4, 6][rng.gen_range(0..5)],
            min_mix: rng.gen_range(1..=3),
            batch_time,
            solo_wait: batch_time * rng.gen_range(1..=3),
            flush_horizon: rng.gen_range(30..300),
            confirmations: [0, 0, 1, 3][rng.gen_range(0..4)],
        };
        // An election of 20 to 50 minutes and a grace in the registry's range.
        s.chain.set_end(T0 + rng.gen_range(1500..3500));
        s.chain.set_grace(rng.gen_range(150..=600));
        let run = Run {
            mode: Mode::Blob,
            l1: None,
            dkg_sk: None,
            combined: None,
            revealed: false,
            census: Vec::new(),
            no_votes: false,
            rng,
            s,
            dir,
            sh: Arc::new(Shared {
                max_voters: AtomicU64::new(1000),
                ..Default::default()
            }),
            tune,
            node: None,
            first_db: Some(db),
            votes: Vec::new(),
            admitted: Vec::new(),
            kseed: seed * 1_000_000,
            trace: Vec::new(),
            step: 0,
            crash_at: None,
            resume_at: None,
            unfreeze_at: None,
            unhide_at: None,
            unstale_at: None,
            l1_undo: Vec::new(),
            unfail_tx_at: None,
            restarts: 0,
            number_reads: false,
            onchain: None,
        };
        watch_decisions(&run.sh);
        run
    }

    fn log(&mut self, what: String) {
        let t = self.s.chain.time().saturating_sub(T0);
        let notes = std::mem::take(&mut *self.sh.notes.lock().unwrap());
        for n in notes {
            self.trace.push(format!("    {n}"));
        }
        self.trace.push(format!("#{} t+{t}: {what}", self.step));
    }

    fn config(&self) -> Config {
        let t = &self.tune;
        let (bm, mm) = (t.batch_max.to_string(), t.min_mix.to_string());
        let conf = t.confirmations.to_string();
        let (bt, sw, fh) = (
            format!("{}s", t.batch_time),
            format!("{}s", t.solo_wait),
            format!("{}s", t.flush_horizon),
        );
        let datadir = self.dir.path().to_str().unwrap();
        let census = self.s.census_dir.to_str().unwrap();
        let nr = if self.number_reads { "true" } else { "false" };
        // Origin 3: the census contract's RPC (the registry is the fake).
        let rpc = self
            .onchain
            .as_ref()
            .map_or("http://127.0.0.1:8545".to_string(), |o| o.proxy.url.clone());
        let args = [
            "davinci-sequencer",
            "--datadir",
            datadir,
            "--network",
            "custom",
            "--registry",
            "0x0000000000000000000000000000000000000001",
            "--rpc-url",
            &rpc,
            "--blob-source",
            "anvil",
            "--batch-max",
            &bm,
            "--min-mix",
            &mm,
            "--batch-time",
            &bt,
            "--solo-wait",
            &sw,
            "--flush-horizon",
            &fh,
            "--poll-interval",
            "200ms",
            "--settle-margin",
            "0s",
            "--confirmations",
            &conf,
            "--census-dir",
            census,
            "--census-sync-every",
            "0s",
            "--degraded-number-reads",
            nr,
        ];
        Config::parse_args(args.iter().copied()).unwrap()
    }

    async fn start(&mut self) {
        let db = match self.first_db.take() {
            Some(db) => {
                // The creation event confirmed under `--confirmations`.
                let head = self.s.chain.head_block();
                self.s.chain.set_head_block(head + self.tune.confirmations);
                db
            }
            None => reopen_db(self.dir.path()).await,
        };
        let epoch = self.sh.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.sh.crash.store(false, Ordering::SeqCst);
        self.sh.down.store(false, Ordering::SeqCst);
        let (blobs, clock) = self.s.chain.sources();
        let tasks = tokio_util::task::TaskTracker::new();
        let cfg = self.config();
        let l1 = match &self.l1 {
            Some(l1) => {
                self.s.chain.attach_l1(l1);
                let chunk = cfg
                    .max_blobs_per_tx
                    .unwrap_or(davinci_zkvm_sdk::limits::TX_BLOB_CAP);
                // The node's default cap (0.001 ETH): a post held unmined
                // doubles its fees up to it, not without end.
                let da = davinci_sequencer::l1da::L1Da::new(
                    Arc::new(l1.clone()),
                    db.clone(),
                    1_000_000_000_000_000,
                    cfg.poll_interval,
                    chunk,
                )
                .unwrap();
                Some(Arc::new(da))
            }
            None => None,
        };
        let deps = Deps {
            db: db.clone(),
            contracts: Arc::new(ChaosChain {
                inner: self.s.chain.clone(),
                sh: self.sh.clone(),
                epoch,
            }),
            prover: Arc::new(ChaosProver {
                sh: self.sh.clone(),
                epoch,
            }),
            blobs,
            clock,
            grace: self.s.chain.grace_params(),
            tasks: tasks.clone(),
            l1,
        };
        self.onchain_level().await;
        let shutdown = CancellationToken::new();
        let node = spawn_node(cfg, deps, shutdown.clone()).await.unwrap();
        let pid = pid_fr();
        let mut h = None;
        for _ in 0..600 {
            if let Some(x) = node.processes.read().await.get(&pid).cloned() {
                h = Some(x);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let Some(h) = h else {
            self.sh
                .violation("the actor never spawned after a start".into());
            shutdown.cancel();
            return;
        };
        self.node = Some(Live {
            node,
            h,
            db,
            shutdown,
            tasks,
        });
    }

    /// A crash: the process dies with whatever it was doing; nothing it
    /// had not sent reaches the chain.
    async fn crash(&mut self) {
        let Some(live) = self.node.take() else {
            return;
        };
        self.sh.down.store(true, Ordering::SeqCst);
        live.shutdown.cancel();
        live.tasks.close();
        if tokio::time::timeout(Duration::from_secs(60), live.tasks.wait())
            .await
            .is_err()
        {
            self.sh
                .violation("the node's tasks outlived its shutdown by 60 s".into());
        }
        drop(live);
        self.restarts += 1;
    }

    async fn restart(&mut self, why: &str) {
        self.log(format!("crash ({why}) and restart"));
        self.crash().await;
        self.start().await;
    }

    /// An endpoint stuck where it is, block hashes included (unless the
    /// split view serves those from a fresh backend).
    fn freeze(&mut self, on: bool) {
        self.s.chain.freeze_view(on);
        *self.sh.frozen_forks.lock().unwrap() = on.then(|| self.sh.forks.lock().unwrap().len());
    }

    fn h(&self) -> Option<ActorHandle> {
        self.node.as_ref().map(|l| l.h.clone())
    }

    /// Lets the node run `secs` virtual seconds, acting on crash requests.
    async fn run_for(&mut self, secs: u64) {
        // Origin 3 runs in real time (anvil's I/O and a paused clock do not
        // mix): a harness second is a tenth of one until the drain, which
        // waits out the node's own timers (backoffs, the 90 s landing wait).
        let tick = match &self.onchain {
            Some(o) if !o.draining => Duration::from_millis(100),
            _ => Duration::from_secs(1),
        };
        for t in 0..secs {
            tokio::time::sleep(tick).await;
            self.onchain_level().await;
            // Ethereum's slots come whether or not the node asks.
            if let Some(l1) = &self.l1
                && t % 12 == 11
            {
                l1.mine(1);
            }
            if self.sh.crash.swap(false, Ordering::SeqCst) {
                self.restart("hook").await;
            }
        }
    }

    async fn status(&self, h: &ActorHandle, vid: u64) -> Option<VoteStatus> {
        within(&self.sh, "status", h.status(vid))
            .await
            .and_then(|r| r.ok())
            .flatten()
            .map(|v| v.status)
    }

    /// Admitted votes still open, one per voter: the earliest unsettled of
    /// its voter (a peer taking a later one would reorder the voter). Also
    /// the voters with nothing open.
    async fn open_votes(&self) -> (Vec<usize>, Vec<usize>) {
        let Some(h) = self.h() else {
            return (Vec::new(), Vec::new());
        };
        let mut blocked = HashSet::new();
        let mut out = Vec::new();
        for &i in &self.admitted {
            let voter = self.votes[i].voter;
            if blocked.contains(&voter) {
                continue;
            }
            match self.status(&h, self.votes[i].v.pkg.vote_id).await {
                Some(VoteStatus::Settled | VoteStatus::Error) => {}
                _ => {
                    blocked.insert(voter);
                    out.push(i);
                }
            }
        }
        let free = (0..VOTERS).filter(|v| !blocked.contains(v)).collect();
        (out, free)
    }

    fn new_vote(&mut self, voter: usize) -> usize {
        self.kseed += 1;
        let fields = [self.rng.gen_range(0..=5), self.rng.gen_range(0..=5)];
        let v = fake_vote(&self.s.env, voter, &fields, self.kseed);
        self.votes.push(Cast {
            v,
            voter,
            fields,
            admitted: None,
            by_peer: false,
            admitted_step: 0,
            version: 0,
        });
        self.votes.len() - 1
    }

    async fn submit(&mut self, i: usize) {
        let Some(h) = self.h() else {
            return;
        };
        let vid = self.votes[i].v.pkg.vote_id;
        let r = within(&self.sh, "submit", h.submit(self.votes[i].v.clone())).await;
        let what = match r {
            Some(Ok(())) => {
                if self.votes[i].admitted.is_some() {
                    self.sh.violation(format!("vote {vid:#x} admitted twice"));
                } else {
                    self.votes[i].admitted = Some(self.admitted.len());
                    self.votes[i].admitted_step = self.step;
                    self.admitted.push(i);
                }
                "admitted".to_string()
            }
            Some(Err(ActorError::Duplicate(_))) => {
                if self.votes[i].admitted.is_none() && !self.votes[i].by_peer {
                    self.sh
                        .violation(format!("fresh vote {vid:#x} refused as a duplicate"));
                }
                "duplicate".into()
            }
            Some(Err(e)) => format!("refused: {e}"),
            None => "no answer".into(),
        };
        let voter = self.votes[i].voter;
        self.log(format!("submit {vid:#x} (voter {voter}): {what}"));
    }

    /// Whether the registry takes a transition now: READY or ENDED (PAUSED
    /// past the end) and inside `[start, graceEnd)`.
    fn registry_open(&self) -> bool {
        let c = &self.s.chain;
        let t = c.time();
        let status = match c.status() {
            ProcessStatus::Ready | ProcessStatus::Ended => true,
            ProcessStatus::Paused => t >= c.end_time(),
            _ => false,
        };
        status && t >= T0 && t < c.grace_end()
    }

    /// A peer's batch of `idx` on the chain's root; `None` when the registry
    /// would refuse it or a vote is already in the tree.
    fn peer_batch(&mut self, idx: &[usize]) -> Option<davinci_state::PreparedBatch> {
        if idx.is_empty() || !self.registry_open() {
            return None;
        }
        let votes: Vec<VerifiedVote> = idx.iter().map(|&i| self.votes[i].v.clone()).collect();
        let b = self.s.chain.try_peer_batch(&self.s, &votes)?;
        if self.sh.over_cap(&self.s.chain, &b.expected) {
            return None;
        }
        for &i in idx {
            self.votes[i].by_peer = true;
        }
        let ids: Vec<String> = b.vote_ids.iter().map(|v| format!("{v:#x}")).collect();
        self.log(format!(
            "peer batch {} -> {}: {ids:?}",
            short(&b.old_root),
            short(&b.new_root)
        ));
        Some(b)
    }

    /// Up to three peer votes on chain: open votes of ours, fresh ones of
    /// voters with nothing open here, or both.
    async fn peer_votes(&mut self) -> Vec<usize> {
        let (open, free) = self.open_votes().await;
        let mut idx = Vec::new();
        let mut voters = HashSet::new();
        let n = self.rng.gen_range(1..=3);
        for _ in 0..n {
            if !open.is_empty() && self.rng.gen_bool(0.6) {
                let i = open[self.rng.gen_range(0..open.len())];
                if voters.insert(self.votes[i].voter) {
                    idx.push(i);
                }
            } else if !free.is_empty() && self.mode != Mode::Census {
                let voter = free[self.rng.gen_range(0..free.len())];
                if voters.insert(voter) {
                    idx.push(self.new_vote(voter));
                }
            }
        }
        idx
    }

    fn reorg(&mut self) {
        let genesis = genesis_root(&self.s.env.cfg).unwrap();
        let depth = self.rng.gen_range(1..=3);
        if self.s.chain.root() == genesis {
            return;
        }
        let old = self.sh.old_branch.load(Ordering::SeqCst);
        if old {
            let mut st = self.sh.stale_proc.lock().unwrap();
            if st.is_none() {
                *st = Some(self.s.chain.snapshot_process());
                drop(st);
                self.log("reads by number stay on this branch for good".into());
            }
        }
        // The results block follows every transition: a reorg deep enough
        // to take one takes the results too. A finalized process reads the
        // chain no more, so the cursor must see that one (docs/security.md).
        let finalized = self.s.chain.status() == ProcessStatus::Results;
        if finalized {
            self.s.chain.reorg_results();
            self.log("reorg takes the results".into());
        }
        // So does a decryption request, and the combines of it.
        let requested = self.s.chain.dkg_state().requested;
        if requested {
            self.s.chain.reorg_dkg_request();
            self.combined = None;
            self.log("reorg takes the decryption request".into());
        }
        let finalized = finalized || requested;
        let mut removed = Vec::new();
        for _ in 0..depth {
            if self.s.chain.root() == genesis {
                break;
            }
            removed.push(self.s.chain.reorg_out_last_transition());
        }
        let Some(first) = removed.last() else {
            return;
        };
        let seen = finalized || self.rng.gen_bool(0.7);
        if seen {
            self.sh.forks.lock().unwrap().push(first.block);
        }
        // The new branch may carry some of them again, in order.
        let back = if self.rng.gen_bool(0.35) && self.registry_open() {
            self.rng.gen_range(0..removed.len())
        } else {
            0
        };
        // The old-branch runs also re-include everything (no rebuild due).
        let back = if old && self.registry_open() && self.rng.gen_bool(0.5) {
            removed.len()
        } else {
            back
        };
        if back < removed.len() {
            self.sh.partial_reorgs.fetch_add(1, Ordering::Relaxed);
        } else {
            self.sh.full_reorgs.fetch_add(1, Ordering::Relaxed);
        }
        for ev in removed.iter().rev().take(back) {
            self.s.chain.restore_transition(ev.clone());
        }
        self.log(format!(
            "reorg of {} transitions ({} re-included, {})",
            removed.len(),
            back,
            if seen { "seen at the cursor" } else { "unseen" }
        ));
    }

    async fn land_stashed(&mut self) {
        let item = {
            let mut m = self.sh.mempool.lock().unwrap();
            if m.is_empty() {
                None
            } else {
                let i = self.rng.gen_range(0..m.len());
                Some(m.swap_remove(i))
            }
        };
        let Some((snark, blobs)) = item else {
            return;
        };
        if let Ok(p) = BatchPublics::from_public_values(&snark.public_values)
            && self.sh.over_cap(&self.s.chain, &p)
        {
            self.log("a lost-receipt tx reverts MaxVotersReached".into());
            return;
        }
        let r = self
            .s
            .chain
            .submit_transition(&pid31(), &snark, &blobs)
            .await;
        self.log(format!("a lost-receipt tx is mined late: {}", r.is_ok()));
    }

    /// One random action.
    async fn act(&mut self) {
        let end = self.s.chain.end_time();
        let now = self.s.chain.time();
        let mut roll = self.rng.gen_range(0..130);
        // A zero-vote election: no ballots at all; blob-mode faults off the
        // L1 path.
        let votes = matches!(roll, 0..=33 | 110..=111 | 130..);
        if (self.no_votes && votes) || (self.mode == Mode::L1 && matches!(roll, 60..=65)) {
            roll = 84;
        }
        if roll >= 116 {
            return self.mode_act(roll - 116).await;
        }
        match roll {
            // A vote or a revote.
            0..=29 => {
                if let Some(i) = self.cast() {
                    self.submit(i).await;
                }
            }
            // A resend of a vote seen before.
            30..=33 => {
                if !self.votes.is_empty() {
                    let i = self.rng.gen_range(0..self.votes.len());
                    self.submit(i).await;
                }
            }
            // Another sequencer lands first.
            34..=39 => {
                let idx = self.peer_votes().await;
                if let Some(b) = self.peer_batch(&idx) {
                    self.s.chain.settle_externally(&b);
                    self.log(format!("peer lands {} votes", idx.len()));
                }
            }
            // ... in the very block of our next submission.
            40..=43 => {
                let idx = self.peer_votes().await;
                if let Some(b) = self.peer_batch(&idx) {
                    self.s.chain.race_next_submit(b);
                    self.s.chain.advance_time(self.rng.gen_range(50..300));
                    self.log(format!("race armed ({} votes)", idx.len()));
                    self.run_for(40).await;
                    self.s.chain.clear_race();
                }
            }
            44..=46 => {
                let n = self.rng.gen_range(1..=4);
                self.sh.knobs.lock().unwrap().transient += n;
                self.log(format!("prover: {n} transient failures"));
            }
            47..=48 => {
                let n = self.rng.gen_range(1..=2);
                self.sh.knobs.lock().unwrap().permanent += n;
                self.log(format!("prover: {n} failed jobs"));
            }
            49 => {
                self.sh.knobs.lock().unwrap().guest += 1;
                self.log("prover: a guest rejection".into());
            }
            50..=52 => {
                let t = [Tamper::Vk, Tamper::RootC, Tamper::Publics, Tamper::Values]
                    [self.rng.gen_range(0..4)];
                let mut k = self.sh.knobs.lock().unwrap();
                if self.rng.gen_bool(0.5) {
                    k.tamper_once = Some(t);
                } else {
                    k.tamper_sticky = match k.tamper_sticky {
                        Some(_) => None,
                        None => Some(t),
                    };
                }
                drop(k);
                self.log(format!("prover: tampers ({t:?})"));
            }
            53..=55 => {
                let s = [0, 0, 20, 90, 400][self.rng.gen_range(0..5)];
                self.sh.knobs.lock().unwrap().slow_secs = s;
                self.log(format!("prover: proofs take {s} s"));
            }
            56 => {
                self.sh.knobs.lock().unwrap().crash_on_prove = true;
                self.log("crash at the next proof".into());
            }
            57 => {
                self.sh.knobs.lock().unwrap().crash_after_land = true;
                self.log("crash right after the next landing".into());
            }
            58..=59 => {
                let n = self.rng.gen_range(1..=2);
                self.s.chain.fail_submits(n);
                self.log(format!("chain: {n} failed sends"));
            }
            60..=61 => {
                self.s.chain.timeout_submits(1);
                self.log("chain: the next landing's receipt is lost".into());
            }
            62..=63 => {
                self.sh.knobs.lock().unwrap().stash += 1;
                self.log("chain: the next send waits in the mempool".into());
            }
            64..=65 => self.land_stashed().await,
            66 => {
                let name = ["InvalidTimeBounds", "InvalidStatus", "InvalidStateRoot"]
                    [self.rng.gen_range(0..3)];
                self.s.chain.revert_simulates(name, 1);
                self.log(format!("chain: the next simulation reverts {name}"));
            }
            67..=68 => {
                let n = self.rng.gen_range(1..=4);
                self.s.chain.fail_blobs(n);
                self.log(format!("chain: {n} blob fetches fail"));
            }
            69 => {
                let n = self.rng.gen_range(1..=3);
                if self.rng.gen_bool(0.5) {
                    self.s.chain.fail_process(n);
                } else {
                    self.s.chain.lag_process(n);
                }
                self.log(format!("chain: {n} bad process reads"));
            }
            70..=74 => self.reorg(),
            75..=76 => {
                let n = self.rng.gen_range(1..=3);
                if let Some(l) = &self.node {
                    l.db.fail_next_writes(n);
                }
                self.log(format!("db: {n} record writes fail"));
            }
            77..=78 => {
                let n = self.rng.gen_range(1..=3);
                if let Some(l) = &self.node {
                    l.db.fail_next_arbo_writes(n);
                }
                self.log(format!("db: {n} tree writes fail"));
            }
            79 => {
                let Some(db) = self.node.as_ref().map(|l| l.db.clone()) else {
                    return;
                };
                if self.rng.gen_bool(0.5) {
                    db.fail_writes_after_history();
                    self.crash_at = Some(self.step + self.rng.gen_range(1..=2));
                    drop(db);
                    self.log("db: dies right after the next history write".into());
                } else {
                    db.fail_transition_writes(true);
                    self.unfail_tx_at = Some(self.step + self.rng.gen_range(1..=3));
                    drop(db);
                    self.log("db: transition records fail".into());
                }
            }
            80..=83 => self.restart("random").await,
            84..=91 => {
                let dt = self.rng.gen_range(20..200);
                self.s.chain.advance_time(dt);
                self.log(format!("time +{dt}"));
            }
            92 => {
                let dt = self.rng.gen_range(200..800);
                self.s.chain.advance_time(dt);
                self.log(format!("time jumps +{dt}"));
            }
            93 => {
                if now < end && self.s.chain.status() == ProcessStatus::Ready {
                    self.s.chain.set_status(ProcessStatus::Ended);
                    self.log("organizer ends the election".into());
                }
            }
            94 => {
                let new_end = now + self.rng.gen_range(60..400);
                if new_end < end && self.s.chain.status() == ProcessStatus::Ready {
                    self.s.chain.set_end(new_end);
                    self.log(format!("end moved to t+{}", new_end - T0));
                }
            }
            95 => {
                if now < end {
                    let g = self.rng.gen_range(150..=600);
                    self.s.chain.set_grace(g);
                    self.log(format!("grace set to {g}"));
                }
            }
            96 => {
                if now < end && self.s.chain.status() == ProcessStatus::Ready {
                    self.s.chain.set_status(ProcessStatus::Paused);
                    self.resume_at = Some(self.step + self.rng.gen_range(1..=3));
                    self.log("organizer pauses".into());
                }
            }
            97 => {
                let n = self.rng.gen_range(1..=2);
                self.sh.knobs.lock().unwrap().results_transient += n;
                self.log(format!("prover: {n} results failures"));
            }
            // An endpoint stuck behind the head, for a step or two.
            100..=101 => {
                self.freeze(true);
                self.unfreeze_at = Some(self.step + self.rng.gen_range(1..=2));
                self.log("chain: reads freeze".into());
            }
            // The next block's events show late (a lagging log index).
            102..=103 => {
                let b = self.s.chain.head_block() + 1;
                self.s.chain.hide_block(Some(b));
                self.unhide_at = Some(self.step + self.rng.gen_range(1..=3));
                self.log(format!("chain: events of block {b} hidden"));
            }
            104 => {
                self.s.chain.vanish_next_results();
                self.log("chain: the next results tx never lands".into());
            }
            105 => {
                let name = ["InvalidStateRoot", "GraceOpen", "InvalidTimeBounds"]
                    [self.rng.gen_range(0..3)];
                self.s.chain.revert_results_once(name);
                self.log(format!("chain: the next results tx reverts {name}"));
            }
            106..=107 => {
                if now < end {
                    let cap = if self.rng.gen_bool(0.6) {
                        self.s.chain.voters() + self.rng.gen_range(0..=3)
                    } else {
                        1000
                    };
                    self.sh.max_voters.store(cap, Ordering::SeqCst);
                    self.s.chain.change_max_voters(cap);
                    self.log(format!("organizer sets max voters {cap}"));
                }
            }
            // A voter casts twice in one second.
            110..=111 => {
                if let Some(a) = self.cast() {
                    let voter = self.votes[a].voter;
                    if let Some(b) = self.cast_by(voter) {
                        self.submit(a).await;
                        self.submit(b).await;
                    }
                }
            }
            // The disk dies part way through the writes to come; a crash.
            112 => {
                let skip = self.rng.gen_range(0..6);
                if let Some(l) = &self.node {
                    l.db.fail_writes_after(skip, u32::MAX);
                }
                self.crash_at = Some(self.step + 1);
                self.log(format!("db: dies after {skip} more writes"));
            }
            // Reads by number from a backend stuck on what it last saw.
            113..=114 if !self.sh.old_branch.load(Ordering::SeqCst) => {
                if let Ok(p) = self.s.chain.process(&pid31()).await {
                    *self.sh.stale_proc.lock().unwrap() = Some(p);
                    self.unstale_at = Some(self.step + self.rng.gen_range(1..=3));
                    self.log("chain: reads by number go stale".into());
                }
            }
            _ => {
                // An unsent vote another node settled, resent here.
                let unsent: Vec<usize> = (0..self.votes.len())
                    .filter(|&i| self.votes[i].by_peer && self.votes[i].admitted.is_none())
                    .collect();
                if !unsent.is_empty() {
                    let i = unsent[self.rng.gen_range(0..unsent.len())];
                    self.submit(i).await;
                }
            }
        }
    }

    /// A vote by a random voter the census admits now; `None` while the
    /// current census does not load (the API answers busy).
    fn cast(&mut self) -> Option<usize> {
        if self.mode == Mode::Onchain {
            let n = self.onchain_indexed();
            if n == 0 {
                return None;
            }
            let members = &self.onchain.as_ref()?.members;
            let voter = members[self.rng.gen_range(0..n.min(members.len()))];
            return Some(self.new_vote(voter));
        }
        if self.mode != Mode::Census {
            let voter = self.rng.gen_range(0..VOTERS);
            return Some(self.new_vote(voter));
        }
        let v = self.census.last()?;
        v.imt.as_ref()?;
        let ones: Vec<usize> = v
            .members
            .iter()
            .filter(|(_, w)| *w == 1)
            .map(|(m, _)| *m)
            .collect();
        if ones.is_empty() {
            return None;
        }
        let voter = ones[self.rng.gen_range(0..ones.len())];
        self.cast_by(voter)
    }

    /// A vote by `voter`, under the current census in census mode.
    fn cast_by(&mut self, voter: usize) -> Option<usize> {
        if self.mode == Mode::Onchain {
            // The API checks membership at the node's index: only members
            // it has synced get a proof.
            let n = self.onchain_indexed();
            let pos = self
                .onchain
                .as_ref()?
                .members
                .iter()
                .position(|m| *m == voter)?;
            return (pos < n).then(|| self.new_vote(voter));
        }
        if self.mode != Mode::Census {
            return Some(self.new_vote(voter));
        }
        let version = self.census.len().checked_sub(1)?;
        let v = self.census.last()?;
        let imt = v.imt.as_ref()?;
        let leaf = v.members.iter().position(|(m, w)| *m == voter && *w == 1)?;
        // The API checks the proof against the node's census root: a client
        // with a proof at a root the node has not seen yet is refused.
        let rec = self.node.as_ref()?.db.process(&pid_fr()).ok()??;
        if rec.onchain.census.root != v.root {
            return None;
        }
        self.kseed += 1;
        let fields = [self.rng.gen_range(0..=5), self.rng.gen_range(0..=5)];
        let vote = census_vote(&self.s.env, imt, leaf, voter, &fields, self.kseed);
        self.votes.push(Cast {
            v: vote,
            voter,
            fields,
            admitted: None,
            by_peer: false,
            admitted_step: 0,
            version,
        });
        Some(self.votes.len() - 1)
    }

    /// Origin 3: members the node's index holds, in insertion order.
    fn onchain_indexed(&self) -> usize {
        let (Some(o), Some(l)) = (&self.onchain, &self.node) else {
            return 0;
        };
        l.node
            .onchain
            .latest(&o.ch.census.into_array())
            .map_or(0, |(_, size, _)| size as usize)
    }

    /// Origin 3: the census contract and the registry share one chain in
    /// production: the anvil head is kept at the registry's.
    async fn onchain_level(&mut self) {
        use alloy::providers::Provider;
        let Some(o) = &self.onchain else {
            return;
        };
        let (ah, fh) = (o.ch.head().await, self.s.chain.head_block());
        if ah < fh {
            let _: serde_json::Value =
                o.ch.owner
                    .raw_request("anvil_mine".into(), (format!("0x{:x}", fh - ah),))
                    .await
                    .unwrap();
        }
        while self.s.chain.head_block() < ah {
            self.s.chain.advance_time(0);
        }
    }

    async fn onchain_act(&mut self, roll: usize) {
        let later = self.step + self.rng.gen_range(1..=3);
        let o = self.onchain.as_ref().unwrap();
        match roll {
            0..=3 => {
                let Some(v) = (0..VOTERS).find(|v| !o.members.contains(v)) else {
                    return;
                };
                o.ch.add(&[(voter_address(v), 1)]).await;
                let (root, _) = o.ch.state().await;
                let o = self.onchain.as_mut().unwrap();
                o.members.push(v);
                let n = o.members.len();
                o.roots.push((root, n));
                self.onchain_level().await;
                self.log(format!("census contract adds voter {v}"));
            }
            4 => {
                o.proxy.knobs.down.store(true, Ordering::SeqCst);
                self.l1_undo.push((later, "census-up"));
                self.log("census rpc: down".into());
            }
            5 => {
                let lag = self.rng.gen_range(1..=4);
                o.proxy.knobs.lag.store(lag, Ordering::SeqCst);
                self.l1_undo.push((later, "census-lag"));
                self.log(format!("census rpc: a backend {lag} blocks behind"));
            }
            6 => {
                o.proxy.knobs.max_range.store(2, Ordering::SeqCst);
                self.l1_undo.push((later, "census-range"));
                self.log("census rpc: log ranges capped at 2 blocks".into());
            }
            _ => {
                let dt = self.rng.gen_range(5..60);
                self.s.chain.advance_time(dt);
            }
        }
    }

    /// Origin 3: every census fault off.
    fn onchain_heal(&self) {
        if let Some(o) = &self.onchain {
            let k = &o.proxy.knobs;
            k.down.store(false, Ordering::SeqCst);
            k.lag.store(0, Ordering::SeqCst);
            k.max_range.store(0, Ordering::SeqCst);
        }
    }

    /// An action of the run's mode.
    async fn mode_act(&mut self, roll: usize) {
        match self.mode {
            Mode::L1 => self.l1_act(roll).await,
            Mode::Onchain => self.onchain_act(roll).await,
            Mode::Census if roll < 6 => self.census_update(false),
            Mode::Census if roll < 8 => self.census_update(true),
            _ => {
                let dt = self.rng.gen_range(5..60);
                self.s.chain.advance_time(dt);
            }
        }
    }

    async fn l1_act(&mut self, roll: usize) {
        let l1 = self.l1.clone().unwrap();
        let later = self.step + self.rng.gen_range(1..=3);
        match roll {
            0 => {
                let d = self.rng.gen_range(1..=4);
                l1.reorg(d);
                self.log(format!("ethereum: reorg of {d} blocks"));
            }
            1 => {
                l1.hold(true);
                self.l1_undo.push((later, "hold"));
                self.log("ethereum: posts stop mining".into());
            }
            2 => {
                l1.drop_next(1);
                self.log("ethereum: the next post is lost".into());
            }
            3 => {
                l1.refuse_next(self.rng.gen_range(1..=2));
                self.log("ethereum: posts refused".into());
            }
            4 => {
                let lag = self.rng.gen_range(0..10);
                l1.set_lag(lag);
                self.log(format!("op: origin lags {lag} blocks"));
            }
            5 => {
                let n = l1.head_number().saturating_sub(self.rng.gen_range(0..5));
                l1.pin_origin(Some(n));
                self.l1_undo.push((later, "pin"));
                self.log(format!("op: origin stuck at {n}"));
            }
            6 => {
                let secs = self.rng.gen_range(30..150);
                l1.fail_proofs_for(Duration::from_secs(secs));
                self.log(format!("ethereum: proofs fail for {secs} s"));
            }
            7 => {
                let f = [ProofFault::Data, ProofFault::BrokenChain][self.rng.gen_range(0..2)];
                l1.fault_proofs(Some(f));
                self.l1_undo.push((later, "fault"));
                self.log(format!("ethereum: proofs fault ({f:?})"));
            }
            8 => {
                self.s.chain.vanish_next_l1_submit();
                self.log("op: the next settlement is reorged out".into());
            }
            9 => {
                self.s.chain.gate_l1_submits(true);
                self.l1_undo.push((later, "gate"));
                self.log("op: settlement receipts wait".into());
            }
            10 => {
                let mut m = l1.market();
                m.base_fee *= self.rng.gen_range(2..20);
                m.blob_base_fee *= self.rng.gen_range(2..20);
                l1.set_market(m);
                self.l1_undo.push((later, "fees"));
                self.log("ethereum: fees spike".into());
            }
            11 => {
                // A crash while a blob transaction waits in the pool.
                l1.hold(true);
                self.run_for(30).await;
                self.restart("with a post pending").await;
                self.l1_undo.push((later, "hold"));
            }
            _ => {
                let n = self.rng.gen_range(1..10);
                l1.mine(n);
            }
        }
    }

    /// The organizer announces a new census: members added, removed and
    /// reweighted; `bad`: one nothing serves (it never loads).
    fn census_update(&mut self, bad: bool) {
        let Some(last) = self.census.iter().rev().find(|v| v.imt.is_some()) else {
            return;
        };
        let mut members = last.members.clone();
        for _ in 0..self.rng.gen_range(1..=3) {
            match self.rng.gen_range(0..3) {
                0 => {
                    let free: Vec<usize> = (0..VOTERS)
                        .filter(|v| members.iter().all(|(m, _)| m != v))
                        .collect();
                    if !free.is_empty() {
                        members.push((free[self.rng.gen_range(0..free.len())], 1));
                    }
                }
                1 if members.len() > 2 => {
                    let i = self.rng.gen_range(0..members.len());
                    members.remove(i);
                }
                _ => {
                    let i = self.rng.gen_range(0..members.len());
                    members[i].1 = 3 - members[i].1;
                }
            }
        }
        let k = self.census.len();
        let dir = self.s.census_dir.join(format!("v{k}"));
        std::fs::create_dir_all(&dir).unwrap();
        let (uri, imt) = write_members(&dir, &members);
        let root = imt.root();
        let uri = if bad {
            std::fs::remove_file(dir.join("census.json")).unwrap();
            uri
        } else {
            uri
        };
        self.s.chain.set_census(root, uri);
        self.s.chain.set_census_root_check(Some(census_le(&root)));
        self.census.push(Version {
            members,
            root,
            imt: (!bad).then_some(imt),
        });
        self.log(format!(
            "organizer re-roots the census{}",
            if bad { " (nothing serves it)" } else { "" }
        ));
    }

    /// The committee: once a request landed (and, locked, the organizer
    /// revealed), it decrypts the requested accumulator, honestly.
    fn committee(&mut self) {
        let (Some(sk), Mode::Dkg(m)) = (self.dkg_sk, self.mode) else {
            return;
        };
        if m == KeyMode::DkgLocked && !self.revealed {
            return;
        }
        let Some((acc, _)) = self.s.chain.dkg_request() else {
            return;
        };
        if self.combined == Some(acc) {
            return;
        }
        let max = (VOTERS as u64) * 5;
        let point = |x: &[u8; 32], y: &[u8; 32]| Point::from_be(x, y).unwrap();
        let vals: Vec<u64> = (0..NF as usize)
            .map(|f| {
                let c = Ciphertext {
                    c1: point(&acc[4 * f], &acc[4 * f + 1]),
                    c2: point(&acc[4 * f + 2], &acc[4 * f + 3]),
                };
                decrypt(&sk, &c, max).unwrap_or(u64::MAX)
            })
            .collect();
        self.s.chain.dkg_combine(vals.clone());
        self.combined = Some(acc);
        self.log(format!("committee combines {vals:?}"));
    }

    /// Waits until the monitor's cursor reached the confirmed head; the
    /// block a reorg it is to see must fork at (at most the cursor).
    async fn cursor_at_head(&mut self) -> u64 {
        let head = self.s.chain.head_block();
        let want = head.saturating_sub(self.tune.confirmations);
        let Some(db) = self.node.as_ref().map(|l| l.db.clone()) else {
            return want;
        };
        for _ in 0..60 {
            if let Ok(Some(c)) = db.meta_u64("monitor_last_block")
                && c >= want
            {
                return c.min(head);
            }
            self.run_for(1).await;
        }
        want
    }

    /// After the grace end: the committee, the reveal and the gate, races
    /// with other nodes, and reorgs of the request and of the results.
    async fn dkg_act(&mut self, m: KeyMode) {
        match self.rng.gen_range(0..10) {
            0 | 1 => self.committee(),
            2 => {
                self.revealed = true;
                self.log("organizer reveals its key".into());
            }
            3 if self.s.chain.dkg_state().requested => {
                let b = self.cursor_at_head().await;
                if self.s.chain.status() == ProcessStatus::Results {
                    self.s.chain.reorg_results();
                }
                self.s.chain.reorg_dkg_request();
                self.combined = None;
                self.sh.forks.lock().unwrap().push(b);
                self.log("reorg takes the decryption request".into());
            }
            4 if self.s.chain.status() == ProcessStatus::Results => {
                let b = self.cursor_at_head().await;
                self.s.chain.reorg_results();
                self.sh.forks.lock().unwrap().push(b);
                self.log("reorg takes the results".into());
            }
            5 => {
                self.s.chain.race_dkg_request();
                self.log("another node's request races ours".into());
            }
            6 => {
                if self.rng.gen_bool(0.5) {
                    self.s.chain.lose_dkg_finalize();
                } else {
                    self.s.chain.race_dkg_finalize_after_ready();
                }
                self.log("another node's finalize races ours".into());
            }
            7 => self.restart("in the results").await,
            8 if m == KeyMode::Council => {
                let open = self.rng.gen_bool(0.6);
                self.s.chain.set_council_gate(open);
                self.log(format!(
                    "council gate {}",
                    if open { "opens" } else { "closes" }
                ));
            }
            _ => self.s.chain.advance_time(self.rng.gen_range(1..30)),
        }
    }

    async fn scheduled(&mut self) {
        let step = self.step;
        let due: Vec<&'static str> = self
            .l1_undo
            .iter()
            .filter(|(at, _)| *at == step)
            .map(|(_, w)| *w)
            .collect();
        self.l1_undo.retain(|(at, _)| *at != step);
        if let Some(o) = &self.onchain {
            for what in &due {
                let k = &o.proxy.knobs;
                match *what {
                    "census-up" => k.down.store(false, Ordering::SeqCst),
                    "census-lag" => k.lag.store(0, Ordering::SeqCst),
                    "census-range" => k.max_range.store(0, Ordering::SeqCst),
                    _ => continue,
                }
            }
            for what in &due {
                self.log(format!("undo {what}"));
            }
        }
        if let Some(l1) = self.l1.clone() {
            for what in due {
                match what {
                    "hold" => l1.hold(false),
                    "pin" => l1.pin_origin(None),
                    "fault" => l1.fault_proofs(None),
                    "gate" => self.s.chain.gate_l1_submits(false),
                    _ => l1.set_market(FakeL1::new().market()),
                }
                self.log(format!("undo {what}"));
            }
        }
        if self.unstale_at == Some(self.step) {
            self.unstale_at = None;
            *self.sh.stale_proc.lock().unwrap() = None;
            self.log("chain: reads by number fresh again".into());
        }
        if self.unfreeze_at == Some(self.step) {
            self.unfreeze_at = None;
            self.freeze(false);
            self.log("chain: reads thaw".into());
        }
        if self.unhide_at == Some(self.step) {
            self.unhide_at = None;
            self.s.chain.hide_block(None);
            self.log("chain: hidden events show".into());
        }
        if self.crash_at == Some(self.step) {
            self.crash_at = None;
            self.restart("after a storage crash").await;
        }
        if self.unfail_tx_at == Some(self.step) {
            self.unfail_tx_at = None;
            if let Some(l) = &self.node {
                l.db.fail_transition_writes(false);
            }
            self.log("db: transition records write again".into());
        }
        if self.resume_at == Some(self.step) {
            self.resume_at = None;
            if self.s.chain.status() == ProcessStatus::Paused {
                self.s.chain.set_status(ProcessStatus::Ready);
                self.log("organizer resumes".into());
            }
        }
    }

    /// Faults off, then everything the node can settle settles, the window
    /// closes and the results go in.
    async fn drain(&mut self) {
        *self.sh.knobs.lock().unwrap() = Knobs::default();
        self.s.chain.fail_submits(0);
        self.s.chain.timeout_submits(0);
        self.s.chain.fail_blobs(0);
        self.s.chain.fail_process(0);
        self.s.chain.lag_process(0);
        self.s.chain.revert_simulates("InvalidStatus", 0);
        self.s.chain.clear_race();
        self.freeze(false);
        if !self.sh.old_branch.load(Ordering::SeqCst) {
            *self.sh.stale_proc.lock().unwrap() = None;
        }
        self.s.chain.hide_block(None);
        if let Some(l1) = &self.l1 {
            l1.hold(false);
            l1.pin_origin(None);
            l1.set_lag(0);
            l1.fault_proofs(None);
            l1.refuse_next(0);
            l1.drop_next(0);
            l1.limit_mined(None);
            l1.set_market(FakeL1::new().market());
            self.s.chain.gate_l1_submits(false);
        }
        self.l1_undo.clear();
        self.onchain_heal();
        if let Some(o) = &mut self.onchain {
            o.draining = true;
        }
        let land = self.rng.gen_bool(0.5);
        if land {
            while !self.sh.mempool.lock().unwrap().is_empty() {
                self.land_stashed().await;
            }
        }
        self.sh.mempool.lock().unwrap().clear();
        if let Some(l) = &self.node {
            l.db.fail_next_writes(0);
            l.db.fail_next_arbo_writes(0);
            l.db.fail_transition_writes(false);
        }
        if self.crash_at.take().is_some() || self.node.is_none() {
            self.restart("before the drain").await;
        }
        if self.s.chain.status() == ProcessStatus::Paused {
            self.s.chain.set_status(ProcessStatus::Ready);
        }
        if self.s.chain.time() < self.s.chain.end_time()
            && self.s.chain.status() == ProcessStatus::Ready
        {
            self.s.chain.set_status(ProcessStatus::Ended);
        }
        self.log("drain: faults off, election ended".into());
        // The window stays open while the chain clock stands still.
        for _ in 0..40 {
            self.run_for(30).await;
            let Some(h) = self.h() else { break };
            let Some(Ok(snap)) = within(&self.sh, "snapshot", h.snapshot()).await else {
                break;
            };
            if snap.pending == 0 && !snap.in_flight && snap.synced {
                break;
            }
        }
        self.s.chain.pass_grace();
        self.log("drain: grace passed".into());
        if let Mode::Dkg(m) = self.mode {
            for _ in 0..self.rng.gen_range(4..12) {
                self.dkg_act(m).await;
                let wait = self.rng.gen_range(5..40);
                self.run_for(wait).await;
            }
            self.s.chain.set_council_gate(true);
            self.revealed = true;
            self.log("drain: gate open, key revealed".into());
        }
        // Quiet for two rounds in a row: an event routed after the first
        // (a peer's finalize) has its anchored check by the second.
        let mut quiet = 0;
        for _ in 0..60 {
            self.committee();
            self.run_for(10).await;
            self.s.chain.advance_time(1);
            let Some(h) = self.h() else { break };
            let Some(Ok(snap)) = within(&self.sh, "snapshot", h.snapshot()).await else {
                break;
            };
            let done = snap.local == LocalStatus::Finalized && snap.synced && snap.pending == 0;
            quiet = if done { quiet + 1 } else { 0 };
            if quiet == 2 {
                break;
            }
        }
    }

    /// Every invariant the chain and the node's store can witness.
    async fn check(&mut self) -> Vec<String> {
        let mut bad: Vec<String> = std::mem::take(&mut *self.sh.violations.lock().unwrap());
        let me = std::thread::current().name().map(str::to_string);
        PANICS.lock().unwrap().retain(|(t, p)| {
            if *t == me {
                bad.push(format!("panic: {p}"));
            }
            *t != me
        });
        // What the last rounds set off (a peer's finalize, a combine, an
        // event the fakes land on a read) routes with no harness action:
        // quiescent once the anchor passes it.
        let mut quiet = 0;
        for _ in 0..30 {
            let Some(h) = self.h() else { break };
            let done = matches!(within(&self.sh, "snapshot", h.snapshot()).await,
                Some(Ok(snap)) if snap.synced
                    && !snap.in_flight
                    && snap.pending == 0
                    && snap.local == LocalStatus::Finalized);
            quiet = if done { quiet + 1 } else { 0 };
            if quiet == 3 {
                break;
            }
            self.run_for(5).await;
            self.s.chain.advance_time(1);
        }
        let Some(live) = &self.node else {
            bad.push("node down at the check".into());
            return bad;
        };
        let (h, db) = (live.h.clone(), live.db.clone());
        let pid = pid_fr();
        let chain_root = self.s.chain.root();
        match within(&self.sh, "snapshot", h.snapshot()).await {
            Some(Ok(snap)) => {
                if snap.root != chain_root {
                    bad.push(format!(
                        "node root {} is not the chain's {}",
                        short(&snap.root),
                        short(&chain_root)
                    ));
                }
                if !snap.synced || snap.in_flight || snap.pending != 0 {
                    bad.push(format!(
                        "not quiescent: synced {} in flight {} pending {}",
                        snap.synced, snap.in_flight, snap.pending
                    ));
                }
                if snap.local != LocalStatus::Finalized {
                    bad.push(format!("not finalized: {:?}", snap.local));
                }
            }
            _ => bad.push("no snapshot".into()),
        }

        // The canonical chain, from genesis by roots.
        let genesis = genesis_root(&self.s.env.cfg).unwrap();
        let events = self.s.chain.events(0, u64::MAX).await.unwrap();
        // Every batch published, in order: the fake registry's sends, with
        // our L1 settlements (which it does not list) where they went out.
        let mut sent = self.s.chain.sent_batches();
        for (at, tx, blobs) in self.sh.l1_sent.lock().unwrap().iter().rev() {
            sent.insert((*at).min(sent.len()), (*tx, blobs.clone(), true));
        }
        let mut canon: Vec<(B256, davinci_zkvm_sdk::blob::TransitionData, bool, [u8; 32])> =
            Vec::new();
        let mut cur = genesis;
        while let Some((tx, new_root, n_blobs, l1b)) = events.iter().find_map(|e| match &e.kind {
            EventKind::StateTransitioned {
                old_root,
                new_root,
                n_blobs,
                l1,
                ..
            } if *old_root == cur => Some((e.tx_hash, *new_root, *n_blobs, l1.clone())),
            _ => None,
        }) {
            let found = sent
                .iter()
                .rev()
                .find(|(t, _, _)| *t == tx)
                .map(|(_, b, own)| (b.clone(), *own));
            // L1-blob mode: the blobs the event names, from Ethereum.
            let found = found.or_else(|| {
                let (l1, l1b) = (self.l1.as_ref()?, l1b.as_ref()?);
                let b: Option<Vec<Blob>> = l1b.vhs.iter().map(|vh| l1.blob(vh)).collect();
                Some((b?, tx[0] == 0))
            });
            let Some((blobs, own)) = found else {
                bad.push(format!("no blobs for canonical tx {tx}"));
                break;
            };
            let n = (n_blobs as usize).min(blobs.len());
            let td = decode_blobs(&blobs[..n], NF).unwrap();
            canon.push((tx, td, own, new_root));
            cur = new_root;
        }
        if cur != chain_root {
            bad.push("the fake chain's events do not reach its root".into());
        }

        // Settled exactly once, in admission order per voter; the tally.
        let by_vid: std::collections::HashMap<u64, usize> = self
            .votes
            .iter()
            .enumerate()
            .map(|(i, c)| (c.v.pkg.vote_id, i))
            .collect();
        let mut at: std::collections::HashMap<u64, usize> = Default::default();
        let mut last: std::collections::HashMap<usize, usize> = Default::default();
        for (n, (tx, td, _, _)) in canon.iter().enumerate() {
            let mut voters = HashSet::new();
            for vid in &td.vote_ids {
                let Some(&i) = by_vid.get(vid) else {
                    bad.push(format!("unknown vote {vid:#x} on chain"));
                    continue;
                };
                if at.insert(*vid, n).is_some() {
                    bad.push(format!("vote {vid:#x} counted twice (again in {tx})"));
                }
                if !voters.insert(self.votes[i].voter) {
                    bad.push(format!(
                        "one transition writes voter {} twice",
                        self.votes[i].voter
                    ));
                }
                last.insert(self.votes[i].voter, i);
            }
        }
        let mut tally = [0u64; 2];
        for i in last.values() {
            tally[0] += self.votes[*i].fields[0];
            tally[1] += self.votes[*i].fields[1];
        }
        let results = self.s.chain.results();
        // The committee publishes one entry per field, the results proof 16.
        let mut want = match self.mode {
            Mode::Dkg(_) => vec![0u64; NF as usize],
            _ => vec![0u64; 16],
        };
        want[..2].copy_from_slice(&tally);
        if results != want {
            bad.push(format!(
                "tally {results:?}, the final ballots sum to {tally:?}"
            ));
        }
        for voter in 0..VOTERS {
            let mut prev: Option<(usize, u64)> = None;
            for &i in &self.admitted {
                if self.votes[i].voter != voter {
                    continue;
                }
                let vid = self.votes[i].v.pkg.vote_id;
                if let Some(&n) = at.get(&vid) {
                    if let Some((p, pvid)) = prev
                        && p >= n
                    {
                        bad.push(format!(
                            "voter {voter}: {vid:#x} settled at transition {n}, its earlier {pvid:#x} at {p}"
                        ));
                    }
                    prev = Some((n, vid));
                }
            }
        }

        // Every admitted vote ends settled (and on chain) or errored with a
        // reason (and off it).
        let stored = db.votes(&pid).unwrap();
        let admitted: HashSet<u64> = self
            .admitted
            .iter()
            .map(|&i| self.votes[i].v.pkg.vote_id)
            .collect();
        for sv in &stored {
            if !admitted.contains(&sv.vote_id) {
                bad.push(format!("vote {:#x} stored but never admitted", sv.vote_id));
            }
        }
        let mut errors: std::collections::BTreeMap<String, usize> = Default::default();
        for &i in &self.admitted {
            let vid = self.votes[i].v.pkg.vote_id;
            let on_chain = at.contains_key(&vid);
            match stored.iter().find(|v| v.vote_id == vid) {
                None => bad.push(format!("admitted vote {vid:#x} lost from the store")),
                Some(sv) => match sv.status {
                    VoteStatus::Settled if on_chain => {}
                    VoteStatus::Settled => {
                        bad.push(format!("vote {vid:#x} settled but not on chain"))
                    }
                    VoteStatus::Error if on_chain => bad.push(format!(
                        "vote {vid:#x} errored ({:?}) but counted on chain",
                        sv.error
                    )),
                    VoteStatus::Error => {
                        let e = sv.error.clone().unwrap_or_default();
                        if e.is_empty() {
                            bad.push(format!("vote {vid:#x} errored without a reason"));
                        }
                        let key = e.split(" (").next().unwrap_or("").to_string();
                        *errors.entry(key).or_default() += 1;
                    }
                    s => bad.push(format!("vote {vid:#x} left {s:?}")),
                },
            }
        }
        if !db.pending(&pid).unwrap().is_empty() {
            bad.push("the stored queue is not empty".into());
        }
        // Origin 2: a vote errored for a census change only when its voter's
        // leaf changed in a census announced after the one its proof is at (a
        // resubmit after a re-root may meet the newer one); a vote counts
        // only under a census that holds its voter at weight 1.
        if self.mode == Mode::Census {
            let leaf_ok =
                |v: &Version, voter: usize| v.members.iter().any(|(m, w)| *m == voter && *w == 1);
            for &i in &self.admitted {
                let vid = self.votes[i].v.pkg.vote_id;
                let voter = self.votes[i].voter;
                let Some(sv) = stored.iter().find(|v| v.vote_id == vid) else {
                    continue;
                };
                if sv.error.as_deref() == Some("census changed, recast") {
                    let after = self.votes[i].version + 1;
                    if !self.census.iter().skip(after).any(|v| !leaf_ok(v, voter)) {
                        bad.push(format!(
                            "vote {vid:#x} of voter {voter} errored for a census change it did not undergo"
                        ));
                    }
                }
            }
            let roots = self.sh.census_roots.lock().unwrap().clone();
            for (tx, td, own, root) in &canon {
                if !own {
                    continue;
                }
                let Some(cr) = roots.get(root) else {
                    continue;
                };
                let Some(v) = self.census.iter().find(|v| census_le(&v.root) == *cr) else {
                    bad.push(format!("landing {tx} under a census root never announced"));
                    continue;
                };
                for vid in &td.vote_ids {
                    if let Some(&i) = by_vid.get(vid)
                        && !leaf_ok(v, self.votes[i].voter)
                    {
                        bad.push(format!(
                            "vote {vid:#x} counted in {tx} under a census without its voter"
                        ));
                    }
                }
            }
        }

        // L1-blob mode: nothing of ours left pending in the pool (a stuck
        // nonce holds every later post of the account), and every post's
        // batch in the coverage history. (Two posts at one pending nonce are
        // the pool's to refuse; a record of a post that vanished is the next
        // post's to clear.)
        let mut posted: Vec<(B256, Vec<Blob>)> = Vec::new();
        if let Some(l1) = &self.l1 {
            // Ethereum mines on whether or not the node polls it.
            l1.mine(4);
            let me = Address::repeat_byte(0x1a);
            let mine: Vec<L1Send> = l1.sends().into_iter().filter(|x| x.from == me).collect();
            if l1.pending_of(me) != 0 {
                let mined: Vec<u64> = l1.mined_by(me).iter().map(|(n, _, _)| *n).collect();
                bad.push(format!(
                    "posts of ours left pending on Ethereum at nonces {:?} (mined {mined:?})",
                    l1.pending_nonces(me)
                ));
            }
            for send in &mine {
                if let Some(vhs) = l1.sent_vhs(&send.hash)
                    && let Some(b) = vhs
                        .iter()
                        .map(|vh| l1.blob(vh))
                        .collect::<Option<Vec<Blob>>>()
                {
                    posted.push((send.hash, b));
                }
            }
        }

        // Bounded: no tree snapshot outlives a message (it would pin every
        // later commit's pages in the shared file).
        tokio::time::sleep(Duration::from_secs(1)).await;
        if db.arbo_snapshots_open() != 0 {
            bad.push(format!(
                "{} tree snapshots left open",
                db.arbo_snapshots_open()
            ));
        }

        // The coverage history and the re-seal rule.
        let history = db.coverage_history(&pid).unwrap();
        let entries: HashSet<(BTreeSet<u64>, BTreeSet<u64>)> = history
            .iter()
            .map(|(_, e)| {
                (
                    e.slots.iter().copied().collect(),
                    e.vote_ids.iter().copied().collect(),
                )
            })
            .collect();
        let decoded: Vec<(B256, bool, BTreeSet<u64>, BTreeSet<u64>)> = sent
            .iter()
            .map(|(tx, b, own)| {
                let td = decode_blobs(b, NF).unwrap();
                (
                    *tx,
                    *own,
                    td.updates.iter().map(|(s, _)| *s).collect(),
                    td.vote_ids.iter().copied().collect(),
                )
            })
            .collect();
        let fetched = self.s.chain.fetched_txs();
        let recorded = |slots: &BTreeSet<u64>, vids: &BTreeSet<u64>| {
            entries.contains(&(slots.clone(), vids.clone()))
        };
        for (tx, own, slots, vids) in &decoded {
            if *own && !vids.is_empty() && !recorded(slots, vids) {
                bad.push(format!("own batch {tx} published without a history entry"));
            }
        }
        // A post carries one whole batch here (one blob): decodable alone.
        for (tx, b) in &posted {
            if let Ok(td) = decode_blobs(b, NF) {
                let slots: BTreeSet<u64> = td.updates.iter().map(|(s, _)| *s).collect();
                let vids: BTreeSet<u64> = td.vote_ids.iter().copied().collect();
                if !vids.is_empty() && !recorded(&slots, &vids) {
                    bad.push(format!(
                        "batch posted on Ethereum in {tx} without a history entry"
                    ));
                }
            }
        }
        for (tx, td, own, _) in &canon {
            let slots: BTreeSet<u64> = td.updates.iter().map(|(s, _)| *s).collect();
            let vids: BTreeSet<u64> = td.vote_ids.iter().copied().collect();
            if !own && !vids.is_empty() && !recorded(&slots, &vids) {
                bad.push(format!("synced batch {tx} missing from the history"));
            }
        }
        let mut reseals = 0;
        for (k, (tx, own, slots, vids)) in decoded.iter().enumerate() {
            // Our landings still on the chain (a reverted send starts 0x10).
            if !own || tx[0] != 0 {
                continue;
            }
            let Some((base, after)) = self.s.chain.transition_roots(tx) else {
                continue;
            };
            // A late-mined transaction covers what was out when it was sent.
            let k = self
                .sh
                .stashed_at
                .lock()
                .unwrap()
                .get(&after)
                .map_or(k, |&at| at.min(k));
            let earlier: Vec<&(B256, bool, BTreeSet<u64>, BTreeSet<u64>)> = decoded[..k]
                .iter()
                .filter(|(t2, own2, s2, v2)| {
                    !v2.is_disjoint(vids) && (*own2 || (fetched.contains(t2) && recorded(s2, v2)))
                })
                .collect();
            if earlier.is_empty() {
                continue;
            }
            let all: Vec<u64> = earlier
                .iter()
                .flat_map(|(_, _, s2, _)| s2.iter().copied())
                .collect::<BTreeSet<u64>>()
                .into_iter()
                .collect();
            let Some(occ) = self.s.chain.occupied_at(&self.s, &base, &all) else {
                continue;
            };
            reseals += 1;
            for (slot, occupied) in all.iter().zip(occ) {
                if occupied && !slots.contains(slot) {
                    let from: Vec<String> = earlier
                        .iter()
                        .filter(|(_, _, s2, _)| s2.contains(slot))
                        .map(|(t2, own2, _, _)| {
                            format!("{}{t2}", if *own2 { "own " } else { "peer " })
                        })
                        .collect();
                    bad.push(format!(
                        "landing {tx} re-sealing a vote left out slot {slot:#x} of {from:?}"
                    ));
                }
            }
        }

        if !bad.is_empty() {
            for (n, (tx, td, own, root)) in canon.iter().enumerate() {
                let ids: Vec<String> = td.vote_ids.iter().map(|v| format!("{v:#x}")).collect();
                bad.push(format!(
                    "  canon {n}: {} {tx} -> {}: {ids:?}",
                    if *own { "own " } else { "peer" },
                    short(root)
                ));
            }
        }
        // Origin 3: members only join, so no member's vote is errored for a
        // census change, and every own landing is under a root the
        // contract had, one that holds each of its voters.
        if let Some(o) = &self.onchain {
            for &i in &self.admitted {
                let vid = self.votes[i].v.pkg.vote_id;
                let voter = self.votes[i].voter;
                if stored.iter().any(|v| {
                    v.vote_id == vid && v.error.as_deref() == Some("census changed, recast")
                }) && o.members.contains(&voter)
                {
                    bad.push(format!(
                        "vote {vid:#x} of member {voter} errored for a census change"
                    ));
                }
            }
            let roots = self.sh.census_roots.lock().unwrap().clone();
            for (tx, td, own, root) in &canon {
                if !own {
                    continue;
                }
                let Some(cr) = roots.get(root) else {
                    continue;
                };
                let Some(&(_, n)) = o.roots.iter().find(|(r, _)| census_le(r) == *cr) else {
                    bad.push(format!("landing {tx} under a root the contract never had"));
                    continue;
                };
                for vid in &td.vote_ids {
                    let Some(&i) = by_vid.get(vid) else {
                        continue;
                    };
                    let voter = self.votes[i].voter;
                    if o.members
                        .iter()
                        .position(|m| *m == voter)
                        .is_none_or(|p| p >= n)
                    {
                        bad.push(format!(
                            "vote {vid:#x} counted in {tx} under a census root without voter {voter}"
                        ));
                    }
                }
            }
        }
        // Every reorg re-included all it took: nothing to rebuild.
        let rebuilds = self.sh.rebuilds.load(Ordering::Relaxed);
        if self.sh.full_reorgs.load(Ordering::Relaxed) > 0
            && self.sh.partial_reorgs.load(Ordering::Relaxed) == 0
            && rebuilds > 0
        {
            bad.push(format!(
                "{rebuilds} rebuilds though every reorg was re-included"
            ));
        }
        let own_landed = canon.iter().filter(|c| c.2).count();
        self.log(format!(
            "check: {} admitted, {} on chain ({own_landed} own transitions of {}), \
             {reseals} re-seals checked, {} restarts, {} decisions, errors {errors:?}",
            self.admitted.len(),
            at.len(),
            canon.len(),
            self.restarts,
            self.sh.decisions.load(Ordering::Relaxed)
        ));
        bad
    }
}

/// One seeded scenario; its violations and its trace.
async fn scenario(mode: Mode, seed: u64, steps: usize, old: bool) -> (Vec<String>, Vec<String>) {
    panic_hook();
    let mut run = match mode {
        Mode::Onchain => Run::onchain(seed).await,
        _ => Run::of(mode, seed),
    };
    if std::env::var_os("CHAOS_SPLIT_VIEW").is_some() {
        run.sh.split_view.store(true, Ordering::SeqCst);
    }
    if old || std::env::var_os("CHAOS_OLD_BRANCH").is_some() {
        run.sh.old_branch.store(true, Ordering::SeqCst);
    }
    run.start().await;
    // Most elections start with slots a peer filled.
    if run.mode != Mode::Census && !run.no_votes && run.rng.gen_bool(0.7) {
        let n = run.rng.gen_range(18..=30);
        let idx: Vec<usize> = (0..n).map(|voter| run.new_vote(voter)).collect();
        if let Some(b) = run.peer_batch(&idx) {
            run.s.chain.settle_externally(&b);
        }
    }
    for step in 0..steps {
        run.step = step;
        run.scheduled().await;
        run.act().await;
        // The clock moves a little every step.
        let dt = run.rng.gen_range(0..25);
        run.s.chain.advance_time(dt);
        let wait = run.rng.gen_range(2..30);
        run.run_for(wait).await;
        if let Some(h) = run.h() {
            within(&run.sh, "snapshot", h.snapshot()).await;
        }
    }
    run.step = steps;
    run.drain().await;
    let bad = run.check().await;
    run.crash().await;
    (bad, std::mem::take(&mut run.trace))
}

/// The mode of `kind`'s runs and its default seeds per shard.
fn mode_of(kind: &str, seed: u64) -> (Mode, u64) {
    match kind {
        "l1" => (Mode::L1, 6),
        "dkg" => {
            let m = [KeyMode::DkgAutomatic, KeyMode::DkgLocked, KeyMode::Council];
            (Mode::Dkg(m[(seed % 3) as usize]), 6)
        }
        "census" => (Mode::Census, 6),
        "old" => (Mode::Blob, 6),
        "onchain" => (Mode::Onchain, 1),
        _ => (Mode::Blob, SEEDS),
    }
}

async fn shard(kind: &str, n: u64) {
    let steps: usize = std::env::var("CHAOS_STEPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let seeds: Vec<u64> = match std::env::var("CHAOS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(s) if s % SHARDS == n => vec![s],
        Some(_) => vec![],
        None => {
            let per: u64 = std::env::var("CHAOS_SEEDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(mode_of(kind, 0).1);
            (0..per).map(|i| i * SHARDS + n).collect()
        }
    };
    let trace_all = std::env::var_os("CHAOS_TRACE").is_some();
    let mut failed = Vec::new();
    for seed in seeds {
        let (bad, trace) = scenario(mode_of(kind, seed).0, seed, steps, kind == "old").await;
        if trace_all || !bad.is_empty() {
            eprintln!("--- seed {seed} trace:\n{}", trace.join("\n"));
        } else if let Some(t) = trace.last() {
            eprintln!("seed {seed}: {t}");
        }
        if !bad.is_empty() {
            eprintln!("--- seed {seed} violations:\n{}", bad.join("\n"));
            failed.push(seed);
        }
    }
    assert!(failed.is_empty(), "failing seeds: {failed:?}");
}

#[tokio::test(start_paused = true)]
async fn chaos_shard_0() {
    shard("blob", 0).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_shard_1() {
    shard("blob", 1).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_shard_2() {
    shard("blob", 2).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_shard_3() {
    shard("blob", 3).await;
}

// Origin 3 needs anvil and the built census contract (ANVIL=1), and runs
// in real time: anvil's I/O under a paused clock lets virtual time race.
async fn onchain_shard(n: u64) {
    if !common::anvil::enabled() {
        eprintln!("skipping: set ANVIL=1 for the origin-3 chaos shards");
        return;
    }
    shard("onchain", n).await;
}

#[tokio::test]
async fn chaos_onchain_shard_0() {
    onchain_shard(0).await;
}

#[tokio::test]
async fn chaos_onchain_shard_1() {
    onchain_shard(1).await;
}

#[tokio::test]
async fn chaos_onchain_shard_2() {
    onchain_shard(2).await;
}

#[tokio::test]
async fn chaos_onchain_shard_3() {
    onchain_shard(3).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_old_branch_shard_0() {
    shard("old", 0).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_old_branch_shard_1() {
    shard("old", 1).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_old_branch_shard_2() {
    shard("old", 2).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_old_branch_shard_3() {
    shard("old", 3).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_l1_shard_0() {
    shard("l1", 0).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_l1_shard_1() {
    shard("l1", 1).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_l1_shard_2() {
    shard("l1", 2).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_l1_shard_3() {
    shard("l1", 3).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_dkg_shard_0() {
    shard("dkg", 0).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_dkg_shard_1() {
    shard("dkg", 1).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_dkg_shard_2() {
    shard("dkg", 2).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_dkg_shard_3() {
    shard("dkg", 3).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_census_shard_0() {
    shard("census", 0).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_census_shard_1() {
    shard("census", 1).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_census_shard_2() {
    shard("census", 2).await;
}

#[tokio::test(start_paused = true)]
async fn chaos_census_shard_3() {
    shard("census", 3).await;
}

// ------------------------------------------------ minimized regressions

/// Polls `f` once a virtual second, for up to ten virtual minutes.
async fn eventually(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timeout waiting for {what}");
}

// BUG: a revote a peer settles first is overwritten by the voter's earlier
// vote, which the node re-seals after losing the race (seeds 6, 76, 114).
#[tokio::test(start_paused = true)]
async fn a_revote_a_peer_settled_is_not_overwritten_by_the_earlier_vote() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let db = Db::open_in(dir.path()).unwrap();
    let node = start_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let first = fake_vote(&s.env, 0, &[1, 1], 10);
    let revote = fake_vote(&s.env, 0, &[5, 5], 11);
    h.submit(first.clone()).await.unwrap();
    eventually("the first vote proving", async || prover.calls() == 1).await;
    h.submit(revote.clone()).await.unwrap();
    // Another sequencer the voter also sent the revote to lands it first.
    let b = s.chain.peer_batch(&s, std::slice::from_ref(&revote));
    s.chain.settle_externally(&b);
    eventually("the revote settled from the sync", async || {
        vote_status(&h, revote.pkg.vote_id).await == Some(VoteStatus::Settled)
    })
    .await;
    prover.release(100);
    eventually("the first vote resolved", async || {
        matches!(
            vote_status(&h, first.pkg.vote_id).await,
            Some(VoteStatus::Settled | VoteStatus::Error)
        )
    })
    .await;
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(
        s.chain.submit_count(),
        0,
        "the earlier vote was sealed over the revote"
    );
    let e = h.status(first.pkg.vote_id).await.unwrap().unwrap();
    assert_eq!(e.status, VoteStatus::Error);
    assert!(e.error.unwrap_or_default().contains("superseded"));
    shutdown.cancel();
}

// BUG: a close-out whose status write fails, while its queue write goes
// through, leaves the vote pending outside the stored queue, where neither
// the node nor a restart finds it again (seed 22).
#[tokio::test(start_paused = true)]
async fn a_failed_close_out_write_does_not_strand_the_vote() {
    for k in 1..=4 {
        let s = setup(2, 8, None);
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let shutdown = CancellationToken::new();
        // No batch fits the window under this margin: the vote waits for
        // the close-out.
        let node = start_node(
            db.clone(),
            dir.path(),
            &s,
            s.chain.clone(),
            FakeProver::open(),
            1,
            "100000s",
            shutdown.clone(),
        )
        .await;
        let h = handle(&node).await;
        let v = fake_vote(&s.env, 0, &[1, 2], 20);
        h.submit(v.clone()).await.unwrap();
        s.chain.set_status(ProcessStatus::Ended);
        tokio::time::sleep(Duration::from_secs(5)).await;
        db.fail_next_writes(k);
        s.chain.pass_grace();
        eventually("closed out", async || {
            h.snapshot().await.unwrap().pending == 0
        })
        .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        let sv = db.vote(&pid_fr(), v.pkg.vote_id).unwrap().unwrap();
        assert_eq!(
            sv.status,
            VoteStatus::Error,
            "{k} failed writes: the closed vote reads {:?}",
            sv.status
        );
        assert!(db.pending(&pid_fr()).unwrap().is_empty());
        shutdown.cancel();
    }
}

// BUG: a vote this node errored, which another sequencer then lands, stays
// errored though it is counted (seeds 50, 99, 187).
#[tokio::test(start_paused = true)]
async fn a_vote_errored_here_and_landed_by_a_peer_reads_settled() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    *prover.fail_mask.lock().unwrap() = Some(1);
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 30);
    let vid = v.pkg.vote_id;
    h.submit(v.clone()).await.unwrap();
    eventually("the guest rejection", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Error)
    })
    .await;
    // The voter also sent it to another sequencer, whose batch lands.
    let b = s.chain.peer_batch(&s, std::slice::from_ref(&v));
    s.chain.settle_externally(&b);
    eventually("synced", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert_eq!(vote_status(&h, vid).await, Some(VoteStatus::Settled));
    h.vote_id_proof(vid).await.unwrap();
    shutdown.cancel();
}

// BUG: in blob mode a flight the node dropped (a peer landed first) still
// settles when its proof returns with the chain back at its root, so two
// batches of one node are in flight at once (seeds 156, 222, 240).
#[tokio::test(start_paused = true)]
async fn a_dropped_flight_never_settles() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let genesis = s.chain.root();
    h.submit(fake_vote(&s.env, 0, &[1, 2], 40)).await.unwrap();
    eventually("proving", async || prover.calls() == 1).await;
    // A peer lands first: the flight is dropped, its vote requeued.
    let b = s.chain.peer_batch(&s, &[fake_vote(&s.env, 1, &[2, 2], 41)]);
    s.chain.settle_externally(&b);
    eventually("synced", async || {
        h.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    // The new branch drops the peer's landing: the chain is back at the
    // dropped flight's root, and its proof returns first.
    s.chain.reorg_out_last_transition();
    assert_eq!(s.chain.root(), genesis);
    prover.release(1);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(s.chain.root(), genesis, "the dropped flight settled");
    assert_eq!(s.chain.submit_count(), 0);
    shutdown.cancel();
}

// BUG: a failed batch whose error write fails stays aggregated in the store;
// the voter's later vote lands, and a restart requeues and seals the earlier
// vote over it (seeds 50, 187).
#[tokio::test(start_paused = true)]
async fn an_unwritten_error_never_revives_a_vote_over_a_later_one() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    *prover.fail_mask.lock().unwrap() = Some(1);
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let first = fake_vote(&s.env, 0, &[1, 1], 50);
    let later = fake_vote(&s.env, 0, &[5, 5], 51);
    h.submit(first.clone()).await.unwrap();
    eventually("sealed", async || prover.calls() == 1).await;
    // The guest rejects the batch and the error write fails.
    db.fail_next_writes(1);
    prover.release(1);
    eventually("rejected", async || !h.snapshot().await.unwrap().in_flight).await;
    h.submit(later.clone()).await.unwrap();
    prover.release(100);
    tokio::time::sleep(Duration::from_secs(60)).await;
    // A crash, and a restart on the same datadir.
    shutdown.cancel();
    drop((h, node, db));
    let db = reopen_db(dir.path()).await;
    let shutdown = CancellationToken::new();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    eventually("the later vote settled", async || {
        vote_status(&h, later.pkg.vote_id).await == Some(VoteStatus::Settled)
    })
    .await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(
        s.chain.submit_count(),
        1,
        "the rejected earlier vote was sealed over the later one"
    );
    assert_eq!(
        vote_status(&h, first.pkg.vote_id).await,
        Some(VoteStatus::Error)
    );
    shutdown.cancel();
}

// BUG: after a reorg the monitor saw at its cursor, the actor's re-read from
// a backend still on the old branch at the same height passed the head check
// and cleared it. A reorg that took the results was forgotten, and they were
// never submitted again until a restart (seed 180 under `CHAOS_SPLIT_VIEW=1`).
#[tokio::test(start_paused = true)]
async fn a_seen_reorg_is_not_forgotten_through_a_stale_backend() {
    panic_hook();
    let mut run = Run::new(1);
    run.sh.split_view.store(true, Ordering::SeqCst);
    run.tune.confirmations = 0;
    run.start().await;
    let i = run.new_vote(0);
    run.submit(i).await;
    run.drain().await;
    assert!(!run.s.chain.results().is_empty(), "no results to reorg out");
    // The reads stick to a backend on the old branch; the reorg takes the
    // results and the transition, and the cursor sees it.
    run.freeze(true);
    run.s.chain.reorg_results();
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    run.run_for(30).await;
    run.freeze(false);
    for _ in 0..60 {
        run.s.chain.advance_time(1);
        run.run_for(10).await;
    }
    assert!(
        !run.s.chain.results().is_empty(),
        "the results a reorg took were never submitted again"
    );
    run.crash().await;
}

// BUG: a rebuild appends the votes it requeues behind the queue, so a vote
// cast in the same second as its voter's later queued vote seals after it
// and overwrites it (seed 448).
#[tokio::test(start_paused = true)]
async fn a_rebuild_requeues_ahead_of_later_votes_of_the_same_second() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    // One voter, twice in the same second (the clock stands still).
    let first = fake_vote(&s.env, 0, &[1, 1], 60);
    let later = fake_vote(&s.env, 0, &[5, 5], 61);
    h.submit(first.clone()).await.unwrap();
    prover.release(1);
    eventually("the first vote settled", async || {
        vote_status(&h, first.pkg.vote_id).await == Some(VoteStatus::Settled)
    })
    .await;
    h.submit(later.clone()).await.unwrap();
    eventually("the later vote proving", async || prover.calls() == 2).await;
    // A reorg takes the first vote's landing: the later one's proof reverts,
    // and the node rebuilds from the chain.
    s.chain.reorg_out_last_transition();
    prover.release(100);
    eventually("both settled", async || {
        all_settled(&h, &[first.pkg.vote_id, later.pkg.vote_id]).await
    })
    .await;
    let nf = s.env.cfg.ballot_mode.num_fields;
    let (tx, blobs, _) = s
        .chain
        .sent_batches()
        .into_iter()
        .rev()
        .find(|(tx, _, own)| *own && tx[0] == 0)
        .unwrap();
    assert_eq!(s.chain.last_transition_tx(), tx);
    let td = decode_blobs(&blobs, nf).unwrap();
    assert!(
        td.vote_ids.contains(&later.pkg.vote_id),
        "the earlier vote landed last, over the later one"
    );
    shutdown.cancel();
}

// BUG: one `Ok(None)` block hash (a backend behind the confirmed head)
// saved the cursor without a hash at once, so a reorg below it went unseen;
// the docs open the check only after 5 failed reads.
#[tokio::test(start_paused = true)]
async fn a_lagging_block_hash_does_not_open_the_reorg_check_at_once() {
    panic_hook();
    let mut run = Run::new(2);
    run.tune.confirmations = 0;
    run.start().await;
    let i = run.new_vote(0);
    run.submit(i).await;
    run.s.chain.advance_time(1000);
    eventually("settled", async || run.s.chain.voters() == 1).await;
    run.run_for(5).await;
    // Hashes lag at the head while the confirmed head moves on.
    *run.sh.hash_lag.lock().unwrap() = Some(run.s.chain.head_block());
    run.s.chain.advance_time(1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // A reorg takes the landing, below the cursor; then the hashes catch up.
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    *run.sh.hash_lag.lock().unwrap() = None;
    run.run_for(5).await;
    let reorgs = run
        .node
        .as_ref()
        .unwrap()
        .node
        .metrics
        .reorgs
        .load(Ordering::SeqCst);
    assert!(reorgs >= 1, "the monitor never saw the reorg");
    run.crash().await;
}

// BUG: the re-read pinned to a seen reorg's block hash had no exit but a
// successful by-hash read: on an endpoint refusing EIP-1898 calls every
// landing read failed for good, and the process never closed out or
// published its results until a restart.
#[tokio::test(start_paused = true)]
async fn a_seen_reorg_on_an_endpoint_without_hash_reads_still_finalizes() {
    panic_hook();
    let mut run = Run::new(3);
    run.sh.no_hash_reads.store(true, Ordering::SeqCst);
    run.number_reads = true;
    run.tune.confirmations = 0;
    run.start().await;
    let i = run.new_vote(0);
    run.submit(i).await;
    run.s.chain.advance_time(1000);
    eventually("settled", async || run.s.chain.voters() == 1).await;
    run.run_for(5).await;
    // A reorg the cursor sees takes the landing.
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    run.run_for(5).await;
    run.drain().await;
    let bad = run.check().await;
    assert!(bad.is_empty(), "{bad:?}");
    assert!(!run.s.chain.results().is_empty(), "no results");
    run.crash().await;
}

// BUG: a revote's settlement and its earlier vote's supersede were separate
// writes, and `recover()` settled an in-tree vote without superseding: a
// write failure or a crash between them let the earlier vote seal over the
// revote.
#[tokio::test(start_paused = true)]
async fn a_supersede_survives_a_crash_anywhere_in_the_sync() {
    for skip in 0..8 {
        let s = setup(2, 8, None);
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let shutdown = CancellationToken::new();
        let prover = FakeProver::gated();
        let node = start_node(
            db.clone(),
            dir.path(),
            &s,
            s.chain.clone(),
            prover.clone(),
            1,
            "0s",
            shutdown.clone(),
        )
        .await;
        let h = handle(&node).await;
        let first = fake_vote(&s.env, 0, &[1, 1], 70);
        let revote = fake_vote(&s.env, 0, &[5, 5], 71);
        h.submit(first.clone()).await.unwrap();
        eventually("proving", async || prover.calls() == 1).await;
        h.submit(revote.clone()).await.unwrap();
        // The disk dies part way through the sync of the peer's landing.
        db.fail_writes_after(skip, u32::MAX);
        let b = s.chain.peer_batch(&s, std::slice::from_ref(&revote));
        s.chain.settle_externally(&b);
        tokio::time::sleep(Duration::from_secs(30)).await;
        shutdown.cancel();
        drop((h, node, db));
        let db = reopen_db(dir.path()).await;
        let shutdown = CancellationToken::new();
        let node = start_node(
            db,
            dir.path(),
            &s,
            s.chain.clone(),
            FakeProver::open(),
            1,
            "0s",
            shutdown.clone(),
        )
        .await;
        let h = handle(&node).await;
        eventually("the revote settled", async || {
            vote_status(&h, revote.pkg.vote_id).await == Some(VoteStatus::Settled)
        })
        .await;
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            s.chain.submit_count(),
            0,
            "{skip} writes in: the earlier vote sealed over the revote"
        );
        assert_eq!(
            vote_status(&h, first.pkg.vote_id).await,
            Some(VoteStatus::Error),
            "{skip} writes in"
        );
        shutdown.cancel();
    }
}

// BUG: a rebuild requeued a voter's two lost votes of one second in vote-id
// order, so the earlier one could seal last and count.
#[tokio::test(start_paused = true)]
async fn a_rebuild_keeps_the_order_of_same_second_votes_it_requeues() {
    panic_hook();
    let mut run = Run::new(7);
    run.tune.batch_max = 1;
    run.tune.confirmations = 0;
    run.start().await;
    let a = run.new_vote(0);
    // The later vote's id sorts first: the requeue cannot lean on ids.
    let b = loop {
        let b = run.new_vote(0);
        if run.votes[b].v.pkg.vote_id < run.votes[a].v.pkg.vote_id {
            break b;
        }
    };
    let h = run.h().unwrap();
    let (va, vb) = (run.votes[a].v.pkg.vote_id, run.votes[b].v.pkg.vote_id);
    run.submit(a).await;
    eventually("a settled", async || {
        vote_status(&h, va).await == Some(VoteStatus::Settled)
    })
    .await;
    run.submit(b).await;
    eventually("b settled", async || {
        vote_status(&h, vb).await == Some(VoteStatus::Settled)
    })
    .await;
    // Once the cursor passed both landings, a reorg it sees takes them.
    let head = run.s.chain.head_block();
    let db = run.node.as_ref().unwrap().db.clone();
    eventually("the cursor past the landings", async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    drop(db);
    run.s.chain.reorg_out_last_transition();
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    run.run_for(200).await;
    run.drain().await;
    let bad = run.check().await;
    assert!(bad.is_empty(), "{bad:?}");
    run.crash().await;
}

// BUG: the anchor of a seen reorg was dropped after the first by-hash read,
// and results confirmations went by number: a backend still on the old
// branch reported the results the reorg took, and the node finalized again
// without resubmitting them.
#[tokio::test(start_paused = true)]
async fn a_stale_backend_cannot_refinalize_results_a_reorg_took() {
    panic_hook();
    let mut run = Run::new(8);
    run.tune.confirmations = 0;
    run.start().await;
    let i = run.new_vote(0);
    run.submit(i).await;
    run.drain().await;
    assert!(!run.s.chain.results().is_empty(), "no results to reorg out");
    let head = run.s.chain.head_block();
    let db = run.node.as_ref().unwrap().db.clone();
    eventually("the cursor at the results", async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    drop(db);
    // Reads by number now come from a backend on the old branch.
    let old = run.s.chain.process(&pid31()).await.unwrap();
    *run.sh.stale_proc.lock().unwrap() = Some(old);
    // The reorg takes the results block, the root unchanged; the cursor
    // sees it.
    run.s.chain.reorg_results();
    run.sh.forks.lock().unwrap().push(head);
    run.run_for(120).await;
    *run.sh.stale_proc.lock().unwrap() = None;
    for _ in 0..60 {
        run.s.chain.advance_time(1);
        run.run_for(10).await;
    }
    assert!(
        !run.s.chain.results().is_empty(),
        "the results a reorg took were never submitted again"
    );
    run.crash().await;
}

// BUG: the by-hash re-read after a reorg is at the monitor's cursor, below
// the last head read, and merged that older snapshot: a census re-rooted and
// a cap raised since were rewound, and a vote the newer record admitted was
// errored.
#[tokio::test(start_paused = true)]
async fn a_reorg_read_below_the_last_head_read_does_not_rewind_the_record() {
    panic_hook();
    let mut run = Run::offchain(9, 4);
    run.tune = Tuning {
        batch_max: 2,
        min_mix: 2,
        batch_time: 2000,
        solo_wait: 4000,
        flush_horizon: 30,
        confirmations: 3,
    };
    // Two voters fill the cap.
    run.s.chain.change_max_voters(2);
    run.sh.max_voters.store(2, Ordering::SeqCst);
    run.start().await;
    run.sh.knobs.lock().unwrap().slow_secs = 30;
    let (a, b) = (run.new_vote(0), run.new_vote(1));
    run.submit(a).await;
    run.submit(b).await;
    eventually("proving", async || {
        !run.sh.started.lock().unwrap().is_empty()
    })
    .await;
    // Blocks before the re-root and the raised cap, enough that the rewind
    // lands among them: the fake keeps each block's state, so the re-read at
    // the rewound anchor shows the old cap and census.
    let head = run.s.chain.head_block();
    run.s.chain.set_head_block(head + 140);
    run.s.chain.advance_time(1);
    run.s.chain.advance_time(1);
    let voters = 0..5;
    let imt = LeanImt::from_leaves(
        voters
            .clone()
            .map(|i| census_leaf(&voter_address(i), 1).unwrap())
            .collect(),
    );
    let dir = run.s.census_dir.join("v2");
    std::fs::create_dir(&dir).unwrap();
    let uri = write_census_of(&dir, voters);
    run.s.chain.set_census(imt.root(), uri);
    run.s.chain.change_max_voters(10);
    run.sh.max_voters.store(10, Ordering::SeqCst);
    // Both reach the record once the anchor passes their blocks (only an
    // anchored read or their events move it).
    eventually("landed", async || run.s.chain.voters() == 2).await;
    for _ in 0..4 {
        run.s.chain.advance_time(1);
        run.run_for(5).await;
    }
    // A voter of the new census only.
    let env2 = Env {
        cfg: run.s.env.cfg.clone(),
        imt,
    };
    let c = fake_vote(&env2, 4, &[1, 1], 90);
    let vc = c.pkg.vote_id;
    let h = run.h().unwrap();
    h.submit(c).await.unwrap();
    run.run_for(30).await;
    assert_eq!(vote_status(&h, vc).await, Some(VoteStatus::Pending));
    // A reorg the cursor sees, below the last head read; the pages after
    // the rewind are slow, so the anchor sits at the rewound block a while.
    run.s.chain.events_delay(Duration::from_secs(10));
    run.sh.forks.lock().unwrap().push(1);
    run.run_for(5).await;
    run.s.chain.events_delay(Duration::ZERO);
    run.run_for(30).await;
    let rec = run
        .node
        .as_ref()
        .unwrap()
        .db
        .process(&pid_fr())
        .unwrap()
        .unwrap();
    assert_eq!(rec.onchain.max_voters, 10, "the cap was rewound");
    assert_eq!(rec.onchain.census.root, env2.imt.root());
    assert_eq!(
        vote_status(&h, vc).await,
        Some(VoteStatus::Pending),
        "{:?}",
        h.status(vc).await.unwrap().unwrap().error
    );
    run.crash().await;
}

// BUG: the repair of a transition record lost after the tree applied it
// settled only aggregated or processed votes: a vote this node errored that
// a peer landed stayed errored, after a failed write or a restart.
#[tokio::test(start_paused = true)]
async fn a_repaired_transition_record_settles_a_vote_this_node_errored() {
    for restart in [false, true] {
        let s = setup(2, 8, None);
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        let shutdown = CancellationToken::new();
        let prover = FakeProver::open();
        *prover.fail_mask.lock().unwrap() = Some(1);
        let node = start_node(
            db.clone(),
            dir.path(),
            &s,
            s.chain.clone(),
            prover.clone(),
            1,
            "0s",
            shutdown.clone(),
        )
        .await;
        let h = handle(&node).await;
        let v = fake_vote(&s.env, 0, &[1, 2], 80);
        let vid = v.pkg.vote_id;
        h.submit(v.clone()).await.unwrap();
        eventually("the guest rejection", async || {
            vote_status(&h, vid).await == Some(VoteStatus::Error)
        })
        .await;
        // A peer lands it; the tree applies, the record write fails.
        db.fail_transition_writes(true);
        let b = s.chain.peer_batch(&s, std::slice::from_ref(&v));
        s.chain.settle_externally(&b);
        eventually("the tree at the chain's root", async || {
            h.snapshot().await.unwrap().root == s.chain.root()
        })
        .await;
        let h = if restart {
            shutdown.cancel();
            drop((h, node, db));
            let db = reopen_db(dir.path()).await;
            let node = start_node(
                db,
                dir.path(),
                &s,
                s.chain.clone(),
                prover.clone(),
                1,
                "0s",
                CancellationToken::new(),
            )
            .await;
            handle(&node).await
        } else {
            db.fail_transition_writes(false);
            h
        };
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            vote_status(&h, vid).await,
            Some(VoteStatus::Settled),
            "restart {restart}"
        );
    }
}

// BUG: a settlement refused because the window closed was requeued with no
// backoff for a close-out that waits on the record's grace end; when that
// is later than the chain's (a reorg took the last landing, and the record's
// landing time only grows), the node re-proved into the same revert without
// pause (seed 68 under the stale-read action).
#[tokio::test(start_paused = true)]
async fn a_closed_window_revert_does_not_reprove_in_a_loop() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let first = fake_vote(&s.env, 0, &[1, 1], 100);
    h.submit(first.clone()).await.unwrap();
    h.submit(fake_vote(&s.env, 1, &[2, 2], 101)).await.unwrap();
    eventually("the first batch proving", async || prover.calls() == 1).await;
    // The election ends; the first batch lands inside the grace, which
    // moves the grace end out.
    s.chain.set_status(ProcessStatus::Ended);
    let end = s.chain.time();
    s.chain.advance_time(100);
    prover.release(1);
    eventually("settled", async || {
        vote_status(&h, first.pkg.vote_id).await == Some(VoteStatus::Settled)
    })
    .await;
    eventually("the next batch proving", async || prover.calls() == 2).await;
    // A reorg takes that landing's block on-chain: the chain's grace end is
    // back before the clock, the record's (its landing time only grows) is
    // not.
    s.chain.advance_time(100);
    s.chain.mutate_process(|p| p.last_vote_at = end);
    assert!(s.chain.time() >= s.chain.grace_end());
    prover.release(200);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(
        prover.calls() < 20,
        "{} proofs into a closed window",
        prover.calls()
    );
    shutdown.cancel();
}

// BUG: a rebuild requeued only votes the store had settled: those of a
// landing of ours whose record write failed stayed processed, out of the
// queue, once a reorg took it (seed 1126).
#[tokio::test(start_paused = true)]
async fn a_rebuild_requeues_the_votes_of_an_unrecorded_landing() {
    panic_hook();
    let mut run = Run::new(11);
    run.tune.batch_max = 1;
    run.tune.confirmations = 0;
    run.start().await;
    let db = run.node.as_ref().unwrap().db.clone();
    db.fail_transition_writes(true);
    let i = run.new_vote(0);
    run.submit(i).await;
    run.s.chain.advance_time(1000);
    eventually("landed", async || run.s.chain.voters() == 1).await;
    let head = run.s.chain.head_block();
    eventually("the cursor past the landing", async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(head)
    })
    .await;
    // A reorg the cursor sees takes it; the disk writes records again.
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    db.fail_transition_writes(false);
    drop(db);
    run.run_for(200).await;
    run.drain().await;
    let bad = run.check().await;
    assert!(bad.is_empty(), "{bad:?}");
    run.crash().await;
}

// BUG: votes a rebuild kept settled that the replay no longer finds were
// requeued by the kept-vote check in vote-id order, so a voter's two votes
// of one second could come back reversed (seed 431).
#[tokio::test(start_paused = true)]
async fn the_kept_vote_check_requeues_same_second_votes_in_cast_order() {
    panic_hook();
    let mut run = Run::new(12);
    run.tune.batch_max = 1;
    run.tune.confirmations = 0;
    run.start().await;
    let a = run.new_vote(0);
    let b = loop {
        let b = run.new_vote(0);
        if run.votes[b].v.pkg.vote_id < run.votes[a].v.pkg.vote_id {
            break b;
        }
    };
    let c = run.new_vote(1);
    let h = run.h().unwrap();
    for i in [a, b, c] {
        let vid = run.votes[i].v.pkg.vote_id;
        run.submit(i).await;
        eventually("settled", async || {
            vote_status(&h, vid).await == Some(VoteStatus::Settled)
        })
        .await;
    }
    let db = run.node.as_ref().unwrap().db.clone();
    let seen = async |run: &Run| {
        let head = run.s.chain.head_block();
        eventually("the cursor at the head", async || {
            db.meta_u64("monitor_last_block").unwrap() >= Some(head)
        })
        .await;
    };
    seen(&run).await;
    // A reorg the cursor sees takes the third landing: the rebuild keeps
    // the first two settled; their replay waits for the blobs.
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    run.s.chain.fail_blobs(u32::MAX);
    let node = &run.node.as_ref().unwrap().node;
    eventually("rebuilt", async || {
        node.metrics.rebuilds.load(Ordering::SeqCst) >= 1
    })
    .await;
    // Then the chain loses those two as well, and the replay converges.
    run.s.chain.reorg_out_last_transition();
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    run.s.chain.fail_blobs(0);
    drop(db);
    run.run_for(200).await;
    run.drain().await;
    let bad = run.check().await;
    assert!(bad.is_empty(), "{bad:?}");
    run.crash().await;
}

// A committee-key process whose decryption request and results a reorg
// the cursor sees takes, after it finalized: it requests again and the
// results go in again.
#[tokio::test(start_paused = true)]
async fn a_reorged_out_decryption_request_is_made_again() {
    panic_hook();
    let mut run = Run::of(Mode::Dkg(KeyMode::DkgAutomatic), 20);
    run.no_votes = false;
    run.tune.confirmations = 0;
    run.start().await;
    let i = run.new_vote(0);
    run.submit(i).await;
    run.drain().await;
    assert!(!run.s.chain.results().is_empty(), "no results to reorg out");
    let b = run.cursor_at_head().await;
    run.s.chain.reorg_results();
    run.s.chain.reorg_dkg_request();
    run.combined = None;
    run.sh.forks.lock().unwrap().push(b);
    for _ in 0..60 {
        run.committee();
        run.run_for(10).await;
        run.s.chain.advance_time(1);
    }
    let bad = run.check().await;
    assert!(bad.is_empty(), "{bad:?}");
    run.crash().await;
}

// ------------------------------------------- anchored reads (each at 0 and 1 confirmations)

/// Submits each vote and waits for it to settle (the clock moving on).
async fn settle_each(run: &mut Run, idx: &[usize]) {
    let h = run.h().unwrap();
    let chain = run.s.chain.clone();
    for &i in idx {
        let vid = run.votes[i].v.pkg.vote_id;
        run.submit(i).await;
        eventually("settled", async || {
            chain.advance_time(1);
            vote_status(&h, vid).await == Some(VoteStatus::Settled)
        })
        .await;
    }
}

/// A clean drain and check, then the node down.
async fn drained_clean(mut run: Run, what: &str) {
    run.drain().await;
    let bad = run.check().await;
    assert!(bad.is_empty(), "{what}: {bad:?}");
    run.crash().await;
}

// A reorg the cursor sees takes our landing and the new branch re-includes
// it above the cursor: the chain comes back on our log once the anchor
// passes it, so nothing is rebuilt.
#[tokio::test(start_paused = true)]
async fn anchored_reinclusion_above_the_cursor_needs_no_rebuild() {
    panic_hook();
    for conf in [0, 1] {
        let mut run = Run::new(31);
        run.tune.batch_max = 1;
        run.tune.confirmations = conf;
        run.start().await;
        let i = run.new_vote(0);
        settle_each(&mut run, &[i]).await;
        run.cursor_at_head().await;
        let ev = run.s.chain.reorg_out_last_transition();
        run.sh.forks.lock().unwrap().push(ev.block);
        run.s.chain.restore_transition(ev);
        for _ in 0..20 {
            run.s.chain.advance_time(1);
            run.run_for(10).await;
        }
        assert_eq!(run.sh.rebuilds.load(Ordering::Relaxed), 0, "conf {conf}");
        drained_clean(run, &format!("conf {conf}")).await;
    }
}

// The rewind window with reads by number stuck on the old branch for good:
// the results a reorg took are not confirmed from those reads; they go in
// again and the node finalizes on the anchored chain.
#[tokio::test(start_paused = true)]
async fn anchored_rewind_with_an_old_branch_backend_does_not_refinalize() {
    panic_hook();
    for conf in [0, 1] {
        let mut run = Run::new(32);
        run.tune.confirmations = conf;
        run.start().await;
        let i = run.new_vote(0);
        run.submit(i).await;
        run.drain().await;
        assert!(!run.s.chain.results().is_empty(), "no results to reorg out");
        let b = run.cursor_at_head().await;
        let old = run.s.chain.process(&pid31()).await.unwrap();
        run.sh.old_branch.store(true, Ordering::SeqCst);
        *run.sh.stale_proc.lock().unwrap() = Some(old);
        run.s.chain.reorg_results();
        run.sh.forks.lock().unwrap().push(b);
        for _ in 0..60 {
            run.s.chain.advance_time(1);
            run.run_for(10).await;
        }
        assert!(
            !run.s.chain.results().is_empty(),
            "conf {conf}: the results a reorg took were never submitted again"
        );
        drained_clean(run, &format!("conf {conf}")).await;
    }
}

// A cancel the node saw is reorged out (its event gone with its block):
// admission reopens and a new vote settles.
#[tokio::test(start_paused = true)]
async fn anchored_cancel_reorged_out_reopens_admission() {
    panic_hook();
    for conf in [0, 1] {
        let mut run = Run::new(33);
        run.tune.confirmations = conf;
        run.start().await;
        let h = run.h().unwrap();
        run.s.chain.set_status(ProcessStatus::Canceled);
        let block = run.s.chain.head_block();
        run.s.chain.advance_time(1);
        eventually("canceled", async || {
            run.s.chain.advance_time(1);
            !h.snapshot().await.unwrap().accepting
        })
        .await;
        run.cursor_at_head().await;
        // The cancel's block is replaced: Ready again, its event gone.
        run.s.chain.hide_block(Some(block));
        run.s
            .chain
            .mutate_process(|p| p.status = ProcessStatus::Ready);
        run.sh.forks.lock().unwrap().push(block);
        eventually("accepting again", async || {
            run.s.chain.advance_time(1);
            h.snapshot().await.unwrap().accepting
        })
        .await;
        let i = run.new_vote(0);
        settle_each(&mut run, &[i]).await;
        drained_clean(run, &format!("conf {conf}")).await;
    }
}

// A rebuild while an earlier one's kept-vote check is pending (its replay
// stuck on withheld blobs): the votes the first one kept, which the second
// requeues, keep their cast order (a voter's two votes of one second, the
// later one with the lower vote id).
#[tokio::test(start_paused = true)]
async fn anchored_nested_rebuild_keeps_same_second_order() {
    panic_hook();
    for conf in [0, 1] {
        let mut run = Run::new(34);
        run.tune.batch_max = 1;
        run.tune.confirmations = conf;
        run.start().await;
        let x = run.new_vote(2);
        let a = run.new_vote(0);
        let b = loop {
            let b = run.new_vote(0);
            if run.votes[b].v.pkg.vote_id < run.votes[a].v.pkg.vote_id {
                break b;
            }
        };
        let c = run.new_vote(1);
        settle_each(&mut run, &[x]).await;
        // a and b in the same second, each its own batch.
        let h = run.h().unwrap();
        for i in [a, b] {
            let vid = run.votes[i].v.pkg.vote_id;
            run.submit(i).await;
            eventually("settled", async || {
                vote_status(&h, vid).await == Some(VoteStatus::Settled)
            })
            .await;
        }
        settle_each(&mut run, &[c]).await;
        run.cursor_at_head().await;
        // The first rebuild keeps x, a, b; its replay applies x's landing
        // and waits on a's blobs.
        let own: Vec<B256> = run
            .s
            .chain
            .sent_batches()
            .into_iter()
            .filter(|(_, _, own)| *own)
            .map(|(tx, _, _)| tx)
            .collect();
        run.s.chain.withhold_blobs(Some(own[1]));
        let ev = run.s.chain.reorg_out_last_transition();
        run.sh.forks.lock().unwrap().push(ev.block);
        let db = run.node.as_ref().unwrap().db.clone();
        eventually("rebuilt, x replayed", async || {
            run.sh.rebuilds.load(Ordering::Relaxed) >= 1
                && db.transitions(&pid_fr()).unwrap().len() == 1
        })
        .await;
        // Then the chain loses every landing: the second rebuild requeues
        // a and b, which the replay never reached.
        run.s.chain.reorg_out_last_transition();
        run.s.chain.reorg_out_last_transition();
        let ev = run.s.chain.reorg_out_last_transition();
        run.sh.forks.lock().unwrap().push(ev.block);
        run.s.chain.withhold_blobs(None);
        drop(db);
        eventually("rebuilt again", async || {
            run.s.chain.advance_time(1);
            run.sh.rebuilds.load(Ordering::Relaxed) >= 2
        })
        .await;
        run.run_for(100).await;
        drained_clean(run, &format!("conf {conf}")).await;
    }
}

// An endpoint without EIP-1898 calls: degraded mode reads at the anchor's
// number between hash checks, and the node closes out and finalizes; once
// reads by hash answer again it leaves it.
#[tokio::test(start_paused = true)]
async fn anchored_degraded_mode_enters_and_leaves() {
    panic_hook();
    for conf in [0, 1] {
        let mut run = Run::new(35);
        run.tune.confirmations = conf;
        run.sh.no_hash_reads.store(true, Ordering::SeqCst);
        // Degraded mode is opt-in (a single-backend endpoint).
        run.number_reads = true;
        run.start().await;
        let i = run.new_vote(0);
        run.submit(i).await;
        run.s.chain.advance_time(1000);
        eventually("settled", async || run.s.chain.voters() == 1).await;
        let ev = run.s.chain.reorg_out_last_transition();
        run.sh.forks.lock().unwrap().push(ev.block);
        for _ in 0..20 {
            run.s.chain.advance_time(1);
            run.run_for(10).await;
        }
        // Only a degraded read can decide the rebuild.
        assert_eq!(
            run.sh.rebuilds.load(Ordering::Relaxed),
            1,
            "conf {conf}: no rebuild while reads by hash fail"
        );
        // Hash reads come back: a probe leaves degraded mode, and the close
        // and the results read by hash again.
        run.sh.no_hash_reads.store(false, Ordering::SeqCst);
        run.sh.hash_served.lock().unwrap().clear();
        run.drain().await;
        assert!(
            !run.sh.hash_served.lock().unwrap().is_empty(),
            "conf {conf}: no read by hash after they answered again"
        );
        let bad = run.check().await;
        assert!(bad.is_empty(), "conf {conf}: {bad:?}");
        run.crash().await;
    }
}

// Block hashes unreadable past a block (the monitor routes on without its
// check): the node keeps sealing on events, but the anchor stays behind,
// so pending votes past the grace end are not closed out until hashes
// return.
#[tokio::test(start_paused = true)]
async fn anchored_fail_open_seals_but_does_not_close_out() {
    panic_hook();
    for conf in [0, 1] {
        let mut run = Run::new(36);
        run.tune.batch_max = 1;
        run.tune.confirmations = conf;
        run.start().await;
        let head = run.cursor_at_head().await;
        *run.sh.hash_lag.lock().unwrap() = Some(head);
        let i = run.new_vote(0);
        settle_each(&mut run, &[i]).await;
        // The prover fails every batch: the next vote stays pending.
        run.sh.knobs.lock().unwrap().transient = u32::MAX;
        let j = run.new_vote(1);
        run.submit(j).await;
        let vid = run.votes[j].v.pkg.vote_id;
        let h = run.h().unwrap();
        run.s.chain.pass_grace();
        for _ in 0..10 {
            run.s.chain.advance_time(5);
            run.run_for(5).await;
        }
        assert_eq!(
            vote_status(&h, vid).await,
            Some(VoteStatus::Pending),
            "conf {conf}: closed out past the anchor"
        );
        *run.sh.hash_lag.lock().unwrap() = None;
        eventually("closed out", async || {
            run.s.chain.advance_time(1);
            vote_status(&h, vid).await == Some(VoteStatus::Error)
        })
        .await;
        run.sh.knobs.lock().unwrap().transient = 0;
        drained_clean(run, &format!("conf {conf}")).await;
    }
}

// BUG: a flight requeue whose write failed left the stored queue behind
// the one in memory (the flight's vote still out of it, a rebuild then
// queuing an earlier vote of its voter, same second, at the head): a restart
// put the later vote first, and it was overwritten (seed 300).
#[tokio::test(start_paused = true)]
async fn a_failed_requeue_write_does_not_reorder_a_voter_after_a_restart() {
    let (bad, trace) = scenario(Mode::Blob, 300, 50, false).await;
    assert!(bad.is_empty(), "{}\n{bad:?}", trace.join("\n"));
}

// BUG: a rebuild that dropped its flight requeued it in a write of its own;
// when that write failed and the node died before a later tick rewrote the
// stored queue, the restart put the flight's vote (the later one)
// ahead of its voter's earlier vote of the same second, which then sealed
// over it.
#[tokio::test(start_paused = true)]
async fn a_rebuild_whose_flight_requeue_fails_keeps_cast_order_across_a_crash() {
    panic_hook();
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_node(
        db.clone(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    // One voter, twice in the same second; the later vote has the lower id.
    let a = fake_vote(&s.env, 0, &[1, 1], 60);
    let mut k = 61;
    let b = loop {
        let b = fake_vote(&s.env, 0, &[5, 5], k);
        if b.pkg.vote_id < a.pkg.vote_id {
            break b;
        }
        k += 1;
    };
    h.submit(a.clone()).await.unwrap();
    prover.release(1);
    eventually("a settled", async || {
        vote_status(&h, a.pkg.vote_id).await == Some(VoteStatus::Settled)
    })
    .await;
    h.submit(b.clone()).await.unwrap();
    eventually("b proving", async || prover.calls() == 2).await;
    // A reorg the cursor sees takes a's landing; the rebuild drops b's
    // flight, and its requeue write fails.
    s.chain.reorg_out_last_transition();
    db.fail_queue_writes(true);
    eventually("rebuilt", async || {
        node.metrics.rebuilds.load(Ordering::SeqCst) >= 1
    })
    .await;
    // The node dies before any queue write gets through.
    shutdown.cancel();
    drop((h, node, db));

    let shutdown = CancellationToken::new();
    let node = start_node(
        reopen_db(dir.path()).await,
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    eventually("both settled", async || {
        all_settled(&h, &[a.pkg.vote_id, b.pkg.vote_id]).await
    })
    .await;
    let nf = s.env.cfg.ballot_mode.num_fields;
    let (_, blobs, _) = s
        .chain
        .sent_batches()
        .into_iter()
        .rev()
        .find(|(tx, _, own)| *own && *tx == s.chain.last_transition_tx())
        .unwrap();
    let td = decode_blobs(&blobs, nf).unwrap();
    assert!(
        td.vote_ids.contains(&b.pkg.vote_id),
        "the earlier vote landed last, over the later one"
    );
    shutdown.cancel();
}

// BUG: after a restart the field-merge floor was the last transition's
// block, below the blocks the stored record's fields came from: a rewind
// to an anchor between them merged an older snapshot, a lower cap among
// it, and errored an admitted pending vote for good.
#[tokio::test(start_paused = true)]
async fn a_rewind_after_a_restart_does_not_merge_fields_older_than_the_record() {
    panic_hook();
    let mut run = Run::offchain(9, 4);
    run.tune = Tuning {
        batch_max: 2,
        min_mix: 2,
        batch_time: 2000,
        solo_wait: 4000,
        flush_horizon: 30,
        confirmations: 3,
    };
    // Two voters fill the cap, and land.
    run.s.chain.change_max_voters(2);
    run.sh.max_voters.store(2, Ordering::SeqCst);
    run.start().await;
    let (a, b) = (run.new_vote(0), run.new_vote(1));
    run.submit(a).await;
    run.submit(b).await;
    eventually("landed", async || run.s.chain.voters() == 2).await;
    // Far enough on that a rewind lands after the landing, then the cap
    // raised and the census re-rooted.
    let head = run.s.chain.head_block();
    run.s.chain.set_head_block(head + 140);
    run.s.chain.advance_time(1);
    let voters = 0..5;
    let imt = LeanImt::from_leaves(
        voters
            .clone()
            .map(|i| census_leaf(&voter_address(i), 1).unwrap())
            .collect(),
    );
    let dir = run.s.census_dir.join("v2");
    std::fs::create_dir(&dir).unwrap();
    let uri = write_census_of(&dir, voters);
    run.s.chain.set_census(imt.root(), uri);
    run.s.chain.change_max_voters(10);
    run.sh.max_voters.store(10, Ordering::SeqCst);
    for _ in 0..4 {
        run.s.chain.advance_time(1);
        run.run_for(5).await;
    }
    // The record with them is stored; the node restarts and reads the
    // current anchor.
    run.restart("the raised cap stored").await;
    run.run_for(10).await;
    let env2 = Env {
        cfg: run.s.env.cfg.clone(),
        imt,
    };
    let c = fake_vote(&env2, 4, &[1, 1], 90);
    let vc = c.pkg.vote_id;
    let h = run.h().unwrap();
    h.submit(c).await.unwrap();
    run.run_for(30).await;
    assert_eq!(vote_status(&h, vc).await, Some(VoteStatus::Pending));
    // A reorg the cursor sees; slow pages hold the anchor at the rewound
    // block, after the landing and before the raised cap.
    run.s.chain.events_delay(Duration::from_secs(10));
    run.sh.forks.lock().unwrap().push(1);
    run.run_for(5).await;
    run.s.chain.events_delay(Duration::ZERO);
    run.run_for(30).await;
    let rec = run
        .node
        .as_ref()
        .unwrap()
        .db
        .process(&pid_fr())
        .unwrap()
        .unwrap();
    assert_eq!(rec.onchain.max_voters, 10, "the cap was rewound");
    assert_eq!(rec.onchain.census.root, env2.imt.root());
    assert_eq!(
        vote_status(&h, vc).await,
        Some(VoteStatus::Pending),
        "{:?}",
        h.status(vc).await.unwrap().unwrap().error
    );
    run.crash().await;
}

// A rebuild-check marker in no known format (here the kept vote ids alone,
// which only unreleased builds wrote) fails closed: the kept-vote check
// never runs on a guessed order, and the process seals nothing until the
// operator removes the marker.
#[tokio::test(start_paused = true)]
async fn a_rebuild_marker_in_no_known_format_holds_the_process() {
    panic_hook();
    let mut run = Run::new(37);
    run.tune.batch_max = 1;
    run.tune.confirmations = 0;
    run.start().await;
    let (x, a, c) = (run.new_vote(2), run.new_vote(0), run.new_vote(1));
    settle_each(&mut run, &[x, a, c]).await;
    run.cursor_at_head().await;
    // The rebuild keeps x and a; their replay waits for the blobs.
    run.s.chain.fail_blobs(u32::MAX);
    let ev = run.s.chain.reorg_out_last_transition();
    run.sh.forks.lock().unwrap().push(ev.block);
    eventually("rebuilt", async || {
        run.sh.rebuilds.load(Ordering::Relaxed) >= 1
    })
    .await;
    run.crash().await;
    let db = reopen_db(run.dir.path()).await;
    let kept: Vec<u64> = [x, a].iter().map(|&i| run.votes[i].v.pkg.vote_id).collect();
    db.put_rebuild_check_raw(&pid_fr(), &serde_json::to_vec(&kept).unwrap())
        .unwrap();
    drop(db);
    run.start().await;
    // The replay converges; the check cannot read its order.
    run.s.chain.fail_blobs(0);
    let d = run.new_vote(3);
    run.submit(d).await;
    for _ in 0..30 {
        run.s.chain.advance_time(1);
        run.run_for(10).await;
    }
    let live = run.node.as_ref().unwrap();
    assert!(
        live.db.rebuild_check(&pid_fr()).unwrap(),
        "the marker is gone"
    );
    let h = run.h().unwrap();
    assert_eq!(
        vote_status(&h, run.votes[d].v.pkg.vote_id).await,
        Some(VoteStatus::Pending),
        "sealed past a marker it cannot read"
    );
    run.crash().await;
}

// BUG: degraded mode took a read by number, between two block-hash checks,
// as an anchored read, though behind a balancer the middle answer can come
// from a backend on another branch: results a reorg took were confirmed
// from such reads, and the finalized process, reading the chain no more,
// never submitted them again once hash reads answered.
#[tokio::test(start_paused = true)]
async fn reads_by_number_do_not_confirm_results_a_reorg_took() {
    panic_hook();
    let mut run = Run::new(38);
    run.tune.confirmations = 0;
    run.start().await;
    let i = run.new_vote(0);
    run.submit(i).await;
    run.drain().await;
    assert!(!run.s.chain.results().is_empty(), "no results to reorg out");
    let b = run.cursor_at_head().await;
    // No hash reads; reads by number from the old branch, block hashes
    // canonical; a reorg the cursor sees takes the results.
    let old = run.s.chain.process(&pid31()).await.unwrap();
    run.sh.no_hash_reads.store(true, Ordering::SeqCst);
    *run.sh.stale_proc.lock().unwrap() = Some(old);
    run.s.chain.reorg_results();
    run.sh.forks.lock().unwrap().push(b);
    for _ in 0..30 {
        run.s.chain.advance_time(1);
        run.run_for(10).await;
    }
    let stalls = &run.node.as_ref().unwrap().node.metrics.hash_read_stalls;
    assert!(
        stalls.load(Ordering::SeqCst) >= 1,
        "the stall is not counted"
    );
    // Healed: the results go in again, with no new election event.
    run.sh.no_hash_reads.store(false, Ordering::SeqCst);
    *run.sh.stale_proc.lock().unwrap() = None;
    for _ in 0..60 {
        run.s.chain.advance_time(1);
        run.run_for(10).await;
    }
    assert!(
        !run.s.chain.results().is_empty(),
        "the results a reorg took were never submitted again"
    );
    run.crash().await;
}
