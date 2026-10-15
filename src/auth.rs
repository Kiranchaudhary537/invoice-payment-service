use axum::{extract::FromRequestParts, http::header::AUTHORIZATION, http::request::Parts};
use chrono::{DateTime, Utc};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{api_keys, app::AppState, error::ApiError};

/// The authenticated business. Every query downstream is scoped by `business_id`.
#[derive(Clone, Copy, Debug)]
pub struct Auth {
    pub business_id: Uuid,
    pub api_key_id: Uuid,
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .ok_or(ApiError::Unauthorized)?;
        let prefix = api_keys::parse_prefix(key).ok_or(ApiError::Unauthorized)?;

        type KeyRow = (Uuid, Uuid, Vec<u8>, Option<DateTime<Utc>>);
        let row: Option<KeyRow> =
            sqlx::query_as("SELECT id, business_id, key_hash, revoked_at FROM api_keys WHERE prefix = $1")
                .bind(prefix)
                .fetch_optional(&state.pool)
                .await?;
        let Some((api_key_id, business_id, stored_hash, revoked_at)) = row else {
            return Err(ApiError::Unauthorized);
        };
        let hash_matches: bool = api_keys::hash_key(key).ct_eq(&stored_hash).into();
        if !hash_matches || revoked_at.is_some() {
            tracing::warn!(api_key_prefix = prefix, revoked = revoked_at.is_some(), "rejected api key");
            return Err(ApiError::Unauthorized);
        }

        // Off the request path: a failed timestamp write must not fail the request.
        let pool = state.pool.clone();
        tokio::spawn(async move {
            let _ = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = $1")
                .bind(api_key_id)
                .execute(&pool)
                .await;
        });

        Ok(Auth { business_id, api_key_id })
    }
}
