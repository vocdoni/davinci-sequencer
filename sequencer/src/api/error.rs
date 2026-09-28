//! API error responses: JSON `{"error", "code"}` bodies. The code is the
//! HTTP status times 100 plus a discriminator, so clients can tell apart
//! same-status cases (41201 "not accepting" vs 41202 "max voters reached").

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use davinci_client::api::ErrorResponse;
use davinci_state::VoteError;

use crate::actor::ActorError;
use crate::storage::StorageError;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400 40001: malformed or unverifiable request.
    #[error("{0}")]
    Invalid(String),
    /// 400 40002: the vote failed a protocol check.
    #[error("{0}")]
    Vote(#[from] VoteError),
    /// 404 40401.
    #[error("not found")]
    NotFound,
    /// 404 40402: the process is unknown to this node.
    #[error("unknown process")]
    UnknownProcess,
    /// 409 40901.
    #[error("vote {0:#x} already submitted")]
    Duplicate(u64),
    /// 409 40902: the slot has a queued vote; retry once it settles.
    #[error("slot {0} already has a queued vote")]
    SlotBusy(u64),
    /// 408 40801: the per-request deadline fired. The request may still
    /// have taken effect (a timed-out `POST /votes` can have admitted the
    /// vote; a retry then answers 409 duplicate).
    #[error("request timed out")]
    Timeout,
    /// 412 41201: the process does not accept votes.
    #[error("not accepting votes: {0}")]
    NotAccepting(String),
    /// 412 41202.
    #[error("max voters reached")]
    MaxVoters,
    /// 412 41203: this node has no signing key and never settles.
    #[error("observer node: {0}")]
    Observer(&'static str),
    /// 413 41301.
    #[error("request body too large")]
    TooLarge,
    /// 429 42901: `POST /processes/keys` per-minute cap.
    #[error("key generation rate limit reached")]
    KeyRate,
    /// 429 42903: the node is at capacity; retry shortly.
    #[error("busy: {0}")]
    Busy(String),
    /// 500 50001.
    #[error("internal: {0}")]
    Internal(String),
}

impl ApiError {
    fn status_code(&self) -> (StatusCode, u32) {
        match self {
            ApiError::Invalid(_) => (StatusCode::BAD_REQUEST, 40001),
            ApiError::Vote(_) => (StatusCode::BAD_REQUEST, 40002),
            ApiError::NotFound => (StatusCode::NOT_FOUND, 40401),
            ApiError::UnknownProcess => (StatusCode::NOT_FOUND, 40402),
            ApiError::Duplicate(_) => (StatusCode::CONFLICT, 40901),
            ApiError::SlotBusy(_) => (StatusCode::CONFLICT, 40902),
            ApiError::Timeout => (StatusCode::REQUEST_TIMEOUT, 40801),
            ApiError::NotAccepting(_) => (StatusCode::PRECONDITION_FAILED, 41201),
            ApiError::MaxVoters => (StatusCode::PRECONDITION_FAILED, 41202),
            ApiError::Observer(_) => (StatusCode::PRECONDITION_FAILED, 41203),
            ApiError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, 41301),
            ApiError::KeyRate => (StatusCode::TOO_MANY_REQUESTS, 42901),
            ApiError::Busy(_) => (StatusCode::TOO_MANY_REQUESTS, 42903),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, 50001),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_code();
        // 500 details go to the log, never to the client.
        let error = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(%self, "API internal error");
            "internal error".into()
        } else {
            self.to_string()
        };
        let body = ErrorResponse { error, code };
        (status, Json(body)).into_response()
    }
}

impl From<ActorError> for ApiError {
    fn from(e: ActorError) -> Self {
        match e {
            ActorError::Closed(m) => ApiError::NotAccepting(m),
            ActorError::MaxVoters => ApiError::MaxVoters,
            ActorError::Duplicate(v) => ApiError::Duplicate(v),
            ActorError::SlotBusy(s) => ApiError::SlotBusy(s),
            ActorError::WrongProcess => ApiError::Invalid("vote is for another process".into()),
            ActorError::NotFound => ApiError::NotFound,
            ActorError::Stopped => ApiError::NotAccepting("process stopped".into()),
            ActorError::Busy(m) => ApiError::Busy(m),
            ActorError::Internal(m) => ApiError::Internal(m),
        }
    }
}

impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        ApiError::Internal(e.to_string())
    }
}
