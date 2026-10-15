use std::{env, sync::Arc, time::Duration};

use dodo::mock_psp::{self, MockConfig, MockState};

#[tokio::main]
async fn main() {
    dodo::init_tracing();
    let ms = |name: &str, default: u64| Duration::from_millis(env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default));
    let config = MockConfig {
        latency: ms("MOCK_LATENCY_MS", 100),
        timeout_delay: ms("MOCK_TIMEOUT_DELAY_MS", 30_000),
        sink_secret: env::var("WEBHOOK_SINK_SECRET").ok(),
    };
    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:4000".into());

    let listener = tokio::net::TcpListener::bind(&bind_addr).await.expect("failed to bind");
    tracing::info!(%bind_addr, "mock psp listening");
    axum::serve(listener, mock_psp::router(config, Arc::new(MockState::default())))
        .await
        .expect("server error");
}
