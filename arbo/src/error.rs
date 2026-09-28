use thiserror::Error as ThisError;

/// Errors mirroring vocdoni/arbo semantics, plus typed decode failures.
#[derive(Debug, ThisError)]
pub enum Error {
    #[error("key not found")]
    KeyNotFound,
    #[error("key already exists")]
    KeyAlreadyExists,
    #[error("invalid value prefix")]
    InvalidValuePrefix,
    #[error("max level reached")]
    MaxLevel,
    #[error("max virtual level reached")]
    MaxVirtualLevel,
    #[error("tree is not empty")]
    TreeNotEmpty,
    #[error("key too long: {len} > {max}")]
    KeyTooLong { len: usize, max: usize },
    #[error("value too long: {len} > {max}")]
    ValueTooLong { len: usize, max: usize },
    #[error("root does not exist in storage")]
    RootNotFound,
    #[error("tree opened with mismatched parameters: {0}")]
    ParamsMismatch(&'static str),
    #[error("corrupted storage entry: {0}")]
    Corrupted(&'static str),
    #[error("malformed packed siblings: {0}")]
    MalformedSiblings(&'static str),
    #[error("malformed dump: {0}")]
    MalformedDump(&'static str),
    #[error("storage error: {0}")]
    Storage(String),
}
