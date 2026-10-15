use sqlx::PgPool;
use uuid::Uuid;

use crate::{api_keys, config::Bootstrap};

/// Creates the demo business, its API key and (optionally) a webhook endpoint pointing at the mock
/// PSP's sink. Safe to run on every start: an existing key prefix means it already ran.
/// Demo-only: a real deployment generates keys and shows them once.
pub async fn run(pool: &PgPool, b: &Bootstrap) -> Result<(), sqlx::Error> {
    let prefix = api_keys::parse_prefix(&b.api_key).expect("BOOTSTRAP_API_KEY must look like dodo_<8 alphanumerics>_<32 alphanumerics>");
    let mut tx = pool.begin().await?;

    let existing: Option<Uuid> = sqlx::query_scalar("SELECT business_id FROM api_keys WHERE prefix = $1")
        .bind(prefix)
        .fetch_optional(&mut *tx)
        .await?;
    let business_id = match existing {
        Some(id) => id,
        None => {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO businesses (id, name, email) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(&b.business_name)
                .bind(&b.business_email)
                .execute(&mut *tx)
                .await?;
            api_keys::insert(&mut tx, id, &b.api_key).await?;
            id
        }
    };

    if let (Some(url), Some(secret)) = (&b.webhook_url, &b.webhook_secret) {
        sqlx::query(
            "INSERT INTO webhook_endpoints (id, business_id, url, secret)
             SELECT $1, $2, $3, $4
             WHERE NOT EXISTS (SELECT 1 FROM webhook_endpoints WHERE business_id = $2 AND url = $3 AND enabled)",
        )
        .bind(Uuid::now_v7())
        .bind(business_id)
        .bind(url)
        .bind(secret)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    tracing::info!(%business_id, api_key_prefix = prefix, "bootstrap business ready");
    Ok(())
}
