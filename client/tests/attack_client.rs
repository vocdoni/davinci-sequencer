//! Adversarial tests of davinci-client: a hostile sequencer, hostile user
//! input, the organizer's census and mode helpers, and the presets.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use arbo::{MemoryStorage, Sha256, Tree};
use axum::Router;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use common::{election, merkle_census, voters};
use davinci_client::api::{self, ProcessId, TrackerProof, vote_id_hex};
use davinci_client::networks::{self, NETWORKS, NetworkDa, OP_L1_BLOCK};
use davinci_client::organizer::{census_file, merkle_census as org_census};
use davinci_client::{Error, SequencerClient};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{CensusWitness, LeanImt, census_leaf, eth_address};
use davinci_zkvm_sdk::crypto::field::Fr;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PID: ProcessId = ProcessId([0x5a; 31]);
const VID: u64 = 0x8000_0000_0000_beef;
const ADDR: [u8; 20] = [0x33; 20];

// ---------------------------------------------------------------------------
// Hostile server helpers

/// Serves `f(path)` for every request.
async fn hostile<F>(f: F) -> SequencerClient
where
    F: Fn(&str) -> Response + Clone + Send + Sync + 'static,
{
    let app = Router::new().fallback(move |uri: Uri| {
        let f = f.clone();
        async move { f(uri.path()) }
    });
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    SequencerClient::new(&url)
}

fn json_ok(body: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        body.into(),
    )
        .into_response()
}

/// A raw TCP server: reads the request head, then runs `serve` on the socket.
async fn raw_server<F, Fut>(serve: F) -> String
where
    F: Fn(tokio::net::TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    let serve = Arc::new(serve);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let serve = serve.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                serve(s).await;
            });
        }
    });
    url
}

// ---------------------------------------------------------------------------
// Malformed and oversized responses

fn garbage() -> Vec<(&'static str, Vec<u8>)> {
    let deep = "[".repeat(200_000);
    vec![
        ("empty", vec![]),
        ("null", b"null".to_vec()),
        ("array", b"[]".to_vec()),
        ("truncated", br#"{"processes":["#.to_vec()),
        ("nul byte", vec![0]),
        ("bad utf8", vec![0xff, 0xfe, b'{', b'}']),
        ("deep nesting", deep.into_bytes()),
        ("wrong type", br#"{"processes":7}"#.to_vec()),
        (
            "number soup",
            format!("{{\"x\":{}}}", "9".repeat(100_000)).into_bytes(),
        ),
        ("html", b"<html>502 bad gateway</html>".to_vec()),
        ("bare string", br#""0x00""#.to_vec()),
        ("empty object", b"{}".to_vec()),
    ]
}

// Every route must turn garbage into an error, never a panic or a success.
#[tokio::test]
async fn garbage_json_errors_on_every_route() {
    for (name, body) in garbage() {
        let b = body.clone();
        let c = hostile(move |_| {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                b.clone(),
            )
                .into_response()
        })
        .await;
        let pid = PID.0;
        assert!(c.info().await.is_err(), "info {name}");
        assert!(c.processes().await.is_err(), "processes {name}");
        assert!(c.process(&pid).await.is_err(), "process {name}");
        assert!(c.new_key(&pid).await.is_err(), "new_key {name}");
        assert!(
            c.participant(&pid, &ADDR).await.is_err(),
            "participant {name}"
        );
        assert!(
            c.vote_status_full(&pid, VID).await.is_err(),
            "status {name}"
        );
        assert!(c.vote_id_proof(&pid, VID).await.is_err(), "proof {name}");
        assert!(c.ballot(&pid, &ADDR).await.is_err(), "ballot {name}");
        assert!(c.transitions(&pid).await.is_err(), "transitions {name}");
        assert!(c.transition_blobs(&pid, 0).await.is_err(), "blobs {name}");
    }
}

// 2xx with an empty body on a route that must carry JSON.
#[tokio::test]
async fn no_content_is_an_error() {
    let c = hostile(|_| StatusCode::NO_CONTENT.into_response()).await;
    assert!(c.info().await.is_err());
    assert!(c.processes().await.is_err());
    // ping only wants a 2xx.
    c.ping().await.unwrap();
}

// A body over the cap errors without being buffered whole, even when the
// server never stops sending or lies about the length.
#[tokio::test]
async fn oversized_and_endless_bodies_are_cut_off() {
    // 17 MiB of valid JSON padding.
    let big = format!("{{\"processes\":[]{}}}", " ".repeat(17 << 20));
    let c = hostile(move |_| json_ok(big.clone())).await;
    match c.processes().await {
        Err(Error::Decode(m)) => assert!(m.contains("too large"), "{m}"),
        other => panic!("want too large, got {other:?}"),
    }

    // Endless chunked stream.
    let url = raw_server(|mut s| async move {
        let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        if s.write_all(head).await.is_err() {
            return;
        }
        let chunk = format!("{:x}\r\n{}\r\n", 1 << 20, " ".repeat(1 << 20));
        loop {
            if s.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
        }
    })
    .await;
    let t = Instant::now();
    assert!(SequencerClient::new(&url).info().await.is_err());
    assert!(
        t.elapsed() < Duration::from_secs(30),
        "took {:?}",
        t.elapsed()
    );

    // A huge Content-Length followed by a short, closed body.
    let url = raw_server(|mut s| async move {
        let _ = s
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999\r\n\r\n{\"a\":1}")
            .await;
    })
    .await;
    assert!(SequencerClient::new(&url).info().await.is_err());

    // The same cap holds for error bodies, and the kept text is bounded.
    let big = "x".repeat(1 << 20);
    let c = hostile(move |_| (StatusCode::BAD_GATEWAY, big.clone()).into_response()).await;
    match c.info().await {
        Err(Error::Api {
            status, message, ..
        }) => {
            assert_eq!(status, 502);
            assert!(message.chars().count() <= 4096);
        }
        other => panic!("{other:?}"),
    }
    let big = "é".repeat(1 << 20);
    let c = hostile(move |_| (StatusCode::BAD_GATEWAY, big.clone()).into_response()).await;
    assert!(matches!(c.info().await, Err(Error::Api { .. })));
}

// A server that accepts and goes silent does not hang a client whose
// timeout is set; one that resets the connection errors.
#[tokio::test]
async fn silent_and_reset_connections_error() {
    let url = raw_server(|s| async move {
        drop(s);
    })
    .await;
    assert!(SequencerClient::new(&url).info().await.is_err());
    let url = raw_server(|mut s| async move {
        let _ = s.write_all(b"garbage that is not http\r\n\r\n").await;
    })
    .await;
    assert!(SequencerClient::new(&url).info().await.is_err());

    let url = raw_server(|s| async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(s);
    })
    .await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let t = Instant::now();
    assert!(SequencerClient::with_http(&url, http).info().await.is_err());
    assert!(t.elapsed() < Duration::from_secs(5));
}

// A sequencer must not steer the client to another host.
#[tokio::test]
async fn redirects_are_not_followed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().fallback(move || {
        let h = h.clone();
        async move {
            h.fetch_add(1, Ordering::SeqCst);
            json_ok(r#"{"processes":[]}"#)
        }
    });
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}/processes", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    for code in [301u16, 302, 307, 308] {
        let t = target.clone();
        let c = hostile(move |_| {
            let r = match code {
                301 => Redirect::permanent(&t),
                307 => Redirect::temporary(&t),
                308 => Redirect::permanent(&t),
                _ => Redirect::to(&t),
            };
            r.into_response()
        })
        .await;
        // BUG: reqwest follows up to 10 redirects by default.
        assert!(c.processes().await.is_err(), "redirect {code} followed");
        let v = davinci_client::api::VoteRequest::clone(&sample_vote());
        assert!(c.submit_vote(&v).await.is_err());
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "client followed a redirect");
}

fn sample_vote() -> davinci_client::api::VoteRequest {
    use davinci_client::api::{CensusProofWire, MerkleProof};
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::types::SnarkJsProof;
    davinci_client::api::VoteRequest {
        process_id: PID,
        address: ADDR,
        vote_id: VID,
        ballot: Ballot::identity(),
        ballot_proof: SnarkJsProof {
            pi_a: ["1".into(), "2".into(), "1".into()],
            pi_b: [
                ["1".into(), "2".into()],
                ["3".into(), "4".into()],
                ["1".into(), "0".into()],
            ],
            pi_c: ["5".into(), "6".into(), "1".into()],
            protocol: "groth16".into(),
            curve: "bn128".into(),
        },
        signature: [0; 65],
        weight: 1,
        census_proof: Some(CensusProofWire::Merkle(MerkleProof {
            root: [1; 32],
            leaf: [1; 32],
            path_bits: 0,
            siblings: vec![],
        })),
    }
}

// ---------------------------------------------------------------------------
// Responses about another object

fn proc_view_json(id: &ProcessId) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(),
        "status": "ready",
        "isAcceptingVotes": true,
        "organizationId": format!("0x{}", hex::encode([1u8; 20])),
        "encryptionKey": {"x": "5299619240641551281634865583518297030282874472190772894086521144482721001553",
                          "y": "16950150798460657717958625567821834550301663161624707787222815936182638968203"},
        "ballotMode": {"numFields": 2, "groupSize": 1, "uniqueValues": false, "costExponent": 1,
                       "maxValue": "3", "minValue": "0", "maxValueSum": "0", "minValueSum": "0"},
        "census": {"censusOrigin": 1, "censusRoot": "1", "censusURI": "file:///x"},
        "stateRoot": format!("0x{}", hex::encode([2u8; 32])),
        "votersCount": 0, "overwrittenVotesCount": 0, "maxVoters": 10,
        "startTime": 1, "duration": 2,
    })
}

// BUG: `process` returns a view of another process when the sequencer swaps ids.
#[tokio::test]
async fn process_view_must_be_for_the_asked_process() {
    let other = ProcessId([0x11; 31]);
    let c = hostile(move |_| json_ok(proc_view_json(&other).to_string())).await;
    match c.process(&PID.0).await {
        Err(Error::Decode(_)) => {}
        Ok(v) => panic!("accepted a view of {} for {}", v.id, PID),
        Err(e) => panic!("{e:?}"),
    }
}

// BUG: `ballot` returns the ballot of another address.
#[tokio::test]
async fn ballot_must_be_for_the_asked_address() {
    use davinci_client::api::BallotResponse;
    use davinci_zkvm_sdk::ballot::Ballot;
    let body = serde_json::to_string(&BallotResponse {
        address: [0x44; 20],
        ballot: Ballot::identity(),
    })
    .unwrap();
    let c = hostile(move |_| json_ok(body.clone())).await;
    assert!(c.ballot(&PID.0, &ADDR).await.is_err());
}

fn tracker_json(pid: &ProcessId, vid: u64, root: [u8; 32], sibs: &[[u8; 32]]) -> String {
    serde_json::to_string(&TrackerProof {
        process_id: *pid,
        vote_id: vid,
        root,
        siblings: sibs.to_vec(),
    })
    .unwrap()
}

#[tokio::test]
async fn tracker_proof_for_another_vote_or_process_is_refused() {
    let wrong_pid = tracker_json(&ProcessId([9; 31]), VID, [1; 32], &[]);
    let c = hostile(move |_| json_ok(wrong_pid.clone())).await;
    assert!(matches!(
        c.vote_id_proof(&PID.0, VID).await,
        Err(Error::Decode(_))
    ));
    let wrong_vid = tracker_json(&PID, VID ^ 1, [1; 32], &[]);
    let c = hostile(move |_| json_ok(wrong_vid.clone())).await;
    assert!(matches!(
        c.vote_id_proof(&PID.0, VID).await,
        Err(Error::Decode(_))
    ));
    // Vote ids below 2^63 and bad hex never decode.
    for bad in ["0x0000000000000001", "0x1", "zz", "0x80000000000000001"] {
        let body = format!(
            r#"{{"processId":"{PID}","voteId":"{bad}","root":"0x{}","siblings":[]}}"#,
            "00".repeat(32)
        );
        let c = hostile(move |_| json_ok(body.clone())).await;
        assert!(c.vote_id_proof(&PID.0, VID).await.is_err(), "{bad}");
    }
    // A sibling of the wrong length.
    let body = format!(
        r#"{{"processId":"{PID}","voteId":"{}","root":"0x{}","siblings":["0x00"]}}"#,
        vote_id_hex(VID),
        "00".repeat(32)
    );
    let c = hostile(move |_| json_ok(body.clone())).await;
    assert!(c.vote_id_proof(&PID.0, VID).await.is_err());
}

// The vote id ack must be for the submitted vote; the participant proof for
// the asked address and weight.
#[tokio::test]
async fn acks_and_participants_are_bound_to_the_request() {
    let c = hostile(|_| json_ok(format!(r#"{{"voteId":"{}"}}"#, vote_id_hex(VID ^ 2)))).await;
    assert!(c.submit_vote(&sample_vote()).await.is_err());
    // Missing field in the ack.
    let c = hostile(|_| json_ok("{}")).await;
    assert!(c.submit_vote(&sample_vote()).await.is_err());

    let mut rng = StdRng::seed_from_u64(1);
    let vs = voters(4, &mut rng);
    let t = merkle_census(&vs, 5);
    let me = vs[2].address();
    let p = t.proof(2).unwrap();
    let ok = |weight: &str, addr: [u8; 20], proof: &davinci_zkvm_sdk::census::CensusProof| {
        serde_json::json!({
            "address": format!("0x{}", hex::encode(addr)),
            "weight": weight,
            "censusProof": api::MerkleProof::from(proof),
        })
        .to_string()
    };
    // Honest answer.
    let body = ok("5", me, &p);
    let c = hostile(move |_| json_ok(body.clone())).await;
    let (_, w) = c.participant(&PID.0, &me).await.unwrap();
    assert_eq!(w, 5);
    // Larger weight than the leaf commits to, proof unchanged.
    for w in ["6", "0", "340282366920938463463374607431768211455"] {
        let body = ok(w, me, &p);
        let c = hostile(move |_| json_ok(body.clone())).await;
        assert!(c.participant(&PID.0, &me).await.is_err(), "weight {w}");
    }
    // Another voter's answer, address relabelled.
    let body = ok("5", me, &t.proof(1).unwrap());
    let c = hostile(move |_| json_ok(body.clone())).await;
    assert!(c.participant(&PID.0, &me).await.is_err());
    // Asked about a different address than the one answered.
    let body = ok("5", me, &p);
    let c = hostile(move |_| json_ok(body.clone())).await;
    assert!(c.participant(&PID.0, &vs[3].address()).await.is_err());
    // Proof with absurd shape: path bits past the siblings, 200k siblings.
    let mut bad = p.clone();
    bad.path_bits = u64::MAX;
    let body = ok("5", me, &bad);
    let c = hostile(move |_| json_ok(body.clone())).await;
    assert!(c.participant(&PID.0, &me).await.is_err());
    let mut bad = p.clone();
    bad.siblings = vec![[1u8; 32]; 200_000];
    let body = ok("5", me, &bad);
    let t0 = Instant::now();
    let c = hostile(move |_| json_ok(body.clone())).await;
    assert!(c.participant(&PID.0, &me).await.is_err());
    assert!(t0.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn new_key_refuses_weak_points() {
    use davinci_zkvm_sdk::crypto::field::fr_to_dec;
    let neg1 = fr_to_dec(&-Fr::from(1u64));
    // identity, order 2, order 4 (x = sqrt, not needed: any off-curve too), off curve.
    let cases = [("0", "1"), ("0", neg1.as_str()), ("1", "2"), ("0", "0")];
    for (x, y) in cases {
        let body = format!(r#"{{"x":"{x}","y":"{y}"}}"#);
        let c = hostile(move |_| json_ok(body.clone())).await;
        assert!(c.new_key(&PID.0).await.is_err(), "({x},{y})");
    }
}

// ---------------------------------------------------------------------------
// verify_tracker against an arbo oracle

fn vid_key(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

struct World {
    tree: Tree<MemoryStorage, Sha256>,
    root: [u8; 32],
    vids: Vec<u64>,
}

fn world(seed: u64, n: usize) -> World {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut tree = Tree::new(MemoryStorage::new(), 64, Sha256).unwrap();
    for k in [0u64, 2, 3, 4, 6, 7, 0x10, 0x11] {
        tree.add(&k.to_le_bytes(), &[k as u8 + 1; 32]).unwrap();
    }
    let mut vids = vec![];
    for _ in 0..n {
        let v = rand::Rng::r#gen::<u64>(&mut rng) | (1 << 63);
        if tree.add(&vid_key(v), &[0u8; 32]).is_ok() {
            vids.push(v);
        }
    }
    let root = tree.root();
    World { tree, root, vids }
}

fn proof_of(w: &World, v: u64) -> TrackerProof {
    let p = w.tree.gen_proof(&vid_key(v)).unwrap();
    assert!(p.exists);
    TrackerProof {
        process_id: PID,
        vote_id: v,
        root: w.root,
        siblings: p.siblings,
    }
}

#[test]
fn tracker_rejects_every_structural_tamper() {
    let w = world(11, 60);
    let absent: Vec<u64> = (0..20u64).map(|i| (1 << 63) | (i * 7919 + 3)).collect();
    for v in &w.vids {
        let tp = proof_of(&w, *v);
        assert!(api::verify_tracker(&tp, &w.root));

        // Truncated at every depth, and with every overlong padding.
        for cut in 0..tp.siblings.len() {
            let mut t = tp.clone();
            t.siblings.truncate(cut);
            assert!(!api::verify_tracker(&t, &w.root), "truncated to {cut}");
        }
        for extra in 1..=3 {
            let mut t = tp.clone();
            t.siblings.extend(std::iter::repeat_n([0u8; 32], extra));
            assert!(!api::verify_tracker(&t, &w.root), "+{extra} zero levels");
            let mut t = tp.clone();
            t.siblings.insert(0, [0u8; 32]);
            assert!(!api::verify_tracker(&t, &w.root), "zero level at the root");
        }
        // Root-to-leaf order swapped.
        if tp.siblings.len() > 1 && tp.siblings.windows(2).any(|p| p[0] != p[1]) {
            let mut t = tp.clone();
            t.siblings.reverse();
            assert!(!api::verify_tracker(&t, &w.root), "reversed siblings");
        }
        // Every flipped bit of every sibling.
        for i in 0..tp.siblings.len() {
            for bit in [0usize, 7, 128, 255] {
                let mut t = tp.clone();
                t.siblings[i][bit / 8] ^= 1 << (bit % 8);
                assert!(!api::verify_tracker(&t, &w.root));
            }
        }
        // Another vote id on this proof, including all one-bit neighbours.
        for b in 0..64 {
            let mut t = tp.clone();
            t.vote_id ^= 1 << b;
            assert!(!api::verify_tracker(&t, &w.root), "vid bit {b}");
        }
        // The proof's own root field is irrelevant: the caller's root rules.
        let mut other = w.root;
        other[31] ^= 1;
        assert!(!api::verify_tracker(&tp, &other));
        let mut t = tp.clone();
        t.root = other;
        assert!(!api::verify_tracker(&t, &w.root));
        assert!(!api::verify_tracker(&t, &other));
        // Zero roots.
        assert!(!api::verify_tracker(&tp, &[0u8; 32]));
    }
    // A vote id the tree does not hold, with the sibling path the tree
    // would give it (arbo's non-inclusion proof) or any other path.
    for v in absent {
        let p = w.tree.gen_proof(&vid_key(v)).unwrap();
        if p.exists {
            continue;
        }
        let tp = TrackerProof {
            process_id: PID,
            vote_id: v,
            root: w.root,
            siblings: p.siblings.clone(),
        };
        assert!(!api::verify_tracker(&tp, &w.root), "absent {v:x}");
        // Borrow a present vote's siblings.
        let other = proof_of(&w, w.vids[0]);
        let tp = TrackerProof {
            vote_id: v,
            siblings: other.siblings,
            ..tp
        };
        assert!(!api::verify_tracker(&tp, &w.root));
    }
}

// Config, ballot and census leaves share the tree; none is a vote id.
#[test]
fn tracker_rejects_non_vote_leaves() {
    let w = world(12, 30);
    for k in [0u64, 2, 3, 4, 6, 7, 0x10, 0x11] {
        let p = w.tree.gen_proof(&k.to_le_bytes()).unwrap();
        let tp = TrackerProof {
            process_id: PID,
            vote_id: k,
            root: w.root,
            siblings: p.siblings,
        };
        assert!(!api::verify_tracker(&tp, &w.root), "key {k:#x}");
    }
    // A leaf with a non-zero value under a vote-id key (a stored ballot).
    let mut tree = Tree::new(MemoryStorage::new(), 64, Sha256).unwrap();
    tree.add(&vid_key(VID), &[7u8; 32]).unwrap();
    tree.add(&vid_key(VID ^ 0xff), &[0u8; 32]).unwrap();
    let root = tree.root();
    let p = tree.gen_proof(&vid_key(VID)).unwrap();
    let tp = TrackerProof {
        process_id: PID,
        vote_id: VID,
        root,
        siblings: p.siblings,
    };
    assert!(!api::verify_tracker(&tp, &root));
}

// Byte-order swaps: a tree keyed or hashed in another order never verifies.
#[test]
fn tracker_rejects_byte_order_swaps() {
    for v in [VID, VID ^ 0x1234_5678_9abc, (1 << 63) | 1, u64::MAX] {
        // Tree keyed with the big-endian vote id.
        let mut tree = Tree::new(MemoryStorage::new(), 64, Sha256).unwrap();
        for k in [v, v ^ 0x0100, v ^ 0x8000_0000_0000_0001u64 | (1 << 63)] {
            let _ = tree.add(&k.to_be_bytes(), &[0u8; 32]);
        }
        let root = tree.root();
        let p = tree.gen_proof(&v.to_be_bytes()).unwrap();
        assert!(p.exists);
        let tp = TrackerProof {
            process_id: PID,
            vote_id: v,
            root,
            siblings: p.siblings.clone(),
        };
        // A palindromic id is the same in both orders.
        let palindrome = v.to_be_bytes() == v.to_le_bytes();
        assert_eq!(
            api::verify_tracker(&tp, &root),
            palindrome,
            "BE keyed {v:x}"
        );
    }
    // Siblings with each byte order reversed.
    let w = world(13, 20);
    for v in &w.vids {
        let mut tp = proof_of(&w, *v);
        for s in tp.siblings.iter_mut() {
            s.reverse();
        }
        let ok = tp.siblings == proof_of(&w, *v).siblings;
        assert_eq!(api::verify_tracker(&tp, &w.root), ok);
        // Root reversed.
        let mut r = w.root;
        r.reverse();
        assert!(!api::verify_tracker(&proof_of(&w, *v), &r));
    }
}

#[test]
fn tracker_edge_roots_and_depths() {
    // A tree with a single vote: the leaf is the root, no siblings.
    let mut tree = Tree::new(MemoryStorage::new(), 64, Sha256).unwrap();
    tree.add(&vid_key(VID), &[0u8; 32]).unwrap();
    let root = tree.root();
    let tp = TrackerProof {
        process_id: PID,
        vote_id: VID,
        root,
        siblings: vec![],
    };
    assert!(api::verify_tracker(&tp, &root));
    // Any other vote id fails under the same root.
    let t2 = TrackerProof {
        vote_id: VID + 1,
        ..tp.clone()
    };
    assert!(!api::verify_tracker(&t2, &root));
    // A 65 and a 1000 level proof are refused without work.
    for n in [65usize, 1000, 100_000] {
        let t3 = TrackerProof {
            siblings: vec![[0u8; 32]; n],
            ..tp.clone()
        };
        assert!(!api::verify_tracker(&t3, &root));
    }
    // Vote ids below the range, including 0 and 2^63 - 1.
    for v in [0u64, 1, (1 << 63) - 1] {
        let t4 = TrackerProof {
            vote_id: v,
            ..tp.clone()
        };
        assert!(!api::verify_tracker(&t4, &root));
    }
    // The empty root.
    let t5 = TrackerProof {
        root: [0; 32],
        ..tp
    };
    assert!(!api::verify_tracker(&t5, &[0; 32]));
}

// ---------------------------------------------------------------------------
// Voter: refuse locally what the circuit would reject

#[derive(Clone, Copy)]
struct M {
    nf: u8,
    max: u64,
    min: u64,
    max_sum: u64,
    min_sum: u64,
    exp: u8,
    unique: bool,
}

const BASE: M = M {
    nf: 4,
    max: 7,
    min: 0,
    max_sum: 0,
    min_sum: 0,
    exp: 1,
    unique: false,
};

/// `Ok` when the ballot is accepted by `prepare_vote`.
fn try_prepare(m: M, fields: &[u64], weight: u128) -> Result<(), Error> {
    let mut rng = StdRng::seed_from_u64(77);
    let vs = voters(2, &mut rng);
    // The census holds the leaf at the tested weight; `weight` above 2^88
    // cannot be a leaf, so the census is built with a legal one and the call
    // gets the oversized weight.
    let leaf_w = weight.min((1u128 << 88) - 1);
    let census = merkle_census(&vs, leaf_w);
    let mut e = election(m.nf.clamp(1, 16), 1, census.root(), &mut rng);
    e.chain.ballot_mode = BallotMode {
        num_fields: m.nf,
        group_size: 1.min(m.nf),
        unique_values: m.unique,
        cost_exponent: m.exp,
        max_value: m.max,
        min_value: m.min,
        max_value_sum: m.max_sum,
        min_value_sum: m.min_sum,
    };
    vs[0]
        .prepare_vote(
            &e.chain,
            fields,
            CensusWitness::Merkle(census.proof(0).unwrap()),
            weight,
            Fr::from(5u64),
        )
        .map(|_| ())
}

#[track_caller]
fn accepts(m: M, fields: &[u64], w: u128) {
    if let Err(e) = try_prepare(m, fields, w) {
        panic!("refused a valid ballot {fields:?} w={w}: {e}");
    }
}

#[track_caller]
fn refuses(m: M, fields: &[u64], w: u128) {
    match try_prepare(m, fields, w) {
        Err(Error::Invalid(msg)) => assert!(!msg.is_empty()),
        Err(e) => panic!("{fields:?} w={w}: wrong error kind {e:?}"),
        Ok(()) => panic!("accepted {fields:?} w={w} under a mode the circuit rejects"),
    }
}

// BUG: prepare_vote leaves every range, sum, uniqueness and cost check to the
// circuit (found at proving time, if at all).
#[test]
fn choices_must_fit_the_ballot_mode() {
    // Plain.
    accepts(BASE, &[1, 2, 3, 4], 100);
    accepts(BASE, &[], 100);
    accepts(BASE, &[7, 7, 7, 7], 28);
    refuses(BASE, &[7, 7, 7, 7], 27);
    refuses(BASE, &[8], 100);
    refuses(BASE, &[0, 0, 0, u64::MAX], 100);
    refuses(BASE, &[1, 2, 3, 4, 5], 100);

    // Lower bound: missing values are zeros the circuit checks too.
    let lo = M { min: 2, ..BASE };
    accepts(lo, &[2, 2, 2, 2], 100);
    refuses(lo, &[2, 2, 2], 100);
    refuses(lo, &[1, 2, 2, 2], 100);
    refuses(lo, &[], 100);

    // Sum bounds. A non-zero max_value_sum replaces the weight bound.
    let ms = M { max_sum: 5, ..BASE };
    accepts(ms, &[2, 2, 1], 1);
    refuses(ms, &[2, 2, 2], 1000);
    let mn = M { min_sum: 3, ..BASE };
    accepts(mn, &[1, 1, 1], 100);
    refuses(mn, &[1, 1], 100);
    refuses(mn, &[], 100);

    // Weight bounds the sum when max_value_sum is zero.
    accepts(BASE, &[2, 2, 1], 5);
    refuses(BASE, &[2, 2, 2], 5);
    accepts(BASE, &[0], 0);
    refuses(BASE, &[1], 0);

    // Uniqueness counts the implied zeros of a short ballot, not the padding
    // beyond num_fields.
    let u = M {
        unique: true,
        ..BASE
    };
    accepts(u, &[1, 2, 3, 4], 100);
    accepts(u, &[1, 2, 3], 100);
    accepts(u, &[0, 1, 2, 3], 100);
    refuses(u, &[0, 1, 2], 100);
    refuses(u, &[1, 2, 3, 1], 100);
    refuses(u, &[1, 0], 100);
    refuses(u, &[], 100);
    refuses(u, &[0], 100);
    let u16 = M {
        unique: true,
        nf: 16,
        max: 15,
        ..BASE
    };
    accepts(u16, &(0..16).collect::<Vec<_>>(), 1000);
    refuses(u16, &(0..16).map(|i| i % 15).collect::<Vec<_>>(), 1000);

    // Cost exponent: sum of f^e.
    let sq = M { exp: 2, ..BASE };
    accepts(sq, &[3, 0, 0, 0], 10);
    refuses(sq, &[3, 3, 0, 0], 10);
    accepts(sq, &[1, 1, 1, 1], 4);
    refuses(sq, &[1, 1, 1, 1], 3);
    // Exponent 0: every field counts as 1, zeros too.
    let e0 = M { exp: 0, ..BASE };
    accepts(e0, &[7, 7, 7, 7], 4);
    refuses(e0, &[0, 0, 0, 0], 3);
    // The largest exponent overflows any u64 or u128 power: no panic, no
    // wrap-around acceptance.
    let big = M { exp: 255, ..BASE };
    refuses(big, &[7, 0, 0, 0], u64::MAX as u128);
    refuses(big, &[2, 0, 0, 0], u64::MAX as u128);
    accepts(big, &[1, 1, 1, 1], 4);
    accepts(big, &[0, 0, 0, 0], 1);
    let wide = M {
        exp: 255,
        max: (1 << 48) - 1,
        ..BASE
    };
    refuses(wide, &[(1 << 48) - 1, 0, 0, 0], 1 << 80);

    // Value limits of the mode encoding.
    let top = M {
        max: (1 << 48) - 1,
        ..BASE
    };
    accepts(top, &[(1 << 48) - 1], 1 << 48);
    refuses(top, &[1 << 48], 1 << 60);
    accepts(
        M {
            max_sum: (1 << 63) - 1,
            ..top
        },
        &[(1 << 48) - 1; 4],
        1,
    );
}

#[test]
fn field_counts_and_weights_are_bounded() {
    accepts(M { nf: 16, ..BASE }, &[1; 16], 100);
    refuses(M { nf: 16, ..BASE }, &[1; 17], 100);
    refuses(M { nf: 17, ..BASE }, &[1], 100);
    refuses(M { nf: 0, ..BASE }, &[], 100);
    // Weights outside 88 bits have no census leaf.
    for w in [1u128 << 88, u128::MAX] {
        assert!(try_prepare(BASE, &[1], w).is_err(), "weight {w:#x}");
    }
    // With max_value_sum zero the circuit compares the cost with the weight
    // in 89 bits: every weight below 2^88 can vote.
    accepts(BASE, &[1], 1 << 63);
    accepts(BASE, &[1], (1 << 63) + 1);
    accepts(BASE, &[1], (1 << 88) - 1);
    accepts(M { max_sum: 9, ..BASE }, &[1], (1 << 88) - 1);
    // A min above max leaves no valid ballot.
    refuses(
        M {
            min: 5,
            max: 3,
            ..BASE
        },
        &[],
        100,
    );
}

// Packing refuses every out-of-range mode instead of overlapping bits.
#[test]
fn ballot_mode_limits() {
    let m = BallotMode {
        num_fields: 16,
        group_size: 16,
        unique_values: true,
        cost_exponent: 255,
        max_value: (1 << 48) - 1,
        min_value: (1 << 48) - 1,
        max_value_sum: (1 << 63) - 1,
        min_value_sum: (1 << 63) - 1,
    };
    m.pack().unwrap();
    assert!(
        BallotMode {
            group_size: 17,
            ..m
        }
        .pack()
        .is_err()
    );
    assert!(
        BallotMode {
            max_value: 1 << 48,
            ..m
        }
        .pack()
        .is_err()
    );
    assert!(
        BallotMode {
            min_value: 1 << 48,
            ..m
        }
        .pack()
        .is_err()
    );
    assert!(
        BallotMode {
            max_value_sum: 1 << 63,
            ..m
        }
        .pack()
        .is_err()
    );
    assert!(
        BallotMode {
            min_value_sum: 1 << 63,
            ..m
        }
        .pack()
        .is_err()
    );
    // Round trip at the limits.
    assert_eq!(BallotMode::unpack(&m.pack().unwrap()).unwrap(), m);
}

// ---------------------------------------------------------------------------
// Organizer census helpers

fn addr(i: u32) -> [u8; 20] {
    let mut a = [0u8; 20];
    a[16..].copy_from_slice(&(i + 1).to_be_bytes());
    a
}

// BUG: a census with no members, a repeated address or a slot collision
// builds a root the node then refuses to serve.
#[test]
fn census_builder_refuses_what_the_node_refuses() {
    // Honest.
    let parts: Vec<_> = (0..5).map(|i| (addr(i), 1 + i as u128)).collect();
    org_census(&census_file(&parts)).unwrap();

    // Empty.
    assert!(org_census(&census_file(&[])).is_err(), "empty census");
    // Same address twice, same or different weight.
    let mut dup = parts.clone();
    dup.push((addr(2), 1));
    assert!(
        org_census(&census_file(&dup)).is_err(),
        "duplicate, same weight"
    );
    let mut dup = parts.clone();
    dup.push((addr(2), 99));
    assert!(
        org_census(&census_file(&dup)).is_err(),
        "duplicate, other weight"
    );
    // Weight above 88 bits.
    let mut big = parts.clone();
    big.push((addr(9), 1 << 88));
    assert!(org_census(&census_file(&big)).is_err());
    // The largest weight is fine.
    let mut max = parts.clone();
    max.push((addr(9), (1 << 88) - 1));
    org_census(&census_file(&max)).unwrap();
    // Zero weights are leaves (the voter just cannot cast anything).
    let mut zero = parts.clone();
    zero.push((addr(9), 0));
    org_census(&census_file(&zero)).unwrap();
}

#[test]
fn census_root_is_order_sensitive_and_matches_the_sdk() {
    let parts: Vec<_> = (0..9).map(|i| (addr(i), 3)).collect();
    let t = org_census(&census_file(&parts)).unwrap();
    let mut by_hand = LeanImt::new();
    for (a, w) in &parts {
        by_hand.insert(census_leaf(a, *w).unwrap());
    }
    assert_eq!(t.root(), by_hand.root());
    let mut rev = parts.clone();
    rev.reverse();
    assert_ne!(org_census(&census_file(&rev)).unwrap().root(), t.root());
    // Every member has a proof that verifies under the root.
    for i in 0..parts.len() {
        let p = t.proof(i).unwrap();
        assert!(davinci_zkvm_sdk::census::verify_census_proof(&p));
        assert_eq!(p.root, t.root());
    }
    assert!(t.proof(parts.len()).is_err());
}

// ---------------------------------------------------------------------------
// Presets

#[test]
fn network_presets_are_consistent() {
    assert!(!NETWORKS.is_empty());
    for n in NETWORKS {
        assert!(n.chain_id > 0, "{}", n.name);
        assert!(!n.registry.is_zero(), "{}", n.name);
        assert!(n.start_block > 0, "{}", n.name);
        assert!(n.confirmations >= 1, "{}", n.name);
        assert!(!n.rpc_urls.is_empty());
        let mut all: Vec<&str> = n.rpc_urls.to_vec();
        match n.da {
            NetworkDa::L1Blobs {
                l1_chain_id,
                l1_block,
                l1_rpc_urls,
            } => {
                assert_ne!(l1_chain_id, n.chain_id);
                assert!(l1_chain_id > 0);
                assert_eq!(l1_block, OP_L1_BLOCK);
                assert!(!l1_rpc_urls.is_empty());
                all.extend(l1_rpc_urls);
                assert!(!n.beacon_urls().is_empty(), "{}", n.name);
            }
            NetworkDa::Blobs => {}
        }
        all.extend(n.beacon_urls());
        let mut seen = std::collections::HashSet::new();
        for u in all {
            // Parses as an https URL with no credentials, no query and no
            // path segment that looks like an API key.
            let url: reqwest::Url = u.parse().unwrap_or_else(|e| panic!("{u}: {e}"));
            assert_eq!(url.scheme(), "https", "{u}");
            assert!(url.username().is_empty() && url.password().is_none(), "{u}");
            assert!(url.query().is_none() && url.fragment().is_none(), "{u}");
            assert!(url.host_str().is_some_and(|h| h.contains('.')), "{u}");
            for seg in url.path_segments().into_iter().flatten() {
                assert!(seg.len() < 20, "{u}: path segment {seg:?} looks like a key");
            }
            assert!(seen.insert(u), "{u} listed twice");
        }
        assert_eq!(networks::by_name(n.name), Some(n));
        assert_eq!(networks::by_name(&n.name.to_uppercase()), Some(n));
    }
    // Chain ids are unique across presets.
    let mut ids: Vec<u64> = NETWORKS.iter().map(|n| n.chain_id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), NETWORKS.len());
    assert!(networks::by_name("").is_none());
    assert!(networks::by_name("optimism\0").is_none());
}

// ---------------------------------------------------------------------------
// Secrets in Debug and errors

#[tokio::test]
async fn urls_with_credentials_do_not_leak_in_errors_or_debug() {
    // Closed port: connection refused.
    let c = SequencerClient::new("http://user:hunter2@127.0.0.1:1/");
    let e = c.info().await.unwrap_err();
    let s = format!("{e} {e:?}");
    assert!(!s.contains("hunter2"), "error leaks the URL password: {s}");
    // BUG candidate: Debug of the client prints its base URL.
    assert!(
        !format!("{c:?}").contains("hunter2"),
        "client Debug leaks the URL password"
    );
}

#[test]
fn voter_key_never_prints() {
    let mut rng = StdRng::seed_from_u64(9);
    let v = davinci_client::voter::Voter::random(&mut rng);
    let sk = hex::encode(v.key.to_bytes());
    for s in [format!("{v:?}"), format!("{v:#?}")] {
        assert!(!s.contains(&sk));
    }
    // Its address is what prints.
    assert!(format!("{v:?}").contains(&hex::encode(eth_address(v.key.verifying_key()))));
}

// ---------------------------------------------------------------------------
// The local ballot check agrees with the real circuit (needs CIRCOM_ARTIFACTS)

// Every case: the client refuses exactly when the circuit has no proof.
// The circuit compares the cost with the budget in 89 bits, so with
// max_value_sum 0 every weight below 2^88 can spend up to its weight.
#[test]
fn local_ballot_check_matches_the_circuit() {
    use davinci_client::prover::BallotProver;
    let Some(dir) = std::env::var_os("CIRCOM_ARTIFACTS").map(std::path::PathBuf::from) else {
        eprintln!("CIRCOM_ARTIFACTS not set, skipping");
        return;
    };
    let prover = BallotProver::load(
        &dir.join("ballot_proof.wasm"),
        &dir.join("ballot_proof_pkey.zkey"),
    )
    .unwrap();
    let mut rng = StdRng::seed_from_u64(31);
    let vs = voters(2, &mut rng);
    type Case = (u128, M, Vec<u64>);
    let cases: Vec<Case> = vec![
        (10, BASE, vec![1]),
        (10, BASE, vec![7, 3]),
        (10, BASE, vec![7, 4]),
        (10, BASE, vec![8]),
        (0, BASE, vec![]),
        (0, BASE, vec![1]),
        ((1 << 63) - 1, BASE, vec![1]),
        (1 << 63, BASE, vec![1]),
        ((1 << 63) + 1, BASE, vec![1]),
        ((1 << 63) + 4, BASE, vec![1, 1, 1, 1]),
        ((1 << 63) + 5, BASE, vec![1, 1, 1, 1]),
        (1 << 80, BASE, vec![1]),
        ((1 << 88) - 1, BASE, vec![1]),
        (1 << 80, M { max_sum: 5, ..BASE }, vec![1]),
        (
            1,
            M {
                max_sum: 1000,
                ..BASE
            },
            vec![7, 7, 7, 7],
        ),
        (100, M { min_sum: 3, ..BASE }, vec![1, 1]),
        (100, M { min_sum: 3, ..BASE }, vec![1, 1, 1]),
        (100, M { min: 2, ..BASE }, vec![2, 2, 2]),
        (100, M { min: 2, ..BASE }, vec![2, 2, 2, 2]),
        (
            100,
            M {
                unique: true,
                ..BASE
            },
            vec![1, 2, 3],
        ),
        (
            100,
            M {
                unique: true,
                ..BASE
            },
            vec![1, 2, 3, 1],
        ),
        (
            100,
            M {
                unique: true,
                ..BASE
            },
            vec![1, 0],
        ),
        (10, M { exp: 2, ..BASE }, vec![3, 0]),
        (10, M { exp: 2, ..BASE }, vec![3, 1]),
        (4, M { exp: 0, ..BASE }, vec![7, 7, 7, 7]),
        (3, M { exp: 0, ..BASE }, vec![0]),
        (10, M { exp: 255, ..BASE }, vec![1, 1, 1, 1]),
        (10, M { exp: 255, ..BASE }, vec![2]),
    ];
    for (w, m, fields) in cases {
        let census = merkle_census(&vs, w);
        let mut e = election(m.nf, 1, census.root(), &mut rng);
        e.chain.ballot_mode = BallotMode {
            num_fields: m.nf,
            group_size: 1,
            unique_values: m.unique,
            cost_exponent: m.exp,
            max_value: m.max,
            min_value: m.min,
            max_value_sum: m.max_sum,
            min_value_sum: m.min_sum,
        };
        let local = davinci_client::voter::check_ballot(&e.chain.ballot_mode, &fields, w);
        let circuit = circuit_accepts(&prover, &vs[0], &e.chain, &fields, w);
        assert_eq!(
            local.is_ok(),
            circuit,
            "w={w} mode={:?} fields={fields:?}: local {local:?}, circuit {circuit}",
            e.chain.ballot_mode
        );
    }
}

// Proves `fields` without the local check, as the circuit alone would.
fn circuit_accepts(
    prover: &davinci_client::prover::BallotProver,
    v: &davinci_client::voter::Voter,
    chain: &davinci_client::organizer::OnchainProcess,
    fields: &[u64],
    w: u128,
) -> bool {
    use davinci_zkvm_sdk::ballot::{address_to_fr, encrypt_ballot, vote_id};
    let mode = &chain.ballot_mode;
    let nf = mode.num_fields;
    let pk = chain.encryption_key;
    let k = Fr::from(5u64);
    let addr = address_to_fr(&v.address());
    let pid = ProcessId(chain.id).to_fr();
    let vid = vote_id(&pid, &addr, &k);
    let ballot = encrypt_ballot(&pk, fields, &k, nf);
    let wf = Fr::from(w);
    let mut padded = [Fr::from(0u64); 16];
    for (o, x) in padded.iter_mut().zip(fields) {
        *o = Fr::from(*x);
    }
    let inputs = davinci_client::voter::CircomInputs {
        fields: padded,
        packed_ballot_mode: mode.pack().unwrap(),
        address: addr,
        weight: wf,
        process_id: pid,
        vote_id: Fr::from(vid),
        encryption_pubkey: [pk.x, pk.y],
        k,
        cipherfields: ballot.coords(),
    };
    prover.prove(&inputs).is_ok()
}

// ---------------------------------------------------------------------------
// Organizer against a real registry on anvil (ANVIL=1)

mod chain {
    use super::*;
    use alloy::network::{EthereumWallet, TransactionBuilder};
    use alloy::node_bindings::Anvil;
    use alloy::primitives::{Address, B256, Bytes};
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::rpc::types::TransactionRequest;
    use alloy::signers::local::PrivateKeySigner;
    use alloy::sol_types::SolValue;
    use axum::Router;
    use axum::http::StatusCode;
    use davinci_client::organizer::{KeyMode, NewProcess, Organizer, metadata_hash};
    use davinci_zkvm_sdk::crypto::babyjubjub::Point;
    use davinci_zkvm_sdk::crypto::field::U256;
    use davinci_zkvm_sdk::groth16::BallotVerifier;
    use davinci_zkvm_sdk::release;
    use tokio::net::TcpListener;

    fn bytecode() -> Vec<u8> {
        let dir = std::env::var_os("DAVINCI_CONTRACTS_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-contracts")
            });
        let p = dir.join("out/ProcessRegistry.sol/ProcessRegistry.json");
        let j: serde_json::Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        let h = j["bytecode"]["object"].as_str().unwrap();
        hex::decode(h.trim_start_matches("0x")).unwrap()
    }

    fn anvil_bin() -> std::path::PathBuf {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let p = home.join(".foundry/bin/anvil");
        if p.exists() { p } else { "anvil".into() }
    }

    const DOC: &[u8] = br#"{"title":"attack"}"#;

    fn valid(next: [u8; 31], key: Point) -> NewProcess {
        NewProcess {
            process_id: next,
            start_time: 0,
            duration: 3600,
            max_voters: 24,
            ballot_mode: BallotMode {
                num_fields: 4,
                group_size: 1,
                unique_values: false,
                cost_exponent: 1,
                max_value: 5,
                min_value: 0,
                max_value_sum: 0,
                min_value_sum: 0,
            },
            census_origin: 1,
            census_root: [0xc5; 32],
            census_contract: [0; 20],
            census_uri: "file:///tmp/census.json".into(),
            metadata: "ipfs://metadata".into(),
            metadata_hash: metadata_hash(DOC),
            key_mode: KeyMode::Sequencer(key),
        }
    }

    async fn deploy(url: &str, signer: &PrivateKeySigner) -> anyhow::Result<Address> {
        let vk_hash = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?.vk_hash();
        let args = (
            31337u32,
            Address::repeat_byte(0x11),
            B256::from(release::BATCH_PROGRAM_VK),
            B256::from(release::RESULTS_PROGRAM_VK),
            B256::from(release::ROOT_C_VADCOP_FINAL),
            B256::from(vk_hash),
            Address::ZERO,
            Address::ZERO,
        )
            .abi_encode_params();
        let mut code = bytecode();
        code.extend_from_slice(&args);
        code.extend_from_slice(&(10u32, 2u32, 60u32, 60u32, 5u32).abi_encode_params());
        code.extend_from_slice(&Address::ZERO.abi_encode());
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer.clone()))
            .connect_http(url.parse()?);
        let registry = provider
            .send_transaction(TransactionRequest::default().with_deploy_code(Bytes::from(code)))
            .await?
            .get_receipt()
            .await?
            .contract_address
            .expect("deployed");

        Ok(registry)
    }

    #[tokio::test]
    async fn invalid_configurations_never_reach_the_chain() -> anyhow::Result<()> {
        if std::env::var("ANVIL").as_deref() != Ok("1") {
            eprintln!("ANVIL not set, skipping");
            return Ok(());
        }
        let anvil = Anvil::at(anvil_bin()).try_spawn()?;
        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let url = anvil.endpoint();
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer.clone()))
            .connect_http(url.parse()?);
        let registry = deploy(&url, &signer).await?;

        let org = Organizer::connect(&url, signer.clone(), registry)?;
        let key = Point::generator().mul(&U256::from(4242u64));
        let nonce = || async { provider.get_transaction_count(signer.address()).await };
        let n0 = nonce().await?;

        type Mut = Box<dyn Fn(&mut NewProcess)>;
        let neg1 = -Fr::from(1u64);
        let cases: Vec<(&str, Mut)> = vec![
            ("0 fields", Box::new(|p| p.ballot_mode.num_fields = 0)),
            ("17 fields", Box::new(|p| p.ballot_mode.num_fields = 17)),
            ("255 fields", Box::new(|p| p.ballot_mode.num_fields = 255)),
            ("group > fields", Box::new(|p| p.ballot_mode.group_size = 5)),
            (
                "min > max",
                Box::new(|p| {
                    p.ballot_mode.min_value = 6;
                }),
            ),
            (
                "min sum > max sum",
                Box::new(|p| {
                    p.ballot_mode.max_value_sum = 3;
                    p.ballot_mode.min_value_sum = 4;
                }),
            ),
            (
                "max value over 48 bits",
                Box::new(|p| p.ballot_mode.max_value = 1 << 48),
            ),
            (
                "max sum over 63 bits",
                Box::new(|p| p.ballot_mode.max_value_sum = 1 << 63),
            ),
            (
                "cost exponent 0",
                Box::new(|p| p.ballot_mode.cost_exponent = 0),
            ),
            (
                "cost 2^63",
                Box::new(|p| {
                    p.ballot_mode.num_fields = 2;
                    p.ballot_mode.max_value = 1 << 31;
                    p.ballot_mode.cost_exponent = 2;
                }),
            ),
            (
                "cost overflow",
                Box::new(|p| p.ballot_mode.cost_exponent = 255),
            ),
            ("no voters", Box::new(|p| p.max_voters = 0)),
            (
                "voters x value overflow",
                Box::new(|p| {
                    p.max_voters = u64::MAX;
                    p.ballot_mode.max_value = (1 << 48) - 1;
                }),
            ),
            ("origin 0", Box::new(|p| p.census_origin = 0)),
            ("origin 5", Box::new(|p| p.census_origin = 5)),
            ("origin 255", Box::new(|p| p.census_origin = 255)),
            ("zero root", Box::new(|p| p.census_root = [0; 32])),
            (
                "csp root over 160 bits",
                Box::new(move |p| {
                    p.census_origin = 4;
                    p.census_root = [0xff; 32];
                }),
            ),
            (
                "contract on a merkle census",
                Box::new(|p| p.census_contract = [1; 20]),
            ),
            (
                "origin 3 without a contract",
                Box::new(|p| p.census_origin = 3),
            ),
            (
                "origin 3 with an EOA",
                Box::new(|p| {
                    p.census_origin = 3;
                    p.census_contract = [9; 20];
                }),
            ),
            ("empty census uri", Box::new(|p| p.census_uri.clear())),
            ("empty metadata", Box::new(|p| p.metadata.clear())),
            (
                "zero metadata hash",
                Box::new(|p| p.metadata_hash = [0; 32]),
            ),
            ("zero duration", Box::new(|p| p.duration = 0)),
            ("start in the past", Box::new(|p| p.start_time = 1)),
            (
                "ends in the past",
                Box::new(|p| {
                    p.start_time = 0;
                    p.duration = 0;
                }),
            ),
            (
                "identity key",
                Box::new(|p| p.key_mode = KeyMode::Sequencer(Point::IDENTITY)),
            ),
            (
                "order 2 key",
                Box::new(move |p| {
                    p.key_mode = KeyMode::Sequencer(Point {
                        x: Fr::from(0u64),
                        y: neg1,
                    })
                }),
            ),
            (
                "off-curve key",
                Box::new(|p| {
                    p.key_mode = KeyMode::Sequencer(Point {
                        x: Fr::from(1u64),
                        y: Fr::from(2u64),
                    })
                }),
            ),
            (
                "dkg without a manager",
                Box::new(|p| p.key_mode = KeyMode::DkgAutomatic),
            ),
            (
                "locked dkg without a manager",
                Box::new(|p| p.key_mode = KeyMode::DkgLocked),
            ),
            (
                "council without a manager",
                Box::new(|p| p.key_mode = KeyMode::Council([7; 12])),
            ),
            (
                "council id zero",
                Box::new(|p| p.key_mode = KeyMode::Council([0; 12])),
            ),
        ];
        let mut sent = vec![];
        for (name, f) in &cases {
            let mut p = valid(org.next_process_id().await?, key);
            f(&mut p);
            let before = nonce().await?;
            let r = org.create_process(&p).await;
            if r.is_ok() || nonce().await? != before {
                sent.push(*name);
            }
        }
        assert!(
            sent.is_empty(),
            "configurations that reached the chain: {sent:?}"
        );
        assert_eq!(nonce().await?, n0);
        assert!(org.receipts().is_empty());

        // The valid one goes through, and a second try with the same id is
        // refused locally.
        let p = valid(org.next_process_id().await?, key);
        org.create_process(&p).await?;
        let e = org.create_process(&p).await.unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{e}");

        // Bounds that must be accepted: the widest legal modes under the
        // registry's cost rule (16 * maxValue^costExponent below 2^63).
        for (cost_exponent, max_value) in [(255, 1), (8, 100)] {
            let mut wide = valid(org.next_process_id().await?, key);
            wide.ballot_mode = BallotMode {
                num_fields: 16,
                group_size: 16,
                unique_values: true,
                cost_exponent,
                max_value,
                min_value: 0,
                max_value_sum: (1 << 63) - 1,
                min_value_sum: 0,
            };
            wide.max_voters = 1000;
            wide.duration = u64::MAX / 2;
            org.create_process(&wide).await?;
        }
        Ok(())
    }

    // A rival newProcess from the same account lands between the id check
    // and the send: the process is created for the wrong id, which the
    // organizer must report so it can be cancelled.
    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_process_id_race_is_reported_and_cancellable() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        if std::env::var("ANVIL").as_deref() != Ok("1") {
            eprintln!("ANVIL not set, skipping");
            return Ok(());
        }
        let anvil = Anvil::at(anvil_bin()).try_spawn()?;
        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let url = anvil.endpoint();
        let registry = deploy(&url, &signer).await?;
        let key = Point::generator().mul(&U256::from(4242u64));
        let rival = Arc::new(Organizer::connect(&url, signer.clone(), registry)?);

        let fired = Arc::new(AtomicBool::new(false));
        let (up, f, r) = (url.clone(), fired.clone(), rival.clone());
        let app = Router::new().route(
            "/",
            axum::routing::post(move |body: axum::body::Bytes| async move {
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                if req["method"] == "eth_estimateGas" && !f.swap(true, SeqCst) {
                    let p = valid(r.next_process_id().await.unwrap(), key);
                    r.create_process(&p).await.unwrap();
                }
                let resp = reqwest::Client::new()
                    .post(&up)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                (StatusCode::OK, resp.bytes().await.unwrap().to_vec())
            }),
        );
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let proxy = format!("http://{}/", l.local_addr()?);
        tokio::spawn(async move { axum::serve(l, app).await });

        let org = Organizer::connect(&proxy, signer, registry)?
            .with_receipt_timeout(Duration::from_secs(30));
        let expected = org.next_process_id().await?;
        let e = org.create_process(&valid(expected, key)).await.unwrap_err();
        let Error::WrongProcessId {
            created,
            expected: want,
        } = e
        else {
            panic!("want WrongProcessId, got {e}");
        };
        assert_eq!(want, ProcessId(expected));
        assert_ne!(created, want);
        // Both the rival's and the misplaced process are known and cancelled.
        assert!(org.created().contains(&created.0));
        org.cancel_process(&created.0).await?;
        assert_eq!(
            org.process(&created.0).await?.status,
            davinci_client::api::ProcessStatus::Canceled
        );
        Ok(())
    }
}

#[tokio::test]
async fn odd_urls_and_ids_error_without_panicking() {
    for url in [
        "",
        "not a url",
        "ftp://x",
        "http://",
        "http://[::1",
        "http://\u{0}/",
    ] {
        let c = SequencerClient::new(url);
        assert!(c.info().await.is_err(), "{url:?}");
    }
    for s in [
        "",
        "0x",
        "0x0x00",
        " 0x00",
        "0X00",
        "é",
        &"0".repeat(10_000),
        &"ff".repeat(32),
    ] {
        assert!(s.parse::<ProcessId>().is_err(), "{s:?}");
        assert!(api::parse_vote_id(s).is_err(), "{s:?}");
    }
    let c = hostile(|_| json_ok(r#"{"status":"settled\u0000"}"#)).await;
    assert!(c.vote_status(&PID.0, VID).await.is_err());
}
