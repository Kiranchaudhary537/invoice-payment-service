//! Mock payment processor (runs as its own binary/container).
//!
//! POST /charges        {reference, amount_cents, token} — outcome decided by the token:
//!   tok_success             -> succeeded after ~100 ms
//!   tok_insufficient_funds  -> failed (insufficient_funds) after ~100 ms
//!   tok_card_declined       -> failed (card_declined) after ~100 ms
//!   tok_timeout             -> recorded as processing, succeeds after 30 s (response sent then too)
//!   tok_network_error       -> HTTP 500, nothing recorded
//!   A repeated `reference` returns the existing charge and never charges twice.
//! GET  /charges/{reference} — status lookup used by the reconciler (404 if never received).
//! POST /webhook-sink        — demo webhook receiver: verifies the signature and logs the event.
//! GET  /webhook-sink        — the webhooks received so far (newest first).

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::signing;

#[derive(Clone, Debug)]
pub struct MockConfig {
    pub latency: Duration,
    pub timeout_delay: Duration,
    pub sink_secret: Option<String>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self { latency: Duration::from_millis(100), timeout_delay: Duration::from_secs(30), sink_secret: None }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Charge {
    pub reference: String,
    pub amount_cents: i64,
    pub status: String,
    pub psp_ref: Option<String>,
    pub code: Option<String>,
}

#[derive(Default)]
pub struct MockState {
    charges: Mutex<HashMap<String, Charge>>,
    charge_requests: AtomicUsize,
    received_webhooks: Mutex<Vec<Value>>,
}

impl MockState {
    /// Number of POST /charges requests received (including replays of an existing reference).
    pub fn charge_requests(&self) -> usize {
        self.charge_requests.load(Ordering::SeqCst)
    }

    pub fn charges_with_status(&self, status: &str) -> usize {
        self.charges.lock().unwrap().values().filter(|c| c.status == status).count()
    }

    fn set(&self, reference: &str, status: &str, psp_ref: Option<String>, code: Option<&str>) -> Charge {
        let mut charges = self.charges.lock().unwrap();
        let charge = charges.get_mut(reference).expect("charge exists");
        charge.status = status.into();
        charge.psp_ref = psp_ref;
        charge.code = code.map(Into::into);
        charge.clone()
    }
}

#[derive(Clone)]
struct Ctx {
    config: MockConfig,
    state: Arc<MockState>,
}

pub fn router(config: MockConfig, state: Arc<MockState>) -> Router {
    Router::new()
        .route("/charges", post(create_charge))
        .route("/charges/{reference}", get(get_charge))
        .route("/webhook-sink", post(receive_webhook).get(list_webhooks))
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .with_state(Ctx { config, state })
}

#[derive(Deserialize)]
struct ChargeRequest {
    reference: String,
    amount_cents: i64,
    token: String,
}

async fn create_charge(State(ctx): State<Ctx>, Json(req): Json<ChargeRequest>) -> Response {
    ctx.state.charge_requests.fetch_add(1, Ordering::SeqCst);
    tracing::info!(reference = %req.reference, token = %req.token, amount_cents = req.amount_cents, "charge requested");

    if req.token == "tok_network_error" {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal_error" }))).into_response();
    }
    if req.amount_cents <= 0 {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_amount" }))).into_response();
    }

    // Check-and-insert under one lock: a duplicate reference never creates a second charge.
    {
        let mut charges = ctx.state.charges.lock().unwrap();
        if let Some(existing) = charges.get(&req.reference) {
            tracing::info!(reference = %req.reference, status = %existing.status, "duplicate reference; returning existing charge");
            return Json(existing.clone()).into_response();
        }
        charges.insert(
            req.reference.clone(),
            Charge { reference: req.reference.clone(), amount_cents: req.amount_cents, status: "processing".into(), psp_ref: None, code: None },
        );
    }

    if req.token == "tok_timeout" {
        // Settle in a detached task so the charge completes even after the caller hangs up.
        let (state, reference, delay) = (ctx.state.clone(), req.reference.clone(), ctx.config.timeout_delay);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            state.set(&reference, "succeeded", Some(Uuid::now_v7().to_string()), None);
            tracing::info!(%reference, "slow charge settled as succeeded");
        });
        tokio::time::sleep(ctx.config.timeout_delay + Duration::from_millis(10)).await;
        let charge = ctx.state.charges.lock().unwrap().get(&req.reference).cloned();
        return Json(charge).into_response();
    }

    tokio::time::sleep(ctx.config.latency).await;
    let charge = match req.token.as_str() {
        "tok_success" => ctx.state.set(&req.reference, "succeeded", Some(Uuid::now_v7().to_string()), None),
        "tok_insufficient_funds" => ctx.state.set(&req.reference, "failed", None, Some("insufficient_funds")),
        "tok_card_declined" => ctx.state.set(&req.reference, "failed", None, Some("card_declined")),
        _ => ctx.state.set(&req.reference, "failed", None, Some("invalid_token")),
    };
    tracing::info!(reference = %charge.reference, status = %charge.status, "charge settled");
    Json(charge).into_response()
}

async fn get_charge(State(ctx): State<Ctx>, Path(reference): Path<String>) -> Response {
    match ctx.state.charges.lock().unwrap().get(&reference) {
        Some(charge) => Json(charge.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response(),
    }
}

async fn receive_webhook(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let (webhook_id, signature) = (header("webhook-id"), header("webhook-signature"));
    let event: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let event_type = event["type"].as_str().unwrap_or("unknown").to_owned();

    let verified = match &ctx.config.sink_secret {
        Some(secret) => signing::verify(secret, &signature, &body, Utc::now().timestamp()),
        None => Err("no WEBHOOK_SINK_SECRET configured"),
    };
    if let Err(reason) = verified {
        tracing::warn!(%webhook_id, %event_type, reason, "webhook REJECTED: signature verification failed");
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": reason }))).into_response();
    }

    tracing::info!(%webhook_id, %event_type, invoice_id = %event["data"]["invoice"]["id"], invoice_status = %event["data"]["invoice"]["status"], "webhook received, signature verified");
    let mut received = ctx.state.received_webhooks.lock().unwrap();
    received.push(json!({ "received_at": Utc::now(), "webhook_id": webhook_id, "signature_verified": true, "event": event }));
    if received.len() > 100 {
        received.remove(0);
    }
    Json(json!({ "received": true })).into_response()
}

async fn list_webhooks(State(ctx): State<Ctx>) -> Json<Value> {
    let received = ctx.state.received_webhooks.lock().unwrap();
    Json(json!({ "data": received.iter().rev().cloned().collect::<Vec<_>>() }))
}
