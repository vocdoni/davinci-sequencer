//! HTTP API tests: every route served over a real listener,
//! exercised with `SequencerClient` (and raw reqwest for the malformed
//! cases). Happy-path votes carry real circom proofs, cached under
//! `tests/cache/` (first run needs the artifacts in `CIRCOM_ARTIFACTS`).

mod common;

use std::sync::OnceLock;

use common::*;
use davinci_client::api::{
    self as wire, CensusProofWire, CspWire, ErrorResponse, ProcessId, VoteRequest, verify_tracker,
    vote_id_hex,
};
use davinci_client::prover::BallotProver;
use davinci_client::voter::CircomInputs;
use davinci_client::{Error as ClientError, SequencerClient};
use davinci_sequencer::api::router;
use davinci_zkvm_sdk::census::csp_sign;
use davinci_zkvm_sdk::limits::{BALLOT_MIN, NUM_FIELDS};

// ------------------------------------------------------- real ballot proofs

// `CIRCOM_ARTIFACTS`, else the davinci-circom checkout beside the workspace.
fn artifacts_dir() -> PathBuf {
    std::env::var_os("CIRCOM_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-circom/artifacts")
        })
}

fn ballot_prover() -> &'static BallotProver {
    static P: OnceLock<BallotProver> = OnceLock::new();
    P.get_or_init(|| {
        let a = artifacts_dir();
        BallotProver::load(
            &a.join("ballot_proof.wasm"),
            &a.join("ballot_proof_pkey.zkey"),
        )
        .unwrap()
    })
}

// The inputs hash binds everything the proof binds, so it keys the cache.
fn cached_proof(inputs: &CircomInputs, ih: &Fr) -> SnarkJsProof {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cache");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.json", hex::encode(fr_to_be(ih))));
    if let Ok(s) = std::fs::read_to_string(&path)
        && let Ok(p) = serde_json::from_str(&s)
    {
        return p;
    }
    let (proof, _) = ballot_prover().prove(inputs).unwrap();
    std::fs::write(&path, serde_json::to_string(&proof).unwrap()).unwrap();
    proof
}

/// Like `fake_vote`, but with a real (cached) circom proof.
fn real_vote(env: &Env, idx: usize, fields: &[u64], kseed: u64) -> VerifiedVote {
    let mut v = fake_vote(env, idx, fields, kseed);
    let cfg = &env.cfg;
    let mut padded = [Fr::from(0u64); NUM_FIELDS];
    for (o, f) in padded.iter_mut().zip(fields) {
        *o = Fr::from(*f);
    }
    let inputs = CircomInputs {
        fields: padded,
        packed_ballot_mode: cfg.ballot_mode.pack().unwrap(),
        address: address_to_fr(&v.pkg.address),
        weight: Fr::from(1u64),
        process_id: cfg.process_id,
        vote_id: Fr::from(v.pkg.vote_id),
        encryption_pubkey: [cfg.enc_key.x, cfg.enc_key.y],
        k: Fr::from(kseed),
        cipherfields: v.pkg.ballot.coords(),
        inputs_hash: v.pkg.inputs_hash,
    };
    v.pkg.proof = cached_proof(&inputs, &v.pkg.inputs_hash);
    v
}

/// The wire form of a vote (no census proof: Merkle is re-derived server-side).
fn wire_vote(v: &VerifiedVote) -> VoteRequest {
    let pkg = &v.pkg;
    let mut sig = [0u8; 65];
    sig[..32].copy_from_slice(&pkg.signature.r);
    sig[32..64].copy_from_slice(&pkg.signature.s);
    sig[64] = pkg.signature.v;
    VoteRequest {
        process_id: ProcessId(pid31()),
        address: pkg.address,
        vote_id: pkg.vote_id,
        ballot: pkg.ballot,
        ballot_proof: pkg.proof.clone(),
        ballot_inputs_hash: pkg.inputs_hash,
        signature: sig,
        weight: pkg.weight,
        census_proof: None,
    }
}

// ----------------------------------------------------------- serve harness

struct Served {
    s: TestSetup,
    node: Node,
    client: SequencerClient,
    base: String,
    shutdown: CancellationToken,
    _dir: TempDir,
}

/// Serves an already-started node over a real listener.
async fn serve_node(node: Node, shutdown: CancellationToken) -> (SequencerClient, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(node).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
            .unwrap();
    });
    (SequencerClient::new(&base), base)
}

async fn serve_with(
    s: TestSetup,
    prover: Arc<FakeProver>,
    batch_max: usize,
    extra: &[&str],
) -> Served {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let cfg = test_config_with(dir.path(), &s.census_dir, batch_max, "0s", extra);
    let node = start_node_cfg(db, s.chain.clone(), prover, cfg, shutdown.clone()).await;
    let (client, base) = serve_node(node.clone(), shutdown.clone()).await;
    Served {
        s,
        node,
        client,
        base,
        shutdown,
        _dir: dir,
    }
}

async fn serve(prover: Arc<FakeProver>, batch_max: usize) -> Served {
    serve_with(setup(2, 4, None), prover, batch_max, &[]).await
}

/// A CSP-census flavour of `setup`: root = the CSP address (voter key 99).
fn csp_setup() -> TestSetup {
    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::Csp;
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(&voter_address(99));
    e.cfg.census_root = fr_from_be(&be).unwrap();
    let census_dir = TempDir::new().unwrap();
    let uri = write_census(census_dir.path(), 4); // unused for CSP
    let chain = FakeChain::new(&e, uri);
    let dir = census_dir.path().to_path_buf();
    TestSetup {
        env: e,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    }
}

/// Status and code of an expected API error.
fn api_code(e: ClientError) -> (u16, u32) {
    match e {
        ClientError::Api { status, code, .. } => (status, code.unwrap_or(0)),
        other => panic!("expected an api error, got {other:?}"),
    }
}

async fn raw_post(base: &str, path: &str, body: String) -> (u16, ErrorResponse) {
    let resp = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn ping_info_and_process_views() {
    let t = serve(FakeProver::gated(), 100).await;
    handle(&t.node).await;
    t.client.ping().await.unwrap();

    let info = t.client.info().await.unwrap();
    assert_eq!(info.sequencer_address, Some([0xa1; 20]));
    assert_eq!(info.chain_id, 0);
    let mut reg = [0u8; 20];
    reg[19] = 1;
    assert_eq!(info.process_registry, reg);
    assert_eq!(info.ballot_vk_hash, vk_hash());
    assert_eq!(info.batch_program_vk, release::BATCH_PROGRAM_VK);
    assert_eq!(info.results_program_vk, release::RESULTS_PROGRAM_VK);
    assert!(!info.observer);
    assert_eq!(info.settled_by_self, 0);
    assert_eq!(info.synced_from_others, 0);
    assert_eq!(info.lost_races, 0);

    assert_eq!(
        t.client.processes().await.unwrap(),
        vec![ProcessId(pid31())]
    );
    let p = t.client.process(&pid31()).await.unwrap();
    assert_eq!(p.id, ProcessId(pid31()));
    assert_eq!(p.status, wire::ProcessStatus::Ready);
    assert!(p.is_accepting_votes);
    assert_eq!(p.census.census_origin, 1);
    assert_eq!(p.census.census_root, t.s.env.cfg.census_root);
    assert_eq!(p.state_root, t.s.chain.root());
    // The actor's committed local root equals the chain root at genesis.
    assert_eq!(p.local_state_root, Some(p.state_root));
    assert_eq!(p.max_voters, 1000);
    assert_eq!(p.voters_count, 0);
    assert!(p.result.is_none());

    // Unknown and malformed process ids.
    let e = t.client.process(&bad_pid31()).await.unwrap_err();
    assert_eq!(api_code(e), (404, 40402));
    let resp = reqwest::get(format!("{}/processes/zz", t.base))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert_eq!(resp.json::<ErrorResponse>().await.unwrap().code, 40001);
    t.shutdown.cancel();
}

/// The key is derived from the pid, so asking twice gives the same
/// key and stores nothing; the per-IP rate limit still applies.
#[tokio::test]
async fn keys_are_derived_not_stored_and_rate_limited() {
    let t = serve_with(
        setup(2, 4, None),
        FakeProver::gated(),
        100,
        &["--keys-per-minute", "4"],
    )
    .await;
    handle(&t.node).await;
    let rows = t.node.db.enc_key_count().unwrap();
    assert_eq!(rows, 1, "only the master secret");
    let pk = t.client.new_key(&pid31()).await.unwrap();
    assert_eq!(pk, node_key(&t.node.db));
    assert_eq!(t.client.new_key(&pid31()).await.unwrap(), pk);
    assert_ne!(t.client.new_key(&bad_pid31()).await.unwrap(), pk);
    assert_eq!(t.node.db.enc_key_count().unwrap(), rows);
    // A body without a process id is refused; it still costs a token.
    let (status, e) = raw_post(&t.base, "/processes/keys", "{}".into()).await;
    assert_eq!((status, e.code), (400, 40001));
    // Fifth call: the per-minute tokens are gone.
    let e = t.client.new_key(&pid31()).await.unwrap_err();
    assert_eq!(api_code(e), (429, 42901));
    t.shutdown.cancel();
}

#[tokio::test]
async fn participant_proof_verifies() {
    let t = serve(FakeProver::gated(), 100).await;
    handle(&t.node).await;
    // The client itself checks the leaf and the proof; pin the root too.
    let (proof, weight) = t
        .client
        .participant(&pid31(), &voter_address(0))
        .await
        .unwrap();
    assert_eq!(weight, 1);
    assert_eq!(proof.root, t.s.env.imt.root());
    // An address outside the census has no proof.
    let e = t
        .client
        .participant(&pid31(), &voter_address(9))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    t.shutdown.cancel();
}

#[tokio::test]
async fn vote_flow_settles_with_proof_ballot_and_archive() {
    let t = serve(FakeProver::open(), 1).await;
    handle(&t.node).await;
    let req = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5));
    let vid = req.vote_id;
    t.client.submit_vote(&req).await.unwrap();

    wait_until("vote settled over http", async || {
        t.client.vote_status(&pid31(), vid).await.ok() == Some(wire::VoteStatus::Settled)
    })
    .await;

    // A settled vote id is a duplicate, even without a queued twin.
    let e = t.client.submit_vote(&req).await.unwrap_err();
    assert_eq!(api_code(e), (409, 40901));

    // Recorded-as-cast: the tracker proof verifies against the chain root.
    let p = t.client.vote_id_proof(&pid31(), vid).await.unwrap();
    assert!(verify_tracker(&p, &t.s.chain.root()));

    // The stored ballot is served back for the voter's address.
    let b = t.client.ballot(&pid31(), &voter_address(0)).await.unwrap();
    assert_eq!(b.address, voter_address(0));

    // The archive holds the settled transition and its blobs.
    let ts = t.client.transitions(&pid31()).await.unwrap();
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].index, 0);
    assert_eq!(ts[0].voters, 1);
    assert_eq!(ts[0].new_root, t.s.chain.root());
    assert_eq!(ts[0].sender, [0xa1; 20]);
    let blobs = t.client.transition_blobs(&pid31(), 0).await.unwrap();
    assert_eq!(blobs.len() as u64, ts[0].n_blobs);
    assert!(!blobs.is_empty());
    assert!(blobs.iter().all(|b| b.len() == 131072));
    let e = t.client.transition_blobs(&pid31(), 7).await.unwrap_err();
    assert_eq!(api_code(e), (404, 40401));

    let info = t.client.info().await.unwrap();
    assert_eq!(info.settled_by_self, 1);
    t.shutdown.cancel();
}

#[tokio::test]
async fn vote_submission_error_codes() {
    // Slot depth 2: the second queued vote of a slot fills it.
    let t = serve_with(
        setup(2, 4, None),
        FakeProver::gated(),
        100,
        &["--slot-depth", "2"],
    )
    .await;
    handle(&t.node).await;

    // Malformed JSON.
    let (status, e) = raw_post(&t.base, "/votes", "{not json".into()).await;
    assert_eq!((status, e.code), (400, 40001));

    // A vote whose Groth16 proof does not verify.
    let e = t
        .client
        .submit_vote(&wire_vote(&fake_vote(&t.s.env, 1, &[1, 2], 9)))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (400, 40002));

    // Unknown process id.
    let mut req = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5));
    req.process_id = ProcessId(bad_pid31());
    let e = t.client.submit_vote(&req).await.unwrap_err();
    assert_eq!(api_code(e), (404, 40402));

    // An address outside the census gets no witness.
    let mut req = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5));
    req.address = voter_address(9);
    let e = t.client.submit_vote(&req).await.unwrap_err();
    assert_eq!(api_code(e), (400, 40001));

    // A weight differing from the census leaf is a vote error (the census
    // check runs before the Groth16 proof, so a fake proof suffices).
    let mut req = wire_vote(&fake_vote(&t.s.env, 2, &[1, 2], 11));
    req.weight = 2;
    let e = t.client.submit_vote(&req).await.unwrap_err();
    assert_eq!(api_code(e), (400, 40002));

    // The valid vote is accepted; while it is queued, a resubmission (same
    // vote id) is a duplicate.
    let req = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5));
    let vid = req.vote_id;
    t.client.submit_vote(&req).await.unwrap();
    let e = t.client.submit_vote(&req).await.unwrap_err();
    assert_eq!(api_code(e), (409, 40901));
    // A second vote for the same slot queues behind it; the slot is then
    // full and refuses a third as slot-busy, while a resend of a queued
    // vote id stays a duplicate.
    t.client
        .submit_vote(&wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 6)))
        .await
        .unwrap();
    let e = t
        .client
        .submit_vote(&wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 7)))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (409, 40902));
    let e = t.client.submit_vote(&req).await.unwrap_err();
    assert_eq!(api_code(e), (409, 40901));

    // Status: pending vote, unknown vote id, unknown process.
    assert_eq!(
        t.client.vote_status(&pid31(), vid).await.unwrap(),
        wire::VoteStatus::Pending
    );
    let e = t.client.vote_status(&pid31(), u64::MAX).await.unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    let e = t.client.vote_status(&bad_pid31(), vid).await.unwrap_err();
    assert_eq!(api_code(e), (404, 40402));
    // No tracker proof or stored ballot before the vote settles.
    let e = t.client.vote_id_proof(&pid31(), vid).await.unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    let e = t
        .client
        .ballot(&pid31(), &voter_address(0))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    t.shutdown.cancel();
}

#[tokio::test]
async fn vote_body_limit_413() {
    let t = serve(FakeProver::gated(), 100).await;
    handle(&t.node).await;
    let body = format!("{{\"x\":\"{}\"}}", "a".repeat(300 * 1024));
    let (status, e) = raw_post(&t.base, "/votes", body).await;
    assert_eq!((status, e.code), (413, 41301));
    t.shutdown.cancel();
}

#[tokio::test]
async fn closed_process_answers_412() {
    let t = serve(FakeProver::gated(), 100).await;
    handle(&t.node).await;
    t.s.chain.set_status(ProcessStatus::Ended);
    wait_until("actor stopped accepting", async || {
        !t.client.process(&pid31()).await.unwrap().is_accepting_votes
    })
    .await;
    let e = t
        .client
        .submit_vote(&wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5)))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (412, 41201));
    t.shutdown.cancel();
}

#[tokio::test]
async fn process_before_its_start_answers_412_41204() {
    let s = setup(2, 4, None);
    s.chain.set_start_time(T0 + 600);
    let t = serve_with(s, FakeProver::gated(), 100, &[]).await;
    handle(&t.node).await;
    let p = t.client.process(&pid31()).await.unwrap();
    assert_eq!(p.status, wire::ProcessStatus::Ready);
    assert!(!p.is_accepting_votes);
    let req = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5));
    match t.client.submit_vote(&req).await.unwrap_err() {
        ClientError::Api {
            status,
            code,
            message,
        } => {
            assert_eq!((status, code), (412, Some(41204)));
            assert!(message.contains("not open yet"), "{message}");
        }
        e => panic!("expected an api error, got {e:?}"),
    }
    // Open from the start time on.
    t.s.chain.advance_time(600);
    wait_until("accepting at the start", async || {
        t.client.process(&pid31()).await.unwrap().is_accepting_votes
    })
    .await;
    t.client.submit_vote(&req).await.unwrap();
    t.shutdown.cancel();
}

#[tokio::test]
async fn max_voters_answers_412_with_its_own_code() {
    let s = setup(2, 4, None);
    s.chain.set_max_voters(1);
    let t = serve_with(s, FakeProver::gated(), 100, &[]).await;
    handle(&t.node).await;
    t.client
        .submit_vote(&wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5)))
        .await
        .unwrap();
    let e = t
        .client
        .submit_vote(&wire_vote(&real_vote(&t.s.env, 1, &[1, 2], 7)))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (412, 41202));
    t.shutdown.cancel();
}

#[tokio::test]
async fn csp_census_votes_and_participant_404() {
    let t = serve_with(csp_setup(), FakeProver::open(), 1, &[]).await;
    handle(&t.node).await;

    // No local census: the participant route has nothing to serve.
    let e = t
        .client
        .participant(&pid31(), &voter_address(0))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (404, 40401));

    // A CSP vote must carry the attestation.
    let v = real_vote(&t.s.env, 0, &[1, 2], 5);
    let e = t.client.submit_vote(&wire_vote(&v)).await.unwrap_err();
    assert_eq!(api_code(e), (400, 40001));

    // With it, the vote settles in the CSP slot namespace.
    let att = csp_sign(&voter_key(99), &pid_fr(), &voter_address(0), 1, 0);
    let mut req = wire_vote(&v);
    req.census_proof = Some(CensusProofWire::Csp(CspWire {
        r: att.r,
        s: att.s,
        recid: att.recid,
        index: att.index,
    }));
    let vid = req.vote_id;
    t.client.submit_vote(&req).await.unwrap();
    wait_until("csp vote settled", async || {
        t.client.vote_status(&pid31(), vid).await.ok() == Some(wire::VoteStatus::Settled)
    })
    .await;
    let db_vote = t.node.db.vote(&pid_fr(), vid).unwrap().unwrap();
    assert_eq!(db_vote.slot, BALLOT_MIN);
    // The ballot route finds the CSP slot through the address index.
    let b = t.client.ballot(&pid31(), &voter_address(0)).await.unwrap();
    assert_eq!(b.address, voter_address(0));
    // An address this node never served is a 404.
    let e = t
        .client
        .ballot(&pid31(), &voter_address(1))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    t.shutdown.cancel();
}

#[tokio::test]
async fn results_set_after_status_change_is_persisted() {
    let t = serve(FakeProver::gated(), 100).await;
    handle(&t.node).await;
    // The registry emits StatusChanged(→RESULTS) and ResultsSet from the
    // same tx; both land in one poll and the results must survive it.
    let mut results = vec![0u64; 16];
    results[0] = 3;
    results[1] = 5;
    t.s.chain.set_results_externally(results.clone());
    wait_until("results in the process view", async || {
        t.client.process(&pid31()).await.unwrap().result == Some(results.clone())
    })
    .await;
    let p = t.client.process(&pid31()).await.unwrap();
    assert_eq!(p.status, wire::ProcessStatus::Results);
    let rec = t.node.db.process(&pid_fr()).unwrap().unwrap();
    assert_eq!(rec.onchain.results, results);
    assert_eq!(rec.local, LocalStatus::Finalized);
    t.shutdown.cancel();
}

#[tokio::test]
async fn vote_id_wire_format_is_checked() {
    let t = serve(FakeProver::gated(), 100).await;
    handle(&t.node).await;
    // A malformed vote id path segment is a 400, not a 404.
    let url = format!("{}/votes/{}/voteId/0x1234", t.base, ProcessId(pid31()));
    let resp = reqwest::get(url).await.unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert_eq!(resp.json::<ErrorResponse>().await.unwrap().code, 40001);
    // The canonical form round-trips.
    assert!(vote_id_hex(u64::MAX).starts_with("0x"));
    // A non-integer transition index is a JSON 400 too, not a plain-text one.
    let url = format!(
        "{}/processes/{}/transitions/x/blobs",
        t.base,
        ProcessId(pid31())
    );
    let resp = reqwest::get(url).await.unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    assert_eq!(resp.json::<ErrorResponse>().await.unwrap().code, 40001);
    t.shutdown.cancel();
}

/// An observer node (no signing key) serves every read route,
/// refuses writes with its own 412 code, and never proves or finalizes —
/// even when its keystore holds the election key.
#[tokio::test]
async fn observer_serves_reads_and_never_finalizes() {
    // The observer's keystore holds the election key; a signing sibling
    // on the same chain does the proving.
    let dir_o = TempDir::new().unwrap();
    let db_o = Db::open_in(dir_o.path()).unwrap();
    let pk = node_key(&db_o);
    let s = setup(2, 4, Some(pk));

    let dir_a = TempDir::new().unwrap();
    let shutdown_a = CancellationToken::new();
    let node_a = start_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        1,
        "0s",
        shutdown_a.clone(),
    )
    .await;
    let ha = handle(&node_a).await;

    // The observer, served over HTTP. Its prover is gated: nothing may
    // ever call it.
    let shutdown_o = CancellationToken::new();
    let prover_o = FakeProver::gated();
    let cfg = test_config(dir_o.path(), &s.census_dir, 100, "0s");
    let node_o = start_node_cfg(
        db_o,
        s.chain.observer(),
        prover_o.clone(),
        cfg,
        shutdown_o.clone(),
    )
    .await;
    let (client, _base) = serve_node(node_o.clone(), shutdown_o.clone()).await;
    handle(&node_o).await;

    let info = client.info().await.unwrap();
    assert!(info.observer);
    assert_eq!(info.sequencer_address, None);

    // Votes and key generation are refused with the observer code.
    let e = client
        .submit_vote(&wire_vote(&fake_vote(&s.env, 0, &[1, 2], 5)))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (412, 41203));
    let e = client.new_key(&pid31()).await.unwrap_err();
    assert_eq!(api_code(e), (412, 41203));

    // The sibling settles a vote; the observer syncs it from the blobs.
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    ha.submit(v).await.unwrap();
    wait_until("sibling settled", async || s.chain.voters() == 1).await;
    wait_until("observer synced", async || {
        client.process(&pid31()).await.unwrap().local_state_root == Some(s.chain.root())
    })
    .await;

    // Never stored here but in the synced tree -> settled; an unknown
    // vote id stays a 404. Recorded-as-cast works on the observer too.
    assert_eq!(
        client.vote_status(&pid31(), vid).await.unwrap(),
        wire::VoteStatus::Settled
    );
    let e = client.vote_status(&pid31(), u64::MAX).await.unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    let p = client.vote_id_proof(&pid31(), vid).await.unwrap();
    assert!(verify_tracker(&p, &s.chain.root()));

    // The election ends; the observer holds the key but must not
    // prove results or submit anything.
    let submits_before = s.chain.submit_count();
    s.chain.set_status(ProcessStatus::Ended);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(prover_o.calls(), 0);
    assert_eq!(prover_o.results_calls(), 0);
    assert!(s.chain.results().is_empty());
    assert_eq!(s.chain.submit_count(), submits_before);

    shutdown_a.cancel();
    shutdown_o.cancel();
}

/// A process finalized before a restart is respawned read-only, so the
/// tracker proof and stored ballot are still served.
#[tokio::test]
async fn finalized_process_serves_reads_after_restart() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let pk = node_key(&db);
    let s = setup(2, 8, Some(pk));
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
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    let root = s.chain.root();
    shutdown.cancel();
    drop(h);
    drop(node);

    // Restart on the same database: the Finalized process must still
    // answer the read routes, with no new prover work.
    let db2 = reopen_db(dir.path()).await;
    let shutdown2 = CancellationToken::new();
    let prover2 = FakeProver::gated();
    let cfg = test_config(dir.path(), &s.census_dir, 1, "0s");
    let node2 = start_node_cfg(
        db2,
        s.chain.clone(),
        prover2.clone(),
        cfg,
        shutdown2.clone(),
    )
    .await;
    let (client, _base) = serve_node(node2.clone(), shutdown2.clone()).await;
    handle(&node2).await;
    let p = client.vote_id_proof(&pid31(), vid).await.unwrap();
    assert!(verify_tracker(&p, &root));
    let b = client.ballot(&pid31(), &voter_address(0)).await.unwrap();
    assert_eq!(b.address, voter_address(0));
    assert_eq!(prover2.calls(), 0);
    assert_eq!(prover2.results_calls(), 0);
    shutdown2.cancel();
}

/// Bootstrap attempts recorded for the test process.
fn boot_attempts(node: &Node) -> Option<u32> {
    let b = node.db.meta_bytes("monitor_boot_retries").unwrap()?;
    let list: Vec<(String, u32)> = serde_json::from_slice(&b).unwrap();
    list.into_iter()
        .find(|(p, _)| *p == hex::encode(pid31()))
        .map(|(_, n)| n)
}

// The census RPC is down at bootstrap: one failed sync costs one attempt,
// polling inside its backoff costs none, and the process comes up once
// the RPC answers again.
#[tokio::test]
async fn onchain_bootstrap_waits_out_an_rpc_outage() {
    if !common::anvil::enabled() {
        return;
    }
    let ch = common::anvil::CensusChain::start().await;
    ch.add(&[(voter_address(0), 1)]).await;
    let (root, _) = ch.state().await;
    let proxy = common::anvil::RpcProxy::start(ch.anvil.endpoint()).await;
    proxy
        .knobs
        .down
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::MerkleOnchainDynamic;
    e.cfg.census_root = fr_from_be(&root).unwrap();
    let census_dir = TempDir::new().unwrap();
    let chain = FakeChain::new(&e, String::new());
    let c = ch.census.into_array();
    chain.set_census_contract(c);
    while chain.head_block() < ch.head().await {
        chain.advance_time(0);
    }
    let dir = census_dir.path().to_path_buf();
    let s = TestSetup {
        env: e,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    };
    let t = serve_with(s, FakeProver::gated(), 100, &["--rpc-url", &proxy.url]).await;
    wait_until("sync failed", async || {
        t.node.onchain.retry_in(&c).is_some()
    })
    .await;
    // 50 polls at 20 ms: charging each would pass the cap of 10.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(boot_attempts(&t.node), Some(1));
    assert!(!t.node.processes.read().await.contains_key(&pid_fr()));

    proxy
        .knobs
        .down
        .store(false, std::sync::atomic::Ordering::Relaxed);
    t.node.onchain.expire_backoff(&c);
    handle(&t.node).await;
    let (proof, _) = t
        .client
        .participant(&pid31(), &voter_address(0))
        .await
        .unwrap();
    assert_eq!(fr_to_be(&proof.root), root);
    t.shutdown.cancel();
}

#[tokio::test]
async fn onchain_census_votes_at_the_latest_root() {
    if !common::anvil::enabled() {
        return;
    }
    let ch = common::anvil::CensusChain::start().await;
    ch.add(&[(voter_address(0), 1)]).await;
    let (recorded, _) = ch.state().await;
    ch.add(&[(voter_address(1), 1), (voter_address(2), 1)])
        .await;
    let (latest, _) = ch.state().await;

    // The registry recorded the root of the first block; voter 1 came later.
    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::MerkleOnchainDynamic;
    e.cfg.census_root = fr_from_be(&recorded).unwrap();
    let census_dir = TempDir::new().unwrap();
    let chain = FakeChain::new(&e, String::new());
    chain.set_census_contract(ch.census.into_array());
    while chain.head_block() < ch.head().await {
        chain.advance_time(0);
    }
    let dir = census_dir.path().to_path_buf();
    let s = TestSetup {
        env: e,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    };
    let url = ch.anvil.endpoint();
    let t = serve_with(s, FakeProver::gated(), 100, &["--rpc-url", &url]).await;
    handle(&t.node).await;

    let (proof, weight) = t
        .client
        .participant(&pid31(), &voter_address(1))
        .await
        .unwrap();
    assert_eq!((fr_to_be(&proof.root), weight), (latest, 1));
    t.client
        .submit_vote(&wire_vote(&real_vote(&t.s.env, 1, &[1, 2], 7)))
        .await
        .unwrap();

    // Voter 3 is not in the contract.
    let e = t
        .client
        .submit_vote(&wire_vote(&fake_vote(&t.s.env, 3, &[1, 2], 8)))
        .await
        .unwrap_err();
    assert_eq!(api_code(e).0, 400);
    let e = t
        .client
        .participant(&pid31(), &voter_address(3))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    t.shutdown.cancel();
}

/// Origin 2 with voters 0..4 in `census.json`.
fn offchain_setup() -> TestSetup {
    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::MerkleOffchainDynamic;
    let census_dir = TempDir::new().unwrap();
    let uri = write_census(census_dir.path(), 4);
    let chain = FakeChain::new(&e, uri);
    let dir = census_dir.path().to_path_buf();
    TestSetup {
        env: e,
        chain,
        _census_dir: census_dir,
        census_dir: dir,
    }
}

fn census_root(voters: impl IntoIterator<Item = usize>) -> Fr {
    LeanImt::from_leaves(
        voters
            .into_iter()
            .map(|i| census_leaf(&voter_address(i), 1).unwrap())
            .collect(),
    )
    .root()
}

/// Writes `voters` to `<census dir>/<sub>/census.json` and emits
/// `CensusUpdated` for it with `root`.
fn update_census(t: &Served, sub: &str, voters: &[usize], root: Fr) {
    let dir = t.s.census_dir.join(sub);
    std::fs::create_dir(&dir).unwrap();
    let uri = write_census_of(&dir, voters.iter().copied());
    announce_census(t, uri, root);
}

fn announce_census(t: &Served, uri: String, root: Fr) {
    t.s.chain.advance_time(0);
    t.s.chain.push_event(EventKind::CensusUpdated {
        pid: pid31(),
        root: fr_to_be(&root),
        uri,
    });
}

async fn wait_served(t: &Served, voter: usize, root: Fr) {
    wait_until("census update served", async || {
        t.client
            .participant(&pid31(), &voter_address(voter))
            .await
            .is_ok_and(|(p, _)| p.root == root)
    })
    .await;
}

#[tokio::test]
async fn offchain_census_update_is_fetched_and_served() {
    let t = serve_with(offchain_setup(), FakeProver::open(), 1, &[]).await;
    handle(&t.node).await;
    let root0 = census_root(0..4);
    let req = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5));
    let vid = req.vote_id;
    t.client.submit_vote(&req).await.unwrap();
    wait_until("vote settled", async || {
        t.client.vote_status(&pid31(), vid).await.ok() == Some(wire::VoteStatus::Settled)
    })
    .await;
    let e = t
        .client
        .participant(&pid31(), &voter_address(4))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (404, 40401));

    // The organizer grows the census to five voters.
    let root1 = census_root(0..5);
    update_census(&t, "v2", &[0, 1, 2, 3, 4], root1);
    wait_served(&t, 4, root1).await;
    // Another process's record points at root1, so it outlives the update.
    let mut other = t.node.db.process(&pid_fr()).unwrap().unwrap();
    other.pid = bad_pid_fr();
    other.onchain.census.root = fr_to_be(&root1);
    t.node.db.put_process(&other).unwrap();

    // Then drops voter 0, who already voted.
    let root2 = census_root(1..5);
    update_census(&t, "v3", &[1, 2, 3, 4], root2);
    wait_served(&t, 1, root2).await;
    let e = t
        .client
        .participant(&pid31(), &voter_address(0))
        .await
        .unwrap_err();
    assert_eq!(api_code(e), (404, 40401));
    // The ballot it cast is still served.
    let b = t.client.ballot(&pid31(), &voter_address(0)).await.unwrap();
    assert_eq!(b.address, voter_address(0));

    // Roots nobody uses are deleted, the others kept.
    wait_until("root0 pruned", async || !t.node.census.has(&root0).unwrap()).await;
    assert!(t.node.census.has(&root1).unwrap());
    assert!(t.node.census.has(&root2).unwrap());
    t.shutdown.cancel();
}

// An update that is still loading is 429; one that can never load is 412
// with the reason, until the next update replaces it.
#[tokio::test]
async fn offchain_census_update_that_cannot_load_is_refused() {
    let t = serve_with(offchain_setup(), FakeProver::gated(), 100, &[]).await;
    handle(&t.node).await;
    let missing = format!("file://{}", t.s.census_dir.join("nope.json").display());
    announce_census(&t, missing, census_root(0..5));
    wait_until("census loading", async || {
        t.client
            .participant(&pid31(), &voter_address(0))
            .await
            .is_err_and(|e| api_code(e).0 == 429)
    })
    .await;
    let e = t
        .client
        .submit_vote(&wire_vote(&fake_vote(&t.s.env, 0, &[1, 2], 5)))
        .await
        .unwrap_err();
    assert_eq!(api_code(e).0, 429);

    update_census(&t, "dup", &[0, 1, 1], census_root([0, 1, 1]));
    wait_until("census refused", async || {
        t.client
            .participant(&pid31(), &voter_address(0))
            .await
            .is_err_and(|e| api_code(e) == (412, 41201))
    })
    .await;
    match t
        .client
        .submit_vote(&wire_vote(&fake_vote(&t.s.env, 0, &[1, 2], 5)))
        .await
        .unwrap_err()
    {
        ClientError::Api {
            status, message, ..
        } => {
            assert_eq!(status, 412);
            assert!(
                message.contains("census") && message.contains("listed twice"),
                "{message}"
            );
        }
        e => panic!("{e:?}"),
    }

    let root = census_root(0..5);
    update_census(&t, "good", &[0, 1, 2, 3, 4], root);
    wait_served(&t, 4, root).await;
    t.shutdown.cancel();
}
