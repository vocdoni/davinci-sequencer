//! Vote routes: submit, status, tracker proof, stored ballot.

use axum::Json;
use axum::extract::State;
use davinci_client::api::{
    BallotResponse, TrackerProof, VoteRequest, VoteResponse, VoteStatus, VoteStatusResponse,
    parse_vote_id,
};
use davinci_state::validate_vote;
use davinci_zkvm_sdk::census::{CensusProof, CensusWitness, slot_key_address};
use davinci_zkvm_sdk::crypto::field::Fr;

use super::AppState;
use super::extract::{Path, StrictJson};
use super::processes::{parse_pid, pid_wire};
use crate::api::error::ApiError;
use crate::api::parse_addr;
use crate::census::CensusError;
use crate::storage::{self, ProcessRecord};

fn wire_status(s: storage::VoteStatus) -> VoteStatus {
    match s {
        storage::VoteStatus::Pending => VoteStatus::Pending,
        storage::VoteStatus::Aggregated => VoteStatus::Aggregated,
        storage::VoteStatus::Processed => VoteStatus::Processed,
        storage::VoteStatus::Settled => VoteStatus::Settled,
        storage::VoteStatus::Error => VoteStatus::Error,
    }
}

fn parse_vid(s: &str) -> Result<u64, ApiError> {
    parse_vote_id(s).map_err(|e| ApiError::Invalid(e.to_string()))
}

async fn handle_of(st: &AppState, pid: &Fr) -> Result<crate::actor::ActorHandle, ApiError> {
    st.node
        .processes
        .read()
        .await
        .get(pid)
        .cloned()
        .ok_or(ApiError::UnknownProcess)
}

/// Proof and weight of `addr` at the process's current Merkle census root:
/// the on-chain root for origins 1 and 2, the census contract's latest
/// confirmed root for origin 3. `None` for a non-member (and for CSP).
pub(super) async fn current_proof(
    st: &AppState,
    rec: &ProcessRecord,
    addr: &[u8; 20],
) -> Result<Option<(CensusProof, u128)>, ApiError> {
    let c = &rec.onchain.census;
    let res = match c.origin {
        1 | 2 => {
            // Any bytes32 is a Keccak root: the census store decides.
            if let Some(why) = st.node.census.bad_reason(&rec.pid, &c.root) {
                return Err(ApiError::NotAccepting(format!("census: {why}")));
            }
            st.node.census.proof_async(&c.root, addr).await
        }
        3 => {
            let ix = &st.node.onchain;
            // `None` = the contract is not indexed or no confirmed sync has
            // landed yet; an unusable contract is refused for good.
            let read = || -> Result<Option<([u8; 32], u64, u64)>, ApiError> {
                match ix.usable(&c.contract_address) {
                    // A failed register at resume: the actor re-registers.
                    Err(crate::census::onchain::UsableError::NotIndexed) => Ok(None),
                    Err(e) => Err(ApiError::NotAccepting(e.to_string())),
                    Ok(()) => Ok(ix.latest(&c.contract_address)),
                }
            };
            // A vote or a participant query is waiting: sync the contract on
            // demand, best effort, then read once more before Busy.
            if read()?.is_none() {
                let _ = ix.ensure(&c.contract_address).await;
            }
            let (root, _, _) =
                read()?.ok_or_else(|| ApiError::Busy("census contract not synced yet".into()))?;
            ix.proof(&c.contract_address, &root, addr).await
        }
        _ => Ok(None),
    };
    res.map_err(|e| match e {
        // An origin-2 update still downloading.
        CensusError::Unknown(_) => ApiError::Busy("census not loaded yet".into()),
        e => ApiError::Internal(e.to_string()),
    })
}

/// `POST /votes`. A paused process still accepts votes: sealing is
/// gated on `ready`, so they queue locally and settle on resume; only
/// `ended`/`canceled`/past-end refuse with 41201, and a process before its
/// start time with 41204.
pub async fn submit(
    State(st): State<AppState>,
    payload: Result<StrictJson<VoteRequest>, ApiError>,
) -> Result<Json<VoteResponse>, ApiError> {
    // Observers relay nothing: refuse before doing any work.
    if st.node.statics.sequencer_address.is_none() {
        return Err(ApiError::Observer("this node does not accept votes"));
    }
    let StrictJson(req) = payload?;
    let pid = req.process_id.to_fr();
    let handle = handle_of(&st, &pid).await?;
    let rec = st.node.db.process(&pid)?.ok_or(ApiError::UnknownProcess)?;
    // Closed by status or clock: refused as the actor would, before the
    // vote costs a pairing check.
    if let Some(e) = handle.admission_error(&rec) {
        return Err(e.into());
    }
    let mut cfg = st
        .node
        .process_config(&rec)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // The census witness: re-derived locally at the current root for a
    // Merkle census (the client-supplied one is ignored), required from the
    // client for CSP.
    let census = if cfg.census_origin.is_merkle() {
        let (proof, _) = current_proof(&st, &rec, &req.address)
            .await?
            .ok_or_else(|| ApiError::Invalid("address not in the census".into()))?;
        cfg.census_root = proof.root;
        CensusWitness::Merkle(proof)
    } else {
        match req.census_witness() {
            Some(w @ CensusWitness::Csp(_)) => w,
            _ => return Err(ApiError::Invalid("CSP census proof required".into())),
        }
    };
    // A replayed vote id is a duplicate without paying for its validation.
    if st.node.db.vote(&pid, req.vote_id)?.is_some() {
        return Err(ApiError::Duplicate(req.vote_id));
    }
    let pkg = davinci_state::VotePackage {
        process_id: pid,
        vote_id: req.vote_id,
        address: req.address,
        ballot: req.ballot,
        proof: req.ballot_proof.clone(),
        signature: req.signature(),
        census,
        weight: req.weight,
    };
    let verifier = st.node.verifier.clone();
    // Groth16 + ECDSA are CPU-heavy: keep them off the runtime threads
    // and bound how many run at once.
    let permit = st
        .validate
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::Busy("vote validation at capacity".into()))?;
    let vv = tokio::task::spawn_blocking(move || validate_vote(&cfg, &verifier, pkg))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(ApiError::Vote)?;
    // The permit bounds validations only: a stuck actor must not hold it.
    drop(permit);
    handle.submit_nowait(vv).await?;
    Ok(Json(VoteResponse {
        vote_id: req.vote_id,
    }))
}

pub async fn status(
    State(st): State<AppState>,
    Path((pid, vid)): Path<(String, String)>,
) -> Result<Json<VoteStatusResponse>, ApiError> {
    let pid = parse_pid(&pid)?;
    let vid = parse_vid(&vid)?;
    st.node.db.process(&pid)?.ok_or(ApiError::UnknownProcess)?;
    if let Some(sv) = st.node.db.vote(&pid, vid)? {
        return Ok(Json(VoteStatusResponse {
            status: wire_status(sv.status),
            error: sv.error,
        }));
    }
    // No stored package, but the id may be in the tree: settled by another
    // sequencer (or synced from the blobs).
    if let Ok(h) = handle_of(&st, &pid).await
        && h.vote_id_proof(vid).await.is_ok()
    {
        return Ok(Json(VoteStatusResponse {
            status: VoteStatus::Settled,
            error: None,
        }));
    }
    Err(ApiError::NotFound)
}

pub async fn proof(
    State(st): State<AppState>,
    Path((pid, vid)): Path<(String, String)>,
) -> Result<Json<TrackerProof>, ApiError> {
    let pid = parse_pid(&pid)?;
    let vid = parse_vid(&vid)?;
    let handle = handle_of(&st, &pid).await?;
    // Proof and root come from one actor message, so they cannot straddle
    // a commit.
    let (p, root) = handle.vote_id_proof(vid).await?;
    Ok(Json(TrackerProof {
        process_id: pid_wire(&pid),
        vote_id: vid,
        root,
        siblings: p.siblings,
    }))
}

pub async fn ballot(
    State(st): State<AppState>,
    Path((pid, addr)): Path<(String, String)>,
) -> Result<Json<BallotResponse>, ApiError> {
    let pid = parse_pid(&pid)?;
    let addr = parse_addr(&addr)?;
    let handle = handle_of(&st, &pid).await?;
    let rec = st.node.db.process(&pid)?.ok_or(ApiError::UnknownProcess)?;
    // Merkle: the slot is a pure function of the address, so ballots
    // synced from other sequencers are served too, and a voter a later
    // census dropped still finds the ballot it cast. CSP: the slot index is
    // the CSP's choice, so only locally stored votes can name it (an
    // address this node never served is a 404).
    let slot = if (1..=3).contains(&rec.onchain.census.origin) {
        slot_key_address(&addr)
    } else {
        st.node
            .db
            .vote_slot_by_address(&pid, &addr)?
            .ok_or(ApiError::NotFound)?
    };
    let ballot = handle.slot_ballot(slot).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(BallotResponse {
        address: addr,
        ballot,
    }))
}
