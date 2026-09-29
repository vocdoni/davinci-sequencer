//! Demo elections on a live deployment (Gnosis by default), gated by
//! `DAVINCI_E2E_DEMO`, one phase per run, for the election set
//! `DAVINCI_DEMO_WAVE` names (1, the default, or 2; see
//! `davinci_e2e::demo::Wave`):
//!
//! - `prepare` draws every voter key, the CSP key and the seed behind every
//!   ballot secret and choice into the private directory
//!   (`DAVINCI_DEMO_DIR`, default `~/.davinci-gnosis/demo`, mode 0700), and
//!   writes the public census and metadata files of each election into
//!   `e2e/demo/` (the second wave under `e2e/demo/wave2/`). Run again, it
//!   reuses the keys and rewrites the same files.
//! - `run` creates the elections, their census and metadata URIs under
//!   `DAVINCI_DEMO_BASE_URL` (the committed `e2e/demo`, e.g. on
//!   raw.githubusercontent.com at a pinned commit) and the SHA-256 of each
//!   `metadata.json` as its metadata hash (a draft's for the deliberate
//!   mismatch). It casts the votes through the nodes in `DAVINCI_DEMO_NODES`
//!   (default `http://127.0.0.1:9090,http://127.0.0.1:9091`) in up to three
//!   rounds, runs the organizer actions between the first two (metadata and
//!   census updates, reveals, pause, duration and max voters, cancels) and
//!   the votes the nodes must refuse, ends what the organizer ends, waits for
//!   every result and prints a table. It spawns nothing: the nodes, their
//!   provers and the DKG committee are the deployment's. Progress goes to the
//!   wave's state file in the private directory, so an interrupted run
//!   resumes without duplicates.
//!
//! `check` proves every planned ballot, and every refused vote's, with the
//! circom prover against a stand-in process of each election, offline; `run`
//! does the same before it creates anything. `run` also needs the organizer
//! key file `DAVINCI_DEMO_ORGANIZER_KEY`, the circom artifacts
//! (`CIRCOM_ARTIFACTS`) and the built census contract project
//! (`DAVINCI_CENSUS_CONTRACT_DIR`). The chain comes from
//! `davinci_e2e::net::live_target` (`DAVINCI_E2E_RPC`,
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
use davinci_client::api::{
    Fr, ProcessId, ProcessStatus, ProcessView, TransitionView, VoteRequest, VoteStatus, vote_id_hex,
};
use davinci_client::organizer::{
    KeyMode, NewProcess, OnchainProcess, Organizer, OrganizerSecret, verify_registry,
};
use davinci_client::prover::BallotProver;
use davinci_client::voter::random_k;
use davinci_e2e::census::{self, Census};
use davinci_e2e::cost::{self, TxCost};
use davinci_e2e::demo::{
    self, Action, CensusKind, KeySource, Lifecycle, MetaPlan, Planned, Refusal, RefusedVote,
    SecretHex, Secrets, Spec, State, VoteState, Wave,
};
use davinci_e2e::fixture as fx;
use davinci_e2e::{chain, dkg, net, wait};
use davinci_zkvm_sdk::ballot::{address_to_fr, vote_id};
use davinci_zkvm_sdk::census::{CensusWitness, LeanImt, csp_sign, eth_address, vote_id_sign};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
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
/// Longest wait for every tally after the ends and the reveal, or after the
/// last timed end.
const RESULTS_TIMEOUT: Duration = Duration::from_secs(45 * 60);
/// Longest wait for a DKG epoch with a free pool key.
const DKG_WAIT: Duration = Duration::from_secs(20 * 60);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(600);
const PROGRESS_EVERY: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_secs(5);
/// Between the chunks of a chunked first round: past the 90 s batch time,
/// so each chunk seals on its own.
const CHUNK_GAP: Duration = Duration::from_secs(100);
/// How long the votes sent to a paused election are watched before it
/// resumes: more than two batch windows.
const PAUSE_HOLD: Duration = Duration::from_secs(240);
/// A timed election this close to its end takes no more votes: they would
/// not settle in time.
const LATE_MARGIN: u64 = 10 * 60;
/// Longest wait for the reweighted member's pending vote to leave the queue.
const REWEIGHT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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
    if phase.is_empty() {
        eprintln!("DAVINCI_E2E_DEMO not set, skipping");
        return Ok(());
    }
    let wave = Wave::from_env()?;
    let r = match phase.as_str() {
        "prepare" => prepare(wave),
        "check" => check(wave).await,
        "run" => run(wave).await,
        p => bail!("DAVINCI_E2E_DEMO={p:.20}: prepare, check or run"),
    };
    match &r {
        Ok(()) => say!(
            "wave {} {phase} done in {:.1} min",
            wave.number(),
            t0().elapsed().as_secs_f64() / 60.0
        ),
        Err(e) => say!("wave {} {phase} FAILED: {e:#}", wave.number()),
    }
    r
}

/// Voter keys (drawn once, then reused) and the public files.
fn prepare(wave: Wave) -> Result<()> {
    let specs = wave.elections();
    let path = demo::private_dir()?.join(wave.secrets_file());
    let (secrets, fresh) = Secrets::load_or_generate(&path, &specs)?;
    let keys: usize = secrets.elections.values().map(Vec::len).sum();
    say!(
        "wave {}: {keys} voter keys, the CSP key and the ballot seed in {} ({})",
        wave.number(),
        path.display(),
        if fresh { "new" } else { "existing, reused" }
    );
    let out = demo::public_dir();
    let files = demo::public_files(&specs, &secrets)?;
    demo::write_public(&out, &files)?;
    for f in &files {
        say!("wrote {}", out.join(&f.path).display());
    }
    for s in &specs {
        let root = |updated| -> Result<String> {
            Ok(hex_fr(&demo::census_root(&demo::census_parts(
                s, &secrets, updated,
            )?)?))
        };
        let census = match s.census {
            CensusKind::Static => format!("root {}", root(false)?),
            CensusKind::Updatable { .. } => {
                format!("roots {} then {}", root(false)?, root(true)?)
            }
            CensusKind::Contract { .. } => "members go to a census contract at run time".into(),
            CensusKind::Csp => format!(
                "CSP signer 0x{}",
                hex::encode(eth_address(secrets.csp()?.verifying_key()))
            ),
        };
        say!(
            "election {}: {} ({}; {}), {} members, {census}",
            s.n,
            s.title,
            s.kind(),
            s.lifecycle_name(),
            s.total_members()
        );
    }
    Ok(())
}

/// The voter keys `prepare` drew, checked against the election table.
fn load_secrets(wave: Wave) -> Result<Secrets> {
    let path = demo::private_dir()?.join(wave.secrets_file());
    let secrets = Secrets::load(&path).context("no voter keys: run the prepare phase first")?;
    secrets.check(&wave.elections())?;
    Ok(secrets)
}

async fn check(wave: Wave) -> Result<()> {
    let secrets = load_secrets(wave)?;
    let prover = tokio::task::spawn_blocking(fx::load_prover).await??;
    check_ballots(&wave.elections(), &secrets, &prover)
}

/// A stand-in of `spec`'s process: its ballot mode and origin, key `pk`,
/// census root `root`.
fn stand_in(spec: &Spec, pk: Point, root: Fr) -> OnchainProcess {
    OnchainProcess {
        id: [spec.n as u8; 31],
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
        census_root: root,
        census_contract: [0; 20],
        census_uri: String::new(),
        metadata_uri: String::new(),
        metadata_hash: [0; 32],
        dkg: None,
        grace: 0,
        last_vote_at: 0,
    }
}

/// The stand-in of `spec` under its first or updated census, with the
/// witness and the members it proves.
fn stand_in_census(
    spec: &Spec,
    secrets: &Secrets,
    pk: Point,
    updated: bool,
) -> Result<(OnchainProcess, Witness, Parts)> {
    Ok(match spec.census {
        CensusKind::Csp => {
            let csp = secrets.csp()?;
            let p = stand_in(spec, pk, fx::csp_root(&csp)?);
            let pid = ProcessId(p.id).to_fr();
            let parts = secrets.parts(spec.n, spec.total_members())?;
            (p, Witness::Csp(csp, pid), parts)
        }
        CensusKind::Contract { .. } => {
            let parts = secrets.parts(spec.n, spec.total_members())?;
            let t = demo::census_tree(&parts)?;
            (stand_in(spec, pk, t.root()), Witness::Tree(t), parts)
        }
        _ => {
            let parts = demo::census_parts(spec, secrets, updated)?;
            let t = demo::census_tree(&parts)?;
            (stand_in(spec, pk, t.root()), Witness::Tree(t), parts)
        }
    })
}

/// Proves every planned ballot, and every refused vote's, against a
/// stand-in of each election: its ballot mode and census, a throwaway key
/// and process id, fresh ballot secrets. The prover verifies each proof, so
/// a ballot the circuit refuses fails here, before anything is created
/// on-chain; and every ballot outside the rules must be refused by the
/// client. Nothing is kept.
fn check_ballots(specs: &[Spec], secrets: &Secrets, prover: &BallotProver) -> Result<()> {
    let t = Instant::now();
    let (_, pk) = keygen(&mut OsRng);
    let seed = secrets.seed()?;
    let mut jobs = Vec::new();
    let mut strict = Vec::new();
    for spec in specs {
        let n = spec.n;
        let keys = secrets.keys(n)?;
        let plan = demo::plan(spec, &secrets.weights(n)?, &seed, 2);
        for v in &plan {
            let updated = demo::under_updated_census(spec, v);
            let (p, witness, parts) = stand_in_census(spec, secrets, pk, updated)?;
            ensure!(
                parts[v.voter].1 == u128::from(v.weight),
                "election {n} voter {} round {}: census weight {}, planned {}",
                v.voter,
                v.round,
                parts[v.voter].1,
                v.weight
            );
            jobs.push(fx::Job {
                label: format!("election {n} voter {} round {}", v.voter, v.round),
                key: keys[v.voter].clone(),
                process: p,
                fields: v.fields.clone(),
                census: witness.of(v.voter, &parts)?,
                weight: parts[v.voter].1,
                k: random_k(&mut OsRng),
            });
        }
        for r in spec.refusals() {
            let updated = matches!(spec.census, CensusKind::Updatable { .. });
            let (p, witness, parts) = stand_in_census(spec, secrets, pk, updated)?;
            let first = plan.iter().find(|v| v.round == 1);
            let reused = first.map(|v| (v.voter, v.fields.as_slice()));
            let rb = refusal_ballot(spec, secrets, &seed, r, &p, &witness, &parts, reused)?;
            strict.extend(rb.strict);
            jobs.push(rb.job);
        }
    }
    let reqs = tokio::task::block_in_place(|| fx::prove_all(prover, &jobs))?;
    for j in &strict {
        ensure!(
            fx::prove_all(prover, std::slice::from_ref(j)).is_err(),
            "{}: the client proved a ballot outside the rules",
            j.label
        );
    }
    say!(
        "the circuit takes all {} planned and refused-vote ballots, and the client refuses the \
         {} outside the rules ({:.1} s)",
        reqs.len(),
        strict.len(),
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// A refused vote to prove.
struct RefusalBallot {
    job: fx::Job,
    /// Signs the vote id instead of the voter: a bad signature.
    resign: Option<SigningKey>,
    /// The same ballot under the election's own rules, which the client
    /// must refuse to prove.
    strict: Option<fx::Job>,
}

/// The ballot of refused vote `r` of `spec` for process `p`, whose census
/// `witness` proves `parts`. `reused` is the settled round-1 voter whose
/// ballot secret a reused vote id takes, with that ballot.
#[allow(clippy::too_many_arguments)]
fn refusal_ballot(
    spec: &Spec,
    secrets: &Secrets,
    seed: &[u8; 32],
    r: Refusal,
    p: &OnchainProcess,
    witness: &Witness,
    parts: &[([u8; 20], u128)],
    reused: Option<(usize, &[u64])>,
) -> Result<RefusalBallot> {
    let n = spec.n;
    let label = format!("election {n}: {}", r.describe());
    let keys = secrets.keys(n)?;
    let member = |m: usize| -> Result<fx::Job> {
        let weight = parts.get(m).context("refusing member")?.1;
        Ok(fx::Job {
            label: label.clone(),
            key: keys[m].clone(),
            process: p.clone(),
            fields: demo::choose(spec.ballot, spec.lean, u64::try_from(weight)?, &mut OsRng),
            census: witness.of(m, parts)?,
            weight,
            k: demo::refusal_k(seed, n, m, r),
        })
    };
    let taker = || {
        spec.refusal_member(r)
            .with_context(|| format!("election {n}: no member casts {}", r.describe()))
    };
    Ok(match r {
        // Proved against a census of its own: the inputs hash does not bind
        // the census root, so the proof is valid; the node finds no member.
        Refusal::NotInCensus => {
            let key = demo::outsider(seed, n);
            let weight = u128::from(spec.weights.0);
            let tree = demo::census_tree(&[(eth_address(key.verifying_key()), weight)])?;
            let mut own = p.clone();
            own.census_root = tree.root();
            RefusalBallot {
                job: fx::Job {
                    label,
                    key,
                    process: own,
                    fields: demo::choose(spec.ballot, spec.lean, spec.weights.0, &mut OsRng),
                    census: CensusWitness::Merkle(tree.proof(0)?),
                    weight,
                    k: demo::refusal_k(seed, n, spec.total_members(), r),
                },
                resign: None,
                strict: None,
            }
        }
        Refusal::ReusedVoteId => {
            let (v, prev) = reused.context("no settled first-round vote to reuse")?;
            let weight = parts[v].1;
            RefusalBallot {
                job: fx::Job {
                    label,
                    key: keys[v].clone(),
                    process: p.clone(),
                    fields: demo::revote(
                        spec.ballot,
                        spec.lean,
                        u64::try_from(weight)?,
                        prev,
                        &mut OsRng,
                    ),
                    census: witness.of(v, parts)?,
                    weight,
                    k: demo::vote_k(seed, n, v, 1),
                },
                resign: None,
                strict: None,
            }
        }
        Refusal::BadSignature => {
            let m = taker()?;
            RefusalBallot {
                job: member(m)?,
                resign: Some(keys[(m + 1) % keys.len()].clone()),
                strict: None,
            }
        }
        // Valid under looser rules, so it proves; the node's inputs hash
        // over the registry's ballot mode differs.
        Refusal::BreaksRules => {
            let m = taker()?;
            let (fields, looser) =
                demo::breaking_ballot(spec).context("no ballot outside these rules")?;
            let strict = fx::Job {
                fields: fields.clone(),
                ..member(m)?
            };
            let mut loose = p.clone();
            loose.ballot_mode = looser;
            RefusalBallot {
                job: fx::Job {
                    fields,
                    process: loose,
                    ..member(m)?
                },
                resign: None,
                strict: Some(strict),
            }
        }
        Refusal::OverMaxVoters | Refusal::AfterEnd => RefusalBallot {
            job: member(taker()?)?,
            resign: None,
            strict: None,
        },
    })
}

fn hex_fr(x: &Fr) -> String {
    format!(
        "0x{}",
        hex::encode(davinci_zkvm_sdk::crypto::field::fr_to_be(x))
    )
}

/// Everything a run holds.
struct Run {
    wave: Wave,
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

async fn run(wave: Wave) -> Result<()> {
    let base = std::env::var("DAVINCI_DEMO_BASE_URL")
        .context("DAVINCI_DEMO_BASE_URL (where e2e/demo is served) is required")?;
    let base = base.trim().trim_end_matches('/').to_string();
    ensure!(
        base.starts_with("https://"),
        "DAVINCI_DEMO_BASE_URL must be https: the nodes fetch censuses from public hosts only"
    );
    let specs = wave.elections();
    let dir = demo::private_dir()?;
    let secrets = load_secrets(wave)?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);

    check_published(&base, &specs, &secrets).await?;

    let (rpc, registry, _) = net::live_target()?;
    let info = verify_registry(&rpc, registry)
        .await
        .context("registry pins")?;
    say!(
        "wave {}: chain {} registry {registry} verifier {} (pins ok)",
        wave.number(),
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

    let state_path = dir.join(wave.state_file());
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
        wave,
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

/// Sends a vote that must be refused, once: the node's answer, `(200, None,
/// "")` if it took it. Only transport trouble and 5xx are retried.
async fn submit_refused(
    api: &SequencerClient,
    req: &VoteRequest,
) -> Result<(u16, Option<u32>, String)> {
    let mut tries = 0;
    loop {
        tries += 1;
        match api.submit_vote(req).await {
            Ok(()) => return Ok((200, None, String::new())),
            Err(
                e @ (ClientError::Http(_)
                | ClientError::Api {
                    status: 500..=599, ..
                }),
            ) if tries < 5 => {
                say!("refused vote {}: {e}; retrying", vote_id_hex(req.vote_id));
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            Err(ClientError::Api {
                status,
                code,
                message,
            }) => return Ok((status, code, message)),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Census members and weights, in leaf order.
type Parts = Vec<([u8; 20], u128)>;

/// A public RPC refusing a burst, past the client's own retries.
fn rate_limited(e: &ClientError) -> bool {
    matches!(e, ClientError::Chain(m) if m.contains("429") || m.contains("Too Many Requests"))
}

/// Process `pid` as the registry has it, riding out RPC rate limits: every
/// public Gnosis RPC answers 429 to a busy host for a minute or two.
async fn read_process(org: &Organizer, pid: &[u8; 31]) -> Result<OnchainProcess> {
    let mut tries = 0;
    loop {
        match org.process(pid).await {
            Err(e) if rate_limited(&e) && tries < 10 => {
                tries += 1;
                say!("registry read rate-limited ({tries}); retrying in 30 s");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            r => return Ok(r?),
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

/// Taking votes by the registry: open, started and not past its end.
fn voting(p: &OnchainProcess, now: u64) -> bool {
    is_open(p.status) && now >= p.start_time && now < p.start_time + p.duration
}

impl Run {
    async fn stages(&mut self) -> Result<()> {
        say!("stage 1: create the elections");
        // The ones that end by time last: their clocks start at creation.
        let mut order = self.specs.clone();
        order.sort_by_key(|s| matches!(s.lifecycle, Lifecycle::Timed { .. }));
        for spec in order {
            self.ensure_created(&spec).await?;
            self.after_creation(&spec).await?;
        }
        for spec in self.specs.clone() {
            if !spec.votes_in(1) || self.round_done(&spec, spec.last_round()) {
                continue;
            }
            self.wait_ready(&spec).await?;
        }
        say!("stage 2: first round of votes");
        self.send_round(1).await?;
        self.wait_votes(1).await?;
        say!("stage 3: census changes, organizer actions and refused votes");
        self.midway().await?;
        say!("stage 4: second round");
        self.send_round(2).await?;
        self.hold_paused().await?;
        self.wait_votes(2).await?;
        if self.specs.iter().any(|s| s.votes_in(3)) {
            say!("stage 5: third round, once the later elections open");
            self.open_later().await?;
            self.send_round(3).await?;
            self.wait_votes(3).await?;
        }
        say!("stage 6: ends, reveals, cancels, late votes and results");
        self.close().await?;
        self.report().await
    }

    fn save(&self) -> Result<()> {
        self.state.save(&self.state_path)
    }

    async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess> {
        read_process(&self.org, pid).await
    }

    /// Marks `step` done for election `n` and notes `line`.
    fn done(&mut self, n: usize, step: &str, line: String) -> Result<()> {
        say!("election {n}: {line}");
        let e = self.state.election(n);
        e.mark(step);
        e.note(line);
        self.save()
    }

    fn is_done(&self, n: usize, step: &str) -> bool {
        self.state
            .elections
            .get(&n)
            .is_some_and(|e| e.is_done(step))
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

    /// The transitions a node serves for `pid`, from the first that answers.
    async fn transitions(&self, pid: &[u8; 31]) -> Option<Vec<TransitionView>> {
        for api in &self.nodes {
            if let Ok(t) = api.transitions(pid).await {
                return Some(t);
            }
        }
        None
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
                KeySource::DkgLocked | KeySource::DkgLockedEarly => KeyMode::DkgLocked,
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
        if spec.locked() {
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
        let (census_root, census_contract, census_uri) = match spec.census {
            CensusKind::Static | CensusKind::Updatable { .. } => (
                demo::census_root(&demo::census_parts(spec, &self.secrets, false)?)?,
                [0u8; 20],
                self.url(&spec.census_path(false).context("census path")?),
            ),
            // The registry reads the root from the contract.
            CensusKind::Contract { .. } => {
                let a: Address = self
                    .state
                    .elections
                    .get(&spec.n)
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
            // The bytes check_published found at that URL; the mismatch
            // registers a draft's hash instead.
            metadata: self.url(&spec.metadata_path()),
            metadata_hash: demo::registered_hash(spec)?,
            key_mode,
        })
    }

    /// What happens right after creation: a cancel before the start, a
    /// metadata update before the start.
    async fn after_creation(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        let pid = self.pid(n)?;
        if let Lifecycle::CanceledEarly { .. } = spec.lifecycle
            && !self.is_done(n, "cancel")
        {
            let p = self.process(&pid).await?;
            if is_open(p.status) {
                self.org
                    .cancel_process(&pid)
                    .await
                    .with_context(|| format!("cancel election {n}"))?;
            }
            let ahead = p.start_time.saturating_sub(unix_now()) / 60;
            self.done(
                n,
                "cancel",
                format!("canceled {ahead} min before its start"),
            )?;
        }
        if let MetaPlan::BeforeStart(_) = spec.metadata
            && !self.is_done(n, "metadata")
        {
            let (path, hash) = demo::metadata_update(spec)?.context("no metadata update")?;
            let p = self.process(&pid).await?;
            let ahead = p.start_time.saturating_sub(unix_now());
            if ahead > 0 {
                self.org
                    .set_process_metadata(&pid, &self.url(&path), hash)
                    .await
                    .with_context(|| format!("election {n}: setProcessMetadata"))?;
                let line = format!(
                    "metadata updated {} min before the start, to {path}",
                    ahead / 60
                );
                self.done(n, "metadata", line)?;
            } else {
                self.done(n, "metadata", "started before the metadata update".into())?;
            }
        }
        Ok(())
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
        let parts = match spec.census {
            CensusKind::Csp => return Ok(None),
            CensusKind::Static | CensusKind::Updatable { .. } => {
                let updated = self.state.elections[&n].census_updated;
                demo::census_parts(spec, &self.secrets, updated)?
            }
            CensusKind::Contract { .. } => {
                let size = usize::try_from(self.censuses[&n].size().await?)?;
                self.secrets.parts(n, size)?
            }
        };
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
        let p = self.process(&pid).await?;
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

    /// Sends every vote of `round` not on record yet. Chunked elections go
    /// last, since they take a while.
    async fn send_round(&mut self, round: u8) -> Result<()> {
        let mut specs = self.specs.clone();
        specs.sort_by_key(|s| s.chunk > 0);
        for spec in specs {
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
            let p = self.process(&pid).await?;
            let paused = round == 2 && spec.pauses() && p.status == ProcessStatus::Paused;
            let (now, end) = (unix_now(), p.start_time + p.duration);
            ensure!(now >= p.start_time, "election {n} has not started");
            let closed = !(p.status == ProcessStatus::Ready || paused);
            if closed || end < now + LATE_MARGIN {
                let line = if closed {
                    format!(
                        "round {round}: {} votes not sent, the election is {}",
                        todo.len(),
                        status_name(p.status)
                    )
                } else {
                    format!(
                        "round {round}: {} votes not sent, the election closes in {} s",
                        todo.len(),
                        end.saturating_sub(now)
                    )
                };
                say!("election {n}: {line}");
                self.state.election(n).note(line);
                self.save()?;
                continue;
            }
            self.send(&spec, todo).await?;
        }
        Ok(())
    }

    /// The census witness of a ballot for process `p` as it stands, and the
    /// members and weights it proves.
    fn witness(&self, spec: &Spec, p: &OnchainProcess) -> Result<(Witness, Parts)> {
        let n = spec.n;
        Ok(match spec.census {
            // The root pinned now: the first census or the replacement.
            CensusKind::Static | CensusKind::Updatable { .. } => {
                for updated in [false, true] {
                    let parts = demo::census_parts(spec, &self.secrets, updated)?;
                    let t = demo::census_tree(&parts)?;
                    if t.root() == p.census_root {
                        return Ok((Witness::Tree(t), parts));
                    }
                }
                bail!("election {n}: the registry's census root is not ours")
            }
            // Not pinned to a root: the contract's moves.
            CensusKind::Contract { .. } => {
                let parts = self.secrets.parts(n, spec.total_members())?;
                (Witness::Tree(demo::census_tree(&parts)?), parts)
            }
            CensusKind::Csp => (
                Witness::Csp(self.secrets.csp()?, ProcessId(p.id).to_fr()),
                self.secrets.parts(n, spec.total_members())?,
            ),
        })
    }

    /// Proves and sends `todo`, votes of one round of `spec` not on record,
    /// recording each as it goes. A chunked first round goes out a chunk at
    /// a time, `CHUNK_GAP` apart.
    async fn send(&mut self, spec: &Spec, todo: Vec<Planned>) -> Result<()> {
        let n = spec.n;
        let pid = self.pid(n)?;
        let p = self.process(&pid).await?;
        let (witness, parts) = self.witness(spec, &p)?;
        let keys = self.secrets.keys(n)?;
        let pid_fr = ProcessId(pid).to_fr();
        let mut jobs = Vec::new();
        let mut pending = Vec::new();
        for v in todo {
            let (addr, weight) = parts[v.voter];
            ensure!(
                weight == u128::from(v.weight),
                "election {n} voter {}: census weight {weight}, planned {}",
                v.voter,
                v.weight
            );
            let k = demo::vote_k(&self.seed, n, v.voter, v.round);
            let vid = vote_id(&pid_fr, &address_to_fr(&addr), &k);
            // Sent by a run that stopped before recording it.
            if known(&self.nodes[v.node], &pid, vid).await? {
                self.state.election(n).record(&v, vid);
                self.save()?;
                continue;
            }
            jobs.push(fx::Job {
                label: format!("election {n} voter {} round {}", v.voter, v.round),
                key: keys[v.voter].clone(),
                process: p.clone(),
                fields: v.fields.clone(),
                census: witness.of(v.voter, &parts)?,
                weight,
                k,
            });
            pending.push((v, vid));
        }
        let Some(round) = pending.first().map(|(v, _)| v.round) else {
            return Ok(());
        };
        let t = Instant::now();
        let reqs = tokio::task::block_in_place(|| fx::prove_all(&self.prover, &jobs))?;
        say!(
            "election {n}: {} round-{round} ballots proved in {:.1} s",
            reqs.len(),
            t.elapsed().as_secs_f64()
        );
        let chunk = if spec.chunk > 0 && round == 1 {
            spec.chunk
        } else {
            reqs.len()
        };
        let mut per_node = vec![0usize; self.nodes.len()];
        for (i, (req, (v, vid))) in reqs.iter().zip(&pending).enumerate() {
            if i > 0 && i % chunk == 0 {
                say!(
                    "election {n}: {i} of {} sent; {} more in {} s",
                    reqs.len(),
                    chunk.min(reqs.len() - i),
                    CHUNK_GAP.as_secs()
                );
                tokio::time::sleep(CHUNK_GAP).await;
            }
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
        Ok(())
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

    /// Between the first and the second round: the census changes, the
    /// organizer's actions, the refused votes and, last, the pause.
    async fn midway(&mut self) -> Result<()> {
        self.grow().await?;
        for spec in self.specs.clone() {
            self.organizer_actions(&spec).await?;
        }
        // Revotes and refused votes need every node's tree at the chain's
        // root: an overwrite, a reused id and max voters are judged on it.
        for spec in self.specs.clone() {
            let refusals = spec.refusals().into_iter().any(|r| {
                r != Refusal::AfterEnd
                    && self
                        .state
                        .elections
                        .get(&spec.n)
                        .is_none_or(|e| e.refusal(r).is_none())
            });
            let revotes = spec.revotes > 0 && !self.round_done(&spec, 2);
            if (refusals || revotes) && voting(&self.process(&self.pid(spec.n)?).await?, unix_now())
            {
                self.wait_roots(&spec).await?;
            }
        }
        for spec in self.specs.clone() {
            for r in spec.refusals() {
                if r != Refusal::AfterEnd {
                    self.refuse(&spec, r).await?;
                }
            }
        }
        for spec in self.specs.clone() {
            if spec.pauses() {
                self.pause(&spec).await?;
            }
        }
        Ok(())
    }

    /// The census changes between the rounds: the updatable census is
    /// replaced (reweighting a member whose vote is pending), the census
    /// contract grows.
    async fn grow(&mut self) -> Result<()> {
        for spec in self.specs.clone() {
            let n = spec.n;
            if spec.added() == 0 || self.round_done(&spec, spec.last_round()) {
                continue;
            }
            match spec.census {
                CensusKind::Updatable { reweight, .. } => {
                    let pid = self.pid(n)?;
                    let parts = demo::census_parts(&spec, &self.secrets, true)?;
                    let root = demo::census_root(&parts)?;
                    if self.process(&pid).await?.census_root != root {
                        // The member's vote goes in first, so it is pending
                        // when the census changes under it.
                        if let Some(x) = reweight {
                            let v = self
                                .plan(&spec)?
                                .into_iter()
                                .find(|v| v.round == 2 && v.voter == x)
                                .context("the reweighted member's second-round vote")?;
                            if self.state.election(n).vote(2, x).is_none() {
                                self.send(&spec, vec![v]).await?;
                            }
                        }
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
                    if let Some(x) = reweight {
                        self.watch_reweight(&spec, x).await?;
                    }
                }
                CensusKind::Contract { .. } => {
                    self.ensure_census_contract(&spec, spec.total_members())
                        .await?;
                }
                _ => continue,
            }
            self.wait_ready(&spec).await?;
        }
        Ok(())
    }

    /// Waits until the reweighted member's second-round vote leaves the
    /// queue, and notes how.
    async fn watch_reweight(&mut self, spec: &Spec, x: usize) -> Result<()> {
        let n = spec.n;
        let pid = self.pid(n)?;
        let Some(v) = self.state.election(n).vote(2, x).cloned() else {
            return Ok(());
        };
        if v.state != VoteState::Sent {
            return Ok(());
        }
        let api = &self.nodes[v.node];
        let got = wait::until(
            &format!("the reweighted member's vote in election {n}"),
            REWEIGHT_TIMEOUT,
            POLL,
            || async {
                let s = api.vote_status_full(&pid, v.vote_id).await?;
                Ok(matches!(s.status, VoteStatus::Settled | VoteStatus::Error).then_some(s))
            },
        )
        .await;
        let w = self.secrets.weights(n)?;
        let (old, new) = (w[x], spec.weight_at(&w, x, 3));
        let vid = vote_id_hex(v.vote_id);
        let line = match got {
            Ok(s) if s.status == VoteStatus::Error => {
                let why = s.error.unwrap_or_default();
                let r = self
                    .state
                    .election(n)
                    .votes
                    .iter_mut()
                    .find(|r| r.vote_id == v.vote_id);
                if let Some(r) = r {
                    r.state = VoteState::Error;
                    r.error = Some(why.clone());
                }
                format!(
                    "member {x} was reweighted {old} -> {new} while vote {vid} was pending: the \
                     node errored it ({why}); the member recasts in round 3"
                )
            }
            Ok(_) => {
                if let Some(r) = self
                    .state
                    .election(n)
                    .votes
                    .iter_mut()
                    .find(|r| r.vote_id == v.vote_id)
                {
                    r.state = VoteState::Settled;
                }
                format!(
                    "member {x}'s vote {vid} settled before the reweight {old} -> {new} reached \
                     the node; the round-3 ballot overwrites it"
                )
            }
            Err(e) => format!("member {x}'s vote {vid} still queued after the reweight: {e:#}"),
        };
        self.done(n, "reweight", line)
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
                let chain = self.process(&pid).await?.state_root;
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

    /// Waits until every node's view of election `n` passes `ok`: the
    /// nodes apply registry events a few blocks late.
    async fn wait_nodes(
        &self,
        n: usize,
        what: &str,
        ok: impl Fn(&ProcessView) -> bool,
    ) -> Result<()> {
        let pid = self.pid(n)?;
        for (i, api) in self.nodes.iter().enumerate() {
            let ok = &ok;
            wait::until(
                &format!("node {} to apply {what} of election {n}", i + 1),
                READY_TIMEOUT,
                POLL,
                || async move { Ok(ok(&api.process(&pid).await?).then_some(())) },
            )
            .await?;
        }
        Ok(())
    }

    /// The organizer's steps once the first round settled: the metadata
    /// update while open, the early reveal, the duration and max voters, the
    /// cancel during voting. Each runs once; a closed election skips them.
    async fn organizer_actions(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        let Ok(pid) = self.pid(n) else {
            return Ok(());
        };
        let mut steps: Vec<&str> = Vec::new();
        if let MetaPlan::WhileOpen(_) = spec.metadata {
            steps.push("metadata");
        }
        if spec.key == KeySource::DkgLockedEarly {
            steps.push("reveal");
        }
        for a in spec.actions {
            match a {
                Action::Extend(_) => steps.push("extend"),
                Action::Shorten(_) => steps.push("shorten"),
                Action::MaxVoters(_) => steps.push("max-voters"),
                _ => {}
            }
        }
        if spec.lifecycle == Lifecycle::Canceled && spec.has_votes() {
            steps.push("cancel");
        }
        for step in steps {
            if self.is_done(n, step) {
                continue;
            }
            let p = self.process(&pid).await?;
            if step == "cancel" && p.status == ProcessStatus::Canceled {
                self.done(n, step, "canceled during voting".into())?;
                continue;
            }
            if !voting(&p, unix_now()) {
                let line = format!("{step} skipped: the election is not open");
                self.done(n, step, line)?;
                continue;
            }
            let batches = self.transitions(&pid).await.map_or(0, |t| t.len());
            let line = match step {
                "metadata" => {
                    let (path, hash) = demo::metadata_update(spec)?.context("no update")?;
                    self.org
                        .set_process_metadata(&pid, &self.url(&path), hash)
                        .await
                        .with_context(|| format!("election {n}: setProcessMetadata"))?;
                    format!("metadata updated while open, after {batches} batches, to {path}")
                }
                "reveal" => {
                    self.reveal_key(n).await?;
                    format!("organizer key revealed while open, after {batches} batches")
                }
                "extend" | "shorten" => {
                    let secs = spec
                        .actions
                        .iter()
                        .find_map(|a| match (a, step) {
                            (Action::Extend(s), "extend") | (Action::Shorten(s), "shorten") => {
                                Some(*s)
                            }
                            _ => None,
                        })
                        .context("duration step")?;
                    // From the duration it was created with, so a resumed
                    // run does not extend twice.
                    let (_, created) = spec.timing(0);
                    let d = if step == "extend" {
                        created + secs
                    } else {
                        created.saturating_sub(secs)
                    };
                    let (from, to) = (p.duration / 60, d / 60);
                    if step == "extend" && p.duration >= d {
                        self.done(n, step, format!("duration already {from} min"))?;
                        continue;
                    }
                    match self.org.set_process_duration(&pid, d).await {
                        Ok(()) => {
                            self.wait_nodes(n, "the new duration", |v| v.duration == d)
                                .await?;
                            format!("duration {from} -> {to} min")
                        }
                        // The registry only moves the end later.
                        Err(ClientError::Reverted(r)) => {
                            format!("duration {from} -> {to} min refused by the registry: {r}")
                        }
                        Err(e) => return Err(e).context(format!("election {n}: duration")),
                    }
                }
                "max-voters" => {
                    let m = spec
                        .actions
                        .iter()
                        .find_map(|a| match a {
                            Action::MaxVoters(m) => Some(*m),
                            _ => None,
                        })
                        .context("max voters")?;
                    self.org
                        .set_process_max_voters(&pid, m)
                        .await
                        .with_context(|| format!("election {n}: setProcessMaxVoters"))?;
                    // A node still on the old cap refuses the new voters.
                    self.wait_nodes(n, "the new max voters", |v| v.max_voters == m)
                        .await?;
                    format!(
                        "max voters {} -> {m}, with {} voters in",
                        p.max_voters, p.voters_count
                    )
                }
                "cancel" => {
                    self.org
                        .cancel_process(&pid)
                        .await
                        .with_context(|| format!("cancel election {n}"))?;
                    format!(
                        "canceled during voting, after {batches} batches and {} voters",
                        p.voters_count
                    )
                }
                _ => continue,
            };
            self.done(n, step, line)?;
        }
        Ok(())
    }

    /// Reveals the organizer secret of locked election `n`.
    async fn reveal_key(&mut self, n: usize) -> Result<()> {
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
        match self.org.reveal_process_key(&pid, &sk).await {
            Ok(()) => say!("election {n}: organizer key revealed"),
            Err(ClientError::Reverted(r)) if r == "AlreadyRevealed" => {}
            Err(e) => return Err(e).with_context(|| format!("reveal election {n}")),
        }
        self.state.election(n).revealed = true;
        self.save()
    }

    /// Sends refused vote `r` of `spec` once and records the answer.
    async fn refuse(&mut self, spec: &Spec, r: Refusal) -> Result<()> {
        let n = spec.n;
        if self
            .state
            .elections
            .get(&n)
            .is_some_and(|e| e.refusal(r).is_some())
        {
            return Ok(());
        }
        let pid = self.pid(n)?;
        let p = self.process(&pid).await?;
        if r != Refusal::AfterEnd && !voting(&p, unix_now()) {
            let line = format!("{} not sent: the election is not open", r.describe());
            say!("election {n}: {line}");
            self.state.election(n).note(line);
            return self.save();
        }
        let (witness, parts) = self.witness(spec, &p)?;
        let e = &self.state.elections[&n];
        let reused = e
            .votes
            .iter()
            .find(|v| v.round == 1 && v.state == VoteState::Settled)
            .map(|v| (v.voter, v.fields.clone()));
        let rb = refusal_ballot(
            spec,
            &self.secrets,
            &self.seed,
            r,
            &p,
            &witness,
            &parts,
            reused.as_ref().map(|(v, f)| (*v, f.as_slice())),
        )?;
        if let Some(strict) = &rb.strict {
            let e = tokio::task::block_in_place(|| {
                fx::prove_all(&self.prover, std::slice::from_ref(strict))
            })
            .err()
            .context("the client proved a ballot outside the rules")?;
            let line = format!("the client refuses to prove a ballot outside the rules: {e:#}");
            say!("election {n}: {line}");
            self.state.election(n).note(line);
        }
        let mut req = tokio::task::block_in_place(|| {
            fx::prove_all(&self.prover, std::slice::from_ref(&rb.job))
        })?
        .pop()
        .context("no proof")?;
        if let Some(key) = &rb.resign {
            let sig = vote_id_sign(key, req.vote_id);
            req.signature[..32].copy_from_slice(&sig.r);
            req.signature[32..64].copy_from_slice(&sig.s);
            req.signature[64] = sig.v;
        }
        let at = spec.refusals().iter().position(|x| *x == r).unwrap_or(0);
        let node = at % self.nodes.len();
        let (status, code, error) = submit_refused(&self.nodes[node], &req).await?;
        let rec = RefusedVote {
            case: r,
            node,
            vote_id: req.vote_id,
            status,
            code,
            error,
        };
        let (want_status, want_code) = r.expected();
        say!(
            "election {n}: {} to node {}: {status} {} ({}), expected {want_status} {want_code}",
            r.describe(),
            node + 1,
            code.map_or("-".into(), |c| c.to_string()),
            rec.error
        );
        self.state.election(n).refused.push(rec);
        self.save()
    }

    /// Pauses `spec` once every node can see it, before its second round.
    async fn pause(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        if self.is_done(n, "pause") {
            return Ok(());
        }
        let pid = self.pid(n)?;
        let p = self.process(&pid).await?;
        if p.status == ProcessStatus::Ready {
            self.org
                .pause_process(&pid)
                .await
                .with_context(|| format!("pause election {n}"))?;
        }
        // Every node must know before the votes go in.
        let mut accepting = Vec::new();
        for (i, api) in self.nodes.iter().enumerate() {
            let v = wait::until(
                &format!("node {} to see election {n} paused", i + 1),
                READY_TIMEOUT,
                POLL,
                || async {
                    let v = api.process(&pid).await?;
                    Ok((v.status == ProcessStatus::Paused).then_some(v))
                },
            )
            .await?;
            accepting.push(format!("node {}: {}", i + 1, v.is_accepting_votes));
        }
        let line = format!(
            "paused after round 1 with {} voters; the nodes report paused, still accepting votes \
             ({})",
            p.voters_count,
            accepting.join(", ")
        );
        self.done(n, "pause", line)
    }

    /// Watches the votes sent to each paused election for `PAUSE_HOLD`,
    /// notes what the nodes did with them, then resumes it.
    async fn hold_paused(&mut self) -> Result<()> {
        for spec in self.specs.clone() {
            let n = spec.n;
            if !spec.pauses() || self.is_done(n, "resume") || !self.is_done(n, "pause") {
                continue;
            }
            let pid = self.pid(n)?;
            let before = self.process(&pid).await?;
            let sent: Vec<(usize, u64)> = self.state.elections[&n]
                .votes
                .iter()
                .filter(|v| v.round == 2)
                .map(|v| (v.node, v.vote_id))
                .collect();
            say!(
                "election {n}: {} votes sent while paused; watching them for {} s",
                sent.len(),
                PAUSE_HOLD.as_secs()
            );
            tokio::time::sleep(PAUSE_HOLD).await;
            let mut by: BTreeMap<String, usize> = BTreeMap::new();
            for (node, vid) in &sent {
                let s = match self.nodes[*node].vote_status(&pid, *vid).await {
                    Ok(s) => s.to_string(),
                    Err(e) => format!("unreadable ({e})"),
                };
                *by.entry(s).or_default() += 1;
            }
            let after = self.process(&pid).await?;
            let line = format!(
                "{} votes sent while paused were taken (HTTP 200); {} s later they are {}; \
                 on-chain voters {} -> {}, status {}",
                sent.len(),
                PAUSE_HOLD.as_secs(),
                by.iter()
                    .map(|(s, c)| format!("{c} {s}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                before.voters_count,
                after.voters_count,
                status_name(after.status)
            );
            say!("election {n}: {line}");
            self.state.election(n).note(line);
            if after.status == ProcessStatus::Paused {
                self.org
                    .resume_process(&pid)
                    .await
                    .with_context(|| format!("resume election {n}"))?;
            }
            self.done(n, "resume", "resumed".into())?;
        }
        Ok(())
    }

    /// Waits until every election that votes only once it opens has opened,
    /// and every node serves it.
    async fn open_later(&mut self) -> Result<()> {
        for spec in self.specs.clone() {
            if !matches!(spec.lifecycle, Lifecycle::Later { .. }) || self.round_done(&spec, 3) {
                continue;
            }
            let p = self.process(&self.pid(spec.n)?).await?;
            // A little past the start, for the chain's clock.
            let wait = (p.start_time + 20).saturating_sub(unix_now());
            if wait > 0 {
                say!("election {}: opens in {wait} s; waiting", spec.n);
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            self.wait_ready(&spec).await?;
        }
        Ok(())
    }

    /// Ends what the organizer ends, cancels the rest of the canceled ones,
    /// reveals the locked keys once a sequencer asked the committee, sends
    /// the votes after the end, and waits for every tally.
    async fn close(&mut self) -> Result<()> {
        for spec in self.specs.clone() {
            let n = spec.n;
            let pid = self.pid(n)?;
            let p = self.process(&pid).await?;
            match spec.lifecycle {
                Lifecycle::Tally | Lifecycle::Later { .. } if is_open(p.status) => {
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
            if spec.key == KeySource::DkgLocked && spec.tallied() {
                self.reveal(&spec).await?;
            }
        }
        for spec in self.specs.clone() {
            if spec.refusals().contains(&Refusal::AfterEnd) {
                self.refuse_after_end(&spec).await?;
            }
        }
        self.wait_results().await
    }

    /// Waits for `spec` to end and its receiving node to stop taking votes,
    /// then sends it a vote.
    async fn refuse_after_end(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        if self
            .state
            .elections
            .get(&n)
            .is_some_and(|e| e.refusal(Refusal::AfterEnd).is_some())
        {
            return Ok(());
        }
        let pid = self.pid(n)?;
        let p = self.process(&pid).await?;
        let end = p.start_time + p.duration;
        let wait = (end + 10).saturating_sub(unix_now());
        if is_open(p.status) && wait > 0 {
            say!("election {n}: ends in {wait} s; a vote goes in after that");
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
        let at = spec
            .refusals()
            .iter()
            .position(|r| *r == Refusal::AfterEnd)
            .unwrap_or(0);
        let api = &self.nodes[at % self.nodes.len()];
        wait::until(
            &format!("the node to close election {n}"),
            READY_TIMEOUT,
            POLL,
            || async { Ok((!api.process(&pid).await?.is_accepting_votes).then_some(())) },
        )
        .await?;
        self.refuse(spec, Refusal::AfterEnd).await
    }

    async fn reveal(&mut self, spec: &Spec) -> Result<()> {
        let n = spec.n;
        if self.state.election(n).revealed {
            return Ok(());
        }
        let pid = self.pid(n)?;
        if self.process(&pid).await?.status != ProcessStatus::Results {
            let t = Instant::now();
            wait::until(
                &format!("the decryption request of election {n}"),
                REQUEST_TIMEOUT,
                POLL,
                || async {
                    let p = self.process(&pid).await?;
                    Ok(p.dkg.is_some_and(|d| d.results_requested).then_some(()))
                },
            )
            .await?;
            say!(
                "election {n}: decryption requested {:.0} s after the end; results stay locked \
                 until the reveal",
                t.elapsed().as_secs_f64()
            );
            self.reveal_key(n).await?;
            self.state
                .election(n)
                .note("organizer key revealed after the end");
        }
        self.state.election(n).revealed = true;
        self.save()
    }

    async fn wait_results(&mut self) -> Result<()> {
        let tallied: Vec<Spec> = self.specs.iter().filter(|s| s.tallied()).copied().collect();
        // Timed elections end on their own; wait past the last end.
        let mut last_end = unix_now();
        for spec in &tallied {
            if let Lifecycle::Timed { .. } = spec.lifecycle {
                let p = self.process(&self.pid(spec.n)?).await?;
                last_end = last_end.max(p.start_time + p.duration);
            }
        }
        let deadline = Instant::now()
            + Duration::from_secs(last_end.saturating_sub(unix_now()))
            + RESULTS_TIMEOUT;
        let start = Instant::now();
        let mut report = Instant::now();
        loop {
            let mut waiting = Vec::new();
            for spec in &tallied {
                let n = spec.n;
                if self.state.election(n).results.is_some() {
                    continue;
                }
                let p = self.process(&self.pid(n)?).await?;
                if p.status == ProcessStatus::Results {
                    say!(
                        "election {n}: results on-chain {:?} ({:.0} s into the wait)",
                        p.result,
                        start.elapsed().as_secs_f64()
                    );
                    self.state.election(n).results = Some(p.result);
                    self.save()?;
                } else {
                    let left = (p.start_time + p.duration).saturating_sub(unix_now());
                    waiting.push(if is_open(p.status) && left > 0 {
                        format!("election {n} ends in {left} s")
                    } else {
                        format!("election {n} {}", status_name(p.status))
                    });
                }
            }
            if waiting.is_empty() {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "no results in time: {}",
                waiting.join(", ")
            );
            if report.elapsed() > PROGRESS_EVERY {
                report = Instant::now();
                say!("waiting for results: {}", waiting.join(", "));
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// The table, each tally against the settled ballots, each count
    /// against the votes that settled (so no refused vote reached the
    /// chain), each refusal against its expected answer, each final status
    /// against the plan, and the notes.
    async fn report(&self) -> Result<()> {
        let now = unix_now();
        // An explorer to link each process to, when one is given.
        let explorer = std::env::var("DAVINCI_DEMO_EXPLORER_URL")
            .ok()
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());
        let mut rows = vec![
            "| # | process id | ballot | census | key | lifecycle | status | voters | changed \
             votes | batches | blobs |"
                .to_string(),
            "|---:|---|---|---|---|---|---|---:|---:|---:|---:|".to_string(),
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
            let p = self.process(&pid).await?;
            let transitions = self.transitions(&pid).await;
            let (batches, blobs) = match &transitions {
                Some(t) => (
                    t.len().to_string(),
                    t.iter().map(|t| t.n_blobs).sum::<u64>().to_string(),
                ),
                None => ("?".into(), "?".into()),
            };
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
                "| {n} | {} | {} | {} | {} | {} | {status} | {} | {} | {batches} | {blobs} |",
                ProcessId(pid),
                spec.ballot_name(),
                spec.census_name(),
                spec.key_name(),
                spec.lifecycle_name(),
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
            for l in &e.notes {
                notes.push(format!("election {n}: {l}"));
            }
            // Large elections: how the batches split into transactions.
            if spec.chunk > 0
                && let Some(t) = &transitions
            {
                let split: Vec<String> = t
                    .iter()
                    .map(|t| format!("{}v/{}o/{}b", t.voters, t.overwrites, t.n_blobs))
                    .collect();
                notes.push(format!(
                    "election {n}: transitions (voters/overwrites/blobs): {}",
                    split.join(" ")
                ));
            }
            // What the registry counted is what settled: no refused vote
            // got in.
            let counted = (p.voters_count, p.overwritten_votes_count);
            if counted != e.counts() {
                bad.push(format!(
                    "election {n}: the registry counts {counted:?} (voters, overwrites), the \
                     settled votes {:?}",
                    e.counts()
                ));
            }
            for r in spec.refusals() {
                match e.refusal(r) {
                    Some(x) if x.as_expected() => notes.push(format!(
                        "election {n}: {} refused with {} {} ({})",
                        r.describe(),
                        x.status,
                        x.code.unwrap_or_default(),
                        x.error
                    )),
                    Some(x) => bad.push(format!(
                        "election {n}: {} got {} {:?} ({}), not {:?}",
                        r.describe(),
                        x.status,
                        x.code,
                        x.error,
                        r.expected()
                    )),
                    None => notes.push(format!("election {n}: {} not sent", r.describe())),
                }
            }
            let expected = match spec.lifecycle {
                Lifecycle::Tally | Lifecycle::Later { .. } | Lifecycle::Timed { .. } => {
                    ProcessStatus::Results
                }
                Lifecycle::Canceled | Lifecycle::CanceledEarly { .. } => ProcessStatus::Canceled,
                Lifecycle::Open { .. } | Lifecycle::Upcoming { .. } => ProcessStatus::Ready,
            };
            if p.status != expected {
                bad.push(format!(
                    "election {n} is {}, not {}",
                    status_name(p.status),
                    status_name(expected)
                ));
            }
            if !spec.tallied() {
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
        eprintln!("\nwave {}\n\n{}\n", self.wave.number(), rows.join("\n"));
        for l in &notes {
            say!("{l}");
        }
        ensure!(bad.is_empty(), "{}", bad.join("; "));
        Ok(())
    }
}
