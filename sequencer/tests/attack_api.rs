//! Attacks on the HTTP API, the node's hostile edge: strict decoding over
//! the wire, the body cap, hand-parsed path params, process gating,
//! concurrent submissions, load shedding and the key endpoint.
//!
//! Real ballot proofs come from `tests/cache` (proved from
//! `CIRCOM_ARTIFACTS` when cold); junk proofs use `fake_vote`.

mod common;

use std::sync::OnceLock;
use std::time::Instant;

use common::*;
use davinci_client::SequencerClient;
use davinci_client::api::{self as wire, ErrorResponse, ProcessId, VoteRequest};
use davinci_client::prover::BallotProver;
use davinci_client::voter::CircomInputs;
use davinci_sequencer::api::{router, serve_with_head_timeout};
use davinci_zkvm_sdk::census::csp_sign;
use davinci_zkvm_sdk::limits::NUM_FIELDS;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ------------------------------------------------------- real ballot proofs

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

// Keyed by the public signals (`common::signals_key`).
fn cached_proof(inputs: &CircomInputs) -> SnarkJsProof {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cache");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.json", signals_key(&inputs.public_signals())));
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
    };
    v.pkg.proof = cached_proof(&inputs);
    v
}

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
        signature: sig,
        weight: pkg.weight,
        census_proof: None,
    }
}

// ----------------------------------------------------------- serve harness

/// The request-head bound the test servers run with.
const HEAD_TIMEOUT: Duration = Duration::from_secs(2);

struct Served {
    s: TestSetup,
    node: Node,
    client: SequencerClient,
    base: String,
    shutdown: CancellationToken,
    _dir: TempDir,
}

async fn serve_with(
    s: TestSetup,
    prover: Arc<FakeProver>,
    batch_max: usize,
    extra: &[&str],
) -> Served {
    serve_as(s, prover, batch_max, extra, false).await
}

async fn serve_as(
    s: TestSetup,
    prover: Arc<FakeProver>,
    batch_max: usize,
    extra: &[&str],
    observer: bool,
) -> Served {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let cfg = test_config_with(dir.path(), &s.census_dir, batch_max, "0s", extra);
    let chain = if observer {
        s.chain.observer()
    } else {
        s.chain.clone()
    };
    let node = start_node_cfg(db, chain, prover, cfg, shutdown.clone()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let sd = shutdown.clone();
    let n = node.clone();
    tokio::spawn(async move {
        serve_with_head_timeout(listener, n, sd, HEAD_TIMEOUT)
            .await
            .unwrap()
    });
    Served {
        s,
        node,
        client: SequencerClient::new(&base),
        base,
        shutdown,
        _dir: dir,
    }
}

async fn serve(prover: Arc<FakeProver>, extra: &[&str]) -> Served {
    let t = serve_with(setup(2, 4, None), prover, 100, extra).await;
    handle(&t.node).await;
    t
}

// ---------------------------------------------------------- raw HTTP/1.1

struct Raw {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Raw {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?} / {}", self.head, self.text()))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body)
            .chars()
            .take(300)
            .collect()
    }
    /// The documented error shape: `{error, code}` with `code / 100 == status`.
    fn code(&self) -> u32 {
        let e: ErrorResponse = serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "status {} body is not the error shape ({e}): {:?}",
                self.status,
                self.text()
            )
        });
        assert_eq!(
            e.code / 100,
            self.status as u32,
            "code {} vs status",
            e.code
        );
        e.code
    }
}

/// Sends `bytes` verbatim and reads the response to EOF (or `wait`). The
/// read runs alongside the write: a server that answers early and closes
/// resets the connection under a client still sending.
async fn raw_send(base: &str, bytes: &[u8], wait: Duration) -> Raw {
    let addr = base.trim_start_matches("http://");
    let s = TcpStream::connect(addr).await.unwrap();
    let (mut rd, mut wr) = s.into_split();
    let bytes = bytes.to_vec();
    let w = async move {
        let _ = wr.write_all(&bytes).await;
        wr
    };
    let r = read_from(&mut rd, wait);
    let (_wr, raw) = tokio::join!(w, r);
    raw
}

async fn read_response(s: &mut TcpStream, wait: Duration) -> Raw {
    read_from(s, wait).await
}

async fn read_from(s: &mut (impl AsyncReadExt + Unpin), wait: Duration) -> Raw {
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(wait, async {
        let mut tmp = [0u8; 65536];
        loop {
            match s.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if complete(&buf) {
                        break;
                    }
                }
            }
        }
    })
    .await;
    parse_response(&buf)
}

/// Whether `buf` holds a whole response with a Content-Length body.
fn complete(buf: &[u8]) -> bool {
    let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
    match head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(n) => buf.len() >= p + 4 + n,
        None => false,
    }
}

fn parse_response(buf: &[u8]) -> Raw {
    // No answer at all reads as status 0.
    let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Raw {
            status: 0,
            head: String::new(),
            body: Vec::new(),
        };
    };
    let head = String::from_utf8_lossy(&buf[..p]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap();
    let mut body = buf[p + 4..].to_vec();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    Raw { status, head, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(p) = b.windows(2).position(|w| w == b"\r\n") {
        let n =
            usize::from_str_radix(std::str::from_utf8(&b[..p]).unwrap().trim(), 16).unwrap_or(0);
        if n == 0 || b.len() < p + 2 + n {
            break;
        }
        out.extend_from_slice(&b[p + 2..p + 2 + n]);
        b = &b[(p + 2 + n + 2).min(b.len())..];
    }
    out
}

async fn get_raw(base: &str, target: &str) -> Raw {
    let req = format!("GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    raw_send(base, req.as_bytes(), Duration::from_secs(10)).await
}

async fn post_raw(base: &str, target: &str, ct: Option<&str>, body: &[u8]) -> Raw {
    let ct = ct
        .map(|c| format!("Content-Type: {c}\r\n"))
        .unwrap_or_default();
    let mut req = format!(
        "POST {target} HTTP/1.1\r\nHost: x\r\n{ct}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    raw_send(base, &req, Duration::from_secs(10)).await
}

async fn post_json(base: &str, target: &str, body: &[u8]) -> Raw {
    post_raw(base, target, Some("application/json"), body).await
}

fn good_body(t: &Served, idx: usize, kseed: u64) -> Value {
    serde_json::to_value(wire_vote(&real_vote(&t.s.env, idx, &[1, 2], kseed))).unwrap()
}

// ------------------------------------------------------------- helpers

fn pid_str() -> String {
    format!("0x{}", hex::encode(pid31()))
}

fn addr_str(i: usize) -> String {
    format!("0x{}", hex::encode(voter_address(i)))
}

/// A body of exactly `len` bytes: `v` as JSON padded with trailing spaces.
fn padded(v: &Value, len: usize) -> Vec<u8> {
    let mut b = serde_json::to_vec(v).unwrap();
    assert!(b.len() <= len);
    b.resize(len, b' ');
    b
}

fn rm(v: &mut Value, k: &str) {
    v.as_object_mut().unwrap().remove(k);
}

/// `POST /votes` of a body that must be refused with `(status, code)`.
async fn refused(t: &Served, body: &Value, want: (u16, u32), why: &str) {
    let r = post_json(&t.base, "/votes", body.to_string().as_bytes()).await;
    assert_eq!(r.status, want.0, "{why}: {}", r.text());
    assert_eq!(r.code(), want.1, "{why}: {}", r.text());
}

// ------------------------------------------------- error shape and routes

async fn json_error_answer(r: &Raw) -> bool {
    r.head.to_ascii_lowercase().contains("application/json")
        && serde_json::from_slice::<ErrorResponse>(&r.body).is_ok()
}

// Every failure is `{error, code}` JSON with `code / 100 == status`
// (docs/api.md), whatever the path.
#[tokio::test]
async fn undecodable_path_params_answer_json() {
    let t = serve(FakeProver::gated(), &[]).await;
    let p = pid_str();
    let a = addr_str(0);
    let mut bad = Vec::new();
    for target in [
        "/processes/%ff".to_string(),
        format!("/processes/{p}/participants/%ff"),
        format!("/processes/{p}/transitions/%ff/blobs"),
        "/votes/%ff/voteId/0x8000000000000000".to_string(),
        format!("/votes/{p}/voteId/%ff"),
        format!("/votes/{p}/voteId/%ff/proof"),
        format!("/votes/{p}/address/%ff"),
        "/processes/%c0%af".to_string(),
        format!("/votes/{p}/address/{a}%fe"),
    ] {
        let r = get_raw(&t.base, &target).await;
        if !json_error_answer(&r).await {
            bad.push((target, r.status, r.text()));
        }
    }
    // BUG: axum's Path rejection for invalid UTF-8 answers text/plain.
    assert!(bad.is_empty(), "non-JSON error answers: {bad:#?}");
    t.shutdown.cancel();
}

#[tokio::test]
async fn unknown_routes_answer_json_404() {
    let t = serve(FakeProver::gated(), &[]).await;
    let p = pid_str();
    let mut bad = Vec::new();
    for target in [
        "/nope".to_string(),
        "/".to_string(),
        "/processes/".to_string(),
        "/processes//".to_string(),
        format!("/processes/{p}/"),
        format!("/processes/{p}/transitions/"),
        format!("/votes/{p}/voteId/"),
        "/votes/".to_string(),
        "/ping/".to_string(),
        "/info/".to_string(),
    ] {
        let r = get_raw(&t.base, &target).await;
        if !(r.status == 404 && json_error_answer(&r).await && r.code() == 40401) {
            bad.push((target, r.status));
        }
    }
    // BUG: an unmatched route answers 404 with an empty body.
    assert!(bad.is_empty(), "not the JSON 40401: {bad:#?}");
    t.shutdown.cancel();
}

#[tokio::test]
async fn wrong_methods_answer_json_405() {
    let t = serve(FakeProver::gated(), &[]).await;
    let p = pid_str();
    let mut bad = Vec::new();
    for (m, target) in [
        ("DELETE", "/votes".to_string()),
        ("PUT", "/processes/keys".to_string()),
        ("POST", "/ping".to_string()),
        ("GET", "/votes".to_string()),
        ("GET", "/processes/keys".to_string()),
        ("POST", format!("/processes/{p}")),
    ] {
        let req = format!(
            "{m} {target} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let r = raw_send(&t.base, req.as_bytes(), Duration::from_secs(5)).await;
        if !(r.status == 405 && json_error_answer(&r).await && r.code() == 40501) {
            bad.push((format!("{m} {target}"), r.status));
        }
    }
    // BUG: a wrong method answers 405 with an empty body.
    assert!(bad.is_empty(), "not the JSON 40501: {bad:#?}");
    t.shutdown.cancel();
}

#[tokio::test]
async fn hostile_path_params_never_500() {
    let t = serve(FakeProver::gated(), &[]).await;
    let p = pid_str();
    let a = addr_str(0);
    let long = "a".repeat(10_000);
    let segs = [
        "".to_string(),
        "0x".to_string(),
        "0".to_string(),
        long.clone(),
        format!("0x{}", "0".repeat(62)),
        format!("0x{}", "0".repeat(61)),
        format!("0x{}", "0".repeat(63)),
        format!("0X{}", "0".repeat(62)),
        format!("0x{}", "g".repeat(62)),
        format!("0x{}", "%E2%82%AC".repeat(20)),
        "%00".to_string(),
        "%2f".to_string(),
        "%2e%2e".to_string(),
        "..".to_string(),
        ".".to_string(),
        "%20".to_string(),
        "+".to_string(),
        "%".to_string(),
        "%zz".to_string(),
        format!("{p}%00"),
        format!("{p}%20"),
        format!("{}%3Fx=1", p),
        format!("{p}%23"),
        "-1".to_string(),
        "18446744073709551616".to_string(),
        "1e3".to_string(),
        "0x8000000000000000".to_string(),
        "9223372036854775807".to_string(),
        "18446744073709551615".to_string(),
    ];
    let mut shapes = Vec::new();
    for s in &segs {
        for template in [
            "/processes/{}",
            "/processes/{}/transitions",
            "/processes/{}/transitions/0/blobs",
            "/processes/P/transitions/{}/blobs",
            "/processes/P/participants/{}",
            "/processes/{}/participants/A",
            "/votes/{}/voteId/0x8000000000000000",
            "/votes/P/voteId/{}",
            "/votes/P/voteId/{}/proof",
            "/votes/P/address/{}",
            "/votes/{}/address/A",
        ] {
            let target = template.replace("{}", s).replace('P', &p).replace('A', &a);
            // A template letter inside a hostile segment must stay put.
            let target = if s.contains('P') || s.contains('A') {
                template
                    .replacen("{}", s, 1)
                    .replace("/P/", &format!("/{p}/"))
                    .replace("/A", &format!("/{a}"))
            } else {
                target
            };
            let r = get_raw(&t.base, &target).await;
            assert!(
                matches!(r.status, 400 | 404 | 414 | 431),
                "{target:.80} -> {} {}",
                r.status,
                r.text()
            );
            if r.head.to_ascii_lowercase().contains("application/json") {
                let c = r.code();
                assert!(matches!(c, 40001 | 40401 | 40402), "{target:.80} -> {c}");
            } else {
                shapes.push(target.chars().take(60).collect::<String>());
            }
        }
    }
    // Only the (documented-as-BUG) text answers differ; the node is alive.
    t.client.ping().await.unwrap();
    assert!(t.client.process(&pid31()).await.is_ok());
    let _ = shapes;
    t.shutdown.cancel();
}

// A well-formed id in an odd spelling names the same thing, never a
// different process.
#[tokio::test]
async fn id_spellings_resolve_to_the_same_resource_or_400() {
    let t = serve(FakeProver::gated(), &[]).await;
    let p = pid_str();
    let upper = format!("0x{}", hex::encode_upper(pid31()));
    for ok in [p.clone(), upper, p.trim_start_matches("0x").to_string()] {
        let r = get_raw(&t.base, &format!("/processes/{ok}")).await;
        assert_eq!(r.status, 200, "{ok}");
        assert_eq!(r.json()["id"], json!(p));
    }
    // Percent-encoded hex digits decode to the same id.
    let enc: String = p.bytes().map(|b| format!("%{b:02x}")).collect();
    let r = get_raw(&t.base, &format!("/processes/{enc}")).await;
    assert_eq!(r.status, 200);
    for bad in [
        format!("0x{}", "00".repeat(32)),
        format!("0x{}", "da".repeat(30)),
        format!("{p}00"),
        format!(" {p}"),
    ] {
        let r = get_raw(&t.base, &format!("/processes/{}", bad.replace(' ', "%20"))).await;
        assert_eq!((r.status, r.code()), (400, 40001), "{bad}");
    }
    // A vote id below 2^63 never reaches the db.
    for v in [
        "0x7fffffffffffffff",
        "0x0000000000000000",
        "0x80",
        "80000000000000000",
    ] {
        let r = get_raw(&t.base, &format!("/votes/{p}/voteId/{v}")).await;
        assert_eq!((r.status, r.code()), (400, 40001), "{v}");
    }
    // 0x-less 16 hex digits are accepted, upper case too; the vote is unknown.
    let r = get_raw(&t.base, &format!("/votes/{p}/voteId/FFFFFFFFFFFFFFFF")).await;
    assert_eq!((r.status, r.code()), (404, 40401));
    t.shutdown.cancel();
}

// ------------------------------------------------------- body and framing

#[tokio::test]
async fn body_cap_edges() {
    let t = serve(FakeProver::gated(), &[]).await;
    let body = serde_json::to_value(wire_vote(&fake_vote(&t.s.env, 1, &[1, 2], 9))).unwrap();
    const CAP: usize = 256 * 1024;
    // Exactly at the cap: read and judged (a bad proof), not 413.
    let r = post_json(&t.base, "/votes", &padded(&body, CAP)).await;
    assert_eq!(
        (r.status, r.code()),
        (400, 40002),
        "at the cap: {}",
        r.text()
    );
    // One byte over.
    let r = post_json(&t.base, "/votes", &padded(&body, CAP + 1)).await;
    assert_eq!((r.status, r.code()), (413, 41301));
    // Same on the key route.
    let k = json!({"processId": pid_str()});
    let r = post_json(&t.base, "/processes/keys", &padded(&k, CAP + 1)).await;
    assert_eq!((r.status, r.code()), (413, 41301));

    // Chunked, no Content-Length: over the cap is cut off, at the cap passes.
    for (len, want) in [(CAP, 400u16), (CAP + 1, 413)] {
        let data = padded(&body, len);
        let mut req = b"POST /votes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for c in data.chunks(8000) {
            req.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            req.extend_from_slice(c);
            req.extend_from_slice(b"\r\n");
        }
        req.extend_from_slice(b"0\r\n\r\n");
        let r = raw_send(&t.base, &req, Duration::from_secs(10)).await;
        assert_eq!(r.status, want, "chunked {len}: {}", r.text());
        r.code();
    }
    // BUG: a declared length over the cap is read for until the deadline
    // instead of being refused up front.
    let req = b"POST /votes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 1073741824\r\nConnection: close\r\n\r\n{";
    let t0 = Instant::now();
    let r = raw_send(&t.base, req, Duration::from_secs(5)).await;
    assert_eq!(r.status, 413, "no early answer to a declared 1 GiB body");
    assert_eq!(r.code(), 41301);
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "waited for a body it refuses"
    );
    t.shutdown.cancel();
}

#[tokio::test]
async fn lying_and_broken_framing() {
    let t = serve(FakeProver::gated(), &[]).await;
    let body = serde_json::to_vec(&wire_vote(&fake_vote(&t.s.env, 1, &[1, 2], 9))).unwrap();
    // Content-Length shorter than the body: the cut-off JSON is a 400, and
    // what follows is not parsed as this body.
    let mut req = "POST /votes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n".to_string()
    .into_bytes();
    req.extend_from_slice(&body);
    let r = raw_send(&t.base, &req, Duration::from_secs(5)).await;
    assert_eq!((r.status, r.code()), (400, 40001));
    // Both Content-Length and chunked (request smuggling shape), conflicting
    // lengths: refused or judged, never a hang or a 500.
    let mut req = "POST /votes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 5\r\nContent-Length: 6\r\nConnection: close\r\n\r\n{}".to_string()
    .into_bytes();
    let r = raw_send(&t.base, &req, Duration::from_secs(5)).await;
    assert_eq!(r.status, 400);
    req = b"POST /votes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n".to_vec();
    let r = raw_send(&t.base, &req, Duration::from_secs(5)).await;
    assert!(
        r.status == 400 || r.status == 200 || r.status == 411,
        "{}",
        r.status
    );
    // Content-Length longer than what is sent, then the client half-closes:
    // the server must drop the connection, not wait forever.
    let addr = t.base.trim_start_matches("http://");
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"POST /votes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 5000\r\n\r\n{\"a\":")
        .await
        .unwrap();
    s.shutdown().await.unwrap();
    let mut buf = Vec::new();
    let done = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    assert!(done.is_ok(), "server held a half-closed connection open");
    // Garbage request lines and absurd headers are bounded, not fatal.
    for junk in [
        b"\x00\x01\x02\r\n\r\n".to_vec(),
        b"GET\r\n\r\n".to_vec(),
        format!("GET /{} HTTP/1.1\r\nHost: x\r\n\r\n", "a".repeat(200_000)).into_bytes(),
        format!(
            "GET /ping HTTP/1.1\r\nHost: x\r\nX: {}\r\n\r\n",
            "a".repeat(200_000)
        )
        .into_bytes(),
        b"GET /ping HTTP/9.9\r\nHost: x\r\n\r\n".to_vec(),
    ] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let _ = s.write_all(&junk).await;
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    }
    t.client.ping().await.unwrap();
    t.shutdown.cancel();
}

// A body that never finishes is cut by the 60 s deadline with the JSON 408.
// Takes a minute: `cargo test -- --ignored slow_body`.
#[tokio::test]
#[ignore = "waits out the 60 s request deadline"]
async fn slow_body_gets_the_json_408() {
    let t = serve(FakeProver::gated(), &[]).await;
    let addr = t.base.trim_start_matches("http://");
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"POST /votes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"a\":")
        .await
        .unwrap();
    let t0 = Instant::now();
    let r = read_response(&mut s, Duration::from_secs(90)).await;
    assert_eq!((r.status, r.code()), (408, 40801));
    assert!(t0.elapsed() >= Duration::from_secs(55) && t0.elapsed() < Duration::from_secs(70));
    t.shutdown.cancel();
}

// -------------------------------------------------- HTTP-level decoding

#[tokio::test]
async fn vote_decoding_over_http() {
    let t = serve(FakeProver::gated(), &[]).await;
    let good = good_body(&t, 0, 5);
    let ok = serde_json::to_vec(&good).unwrap();

    // Content types and bodies that are not the vote.
    for (ct, body) in [
        (None, ok.clone()),
        (Some("text/plain"), ok.clone()),
        (Some("application/x-www-form-urlencoded"), ok.clone()),
        (Some("application/json"), Vec::new()),
        (Some("application/json"), b"null".to_vec()),
        (Some("application/json"), b"[]".to_vec()),
        (Some("application/json"), b"\"x\"".to_vec()),
        (Some("application/json"), b"{".to_vec()),
        (
            Some("application/json"),
            [b"\xef\xbb\xbf", &ok[..]].concat(),
        ),
        (Some("application/json"), vec![0xff, 0xfe, 0x00]),
        (
            Some("application/json"),
            br#"{"processId":"\ud800"}"#.to_vec(),
        ),
        (Some("application/json"), "[".repeat(200_000).into_bytes()),
        (
            Some("application/json"),
            format!("{{\"a\":{}1{}}}", "[".repeat(5000), "]".repeat(5000)).into_bytes(),
        ),
    ] {
        let r = post_raw(&t.base, "/votes", ct, &body).await;
        assert_eq!(
            r.status,
            400,
            "{ct:?} {:.40?} -> {}",
            String::from_utf8_lossy(&body),
            r.text()
        );
        assert_eq!(r.code(), 40001);
    }
    // A JSON content type with parameters or a +json suffix is JSON.
    let r = post_raw(
        &t.base,
        "/votes",
        Some("application/json; charset=utf-8"),
        &ok,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());

    // Fresh node for the field-level matrix: nothing admitted yet.
    let t2 = serve(FakeProver::gated(), &[]).await;
    let good = good_body(&t2, 0, 5);
    let mut dup = serde_json::to_string(&good).unwrap();
    dup = dup.replacen('{', "{\"weight\":\"1\",", 1);
    let r = post_json(&t2.base, "/votes", dup.as_bytes()).await;
    assert_eq!((r.status, r.code()), (400, 40001), "duplicate key");
    for (k, v) in [
        ("weight", json!(1)),
        ("weight", json!("-1")),
        ("weight", json!("1.0")),
        ("weight", json!("01x")),
        ("weight", Value::Null),
        ("weight", json!("340282366920938463463374607431768211456")),
        ("voteId", json!("0x7fffffffffffffff")),
        ("voteId", json!(9223372036854775808u64)),
        ("address", json!(addr_str(0)[..41].to_string())),
        ("signature", json!(format!("0x{}", "00".repeat(66)))),
        // An earlier client's inputs hash is an unknown field now.
        ("ballotInputsHash", json!("1")),
        ("ballotInputsHash", json!("0x01")),
        // Merkle census values are `0x` and 64 hex digits, even though the
        // node ignores the client's Merkle proof.
        (
            "censusProof",
            json!({"type": "merkle", "root": "5", "leaf": "5", "pathBits": 0, "siblings": []}),
        ),
        ("processId", json!("")),
        ("ballot", json!([])),
        ("ballotProof", json!({})),
        (
            "ballotProof",
            json!({"pi_a": ["1","2"], "pi_b": [], "pi_c": []}),
        ),
        ("ballotProof", Value::Null),
        ("extra", json!(1)),
    ] {
        let mut b = good.clone();
        b[k] = v.clone();
        refused(&t2, &b, (400, 40001), &format!("{k}={v}")).await;
    }
    for k in [
        "processId",
        "address",
        "voteId",
        "ballot",
        "ballotProof",
        "signature",
        "weight",
    ] {
        let mut b = good.clone();
        rm(&mut b, k);
        refused(&t2, &b, (400, 40001), &format!("missing {k}")).await;
    }
    // Points: off the curve, non-canonical, wrong arity.
    let p = "21888242871839275222246405745257275088548364400416034343698204186575808495617";
    for (x, y) in [("0", "0"), ("1", "1"), (p, "1"), ("0", p), ("2", "3")] {
        let mut b = good.clone();
        b["ballot"][7]["c1"] = json!({"x": x, "y": y});
        refused(&t2, &b, (400, 40001), &format!("point ({x},{y})")).await;
    }
    // Nothing above was admitted.
    assert_eq!(
        t2.client.process(&pid31()).await.unwrap().pending_votes,
        Some(0)
    );
    // The untouched body still goes through.
    let r = post_json(&t2.base, "/votes", good.to_string().as_bytes()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    t.shutdown.cancel();
    t2.shutdown.cancel();
}

// Values that decode but that the circuit would reject are 40002 and never
// reach the queue (one such vote would sink its batch).
#[tokio::test]
async fn circuit_invalid_values_are_refused_before_the_queue() {
    let t = serve(FakeProver::gated(), &[]).await;
    let e = &t.s.env;
    let good = serde_json::to_value(wire_vote(&real_vote(e, 0, &[1, 2], 5))).unwrap();
    let mut variants: Vec<(String, Value)> = Vec::new();
    let mut with = |name: &str, f: &dyn Fn(&mut Value)| {
        let mut b = good.clone();
        f(&mut b);
        variants.push((name.into(), b));
    };
    // Weight against the census leaf and the 88-bit field.
    for w in [
        "0",
        "2",
        "18446744073709551616",
        "309485009821345068724781056",
        "309485009821345068724781055",
        &u128::MAX.to_string(),
    ] {
        with(&format!("weight {w}"), &|b| b["weight"] = json!(w));
    }
    // Wrong binding: another process id is a 404 (below), another vote id,
    // a flipped signature byte, a swapped address.
    with("other vote id", &|b| {
        b["voteId"] = json!("0x8000000000000001")
    });
    with("vote id max", &|b| {
        b["voteId"] = json!("0xffffffffffffffff")
    });
    with("sig v 29", &|b| {
        let s = b["signature"].as_str().unwrap().to_string();
        b["signature"] = json!(format!("{}1d", &s[..s.len() - 2]));
    });
    with("sig v 2", &|b| {
        let s = b["signature"].as_str().unwrap().to_string();
        b["signature"] = json!(format!("{}02", &s[..s.len() - 2]));
    });
    with("sig zero", &|b| {
        b["signature"] = json!(format!("0x{}", "00".repeat(65)))
    });
    with("sig ff", &|b| {
        b["signature"] = json!(format!("0x{}", "ff".repeat(65)))
    });
    // Padding must be the identity.
    with("padding not identity", &|b| {
        b["ballot"][15] = b["ballot"][0].clone();
    });
    with("padding half identity", &|b| {
        b["ballot"][8]["c2"] = b["ballot"][0]["c1"].clone();
    });
    // The ballot is bound by the proof's public inputs.
    with("ballot swapped", &|b| {
        let a = b["ballot"][0].clone();
        b["ballot"][0] = b["ballot"][1].clone();
        b["ballot"][1] = a;
    });
    with("active field identity", &|b| {
        b["ballot"][0] = json!({"c1": {"x":"0","y":"1"}, "c2": {"x":"0","y":"1"}});
    });
    // Order-2 point (0, -1), a torsion component, in an active field.
    with("torsion point", &|b| {
        b["ballot"][0]["c1"] = json!({"x":"0","y":"21888242871839275222246405745257275088548364400416034343698204186575808495616"});
    });
    // Proofs: junk strings of every shape.
    let big = "9".repeat(30_000);
    for (i, pa) in [
        json!(["0", "0", "0"]),
        json!(["1", "2", "1"]),
        json!(["", "", ""]),
        json!(["x", "y", "z"]),
        json!(["-1", "-1", "1"]),
        json!([big.clone(), big.clone(), "1"]),
        json!([
            "21888242871839275222246405745257275088548364400416034343698204186575808495617",
            "2",
            "1"
        ]),
        json!(["0x1", "0x2", "0x1"]),
        json!(["1", "2", "0"]),
        json!(["１", "２", "１"]),
    ]
    .into_iter()
    .enumerate()
    {
        with(&format!("pi_a #{i}"), &|b| {
            b["ballotProof"]["pi_a"] = pa.clone()
        });
        with(&format!("pi_c #{i}"), &|b| {
            b["ballotProof"]["pi_c"] = pa.clone()
        });
        with(&format!("pi_b #{i}"), &|b| {
            b["ballotProof"]["pi_b"] = json!([[pa[0], pa[1]], [pa[1], pa[0]], [pa[2], pa[2]]]);
        });
    }
    with("proof swapped a/c", &|b| {
        let a = b["ballotProof"]["pi_a"].clone();
        b["ballotProof"]["pi_a"] = b["ballotProof"]["pi_c"].clone();
        b["ballotProof"]["pi_c"] = a;
    });
    with("proof protocol", &|b| {
        b["ballotProof"]["protocol"] = json!("plonk")
    });
    with("proof curve", &|b| {
        b["ballotProof"]["curve"] = json!("bls12381")
    });

    let n = variants.len();
    for (name, body) in &variants {
        let r = post_json(&t.base, "/votes", body.to_string().as_bytes()).await;
        // A different protocol/curve label is metadata the verifier may or
        // may not look at; the proof itself is still the genuine one.
        if name.starts_with("proof protocol") || name.starts_with("proof curve") {
            assert!(r.status == 200 || r.status == 400, "{name}: {}", r.status);
            continue;
        }
        assert_eq!((r.status, r.code()), (400, 40002), "{name}: {}", r.text());
    }
    let snap = t.client.process(&pid31()).await.unwrap();
    // Only the two label variants may have been admitted (same vote id: one).
    assert!(
        snap.pending_votes.unwrap() <= 1,
        "{} variants, pending {:?}",
        n,
        snap.pending_votes
    );
    t.shutdown.cancel();
}

// A valid signature and the same vote with v in {27, 28} are the same vote.
#[tokio::test]
async fn signature_v_forms() {
    for (delta, ok) in [(0u8, true), (27, true)] {
        let t = serve(FakeProver::gated(), &[]).await;
        let mut b = good_body(&t, 0, 5);
        let s = b["signature"].as_str().unwrap().to_string();
        let v = u8::from_str_radix(&s[s.len() - 2..], 16).unwrap();
        b["signature"] = json!(format!("{}{:02x}", &s[..s.len() - 2], v + delta));
        let r = post_json(&t.base, "/votes", b.to_string().as_bytes()).await;
        assert_eq!(r.status == 200, ok, "v+{delta}: {}", r.text());
        t.shutdown.cancel();
    }
}

// ------------------------------------------------------------ gating

#[tokio::test]
async fn process_gating_codes() {
    // Unknown pid.
    let t = serve(FakeProver::gated(), &[]).await;
    let mut b = good_body(&t, 0, 5);
    b["processId"] = json!(format!("0x{}", hex::encode(bad_pid31())));
    refused(&t, &b, (404, 40402), "unknown pid").await;
    // The all-zero id and the all-ff id.
    for id in [
        format!("0x{}", "00".repeat(31)),
        format!("0x{}", "ff".repeat(31)),
    ] {
        b["processId"] = json!(id);
        refused(&t, &b, (404, 40402), "extreme pid").await;
    }
    t.shutdown.cancel();

    // Canceled, ended, results.
    for (st, name) in [
        (ProcessStatus::Canceled, "canceled"),
        (ProcessStatus::Ended, "ended"),
    ] {
        let t = serve(FakeProver::gated(), &[]).await;
        t.s.chain.set_status(st);
        wait_until("closed", async || {
            !t.client.process(&pid31()).await.unwrap().is_accepting_votes
        })
        .await;
        let b = good_body(&t, 0, 5);
        refused(&t, &b, (412, 41201), name).await;
        t.shutdown.cancel();
    }

    // Past the end time while the status still says Ready.
    let t = serve(FakeProver::gated(), &[]).await;
    let end = t.client.process(&pid31()).await.unwrap();
    t.s.chain.advance_time(end.duration + 10);
    wait_until("past end", async || {
        !t.client.process(&pid31()).await.unwrap().is_accepting_votes
    })
    .await;
    refused(&t, &good_body(&t, 0, 5), (412, 41201), "past end").await;
    t.shutdown.cancel();

    // Paused still queues.
    let t = serve(FakeProver::gated(), &[]).await;
    t.s.chain.set_status(ProcessStatus::Paused);
    wait_until("paused", async || {
        t.client.process(&pid31()).await.unwrap().status == wire::ProcessStatus::Paused
    })
    .await;
    let r = post_json(
        &t.base,
        "/votes",
        good_body(&t, 0, 5).to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    t.shutdown.cancel();

    // Not started.
    let s = setup(2, 4, None);
    s.chain.set_start_time(T0 + 600);
    let t = serve_with(s, FakeProver::gated(), 100, &[]).await;
    handle(&t.node).await;
    refused(&t, &good_body(&t, 0, 5), (412, 41204), "not started").await;
    t.shutdown.cancel();
}

#[tokio::test]
async fn observer_refuses_votes_and_keys() {
    let t = serve_as(setup(2, 4, None), FakeProver::gated(), 100, &[], true).await;
    handle(&t.node).await;
    // Whatever the body, an observer says so before parsing it.
    for body in [good_body(&t, 0, 5).to_string(), "{".into(), "".into()] {
        let r = post_json(&t.base, "/votes", body.as_bytes()).await;
        assert_eq!((r.status, r.code()), (412, 41203), "{body:.30}");
    }
    let r = post_json(
        &t.base,
        "/processes/keys",
        json!({"processId": pid_str()}).to_string().as_bytes(),
    )
    .await;
    assert_eq!((r.status, r.code()), (412, 41203));
    t.shutdown.cancel();
}

// JSON followed by anything but whitespace is not a vote.
#[tokio::test]
async fn trailing_garbage_after_the_json_is_refused() {
    let t = serve(FakeProver::gated(), &[]).await;
    let ok = serde_json::to_vec(&good_body(&t, 0, 5)).unwrap();
    for tail in [&b"x"[..], b"{}", b"\0", b",", b"\n\n{", b"]"] {
        let r = post_json(&t.base, "/votes", &[&ok[..], tail].concat()).await;
        // BUG: axum's Json stops after the first value and ignores the rest.
        assert_eq!(
            (r.status, r.code()),
            (400, 40001),
            "tail {tail:?}: {}",
            r.text()
        );
    }
    let k = serde_json::to_vec(&json!({"processId": pid_str()})).unwrap();
    let r = post_json(&t.base, "/processes/keys", &[&k[..], b" x"].concat()).await;
    assert_eq!((r.status, r.code()), (400, 40001), "keys: {}", r.text());
    // Whitespace after the value is fine.
    let r = post_json(&t.base, "/votes", &[&ok[..], b" \r\n\t"].concat()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    t.shutdown.cancel();
}

// ------------------------------------------------------- concurrency

/// Fires `bodies` at `POST /votes` at once and returns `(status, code)` of each.
async fn burst(base: &str, bodies: Vec<String>) -> Vec<(u16, u32)> {
    let c = reqwest::Client::new();
    let mut tasks = Vec::new();
    for b in bodies {
        let c = c.clone();
        let url = format!("{base}/votes");
        tasks.push(tokio::spawn(async move {
            let r = c
                .post(url)
                .header("content-type", "application/json")
                .body(b)
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            let v: Value = r.json().await.unwrap();
            (status, v["code"].as_u64().unwrap_or(0) as u32)
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

fn count(rs: &[(u16, u32)], status: u16, code: u32) -> usize {
    rs.iter().filter(|r| **r == (status, code)).count()
}

#[tokio::test]
async fn one_package_submitted_many_times_is_admitted_once() {
    let t = serve(FakeProver::gated(), &[]).await;
    let body = good_body(&t, 0, 5).to_string();
    let rs = burst(&t.base, vec![body; 48]).await;
    assert_eq!(count(&rs, 200, 0), 1, "{rs:?}");
    // The rest are duplicates, or shed when validation is full.
    for r in &rs {
        assert!(matches!(r, (200, _) | (409, 40901) | (429, 42903)), "{r:?}");
    }
    let p = t.client.process(&pid31()).await.unwrap();
    assert_eq!(p.pending_votes, Some(1));
    let vid = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], 5)).vote_id;
    assert_eq!(
        t.client.vote_status(&pid31(), vid).await.unwrap(),
        wire::VoteStatus::Pending
    );
    t.shutdown.cancel();
}

// The same voter's different ballots racing: each is admitted at most
// `slot_depth` deep, in some order, and nothing is counted twice.
#[tokio::test]
async fn one_voters_racing_ballots_respect_the_slot_depth() {
    let t = serve_with(
        setup(2, 4, None),
        FakeProver::gated(),
        100,
        &["--slot-depth", "2"],
    )
    .await;
    handle(&t.node).await;
    let bodies: Vec<String> = [5u64, 6, 7]
        .iter()
        .flat_map(|k| {
            let b = good_body(&t, 0, *k).to_string();
            vec![b; 6]
        })
        .collect();
    let rs = burst(&t.base, bodies).await;
    assert_eq!(count(&rs, 200, 0), 2, "{rs:?}");
    for r in &rs {
        assert!(
            matches!(r, (200, _) | (409, 40901) | (409, 40902) | (429, 42903)),
            "{r:?}"
        );
    }
    assert_eq!(
        t.client.process(&pid31()).await.unwrap().pending_votes,
        Some(2)
    );
    t.shutdown.cancel();
}

#[tokio::test]
async fn racing_revotes_settle_as_one_voter() {
    let t = serve_with(
        setup(2, 4, None),
        FakeProver::open(),
        3,
        &["--slot-depth", "3", "--batch-time", "100ms"],
    )
    .await;
    handle(&t.node).await;
    let bodies = [5u64, 6, 7]
        .iter()
        .map(|k| good_body(&t, 0, *k).to_string())
        .collect();
    let rs = burst(&t.base, bodies).await;
    assert_eq!(count(&rs, 200, 0), 3, "{rs:?}");
    for k in [5u64, 6, 7] {
        let vid = wire_vote(&real_vote(&t.s.env, 0, &[1, 2], k)).vote_id;
        wait_until("settled", async || {
            t.client.vote_status(&pid31(), vid).await.ok() == Some(wire::VoteStatus::Settled)
        })
        .await;
    }
    let p = t.client.process(&pid31()).await.unwrap();
    assert_eq!((p.voters_count, p.overwritten_votes_count), (1, 2));
    t.shutdown.cancel();
}

#[tokio::test]
async fn max_voters_holds_under_a_race() {
    let s = setup(2, 4, None);
    s.chain.set_max_voters(1);
    let t = serve_with(s, FakeProver::gated(), 100, &[]).await;
    handle(&t.node).await;
    let a = good_body(&t, 0, 5).to_string();
    let b = good_body(&t, 1, 7).to_string();
    let bodies: Vec<String> = (0..20)
        .map(|i| if i % 2 == 0 { a.clone() } else { b.clone() })
        .collect();
    let rs = burst(&t.base, bodies).await;
    assert_eq!(count(&rs, 200, 0), 1, "{rs:?}");
    assert!(count(&rs, 412, 41202) >= 1, "{rs:?}");
    assert_eq!(
        t.client.process(&pid31()).await.unwrap().pending_votes,
        Some(1)
    );
    t.shutdown.cancel();
}

// Junk floods are answered with the documented codes, never a 5xx, and the
// node still takes an honest vote afterwards.
#[tokio::test]
async fn junk_flood_is_shed_with_documented_codes() {
    let t = serve(FakeProver::gated(), &[]).await;
    let e = &t.s.env;
    let mut bodies = Vec::new();
    for i in 0..400usize {
        let v = fake_vote(e, i % 4, &[1, 2], 1000 + (i as u64 % 50));
        let mut j = serde_json::to_value(wire_vote(&v)).unwrap();
        match i % 4 {
            0 => {}
            1 => j["weight"] = json!("2"),
            2 => j["ballotProof"]["pi_a"][0] = json!("9".repeat(70)),
            _ => j["signature"] = json!(format!("0x{}", "11".repeat(65))),
        }
        bodies.push(j.to_string());
    }
    let t0 = Instant::now();
    let rs = burst(&t.base, bodies).await;
    for r in &rs {
        assert!(matches!(r, (400, 40002) | (429, 42903)), "{r:?}");
    }
    assert!(t0.elapsed() < Duration::from_secs(40), "{:?}", t0.elapsed());
    let r = post_json(
        &t.base,
        "/votes",
        good_body(&t, 0, 5).to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    t.shutdown.cancel();
}

#[tokio::test]
async fn read_flood_does_not_starve_votes() {
    let t = serve(FakeProver::gated(), &[]).await;
    let p = pid_str();
    let c = reqwest::Client::new();
    let mut tasks = Vec::new();
    for i in 0..1500usize {
        let c = c.clone();
        let url = match i % 5 {
            0 => format!(
                "{}/votes/{p}/voteId/{:#018x}",
                t.base,
                (1u64 << 63) + i as u64
            ),
            1 => format!(
                "{}/votes/{p}/voteId/{:#018x}/proof",
                t.base,
                (1u64 << 63) + i as u64
            ),
            2 => format!("{}/votes/{p}/address/{}", t.base, addr_str(i % 4)),
            3 => format!("{}/processes/{p}/participants/{}", t.base, addr_str(i % 4)),
            _ => format!("{}/processes/{p}", t.base),
        };
        tasks.push(tokio::spawn(async move {
            c.get(url).send().await.map(|r| r.status().as_u16())
        }));
    }
    let t0 = Instant::now();
    let r = post_json(
        &t.base,
        "/votes",
        good_body(&t, 0, 5).to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());
    for h in tasks {
        let st = h.await.unwrap().unwrap();
        assert!(matches!(st, 200 | 404), "{st}");
    }
    t.shutdown.cancel();
}

// ------------------------------------------------------------- keys

#[tokio::test]
async fn key_limit_is_exact_under_a_race_and_counts_bad_bodies() {
    let t = serve(FakeProver::gated(), &["--keys-per-minute", "5"]).await;
    let c = reqwest::Client::new();
    let mut tasks = Vec::new();
    for i in 0..40u8 {
        let c = c.clone();
        let url = format!("{}/processes/keys", t.base);
        let body = if i % 4 == 0 {
            "{".to_string()
        } else {
            json!({"processId": format!("0x{}", hex::encode([i; 31])) }).to_string()
        };
        tasks.push(tokio::spawn(async move {
            let r = c
                .post(url)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            let st = r.status().as_u16();
            let v: Value = r.json().await.unwrap();
            (st, v["code"].as_u64().unwrap_or(0) as u32)
        }));
    }
    let mut rs = Vec::new();
    for h in tasks {
        rs.push(h.await.unwrap());
    }
    // Five tokens in total, whether the body was good or not.
    let passed = count(&rs, 200, 0) + count(&rs, 400, 40001);
    assert_eq!(passed, 5, "{rs:?}");
    assert_eq!(count(&rs, 429, 42901), 35, "{rs:?}");
    // The rate answer comes before the body is looked at.
    let r = post_json(&t.base, "/processes/keys", b"{").await;
    assert_eq!((r.status, r.code()), (429, 42901));
    t.shutdown.cancel();
}

#[tokio::test]
async fn derived_keys_are_usable_election_keys() {
    let t = serve(FakeProver::gated(), &["--keys-per-minute", "100000"]).await;
    let mut seen = std::collections::HashSet::new();
    let mut ids: Vec<[u8; 31]> = vec![[0u8; 31], [0xff; 31], pid31(), bad_pid31()];
    for i in 0..60u8 {
        let mut b = [0u8; 31];
        b[30] = i + 1;
        b[0] = i.wrapping_mul(7);
        ids.push(b);
    }
    for id in ids {
        let k = t.client.new_key(&id).await.unwrap();
        // The circuit demands a non-identity point of the prime-order subgroup.
        assert!(k.is_on_curve());
        assert!(k.in_subgroup(), "{id:?} gave a key outside the subgroup");
        assert_ne!(k, Point::IDENTITY);
        assert!(
            seen.insert((fr_to_be(&k.x), fr_to_be(&k.y))),
            "key collision"
        );
        // Deterministic.
        assert_eq!(t.client.new_key(&id).await.unwrap(), k);
    }
    t.shutdown.cancel();
}

// No error text carries the node's election secret.
#[tokio::test]
async fn error_bodies_never_carry_secrets() {
    let t = serve(FakeProver::gated(), &["--keys-per-minute", "1000"]).await;
    let ks = KeyStore::open(&t.node.db, 0, TEST_REGISTRY).unwrap();
    let sk = ks.secret(&pid31());
    let needles = vec![sk.to_string()];
    let mut bodies = Vec::new();
    let g = good_body(&t, 0, 5);
    for (path, b) in [
        ("/votes", g.to_string()),
        ("/votes", "{".into()),
        (
            "/votes",
            g.to_string()
                .replace("\"weight\":\"1\"", "\"weight\":\"x\""),
        ),
        ("/processes/keys", "{".into()),
        (
            "/processes/keys",
            json!({"processId": pid_str()}).to_string(),
        ),
    ] {
        bodies.push(post_json(&t.base, path, b.as_bytes()).await.text());
    }
    bodies.push(get_raw(&t.base, "/info").await.text());
    bodies.push(
        get_raw(&t.base, &format!("/processes/{}", pid_str()))
            .await
            .text(),
    );
    for b in bodies {
        for n in &needles {
            assert!(!b.contains(n.as_str()), "secret in {b:.200}");
        }
    }
    t.shutdown.cancel();
}

// ------------------------------------------------------- census gating

fn csp_setup() -> TestSetup {
    let mut e = env(2, 4, None);
    e.cfg.census_origin = CensusOrigin::Csp;
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(&voter_address(99));
    e.cfg.census_root = be;
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

#[tokio::test]
async fn csp_witness_attacks() {
    let t = serve_with(csp_setup(), FakeProver::gated(), 100, &[]).await;
    handle(&t.node).await;
    let v = real_vote(&t.s.env, 0, &[1, 2], 5);
    let base = serde_json::to_value(wire_vote(&v)).unwrap();
    let att = |sk: usize, who: usize, w: u128, idx: u64| {
        let a = csp_sign(&voter_key(sk), &pid_fr(), &voter_address(who), w, idx);
        json!({"type": "csp", "r": format!("0x{}", hex::encode(a.r)), "s": format!("0x{}", hex::encode(a.s)),
               "recid": a.recid, "index": a.index})
    };
    let with = |c: Value| {
        let mut b = base.clone();
        b["censusProof"] = c;
        b
    };
    // No proof, null, a Merkle proof where a CSP one is due.
    refused(&t, &base, (400, 40001), "no census proof").await;
    refused(&t, &with(Value::Null), (400, 40001), "null census proof").await;
    let imt_proof = t.s.env.imt.proof(0).unwrap();
    let m = serde_json::to_value(davinci_client::api::CensusProofWire::Merkle(
        davinci_client::api::MerkleProof::from(&imt_proof),
    ))
    .unwrap();
    refused(&t, &with(m), (400, 40001), "merkle proof in a csp process").await;
    // Signed by the wrong key, for another voter, for another weight, with
    // a mangled signature, recid out of range, index out of the namespace.
    refused(&t, &with(att(98, 0, 1, 0)), (400, 40002), "wrong csp key").await;
    refused(
        &t,
        &with(att(99, 1, 1, 0)),
        (400, 40002),
        "attestation for another voter",
    )
    .await;
    refused(
        &t,
        &with(att(99, 0, 2, 0)),
        (400, 40002),
        "attestation for another weight",
    )
    .await;
    let mut c = att(99, 0, 1, 0);
    // 27/28 is the recid shifted, accepted as the guest does (see
    // attack_validate); the shifted wrong parity still fails.
    let wrong_parity = 28 - c["recid"].as_u64().unwrap();
    for recid in [2u64, 3, 4, wrong_parity, 255] {
        c["recid"] = json!(recid);
        refused(
            &t,
            &with(c.clone()),
            (400, 40002),
            &format!("recid {recid}"),
        )
        .await;
    }
    let mut c = att(99, 0, 1, 0);
    c["r"] = json!(format!("0x{}", "00".repeat(32)));
    c["s"] = json!(format!("0x{}", "00".repeat(32)));
    refused(&t, &with(c), (400, 40002), "zero r and s").await;
    let mut c = att(99, 0, 1, 0);
    c["s"] = json!(format!("0x{}", "ff".repeat(32)));
    refused(&t, &with(c), (400, 40002), "s above the group order").await;
    // An index past the ballot namespace: signed by the CSP, still refused.
    for idx in [u64::MAX, 1u64 << 63, (1u64 << 63) - 0x10] {
        refused(
            &t,
            &with(att(99, 0, 1, idx)),
            (400, 40002),
            &format!("index {idx}"),
        )
        .await;
    }
    // Nothing leaked into the queue; the honest attestation works.
    assert_eq!(
        t.client.process(&pid31()).await.unwrap().pending_votes,
        Some(0)
    );
    let r = post_json(
        &t.base,
        "/votes",
        with(att(99, 0, 1, 0)).to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    t.shutdown.cancel();
}

#[tokio::test]
async fn merkle_process_ignores_a_supplied_census_proof() {
    let t = serve(FakeProver::gated(), &[]).await;
    let g = good_body(&t, 0, 5);
    // Garbage and mismatched proofs of either type do not matter: the node
    // derives its own from the on-chain root.
    let csp = json!({"type": "csp", "r": format!("0x{}", "11".repeat(32)), "s": format!("0x{}", "22".repeat(32)), "recid": 9, "index": 7});
    let mut b = g.clone();
    b["censusProof"] = csp;
    let r = post_json(&t.base, "/votes", b.to_string().as_bytes()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // A voter outside the census gets 40001 whatever proof they bring.
    let mut other = g.clone();
    other["address"] = json!(addr_str(9));
    other["censusProof"] = serde_json::to_value(davinci_client::api::CensusProofWire::Merkle(
        davinci_client::api::MerkleProof::from(&t.s.env.imt.proof(0).unwrap()),
    ))
    .unwrap();
    refused(&t, &other, (400, 40001), "address outside the census").await;
    t.shutdown.cancel();
}

#[tokio::test]
async fn query_strings_and_extra_headers_do_not_matter() {
    let t = serve(FakeProver::gated(), &[]).await;
    let p = pid_str();
    let q = "a=1&".repeat(2000);
    for target in [
        format!("/info?{q}"),
        format!("/processes?limit=-1&{q}"),
        format!("/processes/{p}?x=%ff"),
        format!("/votes/{p}/voteId/0xffffffffffffffff?x"),
        "/ping?%".to_string(),
    ] {
        let r = get_raw(&t.base, &target).await;
        assert!(
            matches!(r.status, 200 | 404),
            "{target:.60} -> {}",
            r.status
        );
    }
    t.shutdown.cancel();
}

// A closed process says so before spending a pairing check on the vote.
#[tokio::test]
async fn closed_process_refuses_before_validating() {
    for st in [ProcessStatus::Canceled, ProcessStatus::Ended] {
        let t = serve(FakeProver::gated(), &[]).await;
        t.s.chain.set_status(st);
        wait_until("closed", async || {
            !t.client.process(&pid31()).await.unwrap().is_accepting_votes
        })
        .await;
        let junk = serde_json::to_value(wire_vote(&fake_vote(&t.s.env, 1, &[1, 2], 9))).unwrap();
        let r = post_json(&t.base, "/votes", junk.to_string().as_bytes()).await;
        // BUG: the Groth16 check runs first, so a closed process still pays for it.
        assert_eq!(r.status, 412, "{st:?}: {}", r.text());
        assert_eq!(r.code(), 41201);
        t.shutdown.cancel();
    }
}

// An organizer registers a census root that is not a field element: the
// node refuses to serve the process, and says so instead of failing reads.
#[tokio::test]
async fn non_canonical_census_root_is_reported_not_a_500() {
    let s = setup(2, 4, None);
    let uri = write_census(&s.census_dir, 4);
    s.chain.set_census([0xff; 32], uri);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let shutdown = CancellationToken::new();
    let cfg = test_config_with(dir.path(), &s.census_dir, 100, "0s", &[]);
    let node = start_node_cfg(
        db,
        s.chain.clone(),
        FakeProver::gated(),
        cfg,
        shutdown.clone(),
    )
    .await;
    let pid = pid_fr();
    wait_until("process recorded", async || {
        node.db.process(&pid).unwrap().is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(node.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let p = pid_str();
    let a = addr_str(0);
    for target in [
        format!("/processes/{p}"),
        format!("/processes/{p}/participants/{a}"),
        format!("/processes/{p}/transitions"),
        format!("/votes/{p}/address/{a}"),
        "/processes".to_string(),
    ] {
        let r = get_raw(&base, &target).await;
        // BUG: the process view reads the root with `fr_from_be(..)?` -> 500.
        assert_ne!(r.status, 500, "{target}: {}", r.text());
    }
    let r = get_raw(&base, &format!("/processes/{p}")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["ignored"], json!(true));
    shutdown.cancel();
}

// A vote id that is already stored answers 409 without a pairing check, so
// replaying one package cannot occupy the validation permits.
#[tokio::test]
async fn replayed_vote_id_is_refused_before_validating() {
    let t = serve(FakeProver::gated(), &[]).await;
    let good = good_body(&t, 0, 5);
    let r = post_json(&t.base, "/votes", good.to_string().as_bytes()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // The same vote id with a proof that cannot verify.
    let mut junk = good.clone();
    junk["ballotProof"]["pi_a"] = json!(["1", "2", "1"]);
    let r = post_json(&t.base, "/votes", junk.to_string().as_bytes()).await;
    // BUG: validation runs before the duplicate check, so replays pay for it.
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(r.code(), 40901);
    t.shutdown.cancel();
}

/// One `POST /processes/keys` from the loopback address `127.a.b.c`.
async fn key_from(base: &str, n: u32) -> u16 {
    let addr: std::net::SocketAddr = base.trim_start_matches("http://").parse().unwrap();
    let ip = std::net::Ipv4Addr::new(127, (n >> 16) as u8, (n >> 8) as u8, n as u8);
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.bind((ip, 0).into()).unwrap();
    let mut s = sock.connect(addr).await.unwrap();
    let body = json!({"processId": pid_str()}).to_string();
    let req = format!(
        "POST /processes/keys HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    read_response(&mut s, Duration::from_secs(20)).await.status
}

// The limit is per peer address: one client's spent tokens cost nobody
// else theirs, and many peers do not slow the limiter down.
#[tokio::test]
async fn key_limit_is_per_peer_and_cheap_with_many_peers() {
    let t = serve(FakeProver::gated(), &["--keys-per-minute", "2"]).await;
    for n in 1..=2 {
        assert_eq!(key_from(&t.base, 1).await, 200, "token {n} of peer 1");
    }
    assert_eq!(key_from(&t.base, 1).await, 429);
    assert_eq!(
        key_from(&t.base, 2).await,
        200,
        "another peer is unaffected"
    );
    // 8k distinct peers, one request each.
    let t0 = Instant::now();
    for n in 100..8_100u32 {
        assert_eq!(key_from(&t.base, n).await, 200);
    }
    let first = t0.elapsed();
    // A late request must not pay for the whole table.
    let t1 = Instant::now();
    for n in 8_100..8_200u32 {
        key_from(&t.base, n).await;
    }
    let per_late = t1.elapsed() / 100;
    let per_first = first / 8_000;
    println!("per request: first {per_first:?}, late {per_late:?}");
    assert!(
        per_late < per_first * 20 + Duration::from_millis(5),
        "{per_first:?} -> {per_late:?}"
    );
    t.shutdown.cancel();
}

// A connection that never finishes its request head must not be held for
// ever (the 60 s request deadline only starts once the head is parsed).
#[tokio::test]
async fn half_sent_request_head_is_dropped() {
    let t = serve(FakeProver::gated(), &[]).await;
    let addr = t.base.trim_start_matches("http://");
    let mut conns = Vec::new();
    for _ in 0..20 {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /ping HTTP/1.1\r\nHost: x\r\nX-Slow: ")
            .await
            .unwrap();
        conns.push(s);
    }
    let t0 = Instant::now();
    for s in &mut conns {
        let mut buf = [0u8; 1024];
        // BUG: nothing closes a connection whose head never completes.
        let r = tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf)).await;
        assert!(
            r.is_ok(),
            "half-sent head still held after {:?}",
            t0.elapsed()
        );
    }
    // The node serves meanwhile and afterwards.
    t.client.ping().await.unwrap();
    t.shutdown.cancel();
}

// The head bound does not cut a client that keeps its connection busy.
#[tokio::test]
async fn keep_alive_connection_serves_requests_within_the_bound() {
    let t = serve(FakeProver::gated(), &[]).await;
    let addr = t.base.trim_start_matches("http://");
    let mut s = TcpStream::connect(addr).await.unwrap();
    for _ in 0..3 {
        s.write_all(b"GET /ping HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let r = read_response(&mut s, Duration::from_secs(5)).await;
        assert_eq!((r.status, r.body.as_slice()), (200, &b"pong"[..]));
        tokio::time::sleep(HEAD_TIMEOUT / 2).await;
    }
    t.shutdown.cancel();
}

// A process whose actor is stuck must not take the node's validation
// permits: another process's votes keep being validated.
#[tokio::test]
async fn a_stalled_actor_does_not_hold_the_validation_permits() {
    let t = serve(FakeProver::gated(), &[]).await;
    let permits = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2);
    // The process's mailbox is full and never drains.
    let stalled = ActorHandle::stalled(8, T0 + 10, t.s.chain.clock());
    t.node.processes.write().await.insert(pid_fr(), stalled);
    let body = good_body(&t, 0, 5).to_string();
    let c = reqwest::Client::new();
    let mut tasks = Vec::new();
    for _ in 0..permits + 24 {
        let (c, url, b) = (c.clone(), format!("{}/votes", t.base), body.clone());
        tasks.push(tokio::spawn(async move {
            let r = c
                .post(url)
                .header("content-type", "application/json")
                .body(b)
                .send()
                .await
                .unwrap();
            r.status().as_u16()
        }));
    }
    tokio::time::sleep(Duration::from_secs(4)).await;
    // Any other vote still reaches validation (and fails it) instead of 42903.
    let junk = serde_json::to_value(wire_vote(&fake_vote(&t.s.env, 1, &[1, 2], 9))).unwrap();
    let r = post_json(&t.base, "/votes", junk.to_string().as_bytes()).await;
    // BUG: the permit is held while the vote waits on the actor's mailbox.
    assert_eq!((r.status, r.code()), (400, 40002), "{}", r.text());
    // The votes that did not fit the full mailbox were shed at once.
    let shed = tasks.iter().filter(|h| h.is_finished()).count();
    assert!(
        shed >= permits + 24 - 9,
        "only {shed} of {} shed",
        permits + 24
    );
}

// Admission by the clock is refused before the pairing check, by the rule
// the actor applies (not started; at or past the end while still Ready).
#[tokio::test]
async fn clock_closed_process_refuses_before_validating() {
    let junk =
        |t: &Served| serde_json::to_value(wire_vote(&fake_vote(&t.s.env, 1, &[1, 2], 9))).unwrap();
    // Not started.
    let s = setup(2, 4, None);
    s.chain.set_start_time(T0 + 600);
    let t = serve_with(s, FakeProver::gated(), 100, &[]).await;
    handle(&t.node).await;
    let r = post_json(&t.base, "/votes", junk(&t).to_string().as_bytes()).await;
    // BUG: validation runs first, so the junk vote gets 40002.
    assert_eq!((r.status, r.code()), (412, 41204), "{}", r.text());
    t.shutdown.cancel();

    // At the end time, status still Ready; one second before it, open.
    let t = serve(FakeProver::gated(), &[]).await;
    let p = t.client.process(&pid31()).await.unwrap();
    t.s.chain.advance_time(p.duration - 1);
    wait_until("head moved", async || {
        t.client.process(&pid31()).await.unwrap().is_accepting_votes
    })
    .await;
    let r = post_json(&t.base, "/votes", junk(&t).to_string().as_bytes()).await;
    assert_eq!(
        (r.status, r.code()),
        (400, 40002),
        "last second: {}",
        r.text()
    );
    let r = post_json(
        &t.base,
        "/votes",
        good_body(&t, 0, 5).to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "last second: {}", r.text());
    t.s.chain.advance_time(1);
    wait_until("closed", async || {
        !t.client.process(&pid31()).await.unwrap().is_accepting_votes
    })
    .await;
    let r = post_json(&t.base, "/votes", junk(&t).to_string().as_bytes()).await;
    assert_eq!((r.status, r.code()), (412, 41201), "{}", r.text());
    t.shutdown.cancel();
}
