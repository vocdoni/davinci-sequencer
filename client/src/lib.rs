//! DAVINCI sequencer client: the HTTP wire types (`api`), an API client, the
//! organizer helper (process lifecycle on the ProcessRegistry) and the voter
//! helper that builds and proves ballots.
#![forbid(unsafe_code)]

pub mod api;
mod client;
pub mod organizer;
#[cfg(feature = "prover")]
pub mod prover;
pub mod voter;

pub use client::SequencerClient;

/// Every fallible call of this crate returns this.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Network or transport failure.
    #[error("http: {0}")]
    Http(String),
    /// The sequencer answered with an error status.
    #[error("sequencer returned {status}: {message}")]
    Api {
        status: u16,
        code: Option<u32>,
        message: String,
    },
    /// A response that does not decode or does not check out.
    #[error("bad response: {0}")]
    Decode(String),
    /// Caller input the protocol would reject.
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("prover: {0}")]
    Prover(String),
    #[error("chain: {0}")]
    Chain(String),
    /// A registry call reverted with this ABI error name (`InvalidCensusAddress`,
    /// `Unauthorized`, ...).
    #[error("chain: reverted: {0}")]
    Reverted(String),
    /// The registry created `created`, not the id the key was issued for
    /// (another `newProcess` from this account landed first). No sequencer
    /// holds that process's key: cancel it with `setProcessStatus(CANCELED)`.
    #[error("created process {created}, but the key was issued for {expected}; cancel it")]
    WrongProcessId {
        created: crate::api::ProcessId,
        expected: crate::api::ProcessId,
    },
    /// A DKG key mode on a registry deployed without a DKG manager
    /// (`dkgAdapter()` is zero).
    #[error("the registry has no DKG adapter: DKG key modes are disabled")]
    DkgDisabled,
    /// The registry does not pin what this release proves.
    #[error("registry pin {field}: expected {expected}, got {got}")]
    Pin {
        field: &'static str,
        expected: String,
        got: String,
    },
    #[error(transparent)]
    Sdk(#[from] davinci_zkvm_sdk::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
