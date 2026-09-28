//! Errors: `Error` for state/transition operations, `VoteError` for
//! per-vote validation (one variant per guest rule).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("storage: {0}")]
    Arbo(#[from] arbo::Error),
    #[error("sdk: {0}")]
    Sdk(#[from] davinci_zkvm_sdk::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("publics mismatch: {0}")]
    PublicsMismatch(String),
    #[error("root mismatch: got {got}, want {want}")]
    RootMismatch { got: String, want: String },
    /// The live `must_include` slots alone exceed `MAX_REFRESH`, or the
    /// blob cap with even one vote, so no batch can carry them.
    #[error("{exposed} exposed refresh slots do not fit a transition with {votes} votes")]
    RefreshOverflow { exposed: usize, votes: usize },
}

/// Why a vote package was rejected. Mirrors the guest's per-vote checks.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VoteError {
    #[error("vote is for another process")]
    ProcessMismatch,
    #[error("verifier does not match the process ballot vk")]
    VkMismatch,
    #[error("vote id below 2^63")]
    VoteIdRange,
    #[error("ballot coordinate is not a canonical field element")]
    BadCoordinate,
    #[error("ballot point off curve")]
    OffCurve,
    #[error("padded ballot field is not the identity")]
    PaddingNotIdentity,
    #[error("weight does not fit in 88 bits")]
    WeightRange,
    #[error("inputs hash does not match the vote")]
    InputsHashMismatch,
    #[error("ballot proof does not verify")]
    BadProof,
    #[error("vote id signature invalid: {0}")]
    SignatureInvalid(String),
    #[error("vote id signed by another key")]
    SignatureMismatch,
    #[error("census witness does not match the census origin")]
    OriginMismatch,
    #[error("census proof deeper than 61 levels")]
    CensusDepth,
    #[error("census path bits above the proof depth")]
    CensusPathBits,
    #[error("census proof does not verify")]
    CensusProofInvalid,
    #[error("census proof is for another root")]
    CensusRootMismatch,
    #[error("census leaf is not this address and weight")]
    CensusLeafMismatch,
    #[error("CSP signature invalid: {0}")]
    CspSignatureInvalid(String),
    #[error("CSP attestation signed by another key")]
    CspSignerMismatch,
    #[error("CSP attestation is for another address")]
    CspAddressMismatch,
    #[error("CSP attestation weight differs from the vote")]
    CspWeightMismatch,
    #[error("slot outside the ballot namespace")]
    SlotRange,
}
