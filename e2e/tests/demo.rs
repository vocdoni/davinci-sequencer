//! Demo elections on a live deployment (Gnosis by default), gated by
//! `DAVINCI_E2E_DEMO`, one phase per run:
//!
//! - `prepare` draws every voter key, the CSP key and the seed behind every
//!   ballot secret and choice into the private directory
//!   (`DAVINCI_DEMO_DIR`, default `~/.davinci-gnosis/demo`, mode 0700), and
//!   writes the public census and metadata files of each election into
//!   `e2e/demo/`. Run again, it reuses the keys and rewrites the same files.
//! - `run` creates the elections of `davinci_e2e::demo::elections`, their
//!   census and metadata URIs under `DAVINCI_DEMO_BASE_URL` (the committed
//!   `e2e/demo`, e.g. on raw.githubusercontent.com at a pinned commit) and
//!   the SHA-256 of each `metadata.json` as its metadata hash. It
//!   casts the votes through the nodes in `DAVINCI_DEMO_NODES` (default
//!   `http://127.0.0.1:9090,http://127.0.0.1:9091`), runs each lifecycle and
//!   prints a table. It spawns nothing: the nodes, their provers and the DKG
//!   committee are the deployment's. Progress goes to `state.json` in the
//!   private directory, so an interrupted run resumes without duplicates.
//!
//! `check` proves every planned ballot with the circom prover against a
//! stand-in process of each election, offline; `run` does the same before it
//! creates anything. `run` also needs the organizer key file
//! `DAVINCI_DEMO_ORGANIZER_KEY`, the circom artifacts (`CIRCOM_ARTIFACTS`)
//! and the built census contract project (`DAVINCI_CENSUS_CONTRACT_DIR`). The
//! chain comes from `davinci_e2e::net::live_target` (`DAVINCI_E2E_RPC`,
//! `DAVINCI_E2E_REGISTRY`). Only addresses and process ids are printed.
//! `e2e/bench.sh` runs any phase in Docker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, U256, utils::format_ether};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{Context, Result, bail, ensure};
use davinci_client::Error as ClientError;
use davinci_client::SequencerClient;
use davinci_client::api::{Fr, ProcessId, ProcessStatus, VoteRequest, VoteStatus, vote_id_hex};
use davinci_client::organizer::{
    KeyMode, NewProcess, OnchainProcess, Organizer, OrganizerSecret, metadata_hash, verify_registry,
};
use davinci_client::prover::BallotProver;
use davinci_client::voter::random_k;
use davinci_e2e::census::{self, Census};
use davinci_e2e::cost::{self, TxCost};
use davinci_e2e::demo::{
    self, CensusKind, KeySource, Lifecycle, Planned, SecretHex, Secrets, Spec, State, VoteState,
};
use davinci_e2e::fixture as fx;
use davinci_e2e::{chain, dkg, net, wait};
use davinci_zkvm_sdk::ballot::{address_to_fr, vote_id};
use davinci_zkvm_sdk::census::{CensusWitness, LeanImt, csp_sign};
use davinci_zkvm_sdk::crypto::elgamal::keygen;
use davinci_zkvm_sdk::release;
use k256::ecdsa::SigningKey;
use rand::rngs::OsRng;

const DEFAULT_NODES: &str = "http://127.0.0.1:9090,http://127.0.0.1:9091";
/// Longest wait for a node to list a process and serve its census.
const READY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Longest wait for one round of votes to settle (90 s batches, proving,
/// 5 s blocks and confirmations).
const SETTLE_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// Longest wait for a sequencer's `requestResultsDecryption`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Longest wait for every tally after the ends and the reveal.
const RESULTS_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// Longest wait for a DKG epoch with a free pool key.
const DKG_WAIT: Duration = Duration::from_secs(20 * 60);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(600);
const PROGRESS_EVERY: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_secs(5);

fn t0() -> Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// UTC time of day.
fn clock() -> String {
    let s = unix_now();
    format!("{:02}:{:02}:{:02}Z", s / 3600 % 24, s / 60 % 60, s % 60)
}

macro_rules! say {
    ($($arg:tt)*) => {
        eprintln!("[demo {} {:>6.0}s] {}", clock(), t0().elapsed().as_secs_f64(), format!($($arg)*))
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn demo() -> Result<()> {
    let phase = std::env::var("DAVINCI_E2E_DEMO").unwrap_or_default();
    t0();
    let r = match phase.as_str() {
        "" => {
            eprintln!("DAVINCI_E2E_DEMO not set, skipping");
            return Ok(());
        }
        "prepare" => prepare(),
        "check" => check().await,
        "run" => run().await,
        p => bail!("DAVINCI_E2E_DEMO={p:.20}: prepare, check or run"),
    };
    match &r {
        Ok(()) => say!(
            "{phase} done in {:.1} min",
            t0().elapsed().as_secs_f64() / 60.0
        ),
        Err(e) => say!("{phase} FAILED: {e:#}"),
    }
    r
}

/// Voter keys (drawn once, then reused) and the public files.
fn prepare() -> Result<()> {
    let specs = demo::elections();
    let dir = demo::private_dir()?;
    let (secrets, fresh) = Secrets::load_or_generate(&dir, &specs)?;
    let keys: usize = secrets.elections.values().map(Vec::len).sum();
    say!(
        "{keys} voter keys, the CSP key and the ballot seed in {} ({})",
        dir.join(demo::SECRETS_FILE).display(),
        if fresh { "new" } else { "existing, reused" }
    );
    let out = demo::public_dir();
    let files = demo::public_files(&specs, &secrets)?;
    demo::write_public(&out, &files)?;
    for f in &files {
        say!("wrote {}", out.join(&f.path).display());
    }
    for s in &specs {
        let census = match s.census {
            CensusKind::Static => format!(
                "root {}",
                hex_fr(&demo::census_root(&secrets.parts(s.n, s.members)?)?)
            ),
            CensusKind::Updatable { .. } => format!(
                "roots {} then {}",
                hex_fr(&demo::census_root(&secrets.parts(s.n, s.members)?)?),
                hex_fr(&demo::census_root(&secrets.parts(s.n, s.total_members())?)?)
            ),
            CensusKind::Contract { .. } => "members go to a census contract at run time".into(),
            CensusKind::Csp => format!(
                "CSP signer 0x{}",
                hex::encode(davinci_zkvm_sdk::census::eth_address(
                    secrets.csp()?.verifying_key()
                ))
            ),
        };
        say!(
            "election {}: {} ({}), {} members, {census}",
            s.n,
            s.title,
            s.kind(),
            s.total_members()
        );
    }
    Ok(())
}

/// The voter keys `prepare` drew, checked against the election table.
fn load_secrets() -> Result<Secrets> {
    let dir = demo::private_dir()?;
    let secrets = Secrets::load(&dir.join(demo::SECRETS_FILE))
        .context("no voter keys: run the prepare phase first")?;
    secrets.check(&demo::elections())?;
    Ok(secrets)
}

async fn check() -> Result<()> {
    let secrets = load_secrets()?;
    let prover = tokio::task::spawn_blocking(fx::load_prover).await??;
    check_ballots(&demo::elections(), &secrets, &prover)
}

/// Proves every planned ballot against a stand-in of each election: its
/// ballot mode and census, a throwaway key and process id, fresh ballot
/// secrets. The prover verifies each proof, so a ballot the circuit refuses
/// fails here, before anything is created on-chain. Nothing is kept.
fn check_ballots(specs: &[Spec], secrets: &Secrets, prover: &BallotProver) -> Result<()> {
    let t = Instant::now();
    let (_, pk) = keygen(&mut OsRng);
    let seed = secrets.seed()?;
    let mut jobs = Vec::new();
    for spec in specs.iter().filter(|s| s.has_votes()) {
        let n = spec.n;
        let parts = secrets.parts(n, spec.total_members())?;
        let keys = secrets.keys(n)?;
        let mut p = OnchainProcess {
            id: [n as u8; 31],
            status: ProcessStatus::Ready,
            organization_id: [0; 20],
            encryption_key: pk,
            state_root: [0; 32],
            result: Vec::new(),
            start_time: 0,
            duration: 0,
            max_voters: spec.max_voters,
            voters_count: 0,
            overwritten_votes_count: 0,
            ballot_mode: spec.ballot_mode(),
            census_origin: spec.origin(),
            census_root: demo::census_root(&parts)?,
            census_contract: [0; 20],
            census_uri: String::new(),
            metadata_uri: String::new(),
            metadata_hash: [0; 32],
            dkg: None,
        };
        let witness = match spec.census {
            CensusKind::Csp => {
                p.census_root = fx::csp_root(&secrets.csp()?)?;
                Witness::Csp(secrets.csp()?, ProcessId(p.id).to_fr())
            }
            _ => Witness::Tree(demo::census_tree(&parts)?),
        };
        for v in demo::plan(spec, &secrets.weights(n)?, &seed, 2) {
            jobs.push(fx::Job {
                label: format!("election {n} voter {} round {}", v.voter, v.round),
                key: keys[v.voter].clone(),
                process: p.clone(),
                fields: v.fields,
                census: witness.of(v.voter, &parts)?,
                weight: parts[v.voter].1,
                k: random_k(&mut OsRng),
            });
        }
    }
    let reqs = tokio::task::block_in_place(|| fx::prove_all(prover, &jobs))?;
    say!(
        "the circuit takes all {} planned ballots ({:.1} s)",
        reqs.len(),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

fn hex_fr(x: &Fr) -> String {
    format!(
        "0x{}",
        hex::encode(davinci_zkvm_sdk::crypto::field::fr_to_be(x))
    )
}

/// Everything a run holds.
struct Run {
    specs: Vec<Spec>,
    secrets: Secrets,
    seed: [u8; 32],
    base: String,
    chain_id: u64,
    org: Organizer,
    nodes: Vec<SequencerClient>,
    prover: BallotProver,
    state: State,
    state_path: PathBuf,
    /// Census contracts driven this run, by election.
    censuses: BTreeMap<usize, Census>,
}

async fn run() -> Result<()> {
    let base = std::env::var("DAVINCI_DEMO_BASE_URL")
        .context("DAVINCI_DEMO_BASE_URL (where e2e/demo is served) is required")?;
    let base = base.trim().trim_end_matches('/').to_string();
    ensure!(
        base.starts_with("https://"),
        "DAVINCI_DEMO_BASE_URL must be https: the nodes fetch censuses from public hosts only"
    );
    let specs = demo::elections();
    let dir = demo::private_dir()?;
    let secrets = load_secrets()?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);

    check_published(&base, &specs, &secrets).await?;

    let (rpc, registry, _) = net::live_target()?;
    let info = verify_registry(&rpc, registry)
        .await
        .context("registry pins")?;
    say!(
        "chain {} registry {registry} verifier {} (pins ok)",
        info.chain_id,
        info.verifier
    );
    if specs.iter().any(Spec::dkg) {
        let manager = dkg::registry_manager(&rpc, registry).await?;
        match dkg::check_wiring(&rpc, registry, manager).await {
            Ok(w) => say!(
                "DKG manager {} adapter {} registration epoch {} (wiring ok)",
                w.manager,
                w.adapter,
                w.epoch
            ),
            Err(e) => say!("DKG: {e:#}; the DKG elections wait for a Live epoch"),
        }
    }

    let key = std::env::var_os("DAVINCI_DEMO_ORGANIZER_KEY")
        .context("DAVINCI_DEMO_ORGANIZER_KEY (the organizer key file) is required")?;
    let org = Organizer::connect(&rpc, net::read_key(Path::new(&key))?, registry)?
        .with_receipt_timeout(RECEIPT_TIMEOUT);
    let balance = ProviderBuilder::new()
        .connect_client(chain::rpc(&rpc)?)
        .get_balance(org.address())
        .await?;
    say!(
        "organizer {} balance {}",
        org.address(),
        format_ether(balance)
    );
    ensure!(
        balance >= U256::from(net::MIN_BALANCE),
        "the organizer holds less than {}",
        format_ether(U256::from(net::MIN_BALANCE))
    );

    let urls: Vec<String> = std::env::var("DAVINCI_DEMO_NODES")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_NODES.into())
        .split(',')
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .collect();
    ensure!(urls.len() >= 2, "DAVINCI_DEMO_NODES: two nodes needed");
    let nodes: Vec<SequencerClient> = urls.iter().map(|u| SequencerClient::new(u)).collect();
    check_nodes(&nodes, &urls, info.chain_id, registry).await?;

    let state_path = dir.join(demo::STATE_FILE);
    let mut state = State::load(&state_path)?;
    state.bind(info.chain_id, &registry.to_string(), &base)?;
    if state.base_url != base {
        say!(
            "note: the first run used {}; processes created now use {base}",
            state.base_url
        );
    }
    state.save(&state_path)?;
    let seed = secrets.seed()?;
    let prover = ballot_prover.await??;
    check_ballots(&specs, &secrets, &prover)?;
    let mut r = Run {
        specs,
        secrets,
        seed,
        base,
        chain_id: info.chain_id,
        org,
        nodes,
        prover,
        state,
        state_path,
        censuses: BTreeMap::new(),
    };
    let res = r.stages().await;
    // What the run spent, whatever happened.
    let mut bill: Vec<TxCost> = r
        .org
        .receipts()
        .iter()
        .map(|t| TxCost::of("organizer", t))
        .collect();
    for c in r.censuses.values() {
        bill.extend(c.receipts().iter().map(|(l, t)| TxCost::of(l.clone(), t)));
    }
    say!("{}", cost::report("organizer transactions this run", &bill));
    res
}

/// Every public file must be served at `base` exactly as `prepare` writes
/// it: a node that gets a 404 or another root ignores the process for good.
async fn check_published(base: &str, specs: &[Spec], secrets: &Secrets) -> Result<()> {
    // No redirects, like the nodes.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let files = demo::public_files(specs, secrets)?;
    for f in &files {
        let url = format!("{base}/{}", f.path);
        let resp = http.get(&url).send().await.with_context(|| url.clone())?;
        ensure!(
            resp.status() == reqwest::StatusCode::OK,
            "{url}: status {}",
            resp.status()
        );
        let body = resp.bytes().await.with_context(|| url.clone())?;
        ensure!(
            body.as_ref() == f.body.as_slice(),
            "{url} is not what prepare writes: commit and push e2e/demo, and point \
             DAVINCI_DEMO_BASE_URL at that commit"
        );
    }
    say!("{} public files served at {base} match", files.len());
    Ok(())
}

/// Every node must be a signing sequencer of this deployment and release.
async fn check_nodes(
    nodes: &[SequencerClient],
    urls: &[String],
    chain_id: u64,
    registry: Address,
) -> Result<()> {
    let vk_hash = chain::ballot_vk_hash()?;
    for (i, (api, url)) in nodes.iter().zip(urls).enumerate() {
        let info = api
            .info()
            .await
            .with_context(|| format!("node {} at {url}", i + 1))?;
        ensure!(
            info.chain_id == chain_id && info.process_registry == registry.0.0,
            "node {} at {url} follows chain {} registry 0x{}",
            i + 1,
            info.chain_id,
            hex::encode(info.process_registry)
        );
        ensure!(
            info.batch_program_vk == release::BATCH_PROGRAM_VK
                && info.results_program_vk == release::RESULTS_PROGRAM_VK
                && info.ballot_vk_hash == vk_hash,
            "node {} at {url}: vks differ from the release pins",
            i + 1
        );
        let addr = info
            .sequencer_address
            .filter(|_| !info.observer)
            .with_context(|| format!("node {} at {url} is an observer", i + 1))?;
        say!(
            "node {} {url}: sequencer 0x{} (settled {}, synced {})",
            i + 1,
            hex::encode(addr),
            info.settled_by_self,
            info.synced_from_others
        );
    }
    Ok(())
}

/// Whether `api` has vote `vid` (queued, settled or errored).
async fn known(api: &SequencerClient, pid: &[u8; 31], vid: u64) -> Result<bool> {
    match api.vote_status(pid, vid).await {
        Ok(_) => Ok(true),
        Err(ClientError::Api { status: 404, .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Sends `req`, retrying transport trouble, timeouts and busy answers. A
/// vote id the node already has counts as sent.
async fn submit(api: &SequencerClient, req: &VoteRequest) -> Result<()> {
    let mut tries = 0;
    loop {
        tries += 1;
        match api.submit_vote(req).await {
            Ok(())
            | Err(ClientError::Api {
                code: Some(40901), ..
            }) => return Ok(()),
            // A retry of a vote still queued, or a timed-out one that landed.
            Err(
                e @ ClientError::Api {
                    code: Some(40902), ..
                },
            ) => {
                if known(api, &req.process_id.0, req.vote_id).await? {
                    return Ok(());
                }
                return Err(e.into());
            }
            Err(
                e @ (ClientError::Http(_)
                | ClientError::Api {
                    status: 408 | 429 | 500..=599,
                    ..
                }),
            ) if tries < 10 => {
                say!("vote {}: {e}; retrying", vote_id_hex(req.vote_id));
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// How a round's ballots prove census membership.
enum Witness {
    Tree(LeanImt),
    Csp(SigningKey, Fr),
}

impl Witness {
    fn of(&self, voter: usize, parts: &[([u8; 20], u128)]) -> Result<CensusWitness> {
        Ok(match self {
            Witness::Tree(t) => CensusWitness::Merkle(t.proof(voter)?),
            Witness::Csp(csp, pid) => {
                let (addr, weight) = parts[voter];
                CensusWitness::Csp(csp_sign(csp, pid, &addr, weight, voter as u64))
            }
        })
    }
}

fn status_name(s: ProcessStatus) -> &'static str {
    match s {
        ProcessStatus::Ready => "ready",
        ProcessStatus::Ended => "ended",
        ProcessStatus::Canceled => "canceled",
        ProcessStatus::Paused => "paused",
        ProcessStatus::Results => "results",
        ProcessStatus::Unknown => "unknown",
    }
}

fn is_open(s: ProcessStatus) -> bool {
    matches!(s, ProcessStatus::Ready | ProcessStatus::Paused)
}

impl Run {
    async fn stages(&mut self) -> Result<()> {
        say!("stage 1: create the elections");
        for spec in self.specs.clone() {
            self.ensure_created(&spec).await?;
        }
        for spec in self.specs.clone().iter().filter(|s| s.has_votes()) {
            if self.round_done(spec, 2) {
                continue;
            }
            self.wait_ready(spec).await?;
        }
        say!("stage 2: first round of votes");
        self.cast(1).await?;
        say!("stage 3: census growth, then the second round");
        self.grow().await?;
        self.cast(2).await?;
        say!("stage 4: ends, reveal, cancel and results");
        self.close().await?;
        self.report().await
    }

    fn save(&self) -> Result<()> {
        self.state.save(&self.state_path)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }

    fn pid(&self, n: usize) -> Result<[u8; 31]> {
        self.state
            .elections
            .get(&n)
            .and_then(|e| e.pid)
            .map(|p| p.0)
            .with_context(|| format!("election {n} has no process yet"))
    }

    fn plan(&self, spec: &Spec) -> Result<Vec<Planned>> {
        Ok(demo::plan(
            spec,
            &self.secrets.weights(spec.n)?,
            &self.seed,
            self.nodes.len(),
        ))
    }

    /// Every vote of `round` is on record.
    fn round_done(&self, spec: &Spec, round: u8) -> bool {
        let Ok(plan) = self.plan(spec) else {
            return false;
        };
        let e = self.state.elections.get(&spec.n);
        plan.iter()
            .filter(|v| v.round == round)
            .all(|v| e.is_some_and(|e| e.vote(v.round, v.voter).is_some()))
    }

    /// Creates the process of `spec` unless the state has it, adopting one a
    /// crashed run sent.
    async fn ensure_created(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        if self.state.election(n).pid.is_some() {
            return Ok(());
        }
        if let CensusKind::Contract { .. } = spec.census {
            self.ensure_census_contract(spec, spec.members).await?;
        }
        let start = Instant::now();
        let mut wrong_ids = 0;
        loop {
            if self.adopt_pending(spec).await? {
                return Ok(());
            }
            let next = self.org.next_process_id().await?;
            let key_mode = match spec.key {
                KeySource::Node(i) => KeyMode::Sequencer(
                    self.nodes[i]
                        .new_key(&next)
                        .await
                        .with_context(|| format!("election {n}: key from node {}", i + 1))?,
                ),
                KeySource::DkgAutomatic => KeyMode::DkgAutomatic,
                KeySource::DkgLocked => KeyMode::DkgLocked,
            };
            let np = self.new_process(spec, next, key_mode)?;
            self.state.election(n).pending = Some(ProcessId(next));
            self.save()?;
            match self.org.create_process(&np).await {
                Ok(c) => {
                    let e = self.state.election(n);
                    e.pid = Some(ProcessId(c.pid));
                    e.pending = None;
                    e.organizer_secret = c
                        .organizer_secret
                        .map(|s| SecretHex::from_bytes(&s.to_be_bytes()));
                    self.save()?;
                    say!(
                        "election {n}: created {} ({})",
                        ProcessId(c.pid),
                        spec.kind()
                    );
                    return Ok(());
                }
                Err(ClientError::Reverted(r))
                    if spec.dkg()
                        && (r == "PoolExhausted" || r == "NoLiveEpoch")
                        && start.elapsed() < DKG_WAIT =>
                {
                    say!("election {n}: newProcess reverted {r}; waiting for a DKG epoch");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                Err(ClientError::WrongProcessId { created, expected }) if wrong_ids < 3 => {
                    // Another newProcess from this account landed first: no
                    // node holds the key of what was created. The pending id
                    // is judged again on the next turn.
                    wrong_ids += 1;
                    say!(
                        "election {n}: got {created} instead of {expected}, whose key no node \
                         holds; canceling it"
                    );
                    self.org
                        .cancel_process(&created.0)
                        .await
                        .with_context(|| format!("cancel {created}"))?;
                }
                Err(e) => return Err(e).with_context(|| format!("create election {n}")),
            }
        }
    }

    /// A process a crashed run sent for `spec`: adopted if it exists, else
    /// forgotten. A locked one lost its organizer secret, so it is canceled
    /// and made again.
    async fn adopt_pending(&mut self, spec: &Spec) -> Result<bool> {
        let n = spec.n;
        let Some(pending) = self.state.election(n).pending else {
            return Ok(false);
        };
        // A newProcess still in the mempool would take the id after a
        // second one was sent: judge the id once nothing is in flight.
        self.wait_nonce().await?;
        let p = match self.org.process(&pending.0).await {
            Ok(p) => Some(p),
            Err(ClientError::Chain(m)) if m == "process not found" => None,
            Err(e) => return Err(e.into()),
        };
        // Not created, or created by another transaction of the organizer.
        let Some(p) = p.filter(|p| {
            p.organization_id == self.org.address().0.0
                && p.census_origin == spec.origin()
                && p.metadata_uri.ends_with(&spec.metadata_path())
        }) else {
            self.state.election(n).pending = None;
            self.save()?;
            return Ok(false);
        };
        if spec.key == KeySource::DkgLocked {
            say!(
                "election {n}: {pending} was created but its organizer secret is lost; canceling it"
            );
            if is_open(p.status) {
                self.org.cancel_process(&pending.0).await?;
            }
            self.state.election(n).pending = None;
            self.save()?;
            return Ok(false);
        }
        let e = self.state.election(n);
        e.pid = Some(pending);
        e.pending = None;
        self.save()?;
        say!("election {n}: adopted {pending}, created by an earlier run");
        Ok(true)
    }

    /// Waits until the organizer has nothing in flight (pending nonce equal
    /// to the latest), so a transaction sent before a crash has mined or
    /// dropped.
    async fn wait_nonce(&self) -> Result<()> {
        let (p, a) = (self.org.provider(), self.org.address());
        wait::until(
            "the organizer's transactions in flight",
            RECEIPT_TIMEOUT,
            POLL,
            || async {
                let latest = p.get_transaction_count(a).latest().await?;
                let pending = p.get_transaction_count(a).pending().await?;
                Ok((pending <= latest).then_some(()))
            },
        )
        .await
    }

    fn new_process(&self, spec: &Spec, next: [u8; 31], key_mode: KeyMode) -> Result<NewProcess> {
        let n = spec.n;
        let (census_root, census_contract, census_uri) = match spec.census {
            CensusKind::Static | CensusKind::Updatable { .. } => (
                demo::census_root(&self.secrets.parts(n, spec.members)?)?,
                [0u8; 20],
                self.url(&spec.census_path(false).context("census path")?),
            ),
            // The registry reads the root from the contract.
            CensusKind::Contract { .. } => {
                let a: Address = self
                    .state
                    .elections
                    .get(&n)
                    .and_then(|e| e.census_contract.as_deref())
                    .context("no census contract")?
                    .parse()?;
                let uri = if self.chain_id == 100 {
                    format!("https://gnosisscan.io/address/{a}")
                } else {
                    format!("onchain://{a}")
                };
                (Fr::from(0u64), a.0.0, uri)
            }
            CensusKind::Csp => (
                fx::csp_root(&self.secrets.csp()?)?,
                [0u8; 20],
                self.url(&spec.census_path(false).context("CSP path")?),
            ),
        };
        let (start_time, duration) = spec.timing(unix_now());
        Ok(NewProcess {
            process_id: next,
            start_time,
            duration,
            max_voters: spec.max_voters,
            ballot_mode: spec.ballot_mode(),
            census_origin: spec.origin(),
            census_root,
            census_contract,
            census_uri,
            // The bytes check_published found at that URL.
            metadata: self.url(&spec.metadata_path()),
            metadata_hash: metadata_hash(&demo::metadata(spec)?),
            key_mode,
        })
    }

    /// The `OwnedCensus` of `spec` (deployed on first use) with at least its
    /// first `upto` members, and a root equal to theirs.
    async fn ensure_census_contract(&mut self, spec: &Spec, upto: usize) -> Result<()> {
        let n = spec.n;
        if !self.censuses.contains_key(&n) {
            let known = self.state.election(n).census_contract.clone();
            let c = match known {
                Some(a) => Census::at(a.parse()?, self.org.provider()),
                None => {
                    let c = Census::deploy(self.org.provider(), &census::census_dir())
                        .await
                        .context("deploy OwnedCensus")?;
                    self.state.election(n).census_contract = Some(c.address.to_string());
                    self.save()?;
                    say!("election {n}: OwnedCensus deployed at {}", c.address);
                    c
                }
            };
            self.censuses.insert(n, c);
        }
        let parts = self.secrets.parts(n, spec.total_members())?;
        let c = &self.censuses[&n];
        let size = usize::try_from(c.size().await?)?;
        ensure!(
            size <= parts.len(),
            "election {n}: the census contract has {size} members"
        );
        if size < upto {
            c.add_members(&parts[size..upto]).await?;
            say!("election {n}: census contract {size} -> {upto} members");
        }
        let have = size.max(upto);
        ensure!(
            c.root().await? == demo::census_root(&parts[..have])?,
            "election {n}: the contract's root is not the lean-IMT of its first {have} members"
        );
        Ok(())
    }

    /// The member the nodes must serve, and the root they must serve it at:
    /// the last member of the census in force.
    async fn census_probe(&self, spec: &Spec) -> Result<Option<([u8; 20], Fr)>> {
        let n = spec.n;
        let count = match spec.census {
            CensusKind::Csp => return Ok(None),
            CensusKind::Static => spec.members,
            CensusKind::Updatable { .. } if self.state.elections[&n].census_updated => {
                spec.total_members()
            }
            CensusKind::Updatable { .. } => spec.members,
            CensusKind::Contract { .. } => usize::try_from(self.censuses[&n].size().await?)?,
        };
        let parts = self.secrets.parts(n, count)?;
        let last = parts.last().context("empty census")?.0;
        Ok(Some((last, demo::census_root(&parts)?)))
    }

    /// Waits until every node lists `spec`'s process, agrees with the
    /// registry on it and serves the census in force.
    async fn wait_ready(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        if let CensusKind::Contract { .. } = spec.census {
            // Attach to the contract for the probe.
            self.ensure_census_contract(spec, spec.members).await?;
        }
        let pid = self.pid(n)?;
        let p = self.org.process(&pid).await?;
        let probe = self.census_probe(spec).await?;
        for (i, api) in self.nodes.iter().enumerate() {
            let t = Instant::now();
            wait::until(
                &format!("node {} to serve election {n}", i + 1),
                READY_TIMEOUT,
                POLL,
                || async {
                    if !api.processes().await?.contains(&ProcessId(pid)) {
                        return Ok(None);
                    }
                    let v = api.process(&pid).await?;
                    if v.ignored {
                        bail!(
                            "node {} ignores {}: {}",
                            i + 1,
                            ProcessId(pid),
                            v.note.unwrap_or_default()
                        );
                    }
                    if v.check_against(&p).is_err() {
                        return Ok(None);
                    }
                    let Some((addr, root)) = probe else {
                        return Ok(Some(()));
                    };
                    match api.participant(&pid, &addr).await {
                        Ok((proof, _)) if proof.root == root => Ok(Some(())),
                        Ok(_) => Ok(None),
                        // Not loaded or synced yet.
                        Err(ClientError::Api {
                            status: 400 | 404 | 429,
                            ..
                        }) => Ok(None),
                        Err(e) => Err(e.into()),
                    }
                },
            )
            .await?;
            say!(
                "election {n}: node {} serves {} ({:.0} s)",
                i + 1,
                ProcessId(pid),
                t.elapsed().as_secs_f64()
            );
        }
        Ok(())
    }

    /// Proves and sends every vote of `round` not on record yet, then waits
    /// until they all settle.
    async fn cast(&mut self, round: u8) -> Result<()> {
        for spec in self.specs.clone() {
            let n = spec.n;
            let done = self.state.elections.get(&n);
            let todo: Vec<Planned> = self
                .plan(&spec)?
                .into_iter()
                .filter(|v| v.round == round)
                .filter(|v| done.is_none_or(|e| e.vote(v.round, v.voter).is_none()))
                .collect();
            if todo.is_empty() {
                continue;
            }
            let pid = self.pid(n)?;
            let p = self.org.process(&pid).await?;
            ensure!(
                p.status == ProcessStatus::Ready,
                "election {n} is {}, not taking votes",
                status_name(p.status)
            );
            let parts = self.secrets.parts(n, spec.total_members())?;
            let keys = self.secrets.keys(n)?;
            let witness = self.witness(&spec, &p, &parts)?;
            let pid_fr = ProcessId(pid).to_fr();
            let mut jobs = Vec::new();
            let mut pending = Vec::new();
            for v in todo {
                let k = demo::vote_k(&self.seed, n, v.voter, round);
                let vid = vote_id(&pid_fr, &address_to_fr(&parts[v.voter].0), &k);
                // Sent by a run that stopped before recording it.
                if known(&self.nodes[v.node], &pid, vid).await? {
                    self.state.election(n).record(&v, vid);
                    self.save()?;
                    continue;
                }
                jobs.push(fx::Job {
                    label: format!("election {n} voter {} round {round}", v.voter),
                    key: keys[v.voter].clone(),
                    process: p.clone(),
                    fields: v.fields.clone(),
                    census: witness.of(v.voter, &parts)?,
                    weight: parts[v.voter].1,
                    k,
                });
                pending.push((v, vid));
            }
            if jobs.is_empty() {
                continue;
            }
            let t = Instant::now();
            let reqs = tokio::task::block_in_place(|| fx::prove_all(&self.prover, &jobs))?;
            say!(
                "election {n}: {} round-{round} ballots proved in {:.1} s",
                reqs.len(),
                t.elapsed().as_secs_f64()
            );
            let mut per_node = vec![0usize; self.nodes.len()];
            for (req, (v, vid)) in reqs.iter().zip(&pending) {
                ensure!(req.vote_id == *vid, "election {n}: vote id differs");
                submit(&self.nodes[v.node], req)
                    .await
                    .with_context(|| format!("election {n} voter {} round {round}", v.voter))?;
                self.state.election(n).record(v, req.vote_id);
                self.save()?;
                per_node[v.node] += 1;
            }
            say!(
                "election {n}: {} round-{round} votes sent ({})",
                reqs.len(),
                per_node
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{c} to node {}", i + 1))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        self.wait_votes(round).await
    }

    /// The census witness of a ballot for process `p` as it stands.
    fn witness(
        &self,
        spec: &Spec,
        p: &OnchainProcess,
        parts: &[([u8; 20], u128)],
    ) -> Result<Witness> {
        Ok(match spec.census {
            // The root pinned now: the first census or the replacement.
            CensusKind::Static | CensusKind::Updatable { .. } => {
                let mut found = None;
                for count in [spec.members, spec.total_members()] {
                    let t = demo::census_tree(&parts[..count])?;
                    if t.root() == p.census_root {
                        found = Some(t);
                        break;
                    }
                }
                Witness::Tree(found.with_context(|| {
                    format!(
                        "election {}: the registry's census root is not ours",
                        spec.n
                    )
                })?)
            }
            // Not pinned to a root: the contract's moves.
            CensusKind::Contract { .. } => Witness::Tree(demo::census_tree(parts)?),
            CensusKind::Csp => Witness::Csp(self.secrets.csp()?, ProcessId(p.id).to_fr()),
        })
    }

    /// Waits until no vote of `round` is still on its way; an errored vote
    /// is recorded and not waited on.
    async fn wait_votes(&mut self, round: u8) -> Result<()> {
        let start = Instant::now();
        let mut report = Instant::now();
        loop {
            let mut open: BTreeMap<(usize, String), usize> = BTreeMap::new();
            let mut changed = false;
            let ns: Vec<usize> = self.state.elections.keys().copied().collect();
            for n in ns {
                let Some(pid) = self.state.elections[&n].pid else {
                    continue;
                };
                for i in 0..self.state.elections[&n].votes.len() {
                    let v = &self.state.elections[&n].votes[i];
                    if v.round != round || v.state != VoteState::Sent {
                        continue;
                    }
                    let (node, vid, voter) = (v.node, v.vote_id, v.voter);
                    let s = match self.nodes[node].vote_status_full(&pid.0, vid).await {
                        Ok(s) => s,
                        Err(e) => {
                            *open.entry((n, format!("unreadable ({e})"))).or_default() += 1;
                            continue;
                        }
                    };
                    let v = &mut self.state.election(n).votes[i];
                    match s.status {
                        VoteStatus::Settled => {
                            v.state = VoteState::Settled;
                            changed = true;
                        }
                        VoteStatus::Error => {
                            let why = s.error.unwrap_or_default();
                            say!(
                                "election {n}: vote {} of voter {voter} errored: {why}",
                                vote_id_hex(vid)
                            );
                            v.state = VoteState::Error;
                            v.error = Some(why);
                            changed = true;
                        }
                        other => *open.entry((n, other.to_string())).or_default() += 1,
                    }
                }
            }
            if changed {
                self.save()?;
            }
            if open.is_empty() {
                say!(
                    "round {round}: every vote settled ({:.0} s)",
                    start.elapsed().as_secs_f64()
                );
                return Ok(());
            }
            let detail = || {
                open.iter()
                    .map(|((n, s), c)| format!("election {n} {s}: {c}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            ensure!(
                start.elapsed() < SETTLE_TIMEOUT,
                "round {round}: votes not settled after {SETTLE_TIMEOUT:?}: {}",
                detail()
            );
            if report.elapsed() > PROGRESS_EVERY {
                report = Instant::now();
                say!("round {round}: waiting on {}", detail());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The census changes between the rounds: the updatable census is
    /// replaced, the census contract grows. Then every node must hold the
    /// first round, which the revotes overwrite through the other node.
    async fn grow(&mut self) -> Result<()> {
        for spec in self.specs.clone() {
            let n = spec.n;
            if spec.added() == 0 || self.round_done(&spec, 2) {
                continue;
            }
            match spec.census {
                CensusKind::Updatable { .. } => {
                    let pid = self.pid(n)?;
                    let parts = self.secrets.parts(n, spec.total_members())?;
                    let root = demo::census_root(&parts)?;
                    if self.org.process(&pid).await?.census_root != root {
                        let uri = self.url(&spec.census_path(true).context("census path")?);
                        self.org
                            .set_process_census(&pid, root, &uri)
                            .await
                            .with_context(|| format!("election {n}: setProcessCensus"))?;
                        say!(
                            "election {n}: census replaced, {} -> {} members",
                            spec.members,
                            parts.len()
                        );
                    }
                    self.state.election(n).census_updated = true;
                    self.save()?;
                }
                CensusKind::Contract { .. } => {
                    self.ensure_census_contract(&spec, spec.total_members())
                        .await?;
                }
                _ => continue,
            }
            self.wait_ready(&spec).await?;
        }
        for spec in self.specs.clone() {
            if spec.revotes > 0 && !self.round_done(&spec, 2) {
                self.wait_roots(&spec).await?;
            }
        }
        Ok(())
    }

    /// Waits until every node's committed tree of `spec`'s process is at
    /// the chain's root.
    async fn wait_roots(&self, spec: &Spec) -> Result<()> {
        let pid = self.pid(spec.n)?;
        wait::until(
            &format!("the nodes' trees of election {} to reach the chain", spec.n),
            READY_TIMEOUT,
            POLL,
            || async {
                let chain = self.org.process(&pid).await?.state_root;
                for api in &self.nodes {
                    let v = api.process(&pid).await?;
                    if v.local_state_root.unwrap_or(v.state_root) != chain {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            },
        )
        .await
    }

    /// Ends the tallied elections, cancels the withdrawn one, reveals the
    /// locked key once a sequencer has asked the committee, and waits for
    /// every tally.
    async fn close(&mut self) -> Result<()> {
        for spec in self.specs.clone() {
            let n = spec.n;
            let pid = self.pid(n)?;
            let p = self.org.process(&pid).await?;
            match spec.lifecycle {
                Lifecycle::Tally if is_open(p.status) => {
                    self.org
                        .end_process(&pid)
                        .await
                        .with_context(|| format!("end election {n}"))?;
                    say!("election {n}: ended by the organizer");
                }
                Lifecycle::Canceled if is_open(p.status) => {
                    self.org
                        .cancel_process(&pid)
                        .await
                        .with_context(|| format!("cancel election {n}"))?;
                    say!("election {n}: canceled by the organizer");
                }
                _ => {}
            }
        }
        for spec in self.specs.clone() {
            if spec.key == KeySource::DkgLocked && spec.lifecycle == Lifecycle::Tally {
                self.reveal(&spec).await?;
            }
        }
        self.wait_results().await
    }

    async fn reveal(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        let e = self.state.election(n);
        if e.revealed {
            return Ok(());
        }
        let sk = OrganizerSecret::from_be_bytes(
            &e.organizer_secret
                .as_ref()
                .with_context(|| format!("election {n}: no organizer secret on record"))?
                .bytes32()?,
        )?;
        let pid = self.pid(n)?;
        if self.org.process(&pid).await?.status != ProcessStatus::Results {
            let t = Instant::now();
            wait::until(
                &format!("the decryption request of election {n}"),
                REQUEST_TIMEOUT,
                POLL,
                || async {
                    let p = self.org.process(&pid).await?;
                    Ok(p.dkg.is_some_and(|d| d.results_requested).then_some(()))
                },
            )
            .await?;
            say!(
                "election {n}: decryption requested {:.0} s after the end; results stay locked \
                 until the reveal",
                t.elapsed().as_secs_f64()
            );
            match self.org.reveal_process_key(&pid, &sk).await {
                Ok(()) => say!("election {n}: organizer key revealed"),
                Err(ClientError::Reverted(r)) if r == "AlreadyRevealed" => {}
                Err(e) => return Err(e).with_context(|| format!("reveal election {n}")),
            }
        }
        self.state.election(n).revealed = true;
        self.save()
    }

    async fn wait_results(&mut self) -> Result<()> {
        let tallied: Vec<Spec> = self
            .specs
            .iter()
            .filter(|s| s.lifecycle == Lifecycle::Tally)
            .copied()
            .collect();
        let start = Instant::now();
        let mut report = Instant::now();
        loop {
            let mut waiting = Vec::new();
            for spec in &tallied {
                let n = spec.n;
                if self.state.election(n).results.is_some() {
                    continue;
                }
                let p = self.org.process(&self.pid(n)?).await?;
                if p.status == ProcessStatus::Results {
                    say!(
                        "election {n}: results on-chain {:?} ({:.0} s after the ends)",
                        p.result,
                        start.elapsed().as_secs_f64()
                    );
                    self.state.election(n).results = Some(p.result);
                    self.save()?;
                } else {
                    waiting.push(format!("election {n} {}", status_name(p.status)));
                }
            }
            if waiting.is_empty() {
                return Ok(());
            }
            ensure!(
                start.elapsed() < RESULTS_TIMEOUT,
                "no results after {RESULTS_TIMEOUT:?}: {}",
                waiting.join(", ")
            );
            if report.elapsed() > PROGRESS_EVERY {
                report = Instant::now();
                say!("waiting for results: {}", waiting.join(", "));
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// The table, and each tally against the settled ballots.
    async fn report(&self) -> Result<()> {
        let now = unix_now();
        // An explorer to link each process to, when one is given.
        let explorer = std::env::var("DAVINCI_DEMO_EXPLORER_URL")
            .ok()
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());
        let mut rows = vec![
            "| # | process id | kind | status | voters | overwrites | transitions |".to_string(),
            "|---:|---|---|---|---:|---:|---:|".to_string(),
        ];
        if explorer.is_some() {
            rows[0].push_str(" explorer |");
            rows[1].push_str("---|");
        }
        let mut bad = Vec::new();
        let mut notes = Vec::new();
        for spec in &self.specs {
            let n = spec.n;
            let pid = self.pid(n)?;
            let p = self.org.process(&pid).await?;
            let mut transitions = "?".to_string();
            for api in &self.nodes {
                if let Ok(t) = api.transitions(&pid).await {
                    transitions = t.len().to_string();
                    break;
                }
            }
            let status = match p.status {
                ProcessStatus::Ready if now < p.start_time => {
                    format!("ready, opens in {} h", (p.start_time - now).div_ceil(3600))
                }
                ProcessStatus::Ready => format!(
                    "ready, open for {} h",
                    (p.start_time + p.duration).saturating_sub(now) / 3600
                ),
                s => status_name(s).to_string(),
            };
            let mut row = format!(
                "| {n} | {} | {} | {status} | {} | {} | {transitions} |",
                ProcessId(pid),
                spec.kind(),
                p.voters_count,
                p.overwritten_votes_count,
            );
            if let Some(url) = &explorer {
                row.push_str(&format!(" {url}/processes/{} |", ProcessId(pid)));
            }
            rows.push(row);
            let e = &self.state.elections[&n];
            let errored = e
                .votes
                .iter()
                .filter(|v| v.state == VoteState::Error)
                .count();
            if errored > 0 {
                notes.push(format!("election {n}: {errored} votes errored"));
            }
            if spec.lifecycle != Lifecycle::Tally {
                continue;
            }
            let nf = spec.num_fields();
            let want = e.expected_tally(nf);
            let ok = p.result.len() >= nf
                && p.result[..nf] == want[..]
                && p.result[nf..].iter().all(|x| *x == 0);
            let labels: Vec<String> = spec
                .choices
                .iter()
                .zip(&p.result)
                .map(|(c, r)| format!("{c}: {r}"))
                .collect();
            notes.push(format!(
                "election {n} results: {}{}",
                labels.join("; "),
                if ok { "" } else { " (MISMATCH)" }
            ));
            if !ok {
                bad.push(format!(
                    "election {n}: on-chain {:?}, settled ballots add up to {want:?}",
                    p.result
                ));
            }
        }
        eprintln!("\n{}\n", rows.join("\n"));
        for l in &notes {
            say!("{l}");
        }
        ensure!(bad.is_empty(), "{}", bad.join("; "));
        Ok(())
    }
}
