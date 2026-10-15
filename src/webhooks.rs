//! Webhook endpoints API and the delivery worker.
//!
//! The worker leases due deliveries with `FOR UPDATE SKIP LOCKED` (so several workers never send
//! the same row), pushes `next_attempt_at` forward as a lease, and sends outside any transaction.
//! If the process dies mid-send the lease expires and the delivery is retried: at-least-once,
//! which is why every delivery carries `Webhook-Id` for receiver-side dedup.

use std::time::Duration;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::{
    api_keys::random_base62,
    app::AppState,
    auth::Auth,
    error::ApiError,
    extract::{parse_id, parse_json},
    signing,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateEndpoint {
    url: String,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct EndpointRow {
    pub id: Uuid,
    pub url: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// POST /v1/webhook_endpoints — the signing secret is returned only here.
/// Not idempotency-key aware on purpose: storing the response would persist the secret again.
pub async fn create(State(state): State<AppState>, auth: Auth, body: Bytes) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (input, _) = parse_json::<CreateEndpoint>(&body)?;
    let url = input.url.trim().to_owned();
    if !(url.starts_with("http://") || url.starts_with("https://")) || url.len() > 2048 {
        return Err(ApiError::Validation("url must be an http(s) URL of at most 2048 characters".into()));
    }
    let secret = format!("whsec_{}", random_base62(32));
    let row: EndpointRow = sqlx::query_as(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret) VALUES ($1, $2, $3, $4) RETURNING id, url, enabled, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(&url)
    .bind(&secret)
    .fetch_one(&state.pool)
    .await?;
    let mut body = serde_json::to_value(&row).expect("row serializes");
    body["secret"] = json!(secret);
    Ok((StatusCode::CREATED, Json(body)))
}

pub async fn list(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>, ApiError> {
    let rows: Vec<EndpointRow> =
        sqlx::query_as("SELECT id, url, enabled, created_at FROM webhook_endpoints WHERE business_id = $1 ORDER BY id DESC")
            .bind(auth.business_id)
            .fetch_all(&state.pool)
            .await?;
    Ok(Json(json!({ "data": rows })))
}

/// DELETE /v1/webhook_endpoints/{id} — disables the endpoint and stops its pending deliveries.
pub async fn disable(State(state): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Json<EndpointRow>, ApiError> {
    let id = parse_id(&id, "webhook endpoint")?;
    let mut tx = state.pool.begin().await?;
    let row: EndpointRow = sqlx::query_as(
        "UPDATE webhook_endpoints SET enabled = false WHERE id = $1 AND business_id = $2 RETURNING id, url, enabled, created_at",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound("webhook endpoint"))?;
    sqlx::query("UPDATE webhook_deliveries SET status = 'failed', last_error = 'endpoint_disabled' WHERE endpoint_id = $1 AND status = 'pending'")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(row))
}

#[derive(sqlx::FromRow)]
struct DueDelivery {
    id: Uuid,
    attempt_count: i32,
    event_id: Uuid,
    event_type: String,
    payload: Value,
    event_created_at: DateTime<Utc>,
    url: String,
    secret: String,
    endpoint_enabled: bool,
}

const LEASE_SQL: &str = "
    WITH due AS (
        SELECT id FROM webhook_deliveries
        WHERE status = 'pending' AND next_attempt_at <= now()
        ORDER BY next_attempt_at
        LIMIT 50
        FOR UPDATE SKIP LOCKED
    ), leased AS (
        UPDATE webhook_deliveries d SET next_attempt_at = now() + interval '60 seconds'
        FROM due WHERE d.id = due.id
        RETURNING d.id, d.attempt_count, d.event_id, d.endpoint_id
    )
    SELECT l.id, l.attempt_count, e.id AS event_id, e.type AS event_type, e.payload,
           e.created_at AS event_created_at, ep.url, ep.secret, ep.enabled AS endpoint_enabled
    FROM leased l
    JOIN events e ON e.id = l.event_id
    JOIN webhook_endpoints ep ON ep.id = l.endpoint_id";

pub fn spawn_worker(state: AppState) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(state.config.webhook_timeout)
            .build()
            .expect("http client builds");
        let mut tick = tokio::time::interval(state.config.webhook_poll_interval);
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&state.pool, &client, &state.config.webhook_retry_delays).await {
                tracing::error!(error = %e, "webhook worker pass failed");
            }
        }
    });
}

async fn run_once(pool: &PgPool, client: &reqwest::Client, delays: &[Duration]) -> Result<(), sqlx::Error> {
    let due: Vec<DueDelivery> = sqlx::query_as(LEASE_SQL).fetch_all(pool).await?;
    let mut sends = JoinSet::new();
    for delivery in due {
        let (pool, client, delays) = (pool.clone(), client.clone(), delays.to_vec());
        sends.spawn(async move { deliver(&pool, &client, &delays, delivery).await });
    }
    while let Some(result) = sends.join_next().await {
        if let Ok(Err(e)) = result {
            tracing::error!(error = %e, "failed to record webhook delivery result");
        }
    }
    Ok(())
}

async fn deliver(pool: &PgPool, client: &reqwest::Client, delays: &[Duration], d: DueDelivery) -> Result<(), sqlx::Error> {
    // An event committed concurrently with the endpoint being disabled can still have a delivery row.
    if !d.endpoint_enabled {
        sqlx::query("UPDATE webhook_deliveries SET status = 'failed', last_error = 'endpoint_disabled' WHERE id = $1 AND status = 'pending'")
            .bind(d.id)
            .execute(pool)
            .await?;
        return Ok(());
    }

    let body = serde_json::to_vec(&json!({
        "id": d.event_id,
        "type": d.event_type,
        "created_at": d.event_created_at,
        "data": d.payload,
    }))
    .expect("event serializes");
    let signature = signing::signature_header(&d.secret, Utc::now().timestamp(), &body);

    let result = client
        .post(&d.url)
        .header("content-type", "application/json")
        .header("webhook-id", d.event_id.to_string())
        .header("webhook-signature", signature)
        .body(body)
        .send()
        .await;
    let (response_code, error) = match result {
        Ok(resp) if resp.status().is_success() => (Some(resp.status().as_u16() as i32), None),
        Ok(resp) => (Some(resp.status().as_u16() as i32), Some(format!("HTTP {}", resp.status().as_u16()))),
        Err(e) if e.is_timeout() => (None, Some("timeout".to_owned())),
        Err(e) => (None, Some(format!("connection error: {e}"))),
    };
    let attempt = d.attempt_count + 1;

    let Some(error) = error else {
        // Recorded even if the endpoint was disabled mid-send: the receiver did get it.
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', attempt_count = $2, last_response_code = $3,
             last_error = NULL, delivered_at = now() WHERE id = $1",
        )
        .bind(d.id)
        .bind(attempt)
        .bind(response_code)
        .execute(pool)
        .await?;
        tracing::info!(delivery_id = %d.id, event_id = %d.event_id, event_type = %d.event_type, attempt, "webhook delivered");
        return Ok(());
    };

    match delays.get(attempt as usize - 1) {
        Some(&delay) => {
            let next = Utc::now() + chrono::Duration::from_std(jitter(delay)).expect("delay fits");
            sqlx::query(
                "UPDATE webhook_deliveries SET attempt_count = $2, last_response_code = $3, last_error = $4, next_attempt_at = $5 WHERE id = $1 AND status = 'pending'",
            )
            .bind(d.id)
            .bind(attempt)
            .bind(response_code)
            .bind(&error)
            .bind(next)
            .execute(pool)
            .await?;
            tracing::warn!(delivery_id = %d.id, event_id = %d.event_id, attempt, %error, next_attempt_at = %next, "webhook delivery failed; will retry");
        }
        None => {
            sqlx::query("UPDATE webhook_deliveries SET status = 'failed', attempt_count = $2, last_response_code = $3, last_error = $4 WHERE id = $1 AND status = 'pending'")
                .bind(d.id)
                .bind(attempt)
                .bind(response_code)
                .bind(&error)
                .execute(pool)
                .await?;
            tracing::error!(delivery_id = %d.id, event_id = %d.event_id, attempt, %error, url = %d.url, "webhook retries exhausted; would email the business");
        }
    }
    Ok(())
}

/// ±20% so endpoints that fail together do not retry in lockstep. Integer math only.
fn jitter(delay: Duration) -> Duration {
    let ms = delay.as_millis() as u64;
    let spread = ms / 5;
    Duration::from_millis(ms - spread + rand::thread_rng().gen_range(0..=2 * spread))
}
