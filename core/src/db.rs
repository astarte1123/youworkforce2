use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Begin a transaction on the runtime (app_rt) pool with the tenant bound to it.
/// RLS policies read current_setting('app.current_tenant'); set_config(...,true)
/// makes it transaction-local so the connection pool cannot leak context.
pub async fn begin_tenant<'a>(pool: &'a PgPool, tenant_id: Uuid) -> anyhow::Result<Transaction<'a, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('app.current_tenant', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
