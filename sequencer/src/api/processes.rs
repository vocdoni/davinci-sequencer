//! Process routes: list, view, election keys, participants, the archive.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use davinci_client::api::{
    BlobsResponse, CensusView, EncryptionKeyRequest, EncryptionKeyResponse, MerkleProof,
    ParticipantResponse, ProcessId, ProcessList, ProcessStatus, ProcessView, TransitionList,
    TransitionView,
};
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be};

use super::{AppState, parse_addr};
use crate::api::error::ApiError;
use crate::storage::ProcessRecord;

/// The pid as the client's 31-byte id (the top byte of the BE32 form is 0).
pub(super) fn pid_wire(pid: &Fr) -> ProcessId {
    let be = fr_to_be(pid);
    let mut b = [0u8; 31];
    b.copy_from_slice(&be[1..]);
    ProcessId(b)
}

pub(super) fn parse_pid(s: &str) -> Result<Fr, ApiError> {
    s.parse::<ProcessId>()
        .map(|p| p.to_fr())
        .map_err(|e| ApiError::Invalid(e.to_string()))
}

pub async fn list(State(st): State<AppState>) -> Result<Json<ProcessList>, ApiError> {
    let processes = st
        .node
        .db
        .processes()?
        .iter()
        .map(|r| pid_wire(&r.pid))
        .collect();
    Ok(Json(ProcessList { processes }))
}

fn view(
    rec: &ProcessRecord,
    accepting: bool,
    local_state_root: Option<[u8; 32]>,
) -> Result<ProcessView, ApiError> {
    let p = &rec.onchain;
    let status = ProcessStatus::from_onchain(p.status as u8).unwrap_or(ProcessStatus::Unknown);
    let census_root =
        fr_from_be(&p.census.root).map_err(|e| ApiError::Internal(format!("census root: {e}")))?;
    Ok(ProcessView {
        id: pid_wire(&rec.pid),
        status,
        is_accepting_votes: accepting,
        organization_id: p.organizer,
        encryption_key: p.enc_key,
        ballot_mode: p.ballot_mode,
        census: CensusView {
            census_origin: p.census.origin,
            census_root,
            census_uri: p.census.uri.clone(),
        },
        state_root: p.state_root,
        local_state_root,
        voters_count: p.voters_count,
        overwritten_votes_count: p.overwritten_count,
        max_voters: p.max_voters,
        start_time: p.start_time,
        duration: p.duration,
        result: (!p.results.is_empty()).then(|| p.results.clone()),
        ignored: rec.local == crate::storage::LocalStatus::Ignored,
        note: rec.note.clone(),
    })
}

pub async fn get(
    State(st): State<AppState>,
    Path(pid): Path<String>,
) -> Result<Json<ProcessView>, ApiError> {
    let pid = parse_pid(&pid)?;
    let rec = st.node.db.process(&pid)?.ok_or(ApiError::UnknownProcess)?;
    // The actor snapshot adds the node's own view: whether it accepts
    // votes and its committed local tree root (may lead `state_root`).
    let (accepting, local_root) = match st.node.processes.read().await.get(&pid).cloned() {
        Some(h) => h
            .snapshot()
            .await
            .map(|s| (s.accepting, Some(s.root)))
            .unwrap_or((false, None)),
        None => (false, None),
    };
    Ok(Json(view(&rec, accepting, local_root)?))
}

/// `POST /processes/keys`: this node's key for `processId`, derived, not
/// stored, so repeated or abusive calls cost nothing but the rate limit.
pub async fn new_key(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    payload: Result<Json<EncryptionKeyRequest>, JsonRejection>,
) -> Result<Json<EncryptionKeyResponse>, ApiError> {
    // An observer's keys would lock the election: it can never finalize.
    if st.node.statics.sequencer_address.is_none() {
        return Err(ApiError::Observer("this node cannot finalize elections"));
    }
    st.take_key_token(peer.ip())?;
    let Json(req) = payload.map_err(|r| {
        if r.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::TooLarge
        } else {
            ApiError::Invalid(r.body_text())
        }
    })?;
    let pk = st.node.keys.public_key(&req.process_id.0);
    Ok(Json(EncryptionKeyResponse::from_point(&pk)))
}

pub async fn participant(
    State(st): State<AppState>,
    Path((pid, addr)): Path<(String, String)>,
) -> Result<Json<ParticipantResponse>, ApiError> {
    let pid = parse_pid(&pid)?;
    let addr = parse_addr(&addr)?;
    let rec = st.node.db.process(&pid)?.ok_or(ApiError::UnknownProcess)?;
    // A CSP census has no local proofs (`None`); the CSP gives voters theirs.
    let (proof, weight) = super::votes::current_proof(&st, &rec, &addr)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(ParticipantResponse {
        address: addr,
        weight,
        census_proof: MerkleProof::from(&proof),
    }))
}

pub async fn transitions(
    State(st): State<AppState>,
    Path(pid): Path<String>,
) -> Result<Json<TransitionList>, ApiError> {
    let pid = parse_pid(&pid)?;
    st.node.db.process(&pid)?.ok_or(ApiError::UnknownProcess)?;
    let transitions = st
        .node
        .db
        .transitions(&pid)?
        .iter()
        .map(|t| TransitionView {
            index: t.index,
            old_root: t.old_root,
            new_root: t.new_root,
            tx_hash: t.tx_hash,
            block_number: t.block,
            sender: t.sender,
            voters: t.n_votes,
            overwrites: t.n_overwrites,
            n_blobs: t.n_blobs,
        })
        .collect();
    Ok(Json(TransitionList { transitions }))
}

/// Blobs of one transition, as `0x` hex (see `BlobsResponse`).
pub async fn blobs(
    State(st): State<AppState>,
    // Parsed by hand: axum's u64 path rejection is a text/plain 400.
    Path((pid, idx)): Path<(String, String)>,
) -> Result<Json<BlobsResponse>, ApiError> {
    let pid = parse_pid(&pid)?;
    let idx: u64 = idx
        .parse()
        .map_err(|_| ApiError::Invalid("transition index must be an integer".into()))?;
    st.node.db.process(&pid)?.ok_or(ApiError::UnknownProcess)?;
    let blobs = st.node.db.blobs(&pid, idx)?;
    if blobs.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(BlobsResponse {
        blobs: blobs.iter().map(|b| b.to_vec()).collect(),
    }))
}
