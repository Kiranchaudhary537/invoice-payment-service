pub mod api_keys;
pub mod app;
pub mod auth;
pub mod bootstrap;
pub mod config;
pub mod customers;
pub mod error;
pub mod events;
pub mod extract;
pub mod idempotency;
pub mod invoices;
pub mod mock_psp;
pub mod payments;
pub mod psp;
pub mod reconciler;
pub mod signing;
pub mod state_machine;
pub mod webhooks;

pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();
}
