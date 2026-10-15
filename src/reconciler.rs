//! Resolves payment attempts left `pending` because the PSP timed out, errored, or we crashed
//! between the PSP call and recording its result. It asks the PSP by reference (= attempt id)
//! and records the answer with the same `finish_attempt` the request path uses.
//!
//! An attempt the PSP cannot settle yet is checked again on a later pass, behind the others. It is
//! never failed on our side, because money may have moved; past `STUCK_AFTER` it is logged as an
//! error for a human to resolve with the PSP.

use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    app::AppState,
    payments::finish_attempt,
    psp::{ChargeOutcome, Lookup},
};

/// A pending attempt this old needs a human: the invoice cannot be paid, voided or written off.
const STUCK_AFTER: Duration = Duration::from_secs(15 * 60);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(state.config.reconcile_interval);
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&state).await {
                tracing::error!(error = %e, "reconciler pass failed");
            }
        }
    });
}

pub async fn run_once(state: &AppState) -> Result<(), sqlx::Error> {
    // Older than the request path can still be working on (PSP timeout + margin; enforced at startup).
    let cutoff = Utc::now() - chrono::Duration::from_std(state.config.reconcile_min_age).expect("duration fits");
    let stale: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, created_at FROM payment_attempts
         WHERE status = 'pending' AND created_at < $1 AND (next_reconcile_at IS NULL OR next_reconcile_at <= now())
         ORDER BY COALESCE(next_reconcile_at, created_at) LIMIT 50",
    )
    .bind(cutoff)
    .fetch_all(&state.pool)
    .await?;

    for (attempt_id, created_at) in stale {
        let outcome = match state.psp.lookup(attempt_id).await {
            Lookup::Settled(outcome) => outcome,
            // The PSP has no record of this reference, so it never received the charge (e.g.
            // tok_network_error, or we crashed before calling it) and no money moved: fail the
            // attempt and let the customer retry. This relies on the PSP recording a reference
            // before charging and never forgetting it (DESIGN.md §3b).
            Lookup::NotFound => ChargeOutcome::Failed { code: "psp_unreachable".into() },
            Lookup::Processing => {
                tracing::debug!(%attempt_id, "PSP still processing; will check again");
                check_later(state, attempt_id, created_at).await?;
                continue;
            }
            Lookup::Unavailable(reason) => {
                tracing::warn!(%attempt_id, %reason, "PSP unavailable during reconciliation; will retry");
                check_later(state, attempt_id, created_at).await?;
                continue;
            }
        };
        match finish_attempt(&state.pool, attempt_id, outcome).await {
            Ok((code, _)) => tracing::info!(%attempt_id, status = code.as_u16(), "reconciled pending payment attempt"),
            Err(e) => tracing::error!(%attempt_id, error = ?e, "failed to record reconciled outcome"),
        }
    }
    Ok(())
}

async fn check_later(state: &AppState, attempt_id: Uuid, created_at: DateTime<Utc>) -> Result<(), sqlx::Error> {
    let age = (Utc::now() - created_at).to_std().unwrap_or_default();
    if age > STUCK_AFTER {
        tracing::error!(%attempt_id, age_secs = age.as_secs(), "payment attempt stuck pending; needs manual resolution with the PSP");
    }
    let next = Utc::now() + chrono::Duration::from_std(state.config.reconcile_interval).expect("duration fits");
    sqlx::query("UPDATE payment_attempts SET next_reconcile_at = $2 WHERE id = $1 AND status = 'pending'")
        .bind(attempt_id)
        .bind(next)
        .execute(&state.pool)
        .await?;
    Ok(())
}
