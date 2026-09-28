//! davinci-state: per-process DAVINCI election state on the arbo SMT.
//!
//! Validates votes against every per-vote zkVM guest rule, builds the
//! state transition (`/prove` request, DA blobs, expected publics),
//! commits or rolls back, and follows transitions other sequencers proved.

#![forbid(unsafe_code)]

mod config;
mod error;
mod process;
mod results;
mod sync;
mod transition;
mod validate;

pub use config::{CensusOrigin, ProcessConfig, genesis_root};
pub use error::{Error, VoteError};
pub use process::{Committed, ProcessState};
pub use transition::PreparedBatch;
pub use validate::{
    VerifiedVote, VotePackage, ballot_from_be_coords, validate_vote, vote_still_valid,
};
