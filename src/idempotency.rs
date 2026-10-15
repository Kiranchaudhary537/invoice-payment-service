//! Idempotency-Key handling shared by every POST that accepts one.
//!
//! A key is scoped per business and bound to a hash of method + path + canonical body.
//! - same key, same request, completed   -> replay the stored response (no side effects)
//! - same key, different request         -> 422 idempotency_key_reuse
//! - same key, original still running    -> 409 request_in_progress
//!
//! Keys expire after the configured TTL; expired rows are treated as absent and purged hourly.

use std::{future::Future, pin::Pin, time::Duration};

use axum::{
    Json,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{app::AppState, error::ApiError};

pub enum Claim {
    Proceed,
    Replay(StatusCode, Value),
}

pub fn request_hash(method: &str, path: &str, canonical_body: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(method.as_bytes());
    h.update(b" ");
    h.update(path.as_bytes());
    h.update(b"\n");
    h.update(canonical_body);
    h.finalize().to_vec()
}

fn expires_at(ttl: Duration) -> chrono::DateTime<Utc> {
    Utc::now() + chrono::Duration::from_std(ttl).expect("ttl fits in chrono::Duration")
}

/// Claims the key inside the caller's transaction. An expired row is taken over atomically.
pub async fn claim(conn: &mut PgConnection, business_id: Uuid, key: &str, hash: &[u8], ttl: Duration) -> Result<Claim, ApiError> {
    let claimed = sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_hash, status, expires_at)
         VALUES ($1, $2, $3, 'in_progress', $4)
         ON CONFLICT (business_id, key) DO UPDATE
            SET request_hash = EXCLUDED.request_hash, status = 'in_progress', response_code = NULL,
                response_body = NULL, created_at = now(), expires_at = EXCLUDED.expires_at
            WHERE idempotency_keys.expires_at < now()",
    )
    .bind(business_id)
    .bind(key)
    .bind(hash)
    .bind(expires_at(ttl))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if claimed == 1 {
        return Ok(Claim::Proceed);
    }

    let (stored_hash, status, code, body): (Vec<u8>, String, Option<i32>, Option<Value>) = sqlx::query_as(
        "SELECT request_hash, status, response_code, response_body FROM idempotency_keys WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(key)
    .fetch_one(&mut *conn)
    .await?;
    if stored_hash != hash {
        return Err(ApiError::IdempotencyKeyReuse);
    }
    match (status.as_str(), code, body) {
        ("completed", Some(code), Some(body)) => {
            let code = StatusCode::from_u16(code as u16).map_err(|_| ApiError::Internal)?;
            Ok(Claim::Replay(code, body))
        }
        _ => Err(ApiError::RequestInProgress),
    }
}

/// Stores the final response in the same transaction as the work it describes.
pub async fn complete(conn: &mut PgConnection, business_id: Uuid, key: &str, code: StatusCode, body: &Value) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE idempotency_keys SET status = 'completed', response_code = $3, response_body = $4
         WHERE business_id = $1 AND key = $2 AND status = 'in_progress'",
    )
    .bind(business_id)
    .bind(key)
    .bind(code.as_u16() as i32)
    .bind(body)
    .execute(conn)
    .await?;
    Ok(())
}

/// Records a deterministic error after the work transaction rolled back (and took the claim with it).
pub async fn store_error(pool: &PgPool, business_id: Uuid, key: &str, hash: &[u8], ttl: Duration, err: &ApiError) -> Result<(), ApiError> {
    if err.is_transient() {
        return Ok(());
    }
    let (code, body) = err.parts();
    sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_hash, status, response_code, response_body, expires_at)
         VALUES ($1, $2, $3, 'completed', $4, $5, $6)
         ON CONFLICT (business_id, key) DO NOTHING",
    )
    .bind(business_id)
    .bind(key)
    .bind(hash)
    .bind(code.as_u16() as i32)
    .bind(body)
    .bind(expires_at(ttl))
    .execute(pool)
    .await?;
    Ok(())
}

pub fn respond(code: StatusCode, body: Value, replayed: bool) -> Response {
    let mut res = (code, Json(body)).into_response();
    if replayed {
        res.headers_mut().insert("idempotent-replayed", HeaderValue::from_static("true"));
    }
    res
}

pub type TxWork<'c> = Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + 'c>>;

/// Runs a single-transaction write, honouring an optional Idempotency-Key.
/// The claim, the work and the stored response commit together, so these endpoints never
/// leave an `in_progress` row behind.
pub async fn run_in_tx<F>(state: &AppState, business_id: Uuid, key: Option<String>, hash: Vec<u8>, success: StatusCode, work: F) -> Result<Response, ApiError>
where
    F: for<'c> FnOnce(&'c mut PgConnection) -> TxWork<'c>,
{
    let ttl = state.config.idempotency_ttl;
    let mut tx = state.pool.begin().await?;
    if let Some(key) = &key {
        if let Claim::Replay(code, body) = claim(&mut tx, business_id, key, &hash, ttl).await? {
            return Ok(respond(code, body, true));
        }
    }
    match work(&mut tx).await {
        Ok(body) => {
            if let Some(key) = &key {
                complete(&mut tx, business_id, key, success, &body).await?;
            }
            tx.commit().await?;
            Ok(respond(success, body, false))
        }
        Err(err) => {
            tx.rollback().await?;
            if let Some(key) = &key {
                store_error(&state.pool, business_id, key, &hash, ttl, &err).await?;
            }
            Err(err)
        }
    }
}

/// Deletes expired keys in small batches so the purge never holds long locks or bloats WAL.
pub async fn purge_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut total = 0;
    loop {
        let deleted = sqlx::query(
            "DELETE FROM idempotency_keys WHERE (business_id, key) IN
                (SELECT business_id, key FROM idempotency_keys WHERE expires_at < now() LIMIT 1000)",
        )
        .execute(pool)
        .await?
        .rows_affected();
        total += deleted;
        if deleted < 1000 {
            return Ok(total);
        }
    }
}

pub fn spawn_purge(pool: PgPool, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            match purge_expired(&pool).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(deleted = n, "purged expired idempotency keys"),
                Err(e) => tracing::error!(error = %e, "idempotency purge failed"),
            }
        }
    });
}
