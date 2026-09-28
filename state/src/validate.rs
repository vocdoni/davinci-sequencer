//! Stateless vote validation: every per-vote check the zkVM guest makes,
//! so an invalid vote never reaches a batch.

use davinci_zkvm_sdk::ballot::{Ballot, address_to_fr, inputs_hash};
use davinci_zkvm_sdk::census::{
    CensusWitness, CspProof, EcdsaSignature, census_leaf, csp_recover, slot_key_address,
    slot_key_csp, verify_census_proof,
};
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::{BALLOT_COORDS, MAX_CENSUS_DEPTH, VOTE_ID_MIN};
use davinci_zkvm_sdk::types::SnarkJsProof;

use crate::config::{CensusOrigin, ProcessConfig};
use crate::error::VoteError;

/// A vote as submitted, before any check.
#[derive(Clone, Debug)]
pub struct VotePackage {
    pub process_id: Fr,
    pub vote_id: u64,
    pub address: [u8; 20],
    pub ballot: Ballot,
    pub proof: SnarkJsProof,
    pub inputs_hash: Fr,
    pub signature: EcdsaSignature,
    pub census: CensusWitness,
    pub weight: u128,
}

/// A vote that passed every check, with its derived ballot slot.
#[derive(Clone, Debug)]
pub struct VerifiedVote {
    pub pkg: VotePackage,
    pub slot: u64,
}

/// Ballot from 64 big-endian coordinates, rejecting non-canonical field
/// elements and off-curve points (the trust boundary for raw wire coords).
pub fn ballot_from_be_coords(coords: &[[u8; 32]; BALLOT_COORDS]) -> Result<Ballot, VoteError> {
    let mut frs = [Fr::from(0u64); BALLOT_COORDS];
    for (o, c) in frs.iter_mut().zip(coords) {
        *o = fr_from_be(c).map_err(|_| VoteError::BadCoordinate)?;
    }
    Ballot::from_coords(&frs).map_err(|_| VoteError::OffCurve)
}

fn check_census(cfg: &ProcessConfig, pkg: &VotePackage) -> Result<u64, VoteError> {
    match (&pkg.census, cfg.census_origin) {
        (CensusWitness::Merkle(p), o) if o.is_merkle() => {
            if p.siblings.len() > MAX_CENSUS_DEPTH {
                return Err(VoteError::CensusDepth);
            }
            if p.path_bits >> p.siblings.len() != 0 {
                return Err(VoteError::CensusPathBits);
            }
            if !verify_census_proof(p) {
                return Err(VoteError::CensusProofInvalid);
            }
            if p.root != cfg.census_root {
                return Err(VoteError::CensusRootMismatch);
            }
            let leaf = census_leaf(&pkg.address, pkg.weight).map_err(|_| VoteError::WeightRange)?;
            if p.leaf != leaf {
                return Err(VoteError::CensusLeafMismatch);
            }
            // The leaf binds the address, which alone picks the slot.
            Ok(slot_key_address(&pkg.address))
        }
        (CensusWitness::Csp(p), CensusOrigin::Csp) => {
            if p.address != pkg.address {
                return Err(VoteError::CspAddressMismatch);
            }
            if p.weight != pkg.weight {
                return Err(VoteError::CspWeightMismatch);
            }
            let signer = csp_root_signer(&pkg.process_id, p)?;
            // The census root is the CSP address as uint160.
            let root = fr_to_be(&cfg.census_root);
            if root[..12] != [0u8; 12] || root[12..] != signer {
                return Err(VoteError::CspSignerMismatch);
            }
            slot_key_csp(p.index).map_err(|_| VoteError::SlotRange)
        }
        _ => Err(VoteError::OriginMismatch),
    }
}

fn csp_root_signer(pid: &Fr, p: &CspProof) -> Result<[u8; 20], VoteError> {
    csp_recover(pid, p).map_err(|e| VoteError::CspSignatureInvalid(e.to_string()))
}

/// Runs every guest per-vote rule against `pkg`. `verifier` must be the
/// process ballot VK (its hash is bound to the 0x07 leaf).
pub fn validate_vote(
    cfg: &ProcessConfig,
    verifier: &BallotVerifier,
    pkg: VotePackage,
) -> Result<VerifiedVote, VoteError> {
    if pkg.process_id != cfg.process_id {
        return Err(VoteError::ProcessMismatch);
    }
    if verifier.vk_hash() != cfg.ballot_vk_hash {
        return Err(VoteError::VkMismatch);
    }
    if pkg.vote_id < VOTE_ID_MIN {
        return Err(VoteError::VoteIdRange);
    }
    // Ballot fields are pub, so a caller can hold off-curve points:
    // re-check the whole ballot, then the identity padding.
    for ct in &pkg.ballot.0 {
        if !ct.c1.is_on_curve() || !ct.c2.is_on_curve() {
            return Err(VoteError::OffCurve);
        }
    }
    if !pkg.ballot.is_padded_ok(cfg.ballot_mode.num_fields) {
        return Err(VoteError::PaddingNotIdentity);
    }
    if pkg.weight >> 88 != 0 {
        return Err(VoteError::WeightRange);
    }

    let slot = check_census(cfg, &pkg)?;

    // Vote id signature by the voter's own key (high-S and bad recid are
    // rejected by the recover itself).
    let signer = vote_id_signer(pkg.vote_id, &pkg.signature)?;
    if signer != pkg.address {
        return Err(VoteError::SignatureMismatch);
    }

    let addr = address_to_fr(&pkg.address);
    let ih = inputs_hash(
        &cfg.process_id,
        &cfg.ballot_mode,
        &cfg.enc_key,
        &addr,
        pkg.vote_id,
        &pkg.ballot,
        &Fr::from(pkg.weight),
    )
    .map_err(|_| VoteError::InputsHashMismatch)?;
    if ih != pkg.inputs_hash {
        return Err(VoteError::InputsHashMismatch);
    }

    if !verifier.verify(&pkg.proof, &[addr, Fr::from(pkg.vote_id), ih]) {
        return Err(VoteError::BadProof);
    }

    Ok(VerifiedVote { pkg, slot })
}

fn vote_id_signer(vote_id: u64, sig: &EcdsaSignature) -> Result<[u8; 20], VoteError> {
    davinci_zkvm_sdk::census::vote_id_recover(vote_id, sig)
        .map_err(|e| VoteError::SignatureInvalid(e.to_string()))
}

/// Whether a pending vote survives a census update: for a Merkle
/// origin, `new_leaf` (the address's leaf in the new census, if any) must be
/// the leaf the vote was validated with. A CSP census never changes.
pub fn vote_still_valid(cfg: &ProcessConfig, pkg: &VotePackage, new_leaf: Option<Fr>) -> bool {
    if !cfg.census_origin.is_merkle() {
        return true;
    }
    match (new_leaf, census_leaf(&pkg.address, pkg.weight)) {
        (Some(new), Ok(old)) => new == old,
        _ => false,
    }
}
