use dodo::{app, config::Config};

#[tokio::main]
async fn main() {
    dodo::init_tracing();
    let config = Config::from_env();
    let bind_addr = config.bind_addr.clone();

    let state = app::init(config).await.expect("failed to initialise database");
    app::spawn_background(&state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await.expect("failed to bind");
    tracing::info!(%bind_addr, "invoice api listening");
    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("ctrl-c handler") };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutting down");
}
