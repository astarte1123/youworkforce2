mod state; mod clock; mod auth; mod db; mod goalert; mod worker; mod api;

use std::sync::Arc;
use axum::{routing::{get, post}, Router, Json};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::broadcast;
use uuid::Uuid;
use state::{AppState, TenantEvent};

fn env(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,sqlx=warn".into())).init();

    let owner_url = env("YW_DB_OWNER", "postgres://postgres:postgres@localhost:5432/youworkforce");
    let rt_url    = env("YW_DB_RT",    "postgres://app_rt:app_rt_pw@localhost:5432/youworkforce");

    let owner_pool = PgPoolOptions::new().max_connections(5)
        .after_connect(|c, _| Box::pin(async move {
            sqlx::query("SET search_path TO app, public").execute(c).await.map(|_| ()) }))
        .connect(&owner_url).await?;

    tracing::info!("running migrations");
    sqlx::migrate!("./migrations").run(&owner_pool).await?;
    bootstrap(&owner_pool).await?;

    let rt_pool = PgPoolOptions::new().max_connections(10)
        .after_connect(|c, _| Box::pin(async move {
            sqlx::query("SET search_path TO app, public").execute(c).await.map(|_| ()) }))
        .connect(&rt_url).await?;

    let (bus, _) = broadcast::channel(256);
    let st = AppState {
        owner_pool, rt_pool,
        clock: Arc::new(clock::Clock::new(env("TEST_CLOCK","1")=="1")),
        bus,
        http: reqwest::Client::new(),
        goalert_base: std::env::var("GOALERT_BASE").ok().filter(|s| !s.is_empty()),
        control_host: env("YW_CONTROL_HOST", "tenants.loneworker.eu"),
        base_domain: env("YW_BASE_DOMAIN", "loneworker.eu"),
    };

    tokio::spawn(worker::run(st.clone()));

    // Multi-instance live events: re-broadcast Postgres NOTIFY('yw_events') into the
    // local SSE bus, so SSE clients on ANY core instance receive events from all.
    {
        let bus = st.bus.clone();
        let url = owner_url.clone();
        tokio::spawn(async move {
            loop {
                match sqlx::postgres::PgListener::connect(&url).await {
                    Ok(mut listener) => {
                        if listener.listen("yw_events").await.is_err() {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await; continue;
                        }
                        tracing::info!("listening on NOTIFY yw_events");
                        while let Ok(n) = listener.recv().await {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(n.payload()) {
                                if let Some(tid) = v.get("tenant_id").and_then(|x| x.as_str())
                                    .and_then(|s| Uuid::parse_str(s).ok()) {
                                    let payload = v.get("payload").cloned().unwrap_or(json!({}));
                                    let _ = bus.send(TenantEvent { tenant_id: tid, payload });
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await; // reconnect
            }
        });
    }

    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"ok":true})) }))
        // control plane
        .route("/api/superadmin/login", post(api::superadmin_login))
        .route("/api/superadmin/tenants", post(api::create_tenant).get(api::list_tenants))
        .route("/api/superadmin/tenants/:id/modules", post(api::set_module).get(api::super_list_modules))
        .route("/api/superadmin/tenants/:id/goalert", post(api::super_set_goalert))
        // tenant plane
        .route("/api/login", post(api::tenant_login))
        .route("/api/me", get(api::me))
        .route("/api/logout", post(api::logout))
        .route("/api/modules", get(api::list_modules))
        .route("/api/sites", get(api::list_sites))
        .route("/api/users", get(api::list_users))
        .route("/api/events", post(api::ingest_event))
        .route("/api/ingress/traccar", post(api::ingest_traccar))
        .route("/api/ingress/thingsboard", post(api::ingest_thingsboard))
        .route("/api/incidents", get(api::list_incidents))
        .route("/api/incidents/:id/timeline", get(api::incident_timeline))
        .route("/api/incidents/:id/ack", post(api::ack_incident))
        .route("/api/incidents/:id/close", post(api::close_incident))
        .route("/api/sos", post(api::worker_sos))
        .route("/api/checkins", post(api::create_checkin))
        .route("/api/checkins/:id/confirm", post(api::confirm_checkin))
        .route("/api/shifts/start", post(api::shift_start))
        .route("/api/shifts/end", post(api::shift_end))
        .route("/api/shifts/current", get(api::shift_current))
        .route("/api/diary", post(api::diary_create).get(api::diary_list))
        // Prompt 9: ordini
        .route("/api/orders", post(api::create_order).get(api::list_orders))
        .route("/api/orders/:id/assign", post(api::order_assign))
        .route("/api/orders/:id/start", post(api::order_start))
        .route("/api/orders/:id/complete", post(api::order_complete))
        .route("/api/orders/:id/cancel", post(api::order_cancel))
        // Prompt 9: consuntivi
        .route("/api/timesheets", post(api::create_timesheet).get(api::list_timesheets))
        .route("/api/timesheets/:id", get(api::get_timesheet))
        .route("/api/timesheets/:id/lines", post(api::add_line))
        .route("/api/timesheets/:id/expenses", post(api::add_expense))
        .route("/api/timesheets/:id/submit", post(api::submit_timesheet))
        .route("/api/timesheets/:id/approve", post(api::approve_timesheet))
        .route("/api/timesheets/:id/request-changes", post(api::request_changes))
        .route("/api/timesheets/:id/export.csv", get(api::export_timesheet_csv))
        .route("/api/assistant", post(api::assistant_query))
        .route("/api/stream", get(api::stream))
        .route("/api/_test/advance", post(api::test_advance))
        .layer(axum::middleware::from_fn(api::csrf_guard))
        .with_state(st);

    let bind = env("YW_BIND", "0.0.0.0:8080");
    tracing::info!("core listening on {bind}");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Idempotent seed: default superadmin + demo tenant "acme" (contract: test recipients only).
async fn bootstrap(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM app.superadmins").fetch_one(pool).await?;
    if n == 0 {
        sqlx::query("INSERT INTO app.superadmins(email,pass_hash) VALUES($1,$2)")
            .bind("admin@loneworker.eu").bind(auth::hash_password("admin"))
            .execute(pool).await?;
        tracing::info!("seeded superadmin admin@loneworker.eu / admin");
    }
    let acme: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.tenants WHERE slug='acme'")
        .fetch_optional(pool).await?;
    if acme.is_none() {
        let tid: Uuid = sqlx::query_scalar("INSERT INTO app.tenants(slug,name) VALUES('acme','Acme SpA') RETURNING id")
            .fetch_one(pool).await?;
        sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state)
            SELECT $1, code, CASE WHEN code IN ('loneworker','goalert','traccar','iot','thingsboard','consuntivi','interventi','workforce','assistant') THEN 'attivo' ELSE 'non_abilitato' END
            FROM app.modules_catalog").bind(tid).execute(pool).await?;
        for (email, role) in [("admin@acme.loneworker.eu","tenant_admin"),
                              ("operatore@acme.loneworker.eu","operatore"),
                              ("pianificatore@acme.loneworker.eu","pianificatore"),
                              ("responsabile@acme.loneworker.eu","responsabile"),
                              ("mario@acme.loneworker.eu","lavoratore"),
                              ("auditor@acme.loneworker.eu","auditor")] {
            sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,$2,$3,$4)")
                .bind(tid).bind(email).bind(auth::hash_password("demo")).bind(role).execute(pool).await?;
        }
        let site: Uuid = sqlx::query_scalar("INSERT INTO app.sites(tenant_id,name) VALUES($1,'Milano') RETURNING id")
            .bind(tid).fetch_one(pool).await?;
        sqlx::query("INSERT INTO app.devices(tenant_id,site_id,external_id,source) VALUES($1,$2,'BADGE-001','traccar')")
            .bind(tid).bind(site).execute(pool).await?;
        sqlx::query("INSERT INTO app.ingest_tokens(token,tenant_id,source) VALUES('acme-ingest-token',$1,'traccar')")
            .bind(tid).execute(pool).await?;
        // second tenant to prove isolation (T02)
        let t2: Uuid = sqlx::query_scalar("INSERT INTO app.tenants(slug,name) VALUES('globex','Globex Srl') RETURNING id")
            .fetch_one(pool).await?;
        sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state)
            SELECT $1, code, CASE WHEN code='loneworker' THEN 'attivo' ELSE 'non_abilitato' END FROM app.modules_catalog")
            .bind(t2).execute(pool).await?;
        sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,'admin@globex.loneworker.eu',$2,'tenant_admin')")
            .bind(t2).bind(auth::hash_password("demo")).execute(pool).await?;
        sqlx::query("INSERT INTO app.ingest_tokens(token,tenant_id,source) VALUES('globex-ingest-token',$1,'traccar')")
            .bind(t2).execute(pool).await?;
        tracing::info!("seeded demo tenants acme + globex");
    }
    Ok(())
}
