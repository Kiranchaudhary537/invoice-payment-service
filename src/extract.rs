use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{FromRequestParts, Query, Request},
    http::{HeaderMap, HeaderValue, request::Parts},
    middleware::Next,
    response::Response,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::Instrument;
use uuid::Uuid;

use crate::error::ApiError;

/// Query-string extractor whose rejection uses our error format.
pub struct ApiQuery<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| ApiQuery(v))
            .map_err(|e| ApiError::Validation(e.body_text()))
    }
}

/// Parses a JSON body into `T` and also returns the canonical JSON bytes (sorted keys, no whitespace)
/// used for the idempotency request hash.
pub fn parse_json<T: DeserializeOwned>(body: &Bytes) -> Result<(T, Vec<u8>), ApiError> {
    let value: Value = serde_json::from_slice(body).map_err(|e| ApiError::Validation(format!("Invalid JSON body: {e}")))?;
    let canonical = serde_json::to_vec(&value).expect("a parsed Value always serializes");
    let parsed = serde_json::from_value(value).map_err(|e| ApiError::Validation(e.to_string()))?;
    Ok((parsed, canonical))
}

/// An id that does not parse cannot exist, so it is a 404 rather than a 400.
pub fn parse_id(raw: &str, resource: &'static str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::NotFound(resource))
}

pub fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .ok()
        .filter(|k| !k.is_empty() && k.len() <= 255)
        .ok_or_else(|| ApiError::Validation("Idempotency-Key must be 1-255 visible ASCII characters".into()))?;
    Ok(Some(key.to_owned()))
}

#[derive(serde::Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

impl ListParams {
    pub fn limit(&self) -> Result<i64, ApiError> {
        match self.limit {
            None => Ok(20),
            Some(l) if (1..=100).contains(&l) => Ok(l),
            Some(_) => Err(ApiError::Validation("limit must be between 1 and 100".into())),
        }
    }

    pub fn cursor(&self) -> Result<Option<Uuid>, ApiError> {
        self.cursor
            .as_deref()
            .map(|c| Uuid::parse_str(c).map_err(|_| ApiError::Validation("invalid cursor".into())))
            .transpose()
    }
}

/// Assigns a request id, logs one line per request, and echoes the id in `X-Request-Id`.
pub async fn request_context(req: Request, next: Next) -> Response {
    let request_id = Uuid::now_v7().to_string();
    let span = tracing::info_span!("request", %request_id, method = %req.method(), path = %req.uri().path());
    let started = Instant::now();
    let mut res = next.run(req).instrument(span.clone()).await;
    span.in_scope(|| {
        tracing::info!(status = res.status().as_u16(), latency_ms = started.elapsed().as_millis() as u64, "request completed")
    });
    res.headers_mut()
        .insert("x-request-id", HeaderValue::from_str(&request_id).expect("uuid is a valid header value"));
    res
}
