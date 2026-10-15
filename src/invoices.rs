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
    events,
    extract::{ApiQuery, ListParams, idempotency_key, parse_id, parse_json},
    idempotency::{request_hash, run_in_tx},
    payments::{self, AttemptRow},
    state_machine::{Action, InvoiceStatus, transition},
};

const MAX_LINE_ITEMS: usize = 100;
const MAX_QUANTITY: i64 = 10_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // a client-supplied total is rejected, never trusted
struct CreateInvoice {
    customer_id: Uuid,
    due_date: DateTime<Utc>,
    line_items: Vec<LineItemInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LineItemInput {
    description: String,
    quantity: i64,
    unit_amount_cents: i64,
}

#[derive(sqlx::FromRow)]
pub struct InvoiceRow {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: String,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub paid_at: Option<DateTime<Utc>>,
    pub voided_at: Option<DateTime<Utc>>,
    pub marked_uncollectible_at: Option<DateTime<Utc>>,
}

const INVOICE_COLUMNS: &str =
    "id, customer_id, status, currency, total_cents, due_date, created_at, finalized_at, paid_at, voided_at, marked_uncollectible_at";

#[derive(sqlx::FromRow, Serialize)]
pub struct LineItemRow {
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

fn invoice_json(row: &InvoiceRow) -> Value {
    json!({
        "id": row.id,
        "customer_id": row.customer_id,
        "status": row.status,
        "currency": row.currency,
        "total_cents": row.total_cents,
        "due_date": row.due_date,
        // Overdue is derived, not a state.
        "is_overdue": row.status == "open" && row.due_date < Utc::now(),
        "created_at": row.created_at,
        "finalized_at": row.finalized_at,
        "paid_at": row.paid_at,
        "voided_at": row.voided_at,
        "marked_uncollectible_at": row.marked_uncollectible_at,
    })
}

/// Full invoice representation (with line items, optionally with payment attempts).
pub async fn load(conn: &mut PgConnection, business_id: Uuid, id: Uuid, with_attempts: bool) -> Result<Option<Value>, ApiError> {
    let row: Option<InvoiceRow> = sqlx::query_as(&format!("SELECT {INVOICE_COLUMNS} FROM invoices WHERE id = $1 AND business_id = $2"))
        .bind(id)
        .bind(business_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let items: Vec<LineItemRow> = sqlx::query_as(
        "SELECT description, quantity, unit_amount_cents, amount_cents FROM invoice_line_items WHERE invoice_id = $1 ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let mut invoice = invoice_json(&row);
    invoice["line_items"] = json!(items);
    if with_attempts {
        let attempts: Vec<AttemptRow> = sqlx::query_as("SELECT * FROM payment_attempts WHERE invoice_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
        invoice["payment_attempts"] = json!(attempts.iter().map(payments::attempt_json).collect::<Vec<_>>());
    }
    Ok(Some(invoice))
}

pub async fn create(State(state): State<AppState>, auth: Auth, headers: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    let (input, canonical) = parse_json::<CreateInvoice>(&body)?;
    let key = idempotency_key(&headers)?;
    let hash = request_hash("POST", "/v1/invoices", &canonical);
    let business_id = auth.business_id;
    // Validation runs after the key is claimed, so a retry after `due_date` has passed still
    // replays the original 201 instead of failing the "due_date must be in the future" check.
    run_in_tx(&state, business_id, key, hash, StatusCode::CREATED, move |conn| {
        Box::pin(async move {
            let total_cents = validate(&input)?;
            insert(conn, business_id, input, total_cents).await
        })
    })
    .await
}

/// Validates line items and computes the total with checked integer math. Returns the total in cents.
fn validate(input: &CreateInvoice) -> Result<i64, ApiError> {
    if input.line_items.is_empty() || input.line_items.len() > MAX_LINE_ITEMS {
        return Err(ApiError::Validation(format!("line_items must contain 1-{MAX_LINE_ITEMS} items")));
    }
    if input.due_date <= Utc::now() {
        return Err(ApiError::Validation("due_date must be in the future".into()));
    }
    let overflow = || ApiError::Unprocessable { code: "amount_overflow", message: "Invoice total is too large".into() };
    let mut total: i64 = 0;
    for (i, item) in input.line_items.iter().enumerate() {
        let len = item.description.trim().chars().count();
        if len == 0 || len > 500 {
            return Err(ApiError::Validation(format!("line_items[{i}].description must be 1-500 characters")));
        }
        if !(1..=MAX_QUANTITY).contains(&item.quantity) {
            return Err(ApiError::Validation(format!("line_items[{i}].quantity must be between 1 and {MAX_QUANTITY}")));
        }
        if item.unit_amount_cents < 0 {
            return Err(ApiError::Validation(format!("line_items[{i}].unit_amount_cents must not be negative")));
        }
        let amount = item.unit_amount_cents.checked_mul(item.quantity).ok_or_else(overflow)?;
        total = total.checked_add(amount).ok_or_else(overflow)?;
    }
    Ok(total)
}

async fn insert(conn: &mut PgConnection, business_id: Uuid, input: CreateInvoice, total_cents: i64) -> Result<Value, ApiError> {
    let customer_exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)")
        .bind(input.customer_id)
        .bind(business_id)
        .fetch_one(&mut *conn)
        .await?;
    if !customer_exists {
        return Err(ApiError::NotFound("customer"));
    }

    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO invoices (id, business_id, customer_id, status, total_cents, due_date) VALUES ($1, $2, $3, 'draft', $4, $5)",
    )
    .bind(id)
    .bind(business_id)
    .bind(input.customer_id)
    .bind(total_cents)
    .bind(input.due_date)
    .execute(&mut *conn)
    .await?;
    for (position, item) in input.line_items.iter().enumerate() {
        // The CHECK constraint re-verifies amount_cents = quantity * unit_amount_cents in Postgres.
        sqlx::query(
            "INSERT INTO invoice_line_items (id, invoice_id, position, description, quantity, unit_amount_cents, amount_cents)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(position as i32)
        .bind(item.description.trim())
        .bind(item.quantity as i32)
        .bind(item.unit_amount_cents)
        .bind(item.unit_amount_cents * item.quantity) // cannot overflow: checked in validate()
        .execute(&mut *conn)
        .await?;
    }
    tracing::info!(invoice_id = %id, total_cents, "invoice created as draft");
    load(conn, business_id, id, false).await?.ok_or(ApiError::Internal)
}

pub async fn get(State(state): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id, "invoice")?;
    let mut conn = state.pool.acquire().await?;
    let invoice = load(&mut conn, auth.business_id, id, true).await?.ok_or(ApiError::NotFound("invoice"))?;
    Ok(Json(invoice))
}

#[derive(Deserialize)]
pub struct ListInvoiceParams {
    status: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

/// GET /v1/invoices?status=open — newest first, summaries without line items.
pub async fn list(State(state): State<AppState>, auth: Auth, ApiQuery(params): ApiQuery<ListInvoiceParams>) -> Result<Json<Value>, ApiError> {
    let page = ListParams { limit: params.limit, cursor: params.cursor };
    let limit = page.limit()?;
    if let Some(s) = &params.status {
        InvoiceStatus::parse(s)
            .ok_or_else(|| ApiError::Validation("status must be one of draft, open, paid, void, uncollectible".into()))?;
    }
    let rows: Vec<InvoiceRow> = sqlx::query_as(&format!(
        "SELECT {INVOICE_COLUMNS} FROM invoices
         WHERE business_id = $1 AND ($2::text IS NULL OR status = $2) AND ($3::uuid IS NULL OR id < $3)
         ORDER BY id DESC LIMIT $4"
    ))
    .bind(auth.business_id)
    .bind(&params.status)
    .bind(page.cursor()?)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let rows: Vec<InvoiceRow> = rows.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more { rows.last().map(|r| r.id) } else { None };
    let data: Vec<Value> = rows.iter().map(invoice_json).collect();
    Ok(Json(json!({ "data": data, "next_cursor": next_cursor })))
}

pub async fn finalize(state: State<AppState>, auth: Auth, path: Path<String>, headers: HeaderMap) -> Result<Response, ApiError> {
    run_action(state, auth, path, headers, Action::Finalize).await
}

pub async fn void(state: State<AppState>, auth: Auth, path: Path<String>, headers: HeaderMap) -> Result<Response, ApiError> {
    run_action(state, auth, path, headers, Action::Void).await
}

pub async fn mark_uncollectible(state: State<AppState>, auth: Auth, path: Path<String>, headers: HeaderMap) -> Result<Response, ApiError> {
    run_action(state, auth, path, headers, Action::MarkUncollectible).await
}

async fn run_action(State(state): State<AppState>, auth: Auth, Path(id): Path<String>, headers: HeaderMap, action: Action) -> Result<Response, ApiError> {
    let id = parse_id(&id, "invoice")?;
    let key = idempotency_key(&headers)?;
    let hash = request_hash("POST", &format!("/v1/invoices/{id}/{}", action.as_str()), b"");
    let business_id = auth.business_id;
    run_in_tx(&state, business_id, key, hash, StatusCode::OK, move |conn| {
        Box::pin(async move { apply_action(conn, business_id, id, action).await })
    })
    .await
}

/// Validates the transition, then writes it with a version compare-and-swap so a concurrent
/// change between our read and our write is detected instead of overwritten.
async fn apply_action(conn: &mut PgConnection, business_id: Uuid, id: Uuid, action: Action) -> Result<Value, ApiError> {
    let (status, version, total_cents): (String, i64, i64) =
        sqlx::query_as("SELECT status, version, total_cents FROM invoices WHERE id = $1 AND business_id = $2")
            .bind(id)
            .bind(business_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(ApiError::NotFound("invoice"))?;
    let from = InvoiceStatus::parse(&status).ok_or(ApiError::Internal)?;
    let to = transition(from, action)?;

    if action == Action::Finalize && total_cents == 0 {
        return Err(ApiError::Unprocessable { code: "zero_total", message: "An invoice with a zero total cannot be finalized".into() });
    }
    // Voiding or writing off while the PSP may still charge would leave money on a dead invoice.
    if from == InvoiceStatus::Open && payments::has_pending_attempt(conn, id).await? {
        return Err(ApiError::PaymentInProgress);
    }

    let timestamp_column = match action {
        Action::Finalize => "finalized_at",
        Action::Void => "voided_at",
        Action::MarkUncollectible => "marked_uncollectible_at",
        Action::Pay => unreachable!("pay has its own handler"),
    };
    let updated = sqlx::query(&format!(
        "UPDATE invoices SET status = $1, version = version + 1, updated_at = now(), {timestamp_column} = now()
         WHERE id = $2 AND version = $3"
    ))
    .bind(to.as_str())
    .bind(id)
    .bind(version)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ApiError::ConcurrentModification);
    }

    let invoice = load(conn, business_id, id, false).await?.ok_or(ApiError::Internal)?;
    if action == Action::Finalize {
        // invoice.created fires when the invoice becomes visible/payable for the customer.
        events::emit(conn, business_id, events::INVOICE_CREATED, id, json!({ "invoice": invoice })).await?;
        tracing::info!(invoice_id = %id, "would send invoice email to customer");
    }
    tracing::info!(invoice_id = %id, from = from.as_str(), to = to.as_str(), "invoice transitioned");
    Ok(invoice)
}
