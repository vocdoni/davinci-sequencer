//! SequencerClient against an axum mock of the sequencer API.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use davinci_client::api::{
    BallotResponse, BlobsResponse, CensusProofWire, CensusView, EncryptionKeyRequest,
    EncryptionKeyResponse, ErrorResponse, Info, MerkleProof, ParticipantResponse, ProcessId,
    ProcessList, ProcessStatus, ProcessView, TrackerProof, TransitionList, TransitionView,
    VoteRequest, VoteResponse, VoteStatus, VoteStatusResponse, vote_id_hex,
};
use davinci_client::{Error, SequencerClient};
use davinci_zkvm_sdk::ballot::{Ballot, BallotMode};
use davinci_zkvm_sdk::census::{LeanImt, census_leaf};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::{Fr, U256};
use davinci_zkvm_sdk::types::SnarkJsProof;
use serde_json::json;

const PID: ProcessId = ProcessId([0x5a; 31]);
const VID: u64 = 0x8000_0000_0000_beef;
const ADDR: [u8; 20] = [0x33; 20];

fn pk() -> Point {
    Point::generator().mul(&U256::from(99u64))
}

fn census() -> LeanImt {
    let mut t = LeanImt::new();
    for i in 0..4u8 {
        t.insert(census_leaf(&[0x30 + i; 20], 5).unwrap());
    }
    t
}

fn view() -> ProcessView {
    ProcessView {
        id: PID,
        status: ProcessStatus::Ready,
        is_accepting_votes: true,
        organization_id: [1; 20],
        encryption_key: pk(),
        ballot_mode: BallotMode {
            num_fields: 2,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 3,
            min_value: 0,
            max_value_sum: 0,
            min_value_sum: 0,
        },
        census: CensusView {
            census_origin: 1,
            census_root: census().root(),
            census_uri: "file:///x".into(),
        },
        state_root: [2; 32],
        local_state_root: None,
        synced: false,
        pending_votes: None,
        next_seal_not_before: None,
        voters_count: 0,
        overwritten_votes_count: 0,
        max_voters: 10,
        start_time: 1,
        duration: 2,
        result: None,
        ignored: false,
        note: None,
    }
}

fn vote(weight: u128) -> VoteRequest {
    VoteRequest {
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
        ballot_inputs_hash: Fr::from(1u64),
        signature: [0; 65],
        weight,
        census_proof: Some(CensusProofWire::Merkle(MerkleProof::from(
            &census().proof(3).unwrap(),
        ))),
    }
}

fn err(status: StatusCode, code: u32, msg: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: msg.into(),
            code,
        }),
    )
        .into_response()
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

impl Seen {
    fn push(&self, s: String) {
        self.0.lock().unwrap().push(s);
    }
}

fn router(seen: Seen, evil: bool) -> Router {
    Router::new()
        .route("/ping", get(|| async { StatusCode::OK }))
        .route(
            "/info",
            get(|| async {
                Json(Info {
                    sequencer_address: Some([7; 20]),
                    chain_id: 31337,
                    process_registry: [8; 20],
                    ballot_vk_hash: [9; 32],
                    batch_program_vk: [10; 32],
                    results_program_vk: [11; 32],
                    observer: false,
                    settled_by_self: 1,
                    synced_from_others: 2,
                    lost_races: 1,
                })
            }),
        )
        .route(
            "/processes",
            get(|| async {
                Json(ProcessList {
                    processes: vec![PID],
                })
            }),
        )
        .route(
            "/processes/keys",
            post(
                move |State(s): State<Seen>, Json(r): Json<EncryptionKeyRequest>| async move {
                    s.push(format!("keys {}", r.process_id));
                    if evil {
                        // (0, -1): on the curve, order 2.
                        Json(json!({"x": "0", "y": neg_one()}))
                    } else {
                        Json(
                            serde_json::to_value(EncryptionKeyResponse::from_point(&pk())).unwrap(),
                        )
                    }
                },
            ),
        )
        .route(
            "/processes/:pid",
            get(
                |State(s): State<Seen>, Path(pid): Path<String>| async move {
                    s.push(format!("process {pid}"));
                    if pid == PID.to_string() {
                        Json(view()).into_response()
                    } else {
                        err(StatusCode::NOT_FOUND, 4004, "process not found")
                    }
                },
            ),
        )
        .route(
            "/processes/:pid/participants/:addr",
            get(
                move |State(s): State<Seen>, Path((pid, addr)): Path<(String, String)>| async move {
                    s.push(format!("participant {pid} {addr}"));
                    // Index 3 is ADDR's leaf; 2 is someone else's.
                    let idx = if evil { 2 } else { 3 };
                    Json(ParticipantResponse {
                        address: ADDR,
                        weight: 5,
                        census_proof: MerkleProof::from(&census().proof(idx).unwrap()),
                    })
                },
            ),
        )
        .route(
            "/processes/:pid/transitions",
            get(|| async {
                Json(TransitionList {
                    transitions: vec![TransitionView {
                        index: 0,
                        old_root: [1; 32],
                        new_root: [2; 32],
                        tx_hash: [3; 32],
                        block_number: 10,
                        sender: [4; 20],
                        voters: 5,
                        overwrites: 1,
                        n_blobs: 1,
                    }],
                })
            }),
        )
        .route(
            "/processes/:pid/transitions/:i/blobs",
            get(|Path((_, i)): Path<(String, u64)>| async move {
                Json(BlobsResponse {
                    blobs: vec![vec![i as u8; 4]],
                })
            }),
        )
        .route(
            "/votes",
            post(
                |State(s): State<Seen>, Json(v): Json<VoteRequest>| async move {
                    s.push(format!("vote {}", vote_id_hex(v.vote_id)));
                    match v.weight {
                        400 => err(StatusCode::BAD_REQUEST, 4000, "invalid proof"),
                        404 => err(StatusCode::NOT_FOUND, 4004, "process not found"),
                        409 => err(StatusCode::CONFLICT, 4009, "vote id already used"),
                        412 => err(StatusCode::PRECONDITION_FAILED, 4012, "not accepting"),
                        500 => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
                        _ => Json(VoteResponse { vote_id: v.vote_id }).into_response(),
                    }
                },
            ),
        )
        .route(
            "/votes/:pid/voteId/:vid",
            get(
                |State(s): State<Seen>, Path((pid, vid)): Path<(String, String)>| async move {
                    s.push(format!("status {pid} {vid}"));
                    Json(VoteStatusResponse {
                        status: VoteStatus::Aggregated,
                        error: None,
                    })
                },
            ),
        )
        .route(
            "/votes/:pid/voteId/:vid/proof",
            get(|Path((_, vid)): Path<(String, String)>| async move {
                Json(TrackerProof {
                    process_id: PID,
                    vote_id: davinci_client::api::parse_vote_id(&vid).unwrap(),
                    root: [6; 32],
                    siblings: vec![[1; 32], [0; 32]],
                })
            }),
        )
        .route(
            "/votes/:pid/address/:addr",
            get(|| async {
                Json(BallotResponse {
                    address: ADDR,
                    ballot: Ballot::identity(),
                })
            }),
        )
        .with_state(seen)
}

fn neg_one() -> String {
    davinci_zkvm_sdk::crypto::field::fr_to_dec(&-Fr::from(1u64))
}

async fn serve(evil: bool) -> (SequencerClient, Seen) {
    let seen = Seen::default();
    let app = router(seen.clone(), evil);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (SequencerClient::new(&url), seen)
}

fn status_of(e: &Error) -> Option<u16> {
    match e {
        Error::Api { status, .. } => Some(*status),
        _ => None,
    }
}

#[tokio::test]
async fn every_route() {
    let (c, seen) = serve(false).await;
    c.ping().await.unwrap();
    let info = c.info().await.unwrap();
    assert_eq!(info.chain_id, 31337);
    assert_eq!(info.sequencer_address, Some([7; 20]));
    assert_eq!(c.new_key(&PID.0).await.unwrap(), pk());
    assert_eq!(c.processes().await.unwrap(), vec![PID]);
    assert_eq!(c.process(&PID.0).await.unwrap(), view());

    let (proof, weight) = c.participant(&PID.0, &ADDR).await.unwrap();
    assert_eq!(weight, 5);
    assert_eq!(proof, census().proof(3).unwrap());

    c.submit_vote(&vote(5)).await.unwrap();
    assert_eq!(
        c.vote_status(&PID.0, VID).await.unwrap(),
        VoteStatus::Aggregated
    );
    let tp = c.vote_id_proof(&PID.0, VID).await.unwrap();
    assert_eq!(tp.vote_id, VID);
    assert_eq!(tp.siblings.len(), 2);
    assert_eq!(
        c.ballot(&PID.0, &ADDR).await.unwrap().ballot,
        Ballot::identity()
    );
    let ts = c.transitions(&PID.0).await.unwrap();
    assert_eq!(ts[0].n_blobs, 1);
    assert_eq!(
        c.transition_blobs(&PID.0, 3).await.unwrap(),
        vec![vec![3u8; 4]]
    );

    // Path parameters use the davinci-node encodings.
    let seen = seen.0.lock().unwrap().clone();
    let pid = PID.to_string();
    assert!(seen.contains(&format!("process {pid}")), "{seen:?}");
    assert!(seen.contains(&format!("participant {pid} 0x{}", "33".repeat(20))));
    assert!(seen.contains(&format!("status {pid} 0x800000000000beef")));
    assert!(seen.contains(&format!("vote {}", vote_id_hex(VID))));
    assert!(seen.contains(&format!("keys {pid}")), "{seen:?}");
}

#[tokio::test]
async fn errors_keep_the_status_and_code() {
    let (c, _) = serve(false).await;
    for s in [400u16, 404, 409, 412] {
        let e = c.submit_vote(&vote(s as u128)).await.unwrap_err();
        assert_eq!(status_of(&e), Some(s), "{e}");
        if let Error::Api { code, message, .. } = &e {
            assert_eq!(*code, Some(s as u32 + 3600));
            assert!(!message.is_empty());
        }
    }
    let e = c.submit_vote(&vote(500)).await.unwrap_err();
    assert_eq!(status_of(&e), Some(500));
    let e = c.process(&[0x11; 31]).await.unwrap_err();
    assert_eq!(status_of(&e), Some(404));
}

// A participant proof that is not the voter's own leaf is refused.
#[tokio::test]
async fn participant_proof_is_checked() {
    let (c, _) = serve(true).await;
    let e = c.participant(&PID.0, &ADDR).await.unwrap_err();
    assert!(matches!(e, Error::Decode(_)), "{e}");
}

// An election key outside the prime-order subgroup is refused.
#[tokio::test]
async fn new_key_is_checked() {
    let (c, _) = serve(true).await;
    let e = c.new_key(&PID.0).await.unwrap_err();
    assert!(matches!(e, Error::Decode(_)), "{e}");
}

#[tokio::test]
async fn unreachable_host_is_an_http_error() {
    let c = SequencerClient::new("http://127.0.0.1:1");
    assert!(matches!(c.ping().await, Err(Error::Http(_))));
}
