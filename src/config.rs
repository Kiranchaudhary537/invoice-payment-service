use std::{env, time::Duration};

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub psp_base_url: String,
    /// Upper bound for a single call to the PSP. Far below tok_timeout's 30 s.
    pub psp_timeout: Duration,
    pub reconcile_interval: Duration,
    /// Only attempts older than this are reconciled, so we never race a request that is still waiting on the PSP.
    pub reconcile_min_age: Duration,
    pub webhook_poll_interval: Duration,
    pub webhook_timeout: Duration,
    /// Delay before retry 1, 2, ... ; max attempts = len + 1.
    pub webhook_retry_delays: Vec<Duration>,
    pub idempotency_ttl: Duration,
    pub idempotency_purge_interval: Duration,
    pub bootstrap: Option<Bootstrap>,
}

#[derive(Clone, Debug)]
pub struct Bootstrap {
    pub api_key: String,
    pub business_name: String,
    pub business_email: String,
    pub webhook_url: Option<String>,
    pub webhook_secret: Option<String>,
}

impl Config {
    pub fn with_defaults(database_url: String) -> Self {
        Self {
            database_url,
            bind_addr: "0.0.0.0:3000".into(),
            psp_base_url: "http://localhost:4000".into(),
            psp_timeout: Duration::from_secs(5),
            reconcile_interval: Duration::from_secs(5),
            reconcile_min_age: Duration::from_secs(10),
            webhook_poll_interval: Duration::from_secs(1),
            webhook_timeout: Duration::from_secs(5),
            webhook_retry_delays: [10, 60, 300, 1800, 7200, 21600].map(Duration::from_secs).to_vec(),
            idempotency_ttl: Duration::from_secs(24 * 3600),
            idempotency_purge_interval: Duration::from_secs(3600),
            bootstrap: None,
        }
    }

    pub fn from_env() -> Self {
        let mut c = Self::with_defaults(env::var("DATABASE_URL").expect("DATABASE_URL must be set"));
        if let Ok(v) = env::var("BIND_ADDR") {
            c.bind_addr = v;
        }
        if let Ok(v) = env::var("PSP_BASE_URL") {
            c.psp_base_url = v;
        }
        set_ms("PSP_TIMEOUT_MS", &mut c.psp_timeout);
        set_ms("RECONCILE_INTERVAL_MS", &mut c.reconcile_interval);
        set_ms("RECONCILE_MIN_AGE_MS", &mut c.reconcile_min_age);
        set_ms("WEBHOOK_POLL_INTERVAL_MS", &mut c.webhook_poll_interval);
        set_ms("WEBHOOK_TIMEOUT_MS", &mut c.webhook_timeout);
        if let Ok(v) = env::var("WEBHOOK_RETRY_DELAYS_SECS") {
            c.webhook_retry_delays = v
                .split(',')
                .map(|s| Duration::from_secs(s.trim().parse().expect("WEBHOOK_RETRY_DELAYS_SECS must be comma-separated integers")))
                .collect();
        }
        if let Ok(v) = env::var("IDEMPOTENCY_TTL_SECS") {
            c.idempotency_ttl = Duration::from_secs(v.parse().expect("IDEMPOTENCY_TTL_SECS must be an integer"));
        }
        if let Ok(v) = env::var("IDEMPOTENCY_PURGE_INTERVAL_SECS") {
            c.idempotency_purge_interval = Duration::from_secs(v.parse().expect("IDEMPOTENCY_PURGE_INTERVAL_SECS must be an integer"));
        }
        if let Ok(api_key) = env::var("BOOTSTRAP_API_KEY") {
            c.bootstrap = Some(Bootstrap {
                api_key,
                business_name: env::var("BOOTSTRAP_BUSINESS_NAME").unwrap_or_else(|_| "Demo Business".into()),
                business_email: env::var("BOOTSTRAP_BUSINESS_EMAIL").unwrap_or_else(|_| "billing@demo.test".into()),
                webhook_url: env::var("BOOTSTRAP_WEBHOOK_URL").ok(),
                webhook_secret: env::var("BOOTSTRAP_WEBHOOK_SECRET").ok(),
            });
        }
        c
    }
}

fn set_ms(name: &str, target: &mut Duration) {
    if let Ok(v) = env::var(name) {
        *target = Duration::from_millis(v.parse().unwrap_or_else(|_| panic!("{name} must be an integer (milliseconds)")));
    }
}
