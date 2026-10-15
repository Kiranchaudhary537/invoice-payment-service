//! Transactional outbox: an event and its per-endpoint delivery rows are written in the same
//! transaction as the state change, so an event exists if and only if the change committed.
//! Delivery happens later in `webhooks::spawn_worker`, never on the request path.

use std::collections::HashMap;

use axum::{
    Json,
    extract::State,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::Auth,
    error::ApiError,
    extract::{ApiQuery, ListParams},
};

pub const INVOICE_CREATED: &str = "invoice.created";
pub const INVOICE_PAID: &str = "invoice.paid";
pub const INVOICE_PAYMENT_FAILED: &str = "invoice.payment_failed";

pub async fn emit(conn: &mut PgConnection, business_id: Uuid, event_type: &str, invoice_id: Uuid, payload: Value) -> Result<(), ApiError> {
    let event_id = Uuid::now_v7();
    sqlx::query("INSERT INTO events (id, business_id, type, invoice_id, payload) VALUES ($1, $2, $3, $4, $5)")
        .bind(event_id)
        .bind(business_id)
        .bind(event_type)
        .bind(invoice_id)
        .bind(&payload)
        .execute(&mut *conn)
        .await?;

    let endpoints: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM webhook_endpoints WHERE business_id = $1 AND enabled")
        .bind(business_id)
        .fetch_all(&mut *conn)
        .await?;
    for endpoint_id in &endpoints {
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, event_id, endpoint_id, status, next_attempt_at) VALUES ($1, $2, $3, 'pending', now())",
        )
        .bind(Uuid::now_v7())
        .bind(event_id)
        .bind(endpoint_id)
        .execute(&mut *conn)
        .await?;
    }
    tracing::info!(%event_id, event_type, %invoice_id, deliveries = endpoints.len(), "event recorded");
    Ok(())
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    #[sqlx(rename = "type")]
    event_type: String,
    invoice_id: Uuid,
    payload: Value,
    created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow, Serialize)]
struct DeliveryRow {
    #[serde(skip)]
    event_id: Uuid,
    endpoint_id: Uuid,
    status: String,
    attempt_count: i32,
    next_attempt_at: DateTime<Utc>,
    last_response_code: Option<i32>,
    last_error: Option<String>,
    delivered_at: Option<DateTime<Utc>>,
}

/// GET /v1/events?cursor=<last seen event id> — oldest first, so a business can catch up on
/// anything its webhook endpoint missed. Each event includes its delivery status per endpoint.
pub async fn list(State(state): State<AppState>, auth: Auth, ApiQuery(params): ApiQuery<ListParams>) -> Result<Json<Value>, ApiError> {
    let limit = params.limit()?;
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT id, type, invoice_id, payload, created_at FROM events
         WHERE business_id = $1 AND ($2::uuid IS NULL OR id > $2)
         ORDER BY id ASC LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(params.cursor()?)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let rows: Vec<EventRow> = rows.into_iter().take(limit as usize).collect();

    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let deliveries: Vec<DeliveryRow> = sqlx::query_as(
        "SELECT event_id, endpoint_id, status, attempt_count, next_attempt_at, last_response_code, last_error, delivered_at
         FROM webhook_deliveries WHERE event_id = ANY($1) ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(&state.pool)
    .await?;
    let mut by_event: HashMap<Uuid, Vec<DeliveryRow>> = HashMap::new();
    for d in deliveries {
        by_event.entry(d.event_id).or_default().push(d);
    }

    let next_cursor = if has_more { rows.last().map(|r| r.id) } else { None };
    let data: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "type": r.event_type,
                "invoice_id": r.invoice_id,
                "created_at": r.created_at,
                "data": r.payload,
                "deliveries": by_event.remove(&r.id).unwrap_or_default(),
            })
        })
        .collect();
    Ok(Json(json!({ "data": data, "next_cursor": next_cursor })))
}
