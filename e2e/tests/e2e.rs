//! End-to-end acceptance test, gated by `DAVINCI_E2E=1`.
//!
//! anvil (Osaka, 1 s blocks) with the zkVM contracts, three real
//! `davinci-sequencer` nodes sharing one davinci-zkvm prover, 24 Merkle
//! voters spread round-robin (with duplicate submissions and revotes through
//! other nodes), a CSP process, four rejected packages, and the results
//! proof of process 1 checked on-chain against the expected tally. Then, on
//! the same chain and nodes, the dynamic censuses: an origin-3 process on an
//! `OwnedCensus` that grows while voting (with a node restart between a seal
//! and its settlement), and an origin-2 process whose organizer replaces the
//! census mid-election (with a reweighted member's pending vote errored).
//!
//! Needs anvil and forge (`~/.foundry/bin`), the prover at `DAVINCI_ZKVM_URL`
//! (default `http://127.0.0.1:8080`), the forge project at
//! `DAVINCI_CONTRACTS_DIR` (default `../davinci-contracts`), the census
//! contract project at `DAVINCI_CENSUS_CONTRACT_DIR` (default
//! `../davinci-onchain-census-contract`, branch `davinci-zkvm`) and the circom
//! artifacts at `CIRCOM_ARTIFACTS` (default `../davinci-circom/artifacts`).
//! The node binary is `DAVINCI_SEQUENCER_BIN` or a release build made by the
//! test. On failure the node logs and datadirs are kept and their paths
//! printed.
//!
//! Run it inside a memory-capped scope (`make e2e`): anvil and the nodes are
//! killed only when their handles drop, so a killed test binary leaves them
//! to the scope. The vk fail-fast compares against the SDK release pins; the
//! prover's `/health` carries no vks.
//!
//! `DAVINCI_E2E_LIVE=1` runs the same scenario on an existing deployment
//! (Gnosis by default, see `davinci_e2e::net`): the registry pins are checked
//! first, the accounts must hold 0.05 native each, nodes read blobs from the
//! beacon API, and timeouts scale by `DAVINCI_E2E_TIMEOUT_SCALE`. Opt-in
//! extras: `DAVINCI_E2E_RESILIENCE=1` (full RPC, prover and beacon outages
//! and an observer RPC failover, through loopback proxies) and `DAVINCI_E2E_NEGATIVE=1` (tampered replays through
//! `eth_call`; also a standalone test). Every run ends with its gas bill.
//! `DAVINCI_E2E_DKG=1` adds the DKG key modes (`davinci_e2e::dkg`): on anvil
//! a davinci-dkg committee of three nodes with a Live epoch, deployed before
//! the registry; live the committee of the registry adapter's manager
//! (`DAVINCI_E2E_DKG_MANAGER` overrides it). Then,
//! after the sequencer-key processes, an automatic, a locked and a zero-vote
//! DKG process and the eth_call negatives around their requests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use davinci_client::Error as ClientError;
use davinci_client::api::{
    CensusFile, Fr, ProcessId, ProcessStatus, VoteRequest, VoteStatus, verify_tracker,
};
use davinci_client::organizer::{
    CreatedProcess, KeyMode, NewProcess, OnchainProcess, Organizer, OrganizerSecret,
    RegistryReader, merkle_census,
};
use davinci_client::prover::BallotProver;
use davinci_client::voter::Voter;
use davinci_e2e::census::{self, Census};
use davinci_e2e::chain;
use davinci_e2e::cost::{self, TxCost};
use davinci_e2e::dkg::{self, DkgWiring};
use davinci_e2e::fixture as fx;
use davinci_e2e::negative;
use davinci_e2e::net::{self, Net};
use davinci_e2e::node::{self, Metrics, Node, NodeConfig, WorkDir};
use davinci_e2e::proxy::Proxy;
use davinci_e2e::wait;
use davinci_zkvm_sdk::ballot::{Ballot, address_to_fr};
use davinci_zkvm_sdk::census::{CensusWitness, LeanImt, vote_id_recover};
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::{decrypt, keygen};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;
use k256::ecdsa::SigningKey;
use rand::SeedableRng;
use rand::rngs::StdRng;

const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
/// The observer's index in the node list.
const OBS: usize = 3;
/// Sequencer error codes (HTTP status * 100 + discriminator).
const CODE_INVALID: u32 = 40001;
const CODE_VOTE: u32 = 40002;
const CODE_DUPLICATE: u32 = 40901;
const PROGRESS_EVERY: Duration = Duration::from_secs(20);
/// How long the resilience checks keep a proxy down.
const RPC_OUTAGE: Duration = Duration::from_secs(75);
const PROVER_OUTAGE: Duration = Duration::from_secs(60);
const BEACON_OUTAGE: Duration = Duration::from_secs(75);

/// Longest wait for one round of votes to settle.
fn settle_timeout() -> Duration {
    net::scaled(Duration::from_secs(25 * 60))
}

/// Longest wait for the results transaction after the process ends.
fn results_timeout() -> Duration {
    net::scaled(Duration::from_secs(15 * 60))
}

/// Longest wait for the nodes to catch up with the chain.
fn sync_timeout() -> Duration {
    net::scaled(Duration::from_secs(3 * 60))
}

/// First URL of a `,` list.
fn first(list: &str) -> &str {
    list.split(',').next().unwrap_or(list)
}

/// `proxy` (in front of the list's first URL), then the list's second URL,
/// or the first again when there is none (anvil): a failover pair.
fn failover(list: &str, proxy: &Proxy) -> String {
    let second = list.split(',').nth(1).unwrap_or(first(list));
    format!("{},{second}", proxy.url)
}

fn flag(k: &str) -> bool {
    std::env::var(k).as_deref() == Ok("1")
}

fn t0() -> Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

macro_rules! say {
    ($($arg:tt)*) => {
        eprintln!("[e2e {:>7.1}s] {}", t0().elapsed().as_secs_f64(), format!($($arg)*))
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e() -> Result<()> {
    if std::env::var("DAVINCI_E2E").as_deref() != Ok("1") {
        eprintln!("DAVINCI_E2E not set, skipping");
        return Ok(());
    }
    t0();
    let mut art = WorkDir::new("davinci-e2e-")?;
    let dir = art.path().to_path_buf();
    say!("working directory {}", dir.display());
    let r = scenario(&dir).await;
    match &r {
        Ok(()) => say!("PASSED in {:.1} min", t0().elapsed().as_secs_f64() / 60.0),
        Err(e) => say!("FAILED: {e:#}"),
    }
    art.ok = r.is_ok();
    r
}

/// The negative checks alone, on history already on the live chain: the
/// latest origin-1 process with results since `DAVINCI_E2E_FROM_BLOCK`.
/// Gated by `DAVINCI_E2E_NEGATIVE=1` without `DAVINCI_E2E` (with it, the
/// scenario runs them on its own process). Only `eth_call`, no keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negative_checks() -> Result<()> {
    if !flag("DAVINCI_E2E_NEGATIVE") || flag("DAVINCI_E2E") {
        eprintln!("DAVINCI_E2E_NEGATIVE alone not set, skipping");
        return Ok(());
    }
    ensure!(
        net::is_live(),
        "the standalone negatives need DAVINCI_E2E_LIVE=1: anvil history ends with its run"
    );
    let (rpc, registry, from) = net::live_target()?;
    negative::run(&rpc, registry, from, None).await?;
    Ok(())
}

/// Everything before the nodes: chain, contracts, both processes (with
/// local election keys), the census file and every ballot, checked with the
/// SDK. Gated by `DAVINCI_E2E_SETUP=1`; needs neither the prover service nor
/// the node binary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_without_nodes() -> Result<()> {
    if std::env::var("DAVINCI_E2E_SETUP").as_deref() != Ok("1") {
        eprintln!("DAVINCI_E2E_SETUP not set, skipping");
        return Ok(());
    }
    t0();
    let mut dir = WorkDir::new("davinci-e2e-setup-")?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir.path()).await?;
    net.check_balances(0).await?;
    let org = net.organizer()?;
    let reader = RegistryReader::connect(&net.rpc, net.registry)?;
    let mut census_costs = Vec::new();
    let r = setup_checks(
        &net,
        &org,
        &reader,
        dir.path(),
        ballot_prover,
        &mut census_costs,
    )
    .await;
    let r = cancel_open(&org, r).await;
    let bill = costs(&net, &org, census_costs).await;
    say!("{}", bill.unwrap_or_else(|e| format!("gas bill: {e:#}")));
    dir.ok = r.is_ok();
    r
}

/// Cancels the READY or PAUSED processes listed in `DAVINCI_E2E_CANCEL`
/// (comma-separated pids of this organizer), the leftovers of a run killed
/// before its cleanup. Needs only the organizer key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_leftovers() -> Result<()> {
    let Ok(list) = std::env::var("DAVINCI_E2E_CANCEL") else {
        eprintln!("DAVINCI_E2E_CANCEL not set, skipping");
        return Ok(());
    };
    let mut dir = WorkDir::new("davinci-e2e-cancel-")?;
    let net = Net::open(dir.path()).await?;
    let org = net.organizer()?;
    for pid in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let pid: ProcessId = pid.parse()?;
        let status = org.process(&pid.0).await?.status;
        if matches!(status, ProcessStatus::Ready | ProcessStatus::Paused) {
            org.cancel_process(&pid.0).await?;
            eprintln!("{pid}: {status:?}, canceled");
        } else {
            eprintln!("{pid}: {status:?}, left alone");
        }
    }
    dir.ok = true;
    Ok(())
}

/// Cancels every process `org` created that is still open, so nodes of a
/// later run have nothing to pick up. Keeps `r`'s error first.
async fn cancel_open(org: &Organizer, r: Result<()>) -> Result<()> {
    let n = org.created().len();
    let c = org.cancel_open().await;
    match &c {
        Ok(done) => say!("canceled {} of the {n} processes it created", done.len()),
        Err(e) => say!("cancel: {e:#}"),
    }
    r.and(c.map(|_| ()).context("cancel the open processes"))
}

/// The body of `setup_without_nodes`: records the census transactions in
/// `bill`.
async fn setup_checks(
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    dir: &Path,
    ballot_prover: tokio::task::JoinHandle<Result<BallotProver>>,
    bill: &mut Vec<TxCost>,
) -> Result<()> {
    let mut rng = StdRng::seed_from_u64(fx::SEED);
    let (sk1, pk1) = keygen(&mut rng);
    let (_, pk2) = keygen(&mut rng);
    let e = create_processes(org, reader, dir, async |_| Ok(pk1), async |_| Ok(pk2)).await?;
    ensure!(e.p1.encryption_key == pk1 && e.p2.encryption_key == pk2);
    ensure!(e.p1.census_origin == 1 && e.p2.census_origin == 4);
    ensure!(e.p1.ballot_mode == fx::ballot_mode() && e.p2.ballot_mode == fx::ballot_mode());
    let p1 = e.p1.clone();
    // The census file commits to the on-chain root and holds every voter.
    let file: CensusFile = serde_json::from_slice(&std::fs::read(dir.join("census-1.json"))?)?;
    ensure!(merkle_census(&file)?.root() == e.p1.census_root);
    ensure!(file.participants.len() == fx::N_VOTERS);
    ensure!(e.p1.census_uri.starts_with("file:///"));

    let prover = ballot_prover.await??;
    let t = Instant::now();
    let b = fx::build_ballots(&prover, &e.p1, &e.tree, &e.p2, &e.csp)?;
    say!("proved the ballots in {:.1} s", t.elapsed().as_secs_f64());

    let verifier = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?;
    let proof_ok = |r: &VoteRequest| {
        let pubs = [
            address_to_fr(&r.address),
            Fr::from(r.vote_id),
            r.ballot_inputs_hash,
        ];
        verifier.verify(&r.ballot_proof, &pubs)
    };
    let sig_ok =
        |r: &VoteRequest| vote_id_recover(r.vote_id, &r.signature()).ok() == Some(r.address);
    let honest = b.round1.iter().chain(&b.round2).chain(&b.csp);
    for r in honest.chain([&b.reused, &b.outsider]) {
        ensure!(
            proof_ok(r) && sig_ok(r),
            "vote {} of 0x{}",
            r.vote_id,
            hex::encode(r.address)
        );
    }
    ensure!(!proof_ok(&b.bad_proof) && sig_ok(&b.bad_proof), "bad proof");
    ensure!(proof_ok(&b.bad_sig) && !sig_ok(&b.bad_sig), "bad signature");
    let first = &b.round1[fx::REUSER];
    ensure!(b.reused.vote_id == first.vote_id && b.reused.ballot != first.ballot);
    ensure!(
        !file
            .participants
            .iter()
            .any(|p| p.key == b.outsider.address)
    );
    for (j, v) in fx::REVOTERS.into_iter().enumerate() {
        ensure!(b.round2[j].address == b.round1[v].address);
        ensure!(b.round2[j].vote_id != b.round1[v].vote_id);
    }
    ensure!(b.csp.iter().all(|r| r.process_id.0 == e.pid2));

    // The last ballots decrypt to the expected tally.
    let last = (0..fx::N_VOTERS).map(|v| match fx::REVOTERS.iter().position(|x| *x == v) {
        Some(j) => &b.round2[j],
        None => &b.round1[v],
    });
    let acc = last.fold(Ballot::identity(), |a, r| a.add(&r.ballot));
    let want = fx::expected_tally(&fx::last_choices());
    for (i, w) in want.iter().enumerate() {
        ensure!(decrypt(&sk1, &acc.0[i], 1000) == Some(*w), "field {i}");
    }
    say!("setup ok; expected tally {want:?}");

    // Dynamic censuses: the contract's root is the local lean-IMT's, both
    // origins create, origin 2 updates, and the voter accepts their witnesses.
    let cdir = census::census_dir();
    chain::forge_build(&cdir)?;
    let c = Census::deploy(net.census_provider(org)?, &cdir).await?;
    let voters = fx::onchain_voters();
    let parts: Vec<_> = voters.iter().map(|v| (v.address(), fx::WEIGHT)).collect();
    c.add_members(&parts).await?;
    let tree = fx::tree_of(&parts)?;
    ensure!(c.root().await? == tree.root() && c.size().await? == fx::N_ONCHAIN as u64);
    let e = c.add_member(parts[3].0, fx::WEIGHT).await.unwrap_err();
    ensure!(
        format!("{e:#}").contains("AlreadyRegisteredAddress"),
        "{e:#}"
    );
    let (metadata, metadata_hash) = fx::write_metadata(dir, "metadata-dyn.json", "dynamic census")?;
    let mk = |process_id, census_origin, census_root, census_contract, census_uri| NewProcess {
        process_id,
        start_time: 0,
        duration: 3600,
        max_voters: 32,
        ballot_mode: fx::ballot_mode(),
        census_origin,
        census_root,
        census_contract,
        census_uri,
        metadata: metadata.clone(),
        metadata_hash,
        key_mode: KeyMode::Sequencer(pk1),
    };
    let next = org.next_process_id().await?;
    let pid3 = org
        .create_process(&mk(
            next,
            3,
            Fr::from(0u64),
            c.address.0.0,
            "onchain://x".into(),
        ))
        .await?
        .pid;
    let p3 = reader.process(&pid3).await?;
    ensure!(p3.census_contract == c.address.0.0 && p3.census_root == tree.root());
    let dyn_voters = fx::dynamic_voters();
    let v1: Vec<_> = dyn_voters
        .iter()
        .map(|v| (v.address(), fx::WEIGHT))
        .collect();
    let (t1, t2) = (fx::tree_of(&v1[..fx::N_DYN_OLD])?, fx::tree_of(&v1)?);
    let uri1 = fx::write_census_dump(dir, "dyn-v1.json", &v1[..fx::N_DYN_OLD])?;
    let uri2 = fx::write_census_jsonl(dir, "dyn-v2.jsonl", &v1)?;
    let next = org.next_process_id().await?;
    let pid2 = org
        .create_process(&mk(next, 2, t1.root(), [0; 20], uri1))
        .await?
        .pid;
    org.set_process_census(&pid2, t2.root(), &uri2).await?;
    let p2 = reader.process(&pid2).await?;
    ensure!(p2.census_root == t2.root() && p2.census_uri == uri2);
    let last = fx::N_DYN - 1;
    let w = fx::merkle_witness(&t2, last)?;
    let k = fx::vote_k(6, last, 1);
    dyn_voters[last].prepare_vote(&p2, &fx::choices(last, 1), w, fx::WEIGHT, k)?;
    // Origin 3 takes a witness at any root: the root lives in the contract.
    let w = fx::merkle_witness(&fx::tree_of(&parts[..5])?, 4)?;
    let k = fx::vote_k(5, 4, 1);
    voters[4].prepare_vote(&p3, &fx::choices(4, 1), w, fx::WEIGHT, k)?;
    say!("dynamic census setup ok");
    bill.extend(c.receipts().iter().map(|(l, r)| TxCost::of(l.clone(), r)));

    // DKG: both modes take the committee's key (the locked one through the
    // real PoP check), and registration stays open to any account.
    if let Some(w) = &net.dkg {
        for mode in [KeyMode::DkgAutomatic, KeyMode::DkgLocked] {
            let next = org.next_process_id().await?;
            let np = NewProcess {
                key_mode: mode,
                ..dkg_new_process(next, &p1, 3600)
            };
            let c = create_dkg(net, w, org, &np).await?;
            let p = reader.process(&c.pid).await?;
            dkg::check_process_key(&net.rpc, w, &p, c.organizer_secret.as_ref())
                .await
                .with_context(|| format!("{mode:?} key"))?;
        }
        // An unrelated application from an EOA, simulated only: a random
        // 248-bit aid is nonzero and below the field.
        match dkg::registration_epoch(&net.rpc, w.adapter).await {
            Ok(epoch) => {
                let mut aid = [0u8; 32];
                aid[1..].copy_from_slice(&rand::random::<[u8; 31]>());
                let got = negative::simulate(
                    &net.rpc,
                    org.address(),
                    w.app_manager,
                    dkg::register_app_call(epoch, aid.into()),
                )
                .await?;
                ensure!(
                    got == negative::Outcome::Ok,
                    "registerApplication from the organizer: {got}"
                );
                say!("DKG setup ok: both key modes, open registration");
            }
            // The two creates may have spent the epoch's last pool key.
            Err(e) => say!("DKG setup ok: both key modes; open registration not checked: {e:#}"),
        }
    }
    Ok(())
}

/// The run's gas bill: organizer and census transactions, then every
/// transition and results transaction of its processes since the run began.
async fn costs(net: &Net, org: &Organizer, mut extra: Vec<TxCost>) -> Result<String> {
    let mut all: Vec<TxCost> = org
        .receipts()
        .iter()
        .map(|r| TxCost::of("organizer", r))
        .collect();
    all.append(&mut extra);
    let from = net.start_block.unwrap_or(net.from_block);
    // A live registry is shared: other organizers' processes are not ours.
    let created = org.created();
    let ours = |t: &chain::RegistryTx| created.contains(&t.pid);
    for t in chain::transitions(&net.rpc, net.registry, from)
        .await?
        .into_iter()
        .filter(ours)
    {
        let label = format!("transition {}", ProcessId(t.pid));
        all.push(cost::fetch(&net.rpc, label, t.tx).await?);
    }
    let results: Vec<_> = chain::results_txs(&net.rpc, net.registry, from)
        .await?
        .into_iter()
        .filter(ours)
        .collect();
    // A zero-vote DKG request sets the results in the same transaction.
    for t in chain::dkg_requests(&net.rpc, net.registry, from)
        .await?
        .into_iter()
        .filter(ours)
    {
        if results.iter().any(|r| r.tx == t.tx) {
            continue;
        }
        let label = format!("dkg request {}", ProcessId(t.pid));
        all.push(cost::fetch(&net.rpc, label, t.tx).await?);
    }
    for t in results {
        let label = format!("results {}", ProcessId(t.pid));
        all.push(cost::fetch(&net.rpc, label, t.tx).await?);
    }
    Ok(cost::report(
        &format!("gas bill, chain {}", net.chain_id),
        &all,
    ))
}

/// A vote expected to settle on `node`.
#[derive(Clone, Debug)]
struct Sent {
    label: String,
    node: usize,
    pid: [u8; 31],
    vid: u64,
}

fn sent(label: impl Into<String>, node: usize, req: &VoteRequest) -> Sent {
    Sent {
        label: label.into(),
        node,
        pid: req.process_id.0,
        vid: req.vote_id,
    }
}

async fn submit(n: &Node, req: &VoteRequest, label: &str) -> Result<()> {
    n.api
        .submit_vote(req)
        .await
        .with_context(|| format!("{label} to {}", n.name))
}

/// The package must be refused with HTTP `status` and error `code`.
async fn expect_rejected(
    n: &Node,
    req: &VoteRequest,
    (status, code): (u16, u32),
    what: &str,
) -> Result<()> {
    match n.api.submit_vote(req).await {
        Err(ClientError::Api {
            status: s,
            code: c,
            message,
        }) if s == status && c == Some(code) => {
            say!(
                "{what}: {} answered {s}/{code} ({message}), as expected",
                n.name
            );
            Ok(())
        }
        Err(ClientError::Api {
            status: s,
            code: c,
            message,
        }) => bail!(
            "{what}: {} answered {s}/{c:?} ({message}), want {status}/{code}",
            n.name
        ),
        Ok(()) => bail!(
            "{what}: {} accepted the package, want {status}/{code}",
            n.name
        ),
        Err(e) => bail!("{what}: {}: {e}", n.name),
    }
}

/// Every node's local tree must be at the on-chain root: a tracker proof of
/// `vid` served by the node verifies against `root`. When the process view
/// carries `localStateRoot`, that must equal `root` as well.
async fn check_local_trees(nodes: &[Node], pid: &[u8; 31], vid: u64, root: [u8; 32]) -> Result<()> {
    let want = format!("0x{}", hex::encode(root));
    for n in nodes {
        let last = std::sync::Mutex::new(String::new());
        let note = |m: String| {
            if let Ok(mut l) = last.lock() {
                *l = m;
            }
        };
        let r = wait::until(
            &format!(
                "{}'s tree of {} to reach the chain root",
                n.name,
                ProcessId(*pid)
            ),
            sync_timeout(),
            Duration::from_secs(1),
            || async {
                alive(nodes)?;
                match n.api.vote_id_proof(pid, vid).await {
                    Ok(p) if verify_tracker(&p, &root) => {}
                    Ok(p) => {
                        note(format!(
                            "proof at 0x{} does not verify",
                            hex::encode(p.root)
                        ));
                        return Ok(None);
                    }
                    Err(e) => {
                        note(format!("tracker proof: {e}"));
                        return Ok(None);
                    }
                }
                let v = n.process_json(pid).await?;
                match v.get("localStateRoot").and_then(|x| x.as_str()) {
                    None => Ok(Some(false)),
                    Some(l) if l.eq_ignore_ascii_case(&want) => Ok(Some(true)),
                    Some(l) => {
                        note(format!("localStateRoot {l}"));
                        Ok(None)
                    }
                }
            },
        )
        .await;
        let last = last.lock().map(|l| l.clone()).unwrap_or_default();
        let local_root_checked = r.with_context(|| format!("last: {last}"))?;
        say!(
            "{}: local tree of {} at the chain root{}",
            n.name,
            ProcessId(*pid),
            if local_root_checked {
                " (and localStateRoot)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// Fails when a node has exited.
fn alive(nodes: &[Node]) -> Result<()> {
    nodes.iter().try_for_each(|n| n.check_alive())
}

async fn all_metrics(nodes: &[Node]) -> Result<Vec<Metrics>> {
    let mut out = Vec::new();
    for n in nodes {
        out.push(
            n.metrics()
                .await
                .with_context(|| format!("{} /info", n.name))?,
        );
    }
    Ok(out)
}

fn show_metrics(nodes: &[Node], ms: &[Metrics]) -> String {
    nodes
        .iter()
        .zip(ms)
        .map(|(n, m)| {
            format!(
                "{}: settled_by_self={} synced_from_others={} lost_races={}",
                n.name,
                m.settled_by_self,
                m.synced_from_others,
                m.lost_races.map_or("-".into(), |x| x.to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Waits until every vote in `sent` is `settled` on its node. An `error`
/// status or a dead node fails at once.
async fn wait_settled(nodes: &[Node], sent: &[Sent], what: &str) -> Result<()> {
    let start = Instant::now();
    let mut report = Instant::now();
    let mut last: Vec<String> = vec!["?".into(); sent.len()];
    let mut done = vec![false; sent.len()];
    loop {
        alive(nodes)?;
        for (i, s) in sent.iter().enumerate() {
            if done[i] {
                continue;
            }
            let n = &nodes[s.node];
            match n.api.vote_status_full(&s.pid, s.vid).await {
                Ok(r) if r.status == VoteStatus::Settled => done[i] = true,
                Ok(r) if r.status == VoteStatus::Error => bail!(
                    "{what}: {} on {} is in error: {}",
                    s.label,
                    n.name,
                    r.error.unwrap_or_default()
                ),
                Ok(r) => last[i] = r.status.to_string(),
                Err(e) => last[i] = format!("unreadable ({e})"),
            }
        }
        let open: Vec<usize> = (0..sent.len()).filter(|i| !done[*i]).collect();
        if open.is_empty() {
            say!(
                "{what}: all {} votes settled in {:.0} s",
                sent.len(),
                start.elapsed().as_secs_f64()
            );
            return Ok(());
        }
        let detail = || {
            let mut by: BTreeMap<(String, String), usize> = BTreeMap::new();
            for i in &open {
                *by.entry((nodes[sent[*i].node].name.clone(), last[*i].clone()))
                    .or_default() += 1;
            }
            by.iter()
                .map(|((n, st), c)| format!("{n} {st}: {c}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if start.elapsed() > settle_timeout() {
            let list: Vec<String> = open
                .iter()
                .map(|i| {
                    format!(
                        "{} @ {}: {}",
                        sent[*i].label, nodes[sent[*i].node].name, last[*i]
                    )
                })
                .collect();
            let ms = all_metrics(nodes).await.map(|m| show_metrics(nodes, &m));
            bail!(
                "{what}: {} of {} votes not settled after {:?}:\n  {}\nmetrics: {:?}",
                open.len(),
                sent.len(),
                settle_timeout(),
                list.join("\n  "),
                ms
            );
        }
        if report.elapsed() > PROGRESS_EVERY {
            report = Instant::now();
            let ms = all_metrics(nodes).await.map(|m| show_metrics(nodes, &m));
            say!(
                "{what}: {}/{} settled; open: {}; {}",
                sent.len() - open.len(),
                sent.len(),
                detail(),
                ms.unwrap_or_else(|e| format!("metrics: {e}"))
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Waits until every node's root for `pid` equals the on-chain one; returns it.
async fn wait_roots(nodes: &[Node], reader: &RegistryReader, pid: &[u8; 31]) -> Result<[u8; 32]> {
    let last = std::sync::Mutex::new(String::new());
    let r = wait::until(
        "node roots to match the chain",
        sync_timeout(),
        Duration::from_secs(1),
        || async {
            alive(nodes)?;
            let chain = reader.process(pid).await?.state_root;
            let mut views = Vec::new();
            for n in nodes {
                views.push((n.name.clone(), n.api.process(pid).await?.state_root));
            }
            if let Ok(mut l) = last.lock() {
                *l = views
                    .iter()
                    .map(|(n, r)| format!("{n}=0x{}", hex::encode(&r[..6])))
                    .chain([format!("chain=0x{}", hex::encode(&chain[..6]))])
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            Ok(views.iter().all(|(_, r)| *r == chain).then_some(chain))
        },
    )
    .await;
    let last = last.lock().map(|l| l.clone()).unwrap_or_default();
    r.with_context(|| format!("process {}: last roots {last}", ProcessId(*pid)))
}

struct Elections {
    pid1: [u8; 31],
    pid2: [u8; 31],
    /// Both as the registry holds them.
    p1: OnchainProcess,
    p2: OnchainProcess,
    /// Census of process 1.
    tree: LeanImt,
    /// CSP of process 2.
    csp: SigningKey,
}

/// Process 1: the 24-voter Merkle census as a `file://` JSON in
/// `census_dir`, key `key1(pid)`. Process 2: the CSP census, key
/// `key2(pid)`. Each key is asked for the registry's next id. Both nf=4,
/// starting now, two hours long.
async fn create_processes(
    org: &Organizer,
    reader: &RegistryReader,
    census_dir: &Path,
    key1: impl AsyncFnOnce([u8; 31]) -> Result<Point>,
    key2: impl AsyncFnOnce([u8; 31]) -> Result<Point>,
) -> Result<Elections> {
    let (census, tree) = fx::merkle_census_of(&fx::merkle_voters())?;
    let uri = fx::write_census(census_dir, "census-1.json", &census)?;
    let (meta1, hash1) = fx::write_metadata(census_dir, "metadata-1.json", "process 1")?;
    let next = org.next_process_id().await?;
    let pk1 = key1(next).await.context("key for process 1")?;
    let pid1 = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 2 * 3600,
            max_voters: fx::N_VOTERS as u64,
            ballot_mode: fx::ballot_mode(),
            census_origin: 1,
            census_root: tree.root(),
            census_contract: [0; 20],
            census_uri: uri,
            metadata: meta1,
            metadata_hash: hash1,
            key_mode: KeyMode::Sequencer(pk1),
        })
        .await
        .context("create process 1")?
        .pid;
    let csp = fx::csp_key();
    let (meta2, hash2) = fx::write_metadata(census_dir, "metadata-2.json", "process 2")?;
    let next = org.next_process_id().await?;
    let pk2 = key2(next).await.context("key for process 2")?;
    let pid2 = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 2 * 3600,
            max_voters: fx::N_CSP as u64,
            ballot_mode: fx::ballot_mode(),
            census_origin: 4,
            census_root: fx::csp_root(&csp)?,
            census_contract: [0; 20],
            census_uri: "csp://davinci-e2e".into(),
            metadata: meta2,
            metadata_hash: hash2,
            key_mode: KeyMode::Sequencer(pk2),
        })
        .await
        .context("create process 2")?
        .pid;
    say!(
        "process 1 {} (Merkle), process 2 {} (CSP)",
        ProcessId(pid1),
        ProcessId(pid2)
    );
    let (p1, p2) = (reader.process(&pid1).await?, reader.process(&pid2).await?);
    fx::check_metadata(&p1)?;
    fx::check_metadata(&p2)?;
    Ok(Elections {
        pid1,
        pid2,
        p1,
        p2,
        tree,
        csp,
    })
}

async fn scenario(dir: &Path) -> Result<()> {
    let prover_url =
        std::env::var("DAVINCI_ZKVM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());

    // Fail fast when the prover is not there.
    let health = ProverClient::new(&prover_url)
        .health()
        .await
        .with_context(|| format!("prover health at {prover_url}"))?;
    ensure!(health.status == "ok", "prover health: {health:?}");
    say!("prover {prover_url} ok, queue_len {}", health.queue_len);

    // The circom prover loads in the background (~4 s).
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);

    let net = Net::open(dir).await?;
    net.check_balances(OBS).await?;
    let dep_chain_id = net.chain_id;
    let registry = net.registry;

    // Resilience, through proxies the test can take down. Full outages,
    // recovered by waiting: node C's only RPC (its first URL), node C's
    // prover, the observer's only beacon (its first URL). Failover: the
    // observer's RPC list is its first URL behind a proxy, then its second.
    let resilience = flag("DAVINCI_E2E_RESILIENCE");
    let (rpc_proxy, obs_rpc_proxy, prover_proxy, beacon_proxy) = if resilience {
        let beacon = match &net.beacon {
            Some(b) => Some(Proxy::start(first(b)).await?),
            None => None,
        };
        (
            Some(Proxy::start(first(&net.node_rpc(C))).await?),
            Some(Proxy::start(first(&net.node_rpc(OBS))).await?),
            Some(Proxy::start(&prover_url).await?),
            beacon,
        )
    } else {
        (None, None, None, None)
    };

    // Three nodes on the same chain and prover.
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let census_dir = dir.join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;
    let cfg = |name: &str, i: usize, key: Option<usize>| -> Result<NodeConfig> {
        let mut c = net.node_config(name, key, i, &prover_url, &census_dir, dir)?;
        if i == C {
            if let Some(p) = &rpc_proxy {
                c.rpc_url = p.url.clone();
            }
            if let Some(p) = &prover_proxy {
                c.prover_url = p.url.clone();
            }
        }
        if i == OBS {
            if let Some(p) = &obs_rpc_proxy {
                c.rpc_url = failover(&c.rpc_url, p);
            }
            if let Some(p) = &beacon_proxy {
                c.blob_source = format!("beacon:{}", p.url);
            }
        }
        Ok(c)
    };
    // Three sequencers and, last, an observer without a key that only
    // syncs and serves reads (it is never sent votes or key requests).
    let mut nodes: Vec<Node> = Vec::new();
    for (i, name) in ["node-a", "node-b", "node-c"].into_iter().enumerate() {
        nodes.push(Node::start(&bin, &cfg(name, i, Some(i))?).await?);
    }
    nodes.push(Node::start(&bin, &cfg("node-obs", OBS, None)?).await?);
    for n in &nodes {
        let account = n
            .address
            .map_or("observer".into(), |a| format!("account {a}"));
        say!(
            "{} at {} ({account}), log {}",
            n.name,
            n.url,
            n.log.display()
        );
    }
    // The nodes size transitions from eth_config: 2 blobs on Gnosis.
    for n in &nodes {
        match n.logged_blob_cap() {
            Some(c) if net.live => ensure!(c == 2, "{}: blob cap {c}, want 2", n.name),
            Some(c) => say!("{}: blob cap {c}", n.name),
            None => say!("{}: no blob cap in its log", n.name),
        }
    }
    // The vk fail-fast compares against the SDK release pins: the prover's
    // /health carries no vks. The address check also proves each port is
    // served by the node we started with that key.
    let vk_hash = chain::ballot_vk_hash()?;
    for n in &nodes {
        let info = n
            .api
            .info()
            .await
            .with_context(|| format!("{} /info", n.name))?;
        ensure!(
            info.chain_id == dep_chain_id,
            "{}: chain id {}",
            n.name,
            info.chain_id
        );
        ensure!(
            info.process_registry == registry.0.0,
            "{}: registry",
            n.name
        );
        ensure!(info.ballot_vk_hash == vk_hash, "{}: ballot vk hash", n.name);
        ensure!(
            info.batch_program_vk == release::BATCH_PROGRAM_VK,
            "{}: batch vk",
            n.name
        );
        ensure!(
            info.results_program_vk == release::RESULTS_PROGRAM_VK,
            "{}: results vk",
            n.name
        );
        ensure!(
            info.observer == n.address.is_none(),
            "{}: observer flag {}",
            n.name,
            info.observer
        );
        ensure!(
            info.sequencer_address == n.address.map(|a| a.0.0),
            "{}: sequencer address {:?}",
            n.name,
            info.sequencer_address.map(hex::encode)
        );
    }

    // Process 1 (Merkle) with node A's key, process 2 (CSP) with node C's.
    let org = net.organizer()?;
    let reader = RegistryReader::connect(&net.rpc, registry)?;
    let scan_from = net.start_block.unwrap_or(net.from_block);
    // Census costs, kept when a later step fails.
    let mut census_costs = Vec::new();
    let r: Result<()> = async {
        let Elections {
            pid1,
            pid2,
            p1,
            p2,
            tree,
            csp,
        } = create_processes(
            &org,
            &reader,
            &census_dir,
            async |pid| Ok(nodes[A].api.new_key(&pid).await?),
            async |pid| Ok(nodes[C].api.new_key(&pid).await?),
        )
        .await?;
        let voters = fx::merkle_voters();

        for n in &nodes {
            wait::until(
                &format!("{} to list both processes", n.name),
                sync_timeout(),
                Duration::from_secs(1),
                || async {
                    let l = n.api.processes().await?;
                    Ok(
                        (l.contains(&ProcessId(pid1)) && l.contains(&ProcessId(pid2)))
                            .then_some(()),
                    )
                },
            )
            .await?;
            for p in [&p1, &p2] {
                n.api
                    .process(&p.id)
                    .await?
                    .check_against(p)
                    .with_context(|| format!("{} view of {}", n.name, ProcessId(p.id)))?;
            }
        }
        say!("all nodes (and the observer) serve both processes");

        // The nodes' census proofs are the organizer's.
        for v in [0, 13, fx::N_VOTERS - 1] {
            let n = &nodes[fx::first_node(v)];
            let (proof, w) = n.api.participant(&pid1, &voters[v].address()).await?;
            ensure!(
                proof == tree.proof(v)? && w == fx::WEIGHT,
                "{}: participant {v}",
                n.name
            );
        }

        // Every ballot of the scenario, proved up front.
        let prover = ballot_prover.await??;
        let t = Instant::now();
        let fx::Ballots {
            round1,
            round2,
            csp: csp_votes,
            reused,
            bad_proof,
            bad_sig,
            outsider: outsider_req,
        } = fx::build_ballots(&prover, &p1, &tree, &p2, &csp)?;
        say!("proved the ballots in {:.1} s", t.elapsed().as_secs_f64());

        // Round 1: round-robin, some packages also to a second node.
        let mut expect1 = Vec::new();
        for (v, req) in round1.iter().enumerate() {
            let label = format!("p1 voter {v} round 1");
            submit(&nodes[fx::first_node(v)], req, &label).await?;
            expect1.push(sent(label, fx::first_node(v), req));
        }
        for v in fx::DUPLICATES {
            let n = &nodes[fx::dup_node(v)];
            match n.api.submit_vote(&round1[v]).await {
                Ok(()) => expect1.push(sent(
                    format!("p1 voter {v} duplicate"),
                    fx::dup_node(v),
                    &round1[v],
                )),
                // The first node's batch may already be in this node's tree.
                Err(ClientError::Api {
                    status: 409,
                    code: Some(CODE_DUPLICATE),
                    message,
                }) => {
                    say!("duplicate of voter {v} refused by {}: {message}", n.name)
                }
                Err(e) => bail!("duplicate of voter {v} to {}: {e}", n.name),
            }
        }
        for (i, req) in csp_votes.iter().enumerate() {
            let label = format!("p2 voter {i}");
            submit(&nodes[C], req, &label).await?;
            expect1.push(sent(label, C, req));
        }
        say!("round 1 submitted: {} packages", expect1.len());
        // Resilience: node C loses its RPC and the observer its beacon while
        // round 1 is in flight; both must catch up. The observer also loses
        // its first RPC until it has matched the chain through the second.
        let mut outages = Vec::new();
        if let Some(p) = obs_rpc_proxy.as_ref() {
            p.set_down(true);
            say!("observer first RPC down until it matches the chain via failover");
        }
        if let Some(p) = rpc_proxy.as_ref() {
            outages.push(p.outage(RPC_OUTAGE));
            say!("node-c RPC down for {RPC_OUTAGE:?}");
        }
        if let Some(p) = beacon_proxy.as_ref() {
            outages.push(p.outage(BEACON_OUTAGE));
            say!("observer beacon down for {BEACON_OUTAGE:?}");
        } else if resilience {
            say!("no beacon on anvil: observer outage skipped");
        }

        expect_rejected(&nodes[A], &bad_proof, (400, CODE_VOTE), "bad proof").await?;
        expect_rejected(&nodes[B], &bad_sig, (400, CODE_VOTE), "bad signature").await?;
        let not_in_census = (400, CODE_INVALID);
        expect_rejected(
            &nodes[C],
            &outsider_req,
            not_in_census,
            "address not in the census",
        )
        .await?;

        wait_settled(&nodes, &expect1, "round 1").await?;
        if let Some(p) = obs_rpc_proxy.as_ref() {
            observer_failover(&nodes, &reader, &[pid1, pid2], p).await?;
        }
        for h in outages {
            h.await?;
        }

        // A second ballot under a settled vote id, sent to a node other than the
        // one that took the first, once that node has synced its settlement.
        wait_roots(&nodes, &reader, &pid1).await?;
        let reuse_node = (fx::first_node(fx::REUSER) + 1) % fx::N_NODES;
        let dup = (409, CODE_DUPLICATE);
        expect_rejected(&nodes[reuse_node], &reused, dup, "reused vote id").await?;

        // Round 2: revotes through another node.
        let mut expect2 = Vec::new();
        for (j, v) in fx::REVOTERS.into_iter().enumerate() {
            let label = format!("p1 voter {v} round 2");
            submit(&nodes[fx::revote_node(v)], &round2[j], &label).await?;
            expect2.push(sent(label, fx::revote_node(v), &round2[j]));
        }
        say!("round 2 submitted: {} revotes", expect2.len());
        // Resilience: the prover goes away with a node-C batch in flight.
        if let Some(p) = prover_proxy.as_ref() {
            prover_outage(&nodes[C], &expect2, p).await?;
        }
        wait_settled(&nodes, &expect2, "round 2").await?;

        // Every node, the observer included, has the on-chain root in its view
        // and in its local tree.
        let root1 = wait_roots(&nodes, &reader, &pid1).await?;
        let root2 = wait_roots(&nodes, &reader, &pid2).await?;
        say!(
            "chain roots: p1 0x{} p2 0x{}",
            hex::encode(root1),
            hex::encode(root2)
        );
        check_local_trees(&nodes, &pid1, round1[0].vote_id, root1).await?;
        check_local_trees(&nodes, &pid2, csp_votes[0].vote_id, root2).await?;

        let c1 = reader.process(&pid1).await?;
        let c2 = reader.process(&pid2).await?;
        ensure!(
            c1.voters_count == fx::N_VOTERS as u64,
            "p1 votersCount {}",
            c1.voters_count
        );
        // Duplicates must not count as overwrites.
        ensure!(
            c1.overwritten_votes_count == fx::REVOTERS.len() as u64,
            "p1 overwrittenVotesCount {}, want {}",
            c1.overwritten_votes_count,
            fx::REVOTERS.len()
        );
        ensure!(
            c2.voters_count == fx::N_CSP as u64,
            "p2 votersCount {}",
            c2.voters_count
        );
        ensure!(
            c2.overwritten_votes_count == 0,
            "p2 overwrittenVotesCount {}",
            c2.overwritten_votes_count
        );

        // Recorded-as-cast: every vote id against the on-chain root.
        for s in expect1.iter().chain(&expect2) {
            let n = &nodes[s.node];
            let proof = n
                .api
                .vote_id_proof(&s.pid, s.vid)
                .await
                .with_context(|| format!("{} tracker proof from {}", s.label, n.name))?;
            let root = if s.pid == pid1 { root1 } else { root2 };
            ensure!(
                verify_tracker(&proof, &root),
                "{}: tracker proof from {} does not verify",
                s.label,
                n.name
            );
        }
        say!("{} tracker proofs verify", expect1.len() + expect2.len());

        // Concurrency: the on-chain transitions, who sent them, and the counters.
        let events = chain::transitions(&net.rpc, registry, scan_from).await?;
        let t1 = events.iter().filter(|t| t.pid == pid1).count() as u64;
        let t2 = events.iter().filter(|t| t.pid == pid2).count() as u64;
        let senders1: BTreeSet<_> = events
            .iter()
            .filter(|t| t.pid == pid1)
            .map(|t| t.sender)
            .collect();
        let archive = (
            nodes[A].api.transitions(&pid1).await?.len(),
            nodes[A].api.transitions(&pid2).await?.len(),
        );
        say!(
            "transitions on-chain: p1 {t1} from {} senders, p2 {t2}; node-a archive {archive:?}",
            senders1.len()
        );
        let ms = all_metrics(&nodes).await?;
        say!("metrics: {}", show_metrics(&nodes, &ms));
        let (sm, om) = (&ms[..OBS], &ms[OBS]);
        let settled: u64 = sm.iter().map(|m| m.settled_by_self).sum();
        let settlers = sm.iter().filter(|m| m.settled_by_self > 0).count();
        let synced: u64 = sm.iter().map(|m| m.synced_from_others).sum();
        let lost: Option<u64> = sm.iter().map(|m| m.lost_races).sum();
        let shown = show_metrics(&nodes[..OBS], sm);
        // Who wins a race is timing: hard on anvil, a note live.
        let race = |ok: bool, what: String| -> Result<()> {
            if !ok {
                ensure!(net.live, "{what}");
                say!("note (live, not asserted): {what}");
            }
            Ok(())
        };
        race(
            settled >= 2 && settlers >= 2,
            format!("settled_by_self: {shown}"),
        )?;
        ensure!(
            settled == t1 + t2,
            "settled_by_self sums to {settled}, on-chain transitions {}",
            t1 + t2
        );
        race(synced >= 1, format!("synced_from_others: {shown}"))?;
        race(lost.is_some_and(|l| l >= 1), format!("lost_races: {shown}"))?;
        race(
            senders1.len() >= 2,
            format!("process 1 transitions from {} sender(s)", senders1.len()),
        )?;
        ensure!(
            om.settled_by_self == 0,
            "observer settled {} transitions",
            om.settled_by_self
        );
        ensure!(
            om.synced_from_others == t1 + t2,
            "observer synced {} transitions, on-chain {}",
            om.synced_from_others,
            t1 + t2
        );

        // Results of both processes: p1 keyed by node A, p2 (CSP) by node C.
        org.end_process(&pid1).await.context("end process 1")?;
        org.end_process(&pid2).await.context("end process 2")?;
        say!("processes 1 and 2 ended");
        let csp_last: Vec<_> = (0..fx::N_CSP).map(|i| fx::choices(i, 1)).collect();
        let results = [
            (1, pid1, A, fx::expected_tally(&fx::last_choices())),
            (2, pid2, C, fx::expected_tally(&csp_last)),
        ];
        for (i, pid, keyed, want) in &results {
            let got = wait::until(
                &format!("ProcessResultsSet for process {i}"),
                results_timeout(),
                Duration::from_secs(3),
                || async {
                    alive(&nodes)?;
                    Ok(org.results(pid).await?)
                },
            )
            .await
            .with_context(|| {
                format!("{} log {}", nodes[*keyed].name, nodes[*keyed].log.display())
            })?;
            ensure!(
                &got == want,
                "process {i}: on-chain results {got:?}, expected tally {want:?}"
            );
            say!("process {i} results on-chain {got:?} = expected tally");
            for n in &nodes {
                wait::until(
                    &format!("{} to report the results of process {i}", n.name),
                    sync_timeout(),
                    Duration::from_secs(1),
                    || async {
                        alive(&nodes)?;
                        Ok((n.api.process(pid).await?.result.as_ref() == Some(want)).then_some(()))
                    },
                )
                .await?;
            }
        }
        say!(
            "all nodes and the observer report the results; {t1} transitions p1, {t2} p2; {}",
            show_metrics(&nodes, &all_metrics(&nodes).await?)
        );

        // Dynamic censuses on the same chain, prover and nodes.
        onchain_census(
            &mut nodes,
            &bin,
            &cfg("node-b", B, Some(B))?,
            &net,
            &org,
            &reader,
            &prover,
            &mut census_costs,
        )
        .await
        .context("origin 3")?;
        offchain_dynamic_census(&nodes, &org, &reader, &census_dir, &prover)
            .await
            .context("origin 2")?;

        // The DKG key modes on the same chain and nodes.
        if let Some(w) = &net.dkg {
            dkg_processes(&nodes, &net, &org, &reader, (&p1, &tree), &prover, w)
                .await
                .context("DKG processes")?;
        }

        // Last, so an RPC that cannot simulate them gates nothing.
        if flag("DAVINCI_E2E_NEGATIVE") {
            negative::run(&net.rpc, registry, scan_from, Some(pid1))
                .await
                .context("negative checks")?;
        }
        Ok(())
    }
    .await;
    if let Some(p) = &rpc_proxy {
        say!("node-c RPC outage: {} connections refused", p.refused());
    }
    if let Some(p) = &obs_rpc_proxy {
        p.set_down(false);
    }
    if let Some(p) = &beacon_proxy {
        say!(
            "observer beacon outage: {} connections refused",
            p.refused()
        );
    }
    let r = cancel_open(&org, r).await;
    let bill = costs(&net, &org, census_costs).await;
    say!("{}", bill.unwrap_or_else(|e| format!("gas bill: {e:#}")));
    drop(nodes);
    r
}

/// Waits until every node serves `addr`'s census proof at `root`: the node
/// has loaded (origin 2) or synced (origin 3) that census.
async fn wait_census(nodes: &[Node], pid: &[u8; 31], addr: &[u8; 20], root: Fr) -> Result<()> {
    for n in nodes {
        let last = std::sync::Mutex::new(String::new());
        let r = wait::until(
            &format!(
                "{} to serve 0x{} at the census root",
                n.name,
                hex::encode(addr)
            ),
            sync_timeout(),
            Duration::from_secs(1),
            || async {
                alive(nodes)?;
                let m = match n.api.participant(pid, addr).await {
                    Ok((p, _)) if p.root == root => return Ok(Some(())),
                    Ok((p, _)) => format!("proof at {}", p.root),
                    // Not in the census yet, or still loading (429).
                    Err(ClientError::Api {
                        status: 400 | 404 | 429,
                        message,
                        ..
                    }) => message,
                    Err(e) => return Err(e.into()),
                };
                if let Ok(mut l) = last.lock() {
                    *l = m;
                }
                Ok(None)
            },
        )
        .await;
        let last = last.lock().map(|l| l.clone()).unwrap_or_default();
        r.with_context(|| format!("last: {last}"))?;
    }
    Ok(())
}

/// The process as the registry holds it, once every node lists it and its
/// view checks out against the chain.
async fn wait_listed(
    nodes: &[Node],
    reader: &RegistryReader,
    pid: &[u8; 31],
) -> Result<OnchainProcess> {
    let p = reader.process(pid).await?;
    for n in nodes {
        wait::until(
            &format!("{} to list {}", n.name, ProcessId(*pid)),
            sync_timeout(),
            Duration::from_secs(1),
            || async {
                Ok(n.api
                    .processes()
                    .await?
                    .contains(&ProcessId(*pid))
                    .then_some(()))
            },
        )
        .await?;
        n.api
            .process(pid)
            .await?
            .check_against(&p)
            .with_context(|| format!("{} view of {}", n.name, ProcessId(*pid)))?;
    }
    Ok(p)
}

/// Roots, local trees, counters and tracker proofs of a settled process,
/// then its results: ends it and waits for the on-chain tally `want`.
async fn check_and_tally(
    nodes: &[Node],
    org: &Organizer,
    reader: &RegistryReader,
    pid: &[u8; 31],
    sent: &[Sent],
    counts: (u64, u64),
    want: Vec<u64>,
) -> Result<()> {
    check_settled(nodes, reader, pid, sent, counts).await?;
    org.end_process(pid).await.context("end process")?;
    wait_results(nodes, org, pid, &want).await
}

/// Roots, local trees, counters and tracker proofs of a settled process.
async fn check_settled(
    nodes: &[Node],
    reader: &RegistryReader,
    pid: &[u8; 31],
    sent: &[Sent],
    (voters, overwritten): (u64, u64),
) -> Result<()> {
    let root = wait_roots(nodes, reader, pid).await?;
    check_local_trees(nodes, pid, sent[0].vid, root).await?;
    let c = reader.process(pid).await?;
    ensure!(
        (c.voters_count, c.overwritten_votes_count) == (voters, overwritten),
        "votersCount {} overwrittenVotesCount {}, want {voters} and {overwritten}",
        c.voters_count,
        c.overwritten_votes_count
    );
    for s in sent {
        let n = &nodes[s.node];
        let proof = n
            .api
            .vote_id_proof(&s.pid, s.vid)
            .await
            .with_context(|| format!("{} tracker proof from {}", s.label, n.name))?;
        ensure!(
            verify_tracker(&proof, &root),
            "{}: tracker proof from {} does not verify",
            s.label,
            n.name
        );
    }
    say!(
        "{}: counters ok, {} tracker proofs verify",
        ProcessId(*pid),
        sent.len()
    );
    Ok(())
}

/// Waits for the results of `pid` on-chain (status RESULTS) and in every
/// node's view; both must be `want`.
async fn wait_results(nodes: &[Node], org: &Organizer, pid: &[u8; 31], want: &[u64]) -> Result<()> {
    let got = wait::until(
        "ProcessResultsSet",
        results_timeout(),
        Duration::from_secs(3),
        || async {
            alive(nodes)?;
            Ok(org.results(pid).await?)
        },
    )
    .await?;
    ensure!(
        got == want,
        "on-chain results {got:?}, expected tally {want:?}"
    );
    for n in nodes {
        wait::until(
            &format!("{} to report the results", n.name),
            sync_timeout(),
            Duration::from_secs(1),
            || async {
                alive(nodes)?;
                Ok((n.api.process(pid).await?.result.as_deref() == Some(want)).then_some(()))
            },
        )
        .await?;
    }
    say!(
        "{}: results on-chain {got:?} = expected tally",
        ProcessId(*pid)
    );
    Ok(())
}

/// Ballot of `voter` (index `v` of fixture set `tag`) in `round`.
fn job(
    tag: u64,
    voter: &Voter,
    v: usize,
    round: usize,
    p: &OnchainProcess,
    census: CensusWitness,
) -> fx::Job {
    fx::Job {
        label: format!("set {tag} voter {v} round {round}"),
        key: voter.key.clone(),
        process: p.clone(),
        fields: fx::choices(v, round),
        census,
        weight: fx::WEIGHT,
        k: fx::vote_k(tag, v, round),
    }
}

/// Early voters of the origin-3 process who revote after the growth.
const ONCHAIN_REVOTERS: [usize; 2] = [0, 1];
/// Census contract size after each `addMembers`: 3 founders, then waves that
/// cross 4, 8 and 16.
const ONCHAIN_SIZES: [usize; 5] = [3, 5, 9, 13, 18];

/// With the observer's first RPC still down, the observer must match the
/// chain root of every pid in `pids`: it has failed over to its second URL.
/// Then the proxy comes back.
async fn observer_failover(
    nodes: &[Node],
    reader: &RegistryReader,
    pids: &[[u8; 31]],
    proxy: &Proxy,
) -> Result<()> {
    let obs = &nodes[OBS..];
    let r: Result<()> = async {
        for pid in pids {
            wait_roots(obs, reader, pid)
                .await
                .context("observer roots with its first RPC down")?;
        }
        Ok(())
    }
    .await;
    proxy.set_down(false);
    r?;
    let refused = proxy.refused();
    let lines = nodes[OBS].log_lines(&["RPC failover", "switching"]);
    say!(
        "observer matched the chain with its first RPC down: {refused} connections refused, {} failover log lines",
        lines.len()
    );
    if refused == 0 {
        say!("observer failover MISSED (not a resilience pass): its first RPC was never tried");
    }
    if let Some(l) = lines.first() {
        say!("  {}", l.trim());
    } else {
        say!("note: no failover line in the observer log");
    }
    Ok(())
}

/// Takes the prover away for [`PROVER_OUTAGE`] once a node-C revote is
/// aggregated. The outage counts only if node C hit it: a refused prover
/// connection, or its revotes still unsettled when the prover returns.
async fn prover_outage(node: &Node, sent: &[Sent], proxy: &Proxy) -> Result<()> {
    let mine: Vec<_> = sent.iter().filter(|s| s.node == C).collect();
    if mine.is_empty() {
        say!(
            "prover outage MISSED (not a resilience pass): no round-2 vote on {}",
            node.name
        );
        return Ok(());
    }
    let unsettled = || async {
        let mut n = 0;
        for s in &mine {
            if !matches!(
                node.api.vote_status_full(&s.pid, s.vid).await?.status,
                VoteStatus::Settled | VoteStatus::Error
            ) {
                n += 1;
            }
        }
        anyhow::Ok(n)
    };
    let hit = wait::until(
        &format!("a {} vote to be aggregated", node.name),
        settle_timeout(),
        Duration::from_millis(200),
        || async {
            let mut settled = true;
            for s in &mine {
                match node.api.vote_status_full(&s.pid, s.vid).await?.status {
                    VoteStatus::Aggregated => return Ok(Some(true)),
                    VoteStatus::Settled | VoteStatus::Error => {}
                    _ => settled = false,
                }
            }
            Ok(settled.then_some(false))
        },
    )
    .await?;
    if !hit {
        say!(
            "prover outage MISSED (not a resilience pass): {}'s revotes settled first",
            node.name
        );
        return Ok(());
    }
    let before = proxy.refused();
    proxy.set_down(true);
    say!("prover down for {PROVER_OUTAGE:?} with a node-c batch in flight");
    tokio::time::sleep(PROVER_OUTAGE).await;
    let open = unsettled().await;
    proxy.set_down(false);
    let refused = proxy.refused() - before;
    let open = open?;
    if refused > 0 || open > 0 {
        say!(
            "prover outage observed: {refused} connections refused, {open} revotes unsettled at its end"
        );
    } else {
        say!(
            "prover outage MISSED (not a resilience pass): no connection refused, every revote settled"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Origin 3: an `OwnedCensus` that grows from 3 to 18 members while the
/// process runs, new members voting through every node, two early voters
/// revoting through another node, node B restarted between a seal and its
/// settlement (the exposed set must survive), a duplicate registration
/// refused, and the results. The key comes from node B, asked twice (keys
/// are derived, not stored).
async fn onchain_census(
    nodes: &mut [Node],
    bin: &Path,
    node_b: &NodeConfig,
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    prover: &BallotProver,
    bill: &mut Vec<TxCost>,
) -> Result<()> {
    let dir = census::census_dir();
    chain::forge_build(&dir)?;
    let census = Census::deploy(net.census_provider(org)?, &dir).await?;
    let r = onchain_votes(nodes, bin, node_b, org, reader, prover, &census).await;
    bill.extend(
        census
            .receipts()
            .iter()
            .map(|(l, r)| TxCost::of(l.clone(), r)),
    );
    r
}

async fn onchain_votes(
    nodes: &mut [Node],
    bin: &Path,
    node_b: &NodeConfig,
    org: &Organizer,
    reader: &RegistryReader,
    prover: &BallotProver,
    census: &Census,
) -> Result<()> {
    let voters = fx::onchain_voters();
    let parts: Vec<_> = voters.iter().map(|v| (v.address(), fx::WEIGHT)).collect();
    let add = async |from: usize, to: usize| -> Result<()> {
        census.add_members(&parts[from..to]).await?;
        let root = census.root().await?;
        ensure!(census.size().await? == to as u64, "census size");
        ensure!(
            root == fx::tree_of(&parts[..to])?.root(),
            "the contract's root is not the lean-IMT of its first {to} members"
        );
        Ok(())
    };
    add(0, ONCHAIN_SIZES[0]).await?;
    let (metadata, metadata_hash) =
        fx::write_metadata(&node_b.census_dir, "metadata-3.json", "origin-3 process")?;

    // A key is a function of the pid, so asking again gives the same one.
    let next = org.next_process_id().await?;
    let pk = nodes[B].api.new_key(&next).await?;
    ensure!(
        nodes[B].api.new_key(&next).await? == pk,
        "stateless keys: node-b gave two keys for one pid"
    );
    let pid = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 2 * 3600,
            max_voters: 32,
            ballot_mode: fx::ballot_mode(),
            census_origin: 3,
            // The registry reads the root from the contract.
            census_root: Fr::from(0u64),
            census_contract: census.address.0.0,
            census_uri: "onchain://davinci-e2e".into(),
            metadata,
            metadata_hash,
            key_mode: KeyMode::Sequencer(pk),
        })
        .await
        .context("create the origin-3 process")?
        .pid;
    let p = wait_listed(nodes, reader, &pid).await?;
    ensure!(p.census_origin == 3 && p.census_contract == census.address.0.0);
    ensure!(p.census_root == census.root().await?, "registry root");
    say!(
        "origin-3 process {} on OwnedCensus {}",
        ProcessId(pid),
        census.address
    );

    // Every ballot up front: the ballot proof does not bind the census root,
    // and the voter only checks an origin-3 witness against its own leaf.
    let tree = fx::tree_of(&parts)?;
    let mut jobs = Vec::new();
    for (v, voter) in voters.iter().enumerate() {
        jobs.push(job(5, voter, v, 1, &p, fx::merkle_witness(&tree, v)?));
    }
    for v in ONCHAIN_REVOTERS {
        jobs.push(job(5, &voters[v], v, 2, &p, fx::merkle_witness(&tree, v)?));
    }
    let reqs = fx::prove_all(prover, &jobs)?;

    // Founders vote, then each wave joins and votes once every node has
    // synced the contract's new root.
    let mut first = Vec::new();
    let mut from = 0;
    for (i, to) in ONCHAIN_SIZES.into_iter().enumerate() {
        if i > 0 {
            add(from, to).await?;
            say!("OwnedCensus grew to {to} members");
        }
        wait_census(nodes, &pid, &parts[to - 1].0, census.root().await?).await?;
        for v in from..to {
            let label = format!("p3 voter {v} round 1");
            submit(&nodes[fx::first_node(v)], &reqs[v], &label).await?;
            first.push(sent(label, fx::first_node(v), &reqs[v]));
        }
        from = to;
    }

    // A second registration of a member reverts. SlotTaken (a new address on
    // a taken ballot slot) needs a sha256 collision; the Solidity unit tests
    // cover it with a mock.
    let (root, size) = (census.root().await?, census.size().await?);
    let e = census.add_member(parts[0].0, fx::WEIGHT).await.unwrap_err();
    ensure!(
        format!("{e:#}").contains("AlreadyRegisteredAddress"),
        "duplicate addMember: {e:#}"
    );
    ensure!((census.root().await?, census.size().await?) == (root, size));
    say!("duplicate registration refused: {e:#}");

    // Exposed set: kill node-b once one of its votes is sealed, before it settles,
    // and restart it on the same datadir.
    let s = first
        .iter()
        .rev()
        .find(|s| s.node == B)
        .cloned()
        .context("no vote on node-b")?;
    let sealed = wait::until(
        &format!("{} to be sealed on node-b", s.label),
        settle_timeout(),
        Duration::from_millis(200),
        || async {
            let r = nodes[B].api.vote_status_full(&s.pid, s.vid).await?;
            match r.status {
                VoteStatus::Aggregated | VoteStatus::Processed => Ok(Some(true)),
                VoteStatus::Settled => Ok(Some(false)),
                VoteStatus::Error => bail!("{} in error: {}", s.label, r.error.unwrap_or_default()),
                VoteStatus::Pending => Ok(None),
            }
        },
    )
    .await?;
    if sealed {
        nodes[B].kill()?;
        say!("node-b killed with {} sealed, not settled", s.label);
        nodes[B] = Node::start(bin, node_b).await.context("restart node-b")?;
        ensure!(
            nodes[B].api.new_key(&pid).await? == pk,
            "stateless keys: node-b's key changed across a restart"
        );
        say!("node-b restarted at {}", nodes[B].url);
    } else {
        say!(
            "exposed-set restart skipped: {} settled before it was seen sealed",
            s.label
        );
    }
    wait_settled(nodes, &first, "origin 3 first votes").await?;

    let mut all = first;
    let mut revotes = Vec::new();
    for (j, v) in ONCHAIN_REVOTERS.into_iter().enumerate() {
        let r = &reqs[fx::N_ONCHAIN + j];
        let (label, n) = (format!("p3 voter {v} round 2"), fx::revote_node(v));
        submit(&nodes[n], r, &label).await?;
        revotes.push(sent(label, n, r));
    }
    wait_settled(nodes, &revotes, "origin 3 revotes").await?;
    all.extend(revotes);

    let last: Vec<_> = (0..fx::N_ONCHAIN)
        .map(|v| fx::choices(v, if ONCHAIN_REVOTERS.contains(&v) { 2 } else { 1 }))
        .collect();
    let counts = (fx::N_ONCHAIN as u64, ONCHAIN_REVOTERS.len() as u64);
    check_and_tally(
        nodes,
        org,
        reader,
        &pid,
        &all,
        counts,
        fx::expected_tally(&last),
    )
    .await
}

/// Origin-2 member who revotes after the update.
const DYN_REVOTER: usize = 1;
/// Origin-2 member reweighted by the update while its vote is pending.
const DYN_REWEIGHTED: usize = 4;

/// Origin 2: a CensusDump with six members, four of them vote; the
/// organizer then replaces the census with a JSONL that keeps them (one
/// reweighted while its vote is pending: that vote errors) and adds six
/// more, who vote along with one revote. Key from node C.
async fn offchain_dynamic_census(
    nodes: &[Node],
    org: &Organizer,
    reader: &RegistryReader,
    census_dir: &Path,
    prover: &BallotProver,
) -> Result<()> {
    let voters = fx::dynamic_voters();
    let v1: Vec<_> = voters[..fx::N_DYN_OLD]
        .iter()
        .map(|v| (v.address(), fx::WEIGHT))
        .collect();
    let mut v2 = v1.clone();
    v2[DYN_REWEIGHTED].1 = 2 * fx::WEIGHT;
    v2.extend(
        voters[fx::N_DYN_OLD..]
            .iter()
            .map(|v| (v.address(), fx::WEIGHT)),
    );
    let (t1, t2) = (fx::tree_of(&v1)?, fx::tree_of(&v2)?);
    let uri1 = fx::write_census_dump(census_dir, "census-dyn-v1.json", &v1)?;
    let uri2 = fx::write_census_jsonl(census_dir, "census-dyn-v2.jsonl", &v2)?;
    let (metadata, metadata_hash) =
        fx::write_metadata(census_dir, "metadata-4.json", "origin-2 process")?;

    let next = org.next_process_id().await?;
    let pk = nodes[C].api.new_key(&next).await?;
    let pid = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 2 * 3600,
            max_voters: 16,
            ballot_mode: fx::ballot_mode(),
            census_origin: 2,
            census_root: t1.root(),
            census_contract: [0; 20],
            census_uri: uri1,
            metadata,
            metadata_hash,
            key_mode: KeyMode::Sequencer(pk),
        })
        .await
        .context("create the origin-2 process")?
        .pid;
    let p = wait_listed(nodes, reader, &pid).await?;
    say!("origin-2 process {}", ProcessId(pid));

    // Ballots up front; the new members' against the updated root.
    let mut p2 = p.clone();
    p2.census_root = t2.root();
    let mut jobs = Vec::new();
    for (v, voter) in voters.iter().enumerate() {
        if v <= DYN_REWEIGHTED {
            jobs.push(job(6, voter, v, 1, &p, fx::merkle_witness(&t1, v)?));
        } else if v >= fx::N_DYN_OLD {
            jobs.push(job(6, voter, v, 1, &p2, fx::merkle_witness(&t2, v)?));
        }
    }
    let w = fx::merkle_witness(&t2, DYN_REVOTER)?;
    jobs.push(job(6, &voters[DYN_REVOTER], DYN_REVOTER, 2, &p2, w));
    let reqs = fx::prove_all(prover, &jobs)?;
    let (old, rest) = reqs.split_at(DYN_REWEIGHTED + 1);
    let (new, revote) = rest.split_at(fx::N_DYN - fx::N_DYN_OLD);

    wait_census(nodes, &pid, &v1[0].0, t1.root()).await?;
    let mut all = Vec::new();
    for (v, r) in old[..DYN_REWEIGHTED].iter().enumerate() {
        let label = format!("p4 member {v} round 1");
        submit(&nodes[fx::first_node(v)], r, &label).await?;
        all.push(sent(label, fx::first_node(v), r));
    }
    wait_settled(nodes, &all, "origin 2 before the update").await?;

    // The update lands while the reweighted member's vote is pending.
    let n = fx::first_node(DYN_REWEIGHTED);
    let reweighted = sent("p4 member 4 (reweighted)", n, &old[DYN_REWEIGHTED]);
    submit(&nodes[n], &old[DYN_REWEIGHTED], &reweighted.label).await?;
    org.set_process_census(&pid, t2.root(), &uri2)
        .await
        .context("setProcessCensus")?;
    let c = reader.process(&pid).await?;
    ensure!(
        c.census_root == t2.root() && c.census_uri == uri2,
        "registry census after the update"
    );
    say!("census replaced: {} -> {} members", v1.len(), v2.len());
    let why = wait::until(
        "the reweighted member's vote to error",
        settle_timeout(),
        Duration::from_secs(1),
        || async {
            alive(nodes)?;
            let r = nodes[n].api.vote_status_full(&pid, reweighted.vid).await?;
            match r.status {
                VoteStatus::Error => Ok(Some(r.error.unwrap_or_default())),
                VoteStatus::Settled => bail!("the reweighted member's vote settled"),
                _ => Ok(None),
            }
        },
    )
    .await?;
    ensure!(
        why.contains("census changed"),
        "reweighted vote error: {why}"
    );
    say!("reweighted member's pending vote errored: {why}");

    wait_census(nodes, &pid, &v2[fx::N_DYN - 1].0, t2.root()).await?;
    let mut after = Vec::new();
    for (j, r) in new.iter().enumerate() {
        let v = fx::N_DYN_OLD + j;
        let label = format!("p4 member {v} round 1");
        submit(&nodes[fx::first_node(v)], r, &label).await?;
        after.push(sent(label, fx::first_node(v), r));
    }
    let (label, rn) = (
        format!("p4 member {DYN_REVOTER} round 2"),
        fx::revote_node(DYN_REVOTER),
    );
    submit(&nodes[rn], &revote[0], &label).await?;
    after.push(sent(label, rn, &revote[0]));
    wait_settled(nodes, &after, "origin 2 after the update").await?;
    all.extend(after);

    // Members 0..=3 (1 revoted) and the six added; 4's vote was dropped.
    let last: Vec<_> = (0..DYN_REWEIGHTED)
        .map(|v| fx::choices(v, if v == DYN_REVOTER { 2 } else { 1 }))
        .chain((fx::N_DYN_OLD..fx::N_DYN).map(|v| fx::choices(v, 1)))
        .collect();
    let counts = ((DYN_REWEIGHTED + fx::N_DYN - fx::N_DYN_OLD) as u64, 1);
    check_and_tally(
        nodes,
        org,
        reader,
        &pid,
        &all,
        counts,
        fx::expected_tally(&last),
    )
    .await
}

/// Process-1 voters of the automatic and the locked DKG process.
const DKG_AUTO_VOTERS: std::ops::Range<usize> = 0..6;
const DKG_LOCKED_VOTERS: std::ops::Range<usize> = 6..12;
/// Voter whose ballot for the automatic process is encrypted under the
/// locked process's key.
const DKG_OTHER_KEY_VOTER: usize = 12;
/// How long the locked process runs: it ends by time, so its decryption
/// request is what ends it. Its votes must settle before (scaled live).
const DKG_LOCKED_DURATION: Duration = Duration::from_secs(300);

/// A DKG_AUTOMATIC process on process 1's census and metadata (`p1`),
/// `duration` s long.
fn dkg_new_process(process_id: [u8; 31], p1: &OnchainProcess, duration: u64) -> NewProcess {
    NewProcess {
        process_id,
        start_time: 0,
        duration,
        max_voters: fx::N_VOTERS as u64,
        ballot_mode: fx::ballot_mode(),
        census_origin: 1,
        census_root: p1.census_root,
        census_contract: [0; 20],
        census_uri: p1.census_uri.clone(),
        metadata: p1.metadata_uri.clone(),
        metadata_hash: p1.metadata_hash,
        key_mode: KeyMode::DkgAutomatic,
    }
}

/// Creates a DKG-mode process. The shared committee spends its pool of keys
/// and then opens its next epoch on its own: while no epoch has a free key,
/// waits for the next one and tries again (three times at most).
async fn create_dkg(
    net: &Net,
    w: &DkgWiring,
    org: &Organizer,
    np: &NewProcess,
) -> Result<CreatedProcess> {
    let mode = np.key_mode;
    let mut waits = 0;
    loop {
        match org.create_process(np).await {
            Err(e) if dkg::no_free_pool_key(&e) && waits < 3 => {
                waits += 1;
                say!("create a {mode:?} process: {e}; waiting for the committee's next epoch");
                let t = Instant::now();
                let eid = dkg::wait_registration_epoch(&net.rpc, w.adapter)
                    .await
                    .with_context(|| format!("create a {mode:?} process"))?;
                say!(
                    "DKG epoch 0x{} has a free pool key after {:.0} s",
                    hex::encode(eid),
                    t.elapsed().as_secs_f64()
                );
            }
            r => return r.with_context(|| format!("create a {mode:?} process")),
        }
    }
}

/// Waits for `pid`'s `ResultsDecryptionRequested`; returns it.
async fn dkg_request(
    net: &Net,
    nodes: &[Node],
    pid: &[u8; 31],
    timeout: Duration,
) -> Result<chain::RegistryTx> {
    let from = net.start_block.unwrap_or(net.from_block);
    wait::until(
        &format!("the decryption request of {}", ProcessId(*pid)),
        timeout,
        Duration::from_secs(3),
        || async {
            alive(nodes)?;
            let reqs = chain::dkg_requests(&net.rpc, net.registry, from).await?;
            Ok(reqs.into_iter().find(|r| r.pid == *pid))
        },
    )
    .await
}

/// Block of `pid`'s `ProcessResultsSet`.
async fn results_block(net: &Net, pid: &[u8; 31]) -> Result<u64> {
    let from = net.start_block.unwrap_or(net.from_block);
    chain::results_txs(&net.rpc, net.registry, from)
        .await?
        .iter()
        .find(|r| r.pid == *pid)
        .map(|r| r.block)
        .context("no ProcessResultsSet")
}

/// The DKG key modes, through the registry only: an automatic process
/// (with a ballot under another key refused), a locked one that ends by
/// time and stays undecrypted until its organizer reveals (a wrong secret
/// first), a zero-vote one tallied without the DKG, and the eth_call
/// negatives around the two requests.
#[allow(clippy::too_many_arguments)]
async fn dkg_processes(
    nodes: &[Node],
    net: &Net,
    org: &Organizer,
    reader: &RegistryReader,
    (p1, tree): (&OnchainProcess, &LeanImt),
    prover: &BallotProver,
    w: &DkgWiring,
) -> Result<()> {
    let voters = fx::merkle_voters();
    let locked_duration = net::scaled(DKG_LOCKED_DURATION).as_secs();
    let mut created = Vec::new();
    // The locked one last: its clock starts at creation, and a wait for the
    // committee's next epoch before another create would eat into it.
    for (mode, duration) in [
        (KeyMode::DkgAutomatic, 2 * 3600),
        (KeyMode::DkgAutomatic, 2 * 3600),
        (KeyMode::DkgLocked, locked_duration),
    ] {
        let next = org.next_process_id().await?;
        let np = NewProcess {
            key_mode: mode,
            ..dkg_new_process(next, p1, duration)
        };
        let c = create_dkg(net, w, org, &np).await?;
        let p = wait_listed(nodes, reader, &c.pid).await?;
        dkg::check_process_key(&net.rpc, w, &p, c.organizer_secret.as_ref())
            .await
            .with_context(|| format!("{mode:?} key of {}", ProcessId(c.pid)))?;
        created.push((p, c.organizer_secret));
    }
    let [(pa, _), (pz, _), (pl, sk)] =
        <[_; 3]>::try_from(created).map_err(|_| anyhow::anyhow!("three processes"))?;
    let sk = sk.context("no organizer secret for the locked process")?;
    let (da, dl, dz) = (
        pa.dkg.context("dkg")?,
        pl.dkg.context("dkg")?,
        pz.dkg.context("dkg")?,
    );
    say!(
        "DKG processes: automatic {}, locked {} ({locked_duration} s), zero-vote {}; epochs 0x{}, 0x{}, 0x{}",
        ProcessId(pa.id),
        ProcessId(pl.id),
        ProcessId(pz.id),
        hex::encode(da.epoch_id),
        hex::encode(dl.epoch_id),
        hex::encode(dz.epoch_id)
    );
    // No votes: tallied as zeros on the request, without the committee.
    org.end_process(&pz.id)
        .await
        .context("end the zero-vote process")?;

    let mut jobs = Vec::new();
    for v in DKG_AUTO_VOTERS {
        jobs.push(job(7, &voters[v], v, 1, &pa, fx::merkle_witness(tree, v)?));
    }
    for v in DKG_LOCKED_VOTERS {
        jobs.push(job(8, &voters[v], v, 1, &pl, fx::merkle_witness(tree, v)?));
    }
    let mut other_key = pa.clone();
    other_key.encryption_key = pl.encryption_key;
    let v = DKG_OTHER_KEY_VOTER;
    jobs.push(job(
        9,
        &voters[v],
        v,
        1,
        &other_key,
        fx::merkle_witness(tree, v)?,
    ));
    let reqs = fx::prove_all(prover, &jobs)?;
    let (auto_reqs, rest) = reqs.split_at(DKG_AUTO_VOTERS.len());
    let (locked_reqs, other) = rest.split_at(DKG_LOCKED_VOTERS.len());

    let (mut sa, mut sl) = (Vec::new(), Vec::new());
    for (v, r) in DKG_AUTO_VOTERS.zip(auto_reqs) {
        let label = format!("dkg-auto voter {v}");
        submit(&nodes[fx::first_node(v)], r, &label).await?;
        sa.push(sent(label, fx::first_node(v), r));
    }
    for (v, r) in DKG_LOCKED_VOTERS.zip(locked_reqs) {
        let label = format!("dkg-locked voter {v}");
        submit(&nodes[fx::first_node(v)], r, &label).await?;
        sl.push(sent(label, fx::first_node(v), r));
    }
    // The ballot proof binds the key: the node's inputs hash differs.
    expect_rejected(
        &nodes[fx::first_node(v)],
        &other[0],
        (400, CODE_VOTE),
        "ballot under another process's key",
    )
    .await?;
    let all: Vec<_> = sa.iter().chain(&sl).cloned().collect();
    wait_settled(nodes, &all, "DKG votes").await?;
    let p = reader.process(&pl.id).await?;
    ensure!(
        p.status == ProcessStatus::Ready && !p.dkg.is_some_and(|d| d.results_requested),
        "the locked process ended before its votes settled: raise DAVINCI_E2E_TIMEOUT_SCALE"
    );

    // Automatic: the organizer ends it, a sequencer requests, the committee
    // decrypts, a sequencer finalizes.
    let want_a = fx::expected_tally(
        &DKG_AUTO_VOTERS
            .map(|v| fx::choices(v, 1))
            .collect::<Vec<_>>(),
    );
    let t = Instant::now();
    let counts = (DKG_AUTO_VOTERS.len() as u64, 0);
    check_and_tally(nodes, org, reader, &pa.id, &sa, counts, want_a)
        .await
        .context("automatic")?;
    let req_a = dkg_request(net, nodes, &pa.id, sync_timeout()).await?;
    let res_a = results_block(net, &pa.id).await?;
    let a = reader.process(&pa.id).await?.dkg.context("dkg")?;
    ensure!(
        a.results_requested
            && usize::from(a.count) == usize::from(fx::NUM_FIELDS)
            && a.zero_skipped == 0,
        "automatic DKG state {a:?}"
    );
    let cts = dkg::aid_log_blocks(&net.rpc, w.manager, da.aid, false, req_a.block).await?;
    ensure!(
        cts.len() == usize::from(a.count),
        "{} ciphertexts submitted",
        cts.len()
    );
    say!(
        "automatic: results {:.0} s after the end; request at block {}, results at {}",
        t.elapsed().as_secs_f64(),
        req_a.block,
        res_a
    );

    // Zero votes: all zeros, nothing submitted to the committee.
    wait_results(nodes, org, &pz.id, &fx::expected_tally(&[]))
        .await
        .context("zero-vote")?;
    let z = reader.process(&pz.id).await?.dkg.context("dkg")?;
    let all_fields = (1u16 << fx::NUM_FIELDS) - 1;
    ensure!(
        z.results_requested && z.count == 0 && z.zero_skipped & all_fields == all_fields,
        "zero-vote DKG state {z:?}"
    );
    let from = net.start_block.unwrap_or(net.from_block);
    let cts = dkg::aid_log_blocks(&net.rpc, w.manager, dz.aid, false, from).await?;
    ensure!(
        cts.is_empty(),
        "zero-vote: {} ciphertexts submitted",
        cts.len()
    );
    let req_z = dkg_request(net, nodes, &pz.id, sync_timeout()).await?;
    ensure!(
        results_block(net, &pz.id).await? == req_z.block,
        "zero-vote: results not set by the request"
    );
    say!(
        "zero-vote: results all 0 set by the request at block {}, no ciphertext",
        req_z.block
    );

    // Locked: ends by time; a sequencer requests; nothing is decrypted
    // before the organizer reveals.
    check_settled(
        nodes,
        reader,
        &pl.id,
        &sl,
        (DKG_LOCKED_VOTERS.len() as u64, 0),
    )
    .await?;
    let wait_end = Duration::from_secs(locked_duration) + results_timeout();
    let req_l = dkg_request(net, nodes, &pl.id, wait_end).await?;
    let p = reader.process(&pl.id).await?;
    let l = p.dkg.context("dkg")?;
    ensure!(
        p.status == ProcessStatus::Ended && l.results_requested,
        "locked after its request: {:?} {l:?}",
        p.status
    );
    match org.cancel_process(&pl.id).await {
        Err(ClientError::Reverted(n)) if n == "InvalidStatus" => {}
        other => bail!("cancel after the request: {other:?}, want InvalidStatus"),
    }
    // Give the committee twice what the automatic process needed.
    let settle = 2 * (res_a - req_a.block) + 5;
    let chain_head = alloy::providers::ProviderBuilder::new().connect_client(chain::rpc(&net.rpc)?);
    let head = wait::until(
        "blocks for the committee",
        results_timeout(),
        Duration::from_secs(2),
        || async {
            alive(nodes)?;
            let h = alloy::providers::Provider::get_block_number(&chain_head).await?;
            Ok((h >= req_l.block + settle).then_some(h))
        },
    )
    .await?;
    let partials = dkg::aid_log_blocks(&net.rpc, w.manager, dl.aid, true, req_l.block).await?;
    ensure!(
        partials.is_empty(),
        "locked: {} partial decryptions before the reveal",
        partials.len()
    );
    let cts = dkg::aid_log_blocks(&net.rpc, w.manager, dl.aid, false, req_l.block).await?;
    ensure!(
        cts.len() == usize::from(l.count),
        "locked: {} ciphertexts",
        cts.len()
    );
    let finalize = davinci_client::organizer::ProcessRegistry::finalizeResultsFromDKGCall {
        processId: alloy::primitives::FixedBytes(pl.id),
    };
    let got = negative::simulate(
        &net.rpc,
        org.address(),
        net.registry,
        alloy::sol_types::SolCall::abi_encode(&finalize),
    )
    .await?;
    ensure!(
        got == negative::Outcome::Reverted("ResultsNotReady".into()),
        "finalize before the reveal: {got}"
    );
    ensure!(
        org.results(&pl.id).await?.is_none(),
        "locked results before the reveal"
    );
    say!(
        "locked: requested at block {}, no partials by block {head}, finalize reverts ResultsNotReady",
        req_l.block
    );
    let mut b = [0u8; 32];
    b[31] = 7;
    let wrong = OrganizerSecret::from_be_bytes(&b)?;
    ensure!(wrong != sk, "the wrong secret is the secret");
    match org.reveal_process_key(&pl.id, &wrong).await {
        Err(ClientError::Reverted(n)) if n == "InvalidOrganizerSecret" => {}
        other => bail!("reveal with a wrong secret: {other:?}, want InvalidOrganizerSecret"),
    }
    let t = Instant::now();
    org.reveal_process_key(&pl.id, &sk)
        .await
        .context("reveal")?;
    let want_l = fx::expected_tally(
        &DKG_LOCKED_VOTERS
            .map(|v| fx::choices(v, 1))
            .collect::<Vec<_>>(),
    );
    wait_results(nodes, org, &pl.id, &want_l)
        .await
        .context("locked")?;
    let partials = dkg::aid_log_blocks(&net.rpc, w.manager, dl.aid, true, req_l.block).await?;
    ensure!(!partials.is_empty(), "locked: results without partials");
    say!(
        "locked: results {:.0} s after the reveal ({} partials)",
        t.elapsed().as_secs_f64(),
        partials.len()
    );

    negative::dkg(&net.rpc, net.registry, org.address(), &req_a, &req_l)
        .await
        .context("DKG negative checks")?;
    Ok(())
}
