use std::sync::Arc;

use axum::{
    Json, Router,
    middleware,
    routing::{delete, get, post},
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::{
    api_keys, bootstrap, config::Config, customers, error::ApiError, events, extract, idempotency, invoices, payments, psp::PspClient, reconciler,
    webhooks,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub psp: PspClient,
    pub config: Arc<Config>,
}

/// Connects, runs migrations and the demo bootstrap.
pub async fn init(config: Config) -> Result<AppState, sqlx::Error> {
    // The reconciler must never look at an attempt the request path may still be waiting on:
    // otherwise a 404 from the PSP could fail an attempt whose charge is still in flight.
    assert!(
        config.reconcile_min_age > config.psp_timeout,
        "RECONCILE_MIN_AGE_MS must be greater than PSP_TIMEOUT_MS"
    );
    let pool = PgPoolOptions::new().max_connections(20).connect(&config.database_url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    if let Some(b) = &config.bootstrap {
        bootstrap::run(&pool, b).await?;
    }
    let psp = PspClient::new(config.psp_base_url.clone(), config.psp_timeout);
    Ok(AppState { pool, psp, config: Arc::new(config) })
}

/// Webhook worker, payment reconciler and idempotency-key purge. None of them run on the request path.
pub fn spawn_background(state: &AppState) {
    webhooks::spawn_worker(state.clone());
    reconciler::spawn(state.clone());
    idempotency::spawn_purge(state.pool.clone(), state.config.idempotency_purge_interval);
}

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/customers", post(customers::create).get(customers::list))
        .route("/customers/{id}", get(customers::get))
        .route("/invoices", post(invoices::create).get(invoices::list))
        .route("/invoices/{id}", get(invoices::get))
        .route("/invoices/{id}/finalize", post(invoices::finalize))
        .route("/invoices/{id}/void", post(invoices::void))
        .route("/invoices/{id}/mark_uncollectible", post(invoices::mark_uncollectible))
        .route("/invoices/{id}/pay", post(payments::pay))
        .route("/api_keys", post(api_keys::create).get(api_keys::list))
        .route("/api_keys/{id}", delete(api_keys::revoke))
        .route("/webhook_endpoints", post(webhooks::create).get(webhooks::list))
        .route("/webhook_endpoints/{id}", delete(webhooks::disable))
        .route("/events", get(events::list));

    Router::new()
        .route("/health", get(health))
        .nest("/v1", v1)
        .fallback(|| async { ApiError::NotFound("route") })
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .layer(middleware::from_fn(extract::request_context))
        .with_state(state)
}

async fn health(axum::extract::State(state): axum::extract::State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    sqlx::query("SELECT 1").execute(&state.pool).await?;
    Ok(Json(json!({ "status": "ok" })))
}
