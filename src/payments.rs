//! POST /v1/invoices/{id}/pay
//!
//! 1. Tx1 (milliseconds): claim the Idempotency-Key, check the invoice is open with no pending
//!    attempt, bump `invoices.version` with a compare-and-swap, insert a `pending` attempt. Commit.
//! 2. Call the PSP with reference = attempt id and a hard timeout. No transaction or lock is held.
//! 3. Tx2 (`finish_attempt`): record the outcome, mark the invoice paid if it succeeded, write the
//!    outbox event and store the response for the Idempotency-Key. Commit.
//!
//! If step 2 has no clear answer (timeout, network error), the attempt stays `pending` and the
//! endpoint returns 202. The reconciler later asks the PSP and runs the same `finish_attempt`.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::Auth,
    error::ApiError,
    events,
    extract::{idempotency_key, parse_id, parse_json},
    idempotency::{self, Claim, request_hash, respond},
    invoices,
    psp::ChargeOutcome,
    state_machine::{Action, InvoiceStatus, transition},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PayRequest {
    card_token: String,
}

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct AttemptRow {
    pub id: Uuid,
    pub business_id: Uuid,
    pub invoice_id: Uuid,
    pub amount_cents: i64,
    pub status: String,
    pub card_token: String,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
    pub last_error: Option<String>,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

pub fn attempt_json(a: &AttemptRow) -> Value {
    json!({
        "id": a.id,
        "invoice_id": a.invoice_id,
        "amount_cents": a.amount_cents,
        "status": a.status,
        "psp_ref": a.psp_ref,
        "failure_code": a.failure_code,
        "last_error": a.last_error,
        "created_at": a.created_at,
        "completed_at": a.completed_at,
    })
}

/// The response for an attempt in its current state: 201 succeeded, 402 failed, 202 pending.
fn attempt_response(attempt: &AttemptRow, invoice_status: &str) -> (StatusCode, Value) {
    let body = json!({ "payment_attempt": attempt_json(attempt), "invoice_status": invoice_status });
    match attempt.status.as_str() {
        "succeeded" => (StatusCode::CREATED, body),
        "failed" => ApiError::PaymentFailed {
            code: attempt.failure_code.clone().unwrap_or_else(|| "payment_failed".into()),
            attempt: attempt_json(attempt),
        }
        .parts(),
        _ => (StatusCode::ACCEPTED, body),
    }
}

pub async fn has_pending_attempt(conn: &mut PgConnection, invoice_id: Uuid) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payment_attempts WHERE invoice_id = $1 AND status = 'pending')")
        .bind(invoice_id)
        .fetch_one(conn)
        .await?)
}

/// Cases and how each is handled:
/// - Charged → invoice `paid`, 201. Declined → invoice stays `open` (try another card), 402.
/// - Draft/void invoice → 409 `invalid_state_transition`. Already paid → 409 `invoice_already_paid`.
/// - Same key + same body → stored response replayed, no second PSP call.
///   Same key + different body → 422. Same key still running → 409.
/// - Two pays at once → version compare-and-swap: one wins, the rest get 409 immediately.
/// - Pay or void while an attempt is pending → 409 `payment_in_progress`.
/// - PSP timeout/error → outcome unknown, not failed: stays `pending`, 202; the reconciler finishes it.
/// - Crash after the PSP charged → attempt still `pending`; the reconciler finishes it. No double
///   charge: one pending attempt per invoice, PSP dedupes on attempt id, one `succeeded` per invoice.
pub async fn pay(State(state): State<AppState>, auth: Auth, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    // Step 0, validate: a parseable invoice id, a required Idempotency-Key, a valid body. The hash
    // fingerprints the request so a retry can be told apart from a reused key with a new body.
    let invoice_id = parse_id(&id, "invoice")?;
    let key = idempotency_key(&headers)?.ok_or(ApiError::IdempotencyKeyRequired)?;
    let (req, canonical) = parse_json::<PayRequest>(&body)?;
    if req.card_token.is_empty() || req.card_token.len() > 100 {
        return Err(ApiError::Validation("card_token must be 1-100 characters".into()));
    }
    let hash = request_hash("POST", &format!("/v1/invoices/{invoice_id}/pay"), &canonical);

    // Step 1, reserve (Tx1, milliseconds): claim the key, check the invoice can be paid, win the
    // version CAS, insert a `pending` attempt, commit. A replayed key returns the stored response here.
    let attempt = match start_attempt(&state, auth.business_id, invoice_id, &key, &hash, &req.card_token).await? {
        Started::Attempt(a) => *a,
        Started::Replay(code, body) => return Ok(respond(code, body, true)),
    };
    tracing::info!(attempt_id = %attempt.id, %invoice_id, amount_cents = attempt.amount_cents, "payment attempt started");

    // Step 2, charge: no transaction is open here, so a slow PSP holds no lock and no DB connection.
    // The attempt id is the PSP's reference, so a resend can never create a second charge.
    match state.psp.charge(attempt.id, attempt.amount_cents, &req.card_token).await {
        // Step 3b, outcome unknown (timeout, 5xx, dropped connection): a timeout means "don't know",
        // not "no", so the attempt stays pending, we answer 202 and the reconciler finds out later.
        ChargeOutcome::Unknown { reason } => {
            tracing::warn!(attempt_id = %attempt.id, %reason, "PSP outcome unknown; attempt stays pending for the reconciler");
            let mut conn = state.pool.acquire().await?;
            let pending: Option<AttemptRow> =
                sqlx::query_as("UPDATE payment_attempts SET last_error = $2 WHERE id = $1 AND status = 'pending' RETURNING *")
                    .bind(attempt.id)
                    .bind(&reason)
                    .fetch_optional(&mut *conn)
                    .await?;
            // Not stored under the Idempotency-Key: a retry gets 409 request_in_progress until the
            // reconciler stores the final outcome, then replays that.
            let (code, body) = match pending {
                Some(attempt) => attempt_response(&attempt, InvoiceStatus::Open.as_str()),
                // The reconciler already recorded the outcome; answer with that.
                None => recorded_response(&mut conn, attempt.id).await?,
            };
            Ok(respond(code, body, false))
        }
        // Step 3a, outcome known: record it in Tx2 (shared with the reconciler) and answer 201 or 402.
        settled => {
            let (code, body) = finish_attempt(&state.pool, attempt.id, settled).await?;
            Ok(respond(code, body, false))
        }
    }
}

enum Started {
    Attempt(Box<AttemptRow>),
    Replay(StatusCode, Value),
}

async fn start_attempt(state: &AppState, business_id: Uuid, invoice_id: Uuid, key: &str, hash: &[u8], card_token: &str) -> Result<Started, ApiError> {
    let ttl = state.config.idempotency_ttl;
    let mut tx = state.pool.begin().await?;
    // 1a. New key: proceed. Finished key: replay. Different body: 422. Still running: 409.
    if let Claim::Replay(code, body) = idempotency::claim(&mut tx, business_id, key, hash, ttl).await? {
        return Ok(Started::Replay(code, body));
    }
    match reserve(&mut tx, business_id, invoice_id, key, card_token).await {
        Ok(attempt) => {
            tx.commit().await?;
            Ok(Started::Attempt(Box::new(attempt)))
        }
        Err(err) => {
            tx.rollback().await?;
            idempotency::store_error(&state.pool, business_id, key, hash, ttl, &err).await?;
            Err(err)
        }
    }
}

/// Optimistic concurrency: read the invoice's version, then only the request whose
/// `UPDATE ... WHERE version = <read version>` matches may create the attempt. Every other
/// concurrent request (pay, void, write-off) sees 0 rows and gets 409 instead of waiting on a lock.
async fn reserve(conn: &mut PgConnection, business_id: Uuid, invoice_id: Uuid, key: &str, card_token: &str) -> Result<AttemptRow, ApiError> {
    let (status, version, total_cents): (String, i64, i64) =
        sqlx::query_as("SELECT status, version, total_cents FROM invoices WHERE id = $1 AND business_id = $2")
            .bind(invoice_id)
            .bind(business_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(ApiError::NotFound("invoice"))?;
    // 1b. State machine: only an `open` invoice can be paid (draft/void → 409, paid → already paid).
    transition(InvoiceStatus::parse(&status).ok_or(ApiError::Internal)?, Action::Pay)?;
    // 1c. Money may already be moving for this invoice: never start a second attempt.
    if has_pending_attempt(conn, invoice_id).await? {
        return Err(ApiError::PaymentInProgress);
    }

    // 1d. Compare-and-swap: of several simultaneous requests that read the same version, exactly one
    // updates a row; the rest get 409 at once instead of queueing on a lock.
    let won = sqlx::query("UPDATE invoices SET version = version + 1, updated_at = now() WHERE id = $1 AND version = $2")
        .bind(invoice_id)
        .bind(version)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if won == 0 {
        return Err(ApiError::ConcurrentModification);
    }

    // 1e. Record the attempt as `pending` before calling the PSP, so a crash can't lose track of it.
    // Backstop: the partial unique index allows only one pending attempt per invoice.
    sqlx::query_as(
        "INSERT INTO payment_attempts (id, business_id, invoice_id, amount_cents, status, card_token, idempotency_key)
         VALUES ($1, $2, $3, $4, 'pending', $5, $6) RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(business_id)
    .bind(invoice_id)
    .bind(total_cents)
    .bind(card_token)
    .bind(key)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.constraint() == Some("payment_attempts_one_pending_idx") => ApiError::PaymentInProgress,
        _ => e.into(),
    })
}

/// The response for an attempt as currently recorded, with the invoice's current status.
async fn recorded_response(conn: &mut PgConnection, attempt_id: Uuid) -> Result<(StatusCode, Value), ApiError> {
    let attempt: AttemptRow = sqlx::query_as("SELECT * FROM payment_attempts WHERE id = $1").bind(attempt_id).fetch_one(&mut *conn).await?;
    let invoice_status: String = sqlx::query_scalar("SELECT status FROM invoices WHERE id = $1").bind(attempt.invoice_id).fetch_one(&mut *conn).await?;
    Ok(attempt_response(&attempt, &invoice_status))
}

/// Records a settled PSP outcome. Shared by the request path and the reconciler, and safe to call
/// twice: only the first call finds the attempt still `pending`.
pub async fn finish_attempt(pool: &PgPool, attempt_id: Uuid, outcome: ChargeOutcome) -> Result<(StatusCode, Value), ApiError> {
    let (status, psp_ref, failure_code) = match outcome {
        ChargeOutcome::Succeeded { psp_ref } => ("succeeded", Some(psp_ref), None),
        ChargeOutcome::Failed { code } => ("failed", None, Some(code)),
        ChargeOutcome::Unknown { .. } => unreachable!("only settled outcomes are finished"),
    };

    let mut tx = pool.begin().await?;
    // 3.1 Finish the attempt. `WHERE status = 'pending'` means only the first caller (request path or
    // reconciler) updates it; the second finds nothing and answers with what was recorded.
    let finished: Option<AttemptRow> = sqlx::query_as(
        "UPDATE payment_attempts SET status = $2, psp_ref = $3, failure_code = $4, completed_at = now()
         WHERE id = $1 AND status = 'pending' RETURNING *",
    )
    .bind(attempt_id)
    .bind(status)
    .bind(&psp_ref)
    .bind(&failure_code)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(attempt) = finished else {
        // Already finished by the other path; answer with what is recorded.
        return recorded_response(&mut tx, attempt_id).await;
    };

    // 3.2 On success, the invoice becomes paid, only from `open`.
    if attempt.status == "succeeded" {
        let marked = sqlx::query(
            "UPDATE invoices SET status = 'paid', paid_at = now(), version = version + 1, updated_at = now()
             WHERE id = $1 AND status = 'open'",
        )
        .bind(attempt.invoice_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if marked == 0 {
            // Cannot happen while a pending attempt blocks every other transition; if it ever does,
            // money moved for an invoice that is no longer open and a human must refund it.
            tracing::error!(attempt_id = %attempt.id, invoice_id = %attempt.invoice_id, "charge succeeded but invoice was not open; manual refund required");
        }
    }

    // 3.3 Outbox: the event and its webhook delivery rows commit with the state change, so a webhook
    // is sent if and only if the payment was recorded. The worker delivers it later.
    let invoice = invoices::load(&mut tx, attempt.business_id, attempt.invoice_id, false).await?.ok_or(ApiError::Internal)?;
    let event_type = if attempt.status == "succeeded" { events::INVOICE_PAID } else { events::INVOICE_PAYMENT_FAILED };
    events::emit(
        &mut tx,
        attempt.business_id,
        event_type,
        attempt.invoice_id,
        json!({ "invoice": invoice, "payment_attempt": attempt_json(&attempt) }),
    )
    .await?;

    // 3.4 Store the response under the Idempotency-Key in the same transaction, so a retry replays it.
    let (code, body) = attempt_response(&attempt, invoice["status"].as_str().unwrap_or("open"));
    idempotency::complete(&mut tx, attempt.business_id, &attempt.idempotency_key, code, &body).await?;
    tx.commit().await?;
    tracing::info!(attempt_id = %attempt.id, invoice_id = %attempt.invoice_id, status = %attempt.status, failure_code = ?attempt.failure_code, "payment attempt finished");
    Ok((code, body))
}
