//! Request extractors that fail in the API's JSON error shape.

use axum::async_trait;
use axum::body::Bytes;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use super::MAX_BODY;
use crate::api::error::ApiError;

/// `axum::Json` with the decoding this API promises: a declared length over
/// the cap is refused before the body is read, and anything but whitespace
/// after the JSON value is an error (axum's own extractor ignores it).
pub(super) struct StrictJson<T>(pub T);

fn is_json(ct: &str) -> bool {
    let essence = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

#[async_trait]
impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for StrictJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let ct = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        if !ct.is_some_and(is_json) {
            return Err(ApiError::Invalid(
                "expected a request with `Content-Type: application/json`".into(),
            ));
        }
        let declared = req
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|n| n > MAX_BODY as u64) {
            return Err(ApiError::TooLarge);
        }
        let bytes = Bytes::from_request(req, state).await.map_err(|r| {
            if r.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::TooLarge
            } else {
                ApiError::Invalid(r.body_text())
            }
        })?;
        serde_json::from_slice(&bytes)
            .map(StrictJson)
            .map_err(|e| ApiError::Invalid(format!("invalid JSON body: {e}")))
    }
}

/// `axum::extract::Path` whose rejection (a path segment that is not valid
/// UTF-8 once percent-decoded) is the JSON 400 instead of plain text.
pub(super) struct Path<T>(pub T);

#[async_trait]
impl<T, S> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|p| Path(p.0))
            .map_err(|r: PathRejection| ApiError::Invalid(r.body_text()))
    }
}
