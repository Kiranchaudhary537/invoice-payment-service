use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use rand::{Rng, distributions::Alphanumeric, rngs::OsRng};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{app::AppState, auth::Auth, error::ApiError, extract::parse_id};

const PREFIX_LEN: usize = 8;
const SECRET_LEN: usize = 32; // 32 base62 chars ≈ 190 bits of entropy

pub fn random_base62(len: usize) -> String {
    OsRng.sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

/// Returns `(full_key, prefix)`. Format: `dodo_<8-char prefix>_<32-char secret>`.
pub fn generate() -> (String, String) {
    let prefix = random_base62(PREFIX_LEN);
    let key = format!("dodo_{prefix}_{}", random_base62(SECRET_LEN));
    (key, prefix)
}

/// Extracts the public prefix from a well-formed key, or `None` if the key is malformed.
pub fn parse_prefix(key: &str) -> Option<&str> {
    let (prefix, secret) = key.strip_prefix("dodo_")?.split_once('_')?;
    let alnum = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric());
    (prefix.len() == PREFIX_LEN && secret.len() == SECRET_LEN && alnum(prefix) && alnum(secret)).then_some(prefix)
}

/// Keys are high-entropy random strings, so a fast hash is enough; bcrypt/argon2 would only add latency.
pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

pub async fn insert(conn: &mut PgConnection, business_id: Uuid, key: &str) -> Result<ApiKeyRow, sqlx::Error> {
    let prefix = parse_prefix(key).expect("insert is only called with well-formed keys");
    sqlx::query_as(
        "INSERT INTO api_keys (id, business_id, prefix, key_hash) VALUES ($1, $2, $3, $4)
         RETURNING id, prefix, created_at, last_used_at, revoked_at",
    )
    .bind(Uuid::now_v7())
    .bind(business_id)
    .bind(prefix)
    .bind(hash_key(key))
    .fetch_one(conn)
    .await
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// POST /v1/api_keys — the plaintext key is returned exactly once.
/// Not idempotency-key aware on purpose: storing the response would persist the plaintext key.
pub async fn create(State(state): State<AppState>, auth: Auth) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (key, _) = generate();
    let mut conn = state.pool.acquire().await?;
    let row = insert(&mut conn, auth.business_id, &key).await?;
    tracing::info!(api_key_prefix = %row.prefix, "api key created");
    let mut body = serde_json::to_value(&row).expect("row serializes");
    body["key"] = json!(key);
    Ok((StatusCode::CREATED, Json(body)))
}

pub async fn list(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>, ApiError> {
    let rows: Vec<ApiKeyRow> = sqlx::query_as(
        "SELECT id, prefix, created_at, last_used_at, revoked_at FROM api_keys WHERE business_id = $1 ORDER BY id DESC",
    )
    .bind(auth.business_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({ "data": rows })))
}

/// DELETE /v1/api_keys/{id} — revocation is a timestamp, never a delete, so the audit trail survives.
pub async fn revoke(State(state): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Json<ApiKeyRow>, ApiError> {
    let id = parse_id(&id, "api key")?;
    let row: ApiKeyRow = sqlx::query_as(
        "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, now()) WHERE id = $1 AND business_id = $2
         RETURNING id, prefix, created_at, last_used_at, revoked_at",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound("api key"))?;
    tracing::info!(api_key_prefix = %row.prefix, "api key revoked");
    Ok(Json(row))
}
