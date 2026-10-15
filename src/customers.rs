use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::Auth,
    error::ApiError,
    extract::{ApiQuery, ListParams, idempotency_key, parse_id, parse_json},
    idempotency::{request_hash, run_in_tx},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateCustomer {
    name: String,
    email: String,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

pub async fn create(State(state): State<AppState>, auth: Auth, headers: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    let (input, canonical) = parse_json::<CreateCustomer>(&body)?;
    let name = input.name.trim().to_owned();
    let email = input.email.trim().to_lowercase();
    if name.is_empty() || name.len() > 200 {
        return Err(ApiError::Validation("name must be 1-200 characters".into()));
    }
    if email.len() > 320 || !email.contains('@') {
        return Err(ApiError::Validation("email must be a valid email address".into()));
    }
    let key = idempotency_key(&headers)?;
    let hash = request_hash("POST", "/v1/customers", &canonical);
    let business_id = auth.business_id;
    run_in_tx(&state, business_id, key, hash, StatusCode::CREATED, move |conn| {
        Box::pin(async move { insert(conn, business_id, &name, &email).await })
    })
    .await
}

async fn insert(conn: &mut PgConnection, business_id: Uuid, name: &str, email: &str) -> Result<Value, ApiError> {
    // ON CONFLICT keeps the transaction usable, so the 409 can still be recorded for the idempotency key.
    let created: Option<Customer> = sqlx::query_as(
        "INSERT INTO customers (id, business_id, name, email) VALUES ($1, $2, $3, $4)
         ON CONFLICT (business_id, email) DO NOTHING
         RETURNING id, name, email, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(business_id)
    .bind(name)
    .bind(email)
    .fetch_optional(&mut *conn)
    .await?;
    match created {
        Some(c) => Ok(serde_json::to_value(c).expect("customer serializes")),
        None => {
            let id: Uuid = sqlx::query_scalar("SELECT id FROM customers WHERE business_id = $1 AND email = $2")
                .bind(business_id)
                .bind(email)
                .fetch_one(&mut *conn)
                .await?;
            Err(ApiError::CustomerAlreadyExists { id })
        }
    }
}

pub async fn get(State(state): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Json<Customer>, ApiError> {
    let id = parse_id(&id, "customer")?;
    let customer = sqlx::query_as("SELECT id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2")
        .bind(id)
        .bind(auth.business_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::NotFound("customer"))?;
    Ok(Json(customer))
}

/// Newest first; `next_cursor` is the last id on the page (UUIDv7 ids sort by creation time).
pub async fn list(State(state): State<AppState>, auth: Auth, ApiQuery(params): ApiQuery<ListParams>) -> Result<Json<Value>, ApiError> {
    let limit = params.limit()?;
    let rows: Vec<Customer> = sqlx::query_as(
        "SELECT id, name, email, created_at FROM customers
         WHERE business_id = $1 AND ($2::uuid IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(params.cursor()?)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let data: Vec<Customer> = rows.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more { data.last().map(|c| c.id) } else { None };
    Ok(Json(json!({ "data": data, "next_cursor": next_cursor })))
}
