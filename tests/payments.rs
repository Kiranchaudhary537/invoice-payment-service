//! The three tests the assignment requires (concurrency, idempotency, PSP failure via tok_timeout),
//! plus a second PSP-failure test for tok_network_error.
//!
//! Each test runs the real API and the real mock PSP in-process on random ports, against a fresh
//! Postgres database (created on the server in TEST_DATABASE_URL), so the mock's charge counter
//! can be asserted directly.
//!
//!   docker compose up -d db && cargo test

use std::{sync::Arc, time::Duration};

use dodo::{
    app,
    config::{Bootstrap, Config},
    mock_psp::{self, MockConfig, MockState},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{net::TcpListener, time::Instant};
use uuid::Uuid;

const API_KEY: &str = "dodo_test0001_TestKeyForIntegrationTestsOnly00";

#[derive(Clone)]
struct TestApp {
    base: String,
    http: reqwest::Client,
    psp: Arc<MockState>,
}

async fn spawn_app(mock: MockConfig, tweak: impl FnOnce(&mut Config)) -> TestApp {
    let admin_url = std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://dodo:dodo@localhost:5433/postgres".into());
    let db_name = format!("dodo_test_{}", Uuid::now_v7().simple());
    let admin = PgPool::connect(&admin_url).await.expect("connect to TEST_DATABASE_URL (is `docker compose up -d db` running?)");
    sqlx::query(&format!("CREATE DATABASE {db_name}")).execute(&admin).await.unwrap();
    let database_url = format!("{}/{db_name}", admin_url.rsplit_once('/').unwrap().0);

    let psp = Arc::new(MockState::default());
    let psp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let psp_url = format!("http://{}", psp_listener.local_addr().unwrap());
    tokio::spawn(axum::serve(psp_listener, mock_psp::router(mock, psp.clone())).into_future());

    let mut config = Config::with_defaults(database_url);
    config.psp_base_url = psp_url;
    config.bootstrap = Some(Bootstrap {
        api_key: API_KEY.into(),
        business_name: "Test Business".into(),
        business_email: "test@example.com".into(),
        webhook_url: None,
        webhook_secret: None,
    });
    tweak(&mut config);
    let state = app::init(config).await.unwrap();
    app::spawn_background(&state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(axum::serve(listener, app::router(state)).into_future());

    TestApp { base, http: reqwest::Client::new(), psp }
}

impl TestApp {
    async fn post(&self, path: &str, idempotency_key: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self.http.post(format!("{}{path}", self.base)).bearer_auth(API_KEY).json(&body);
        if let Some(key) = idempotency_key {
            req = req.header("Idempotency-Key", key);
        }
        let resp = req.send().await.unwrap();
        (resp.status().as_u16(), resp.json().await.unwrap())
    }

    async fn get(&self, path: &str) -> Value {
        let resp = self.http.get(format!("{}{path}", self.base)).bearer_auth(API_KEY).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }

    /// Customer + draft invoice of $25.50 (2 x 1000 + 1 x 550 cents), finalized to `open`.
    async fn open_invoice(&self) -> String {
        let (s, customer) = self.post("/v1/customers", None, json!({ "name": "Ada", "email": format!("{}@example.com", Uuid::now_v7()) })).await;
        assert_eq!(s, 201, "{customer}");
        let (s, invoice) = self
            .post(
                "/v1/invoices",
                None,
                json!({
                    "customer_id": customer["id"],
                    "due_date": "2099-01-01T00:00:00Z",
                    "line_items": [
                        { "description": "Widget", "quantity": 2, "unit_amount_cents": 1000 },
                        { "description": "Shipping", "quantity": 1, "unit_amount_cents": 550 }
                    ]
                }),
            )
            .await;
        assert_eq!(s, 201, "{invoice}");
        assert_eq!(invoice["total_cents"], 2550);
        let id = invoice["id"].as_str().unwrap().to_owned();
        let (s, opened) = self.post(&format!("/v1/invoices/{id}/finalize"), None, json!({})).await;
        assert_eq!(s, 200, "{opened}");
        id
    }

    async fn pay(&self, invoice_id: &str, token: &str, key: &str) -> (u16, Value) {
        self.post(&format!("/v1/invoices/{invoice_id}/pay"), Some(key), json!({ "card_token": token })).await
    }
}

/// N concurrent POST /pay for one invoice: exactly one succeeds, exactly one charge, invoice paid.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_payments_charge_exactly_once() {
    let app = spawn_app(MockConfig::default(), |_| {}).await;
    let invoice_id = app.open_invoice().await;

    let requests = (0..20).map(|i| {
        let (app, invoice_id) = (app.clone(), invoice_id.clone());
        tokio::spawn(async move { app.pay(&invoice_id, "tok_success", &format!("concurrent-{i}")).await })
    });
    let mut results = Vec::new();
    for r in requests {
        results.push(r.await.unwrap());
    }

    let succeeded = results.iter().filter(|(s, _)| *s == 201).count();
    assert_eq!(succeeded, 1, "exactly one payment must succeed: {results:#?}");
    for (status, body) in results.iter().filter(|(s, _)| *s != 201) {
        assert_eq!(*status, 409, "losers must get 409: {body}");
        let code = body["error"]["code"].as_str().unwrap();
        assert!(["concurrent_modification", "payment_in_progress", "invoice_already_paid"].contains(&code), "unexpected code {code}");
    }

    assert_eq!(app.psp.charge_requests(), 1, "the PSP must be called exactly once");
    assert_eq!(app.psp.charges_with_status("succeeded"), 1, "no double charge at the PSP");
    let invoice = app.get(&format!("/v1/invoices/{invoice_id}")).await;
    assert_eq!(invoice["status"], "paid");
    let attempts = invoice["payment_attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["status"], "succeeded");
}

/// Same Idempotency-Key twice: identical response, and the PSP is not called a second time.
/// The same key with a different body is rejected (DESIGN.md §3d).
#[tokio::test]
async fn retried_payment_with_same_key_replays_without_second_charge() {
    let app = spawn_app(MockConfig::default(), |_| {}).await;
    let invoice_id = app.open_invoice().await;

    let first = app.pay(&invoice_id, "tok_success", "order-42-attempt-1").await;
    let second = app.pay(&invoice_id, "tok_success", "order-42-attempt-1").await;
    assert_eq!(first.0, 201, "{}", first.1);
    assert_eq!(first, second, "a retry must return the stored response");
    assert_eq!(app.psp.charge_requests(), 1, "a retry must not reach the PSP");

    let reused = app.pay(&invoice_id, "tok_card_declined", "order-42-attempt-1").await;
    assert_eq!(reused.0, 422);
    assert_eq!(reused.1["error"]["code"], "idempotency_key_reuse");
    assert_eq!(app.psp.charge_requests(), 1);
}

/// tok_timeout: the endpoint answers within the client timeout instead of hanging, the invoice is
/// left open with a pending attempt that blocks a second charge, and the reconciler later records
/// the PSP's eventual success. Delays are scaled down (PSP settles at 3 s, client gives up at 1 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn psp_timeout_leaves_invoice_consistent_and_reconciles() {
    let mock = MockConfig { timeout_delay: Duration::from_secs(3), ..MockConfig::default() };
    let app = spawn_app(mock, |c| {
        c.psp_timeout = Duration::from_secs(1);
        c.reconcile_interval = Duration::from_millis(500);
        c.reconcile_min_age = Duration::from_millis(1500);
    })
    .await;
    let invoice_id = app.open_invoice().await;

    let started = Instant::now();
    let (status, body) = app.pay(&invoice_id, "tok_timeout", "slow-key").await;
    assert_eq!(status, 202, "{body}");
    assert!(started.elapsed() < Duration::from_secs(2), "the endpoint must not hang on the PSP");
    assert_eq!(body["payment_attempt"]["status"], "pending");
    assert_eq!(body["payment_attempt"]["last_error"], "psp_timeout");

    let invoice = app.get(&format!("/v1/invoices/{invoice_id}")).await;
    assert_eq!(invoice["status"], "open", "a timeout is not a failure and not a success");

    let (status, body) = app.pay(&invoice_id, "tok_success", "impatient-key").await;
    assert_eq!((status, body["error"]["code"].as_str()), (409, Some("payment_in_progress")), "no second charge while the first is unknown");
    let (status, body) = app.pay(&invoice_id, "tok_timeout", "slow-key").await;
    assert_eq!((status, body["error"]["code"].as_str()), (409, Some("request_in_progress")));

    let deadline = Instant::now() + Duration::from_secs(15);
    let invoice = loop {
        let invoice = app.get(&format!("/v1/invoices/{invoice_id}")).await;
        if invoice["status"] != "open" || Instant::now() > deadline {
            break invoice;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(invoice["status"], "paid", "the reconciler must resolve the pending attempt: {invoice}");
    let attempts = invoice["payment_attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["status"], "succeeded");
    assert_eq!(app.psp.charges_with_status("succeeded"), 1);
    assert_eq!(app.psp.charge_requests(), 1);

    let (status, body) = app.pay(&invoice_id, "tok_timeout", "slow-key").await;
    assert_eq!(status, 201, "after reconciliation the original key replays the final outcome: {body}");
}

/// tok_network_error: the PSP answers 500 without recording a charge. The endpoint returns 202
/// (outcome unknown), the reconciler then finds no charge at the PSP and fails the attempt, and the
/// invoice is payable again: not stuck, and never charged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn psp_network_error_fails_attempt_and_leaves_invoice_payable() {
    let app = spawn_app(MockConfig::default(), |c| {
        c.psp_timeout = Duration::from_secs(1);
        c.reconcile_interval = Duration::from_millis(500);
        c.reconcile_min_age = Duration::from_millis(1500);
    })
    .await;
    let invoice_id = app.open_invoice().await;

    let (status, body) = app.pay(&invoice_id, "tok_network_error", "flaky-key").await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["payment_attempt"]["status"], "pending");
    assert_eq!(body["payment_attempt"]["last_error"], "psp_http_500");

    let deadline = Instant::now() + Duration::from_secs(10);
    let attempt = loop {
        let invoice = app.get(&format!("/v1/invoices/{invoice_id}")).await;
        let attempt = invoice["payment_attempts"][0].clone();
        if attempt["status"] != "pending" || Instant::now() > deadline {
            assert_eq!(invoice["status"], "open", "a network error must not move the invoice: {invoice}");
            break attempt;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(attempt["status"], "failed", "the reconciler must resolve the pending attempt: {attempt}");
    assert_eq!(attempt["failure_code"], "psp_unreachable");
    assert_eq!(app.psp.charges_with_status("succeeded"), 0, "no money moved");

    let (status, body) = app.pay(&invoice_id, "tok_network_error", "flaky-key").await;
    assert_eq!((status, body["error"]["code"].as_str()), (402, Some("psp_unreachable")), "the original key replays the final outcome");
    let (status, body) = app.pay(&invoice_id, "tok_success", "second-try").await;
    assert_eq!(status, 201, "the invoice is payable again: {body}");
}
