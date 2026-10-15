//! HTTP client for the payment processor. Treated as an unreliable external dependency:
//! every call has a hard timeout, and anything other than a clear answer is `Unknown`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    base_url: String,
    timeout: Duration,
}

#[derive(Debug, Clone)]
pub enum ChargeOutcome {
    Succeeded { psp_ref: String },
    Failed { code: String },
    /// We do not know whether money moved (timeout, connection error, 5xx, still processing).
    /// Never treated as a failure: the attempt stays pending until the reconciler finds out.
    Unknown { reason: String },
}

pub enum Lookup {
    Settled(ChargeOutcome),
    Processing,
    /// The PSP has no charge with this reference, so the charge never reached it.
    NotFound,
    Unavailable(String),
}

#[derive(Deserialize)]
struct ChargeResponse {
    status: String,
    psp_ref: Option<String>,
    code: Option<String>,
}

impl ChargeResponse {
    fn outcome(self) -> ChargeOutcome {
        match (self.status.as_str(), self.psp_ref) {
            ("succeeded", Some(psp_ref)) => ChargeOutcome::Succeeded { psp_ref },
            ("failed", _) => ChargeOutcome::Failed { code: self.code.unwrap_or_else(|| "payment_failed".into()) },
            ("processing", _) => ChargeOutcome::Unknown { reason: "psp_processing".into() },
            _ => ChargeOutcome::Unknown { reason: "psp_bad_response".into() },
        }
    }
}

fn transport_error(e: &reqwest::Error) -> String {
    if e.is_timeout() { "psp_timeout".into() } else { "psp_network_error".into() }
}

impl PspClient {
    pub fn new(base_url: String, timeout: Duration) -> Self {
        Self { http: reqwest::Client::new(), base_url, timeout }
    }

    /// `reference` is our payment attempt id; the PSP deduplicates on it, so re-sending the same
    /// reference can never create a second charge.
    pub async fn charge(&self, reference: Uuid, amount_cents: i64, token: &str) -> ChargeOutcome {
        let result = self
            .http
            .post(format!("{}/charges", self.base_url))
            .timeout(self.timeout)
            .json(&json!({ "reference": reference, "amount_cents": amount_cents, "token": token }))
            .send()
            .await;
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => return ChargeOutcome::Unknown { reason: transport_error(&e) },
        };
        if !resp.status().is_success() {
            return ChargeOutcome::Unknown { reason: format!("psp_http_{}", resp.status().as_u16()) };
        }
        match resp.json::<ChargeResponse>().await {
            Ok(body) => body.outcome(),
            Err(e) if e.is_timeout() => ChargeOutcome::Unknown { reason: "psp_timeout".into() },
            Err(_) => ChargeOutcome::Unknown { reason: "psp_bad_response".into() },
        }
    }

    pub async fn lookup(&self, reference: Uuid) -> Lookup {
        let result = self
            .http
            .get(format!("{}/charges/{reference}", self.base_url))
            .timeout(self.timeout)
            .send()
            .await;
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => return Lookup::Unavailable(transport_error(&e)),
        };
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Lookup::NotFound;
        }
        if !resp.status().is_success() {
            return Lookup::Unavailable(format!("psp_http_{}", resp.status().as_u16()));
        }
        match resp.json::<ChargeResponse>().await.map(ChargeResponse::outcome) {
            Ok(ChargeOutcome::Unknown { reason }) if reason == "psp_processing" => Lookup::Processing,
            Ok(ChargeOutcome::Unknown { reason }) => Lookup::Unavailable(reason),
            Ok(settled) => Lookup::Settled(settled),
            Err(_) => Lookup::Unavailable("psp_bad_response".into()),
        }
    }
}
