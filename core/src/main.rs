mod state; mod clock; mod auth; mod db; mod goalert; mod worker; mod api; mod crypto;

use std::sync::Arc;
use axum::{routing::{get, post}, Router, Json};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::broadcast;
use uuid::Uuid;
use sqlx::Row;
use state::{AppState, TenantEvent};

/// P3: cifra a riposo i segreti upstream ancora in chiaro (percorso di migrazione
/// idempotente: salta cio' che e' gia' "enc:v1:"). Richiede YW_SECRET_KEY.
async fn crypto_migrate(st: &AppState) -> anyhow::Result<()> {
    let tr = sqlx::query("SELECT tenant_id,password FROM app.traccar_connections WHERE password NOT LIKE 'enc:v1:%'")
        .fetch_all(&st.owner_pool).await?;
    for r in tr {
        let tid: Uuid = r.get("tenant_id");
        let enc = st.secrets.encrypt(&r.get::<String,_>("password"));
        sqlx::query("UPDATE app.traccar_connections SET password=$2 WHERE tenant_id=$1")
            .bind(tid).bind(enc).execute(&st.owner_pool).await?;
    }
    let ga = sqlx::query("SELECT tenant_id,gql_pass FROM app.goalert_services WHERE gql_pass IS NOT NULL AND gql_pass NOT LIKE 'enc:v1:%'")
        .fetch_all(&st.owner_pool).await?;
    for r in ga {
        let tid: Uuid = r.get("tenant_id");
        let enc = st.secrets.encrypt(&r.get::<String,_>("gql_pass"));
        sqlx::query("UPDATE app.goalert_services SET gql_pass=$2 WHERE tenant_id=$1")
            .bind(tid).bind(enc).execute(&st.owner_pool).await?;
    }
    Ok(())
}

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
        secrets: Arc::new(crypto::Secrets::from_env()),
    };
    // P3: se la chiave e' presente, cifra a riposo i segreti ancora in chiaro.
    if st.secrets.enabled() {
        if let Err(e) = crypto_migrate(&st).await { tracing::warn!("crypto migrate: {e}"); }
        tracing::info!("encryption-at-rest attiva (YW_SECRET_KEY)");
    } else {
        tracing::warn!("YW_SECRET_KEY non impostata: segreti upstream in chiaro nel DB");
    }

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
        .route("/api/superadmin/tenants/:id/traccar", get(api::super_get_traccar).post(api::super_set_traccar))
        .route("/api/superadmin/tenants/:id/goalert/provision", post(api::goalert_provision))
        .route("/api/superadmin/tenants/:id/goalert/config", get(api::goalert_get_config).post(api::goalert_set_config))
        .route("/api/superadmin/tenants/:id/traccar/config", get(api::traccar_get_config).post(api::traccar_set_config))
        .route("/api/superadmin/tenants/:id/tools", get(api::super_list_tools).post(api::super_set_tool))
        .route("/api/superadmin/tenants/:id/admin", post(api::super_set_tenant_admin))
        .route("/api/superadmin/tenants/:id/logo", get(api::super_get_logo).post(api::super_set_logo).delete(api::super_del_logo))
        .route("/api/superadmin/tenants/:id/impersonate", post(api::super_impersonate))
        .route("/api/branding", get(api::branding))
        .route("/api/branding/logo", get(api::branding_logo))
        .route("/api/impersonate/stop", post(api::impersonate_stop))
        // tenant plane
        .route("/api/login", post(api::tenant_login))
        .route("/api/me", get(api::me))
        .route("/api/logout", post(api::logout))
        .route("/api/modules", get(api::list_modules))
        .route("/api/ui/features", get(api::ui_features))
        .route("/api/modules/:code/tools", get(api::list_module_tools))
        .route("/api/modules/:code/tools/:tool", post(api::set_module_tool))
        .route("/api/sites", get(api::list_sites).post(api::create_site))
        .route("/api/sites/:id", post(api::update_site).delete(api::delete_site))
        .route("/api/users", get(api::list_users).post(api::create_user))
        .route("/api/users/:id", post(api::update_user).delete(api::delete_user))
        .route("/api/users/:id/sites", get(api::get_user_sites).post(api::set_user_sites))
        .route("/api/devices", get(api::list_devices).post(api::create_device))
        .route("/api/devices/provision", post(api::provision_device))
        .route("/api/devices/:id", post(api::update_device).delete(api::delete_device))
        .route("/api/devices/:id/assign", post(api::assign_device))
        .route("/api/catalog/devices", get(api::list_catalog))
        .route("/api/catalog/facets", get(api::catalog_facets))
        .route("/api/assets", get(api::list_assets).post(api::create_asset))
        .route("/api/assets/:id", axum::routing::delete(api::delete_asset))
        .route("/api/groups", get(api::list_groups).post(api::create_group))
        .route("/api/groups/:id", axum::routing::delete(api::delete_group))
        .route("/api/groups/:id/members", post(api::add_group_member))
        .route("/api/groups/:gid/members/:uid", axum::routing::delete(api::remove_group_member))
        .route("/api/routing", post(api::set_routing))
        .route("/api/checkins/list", get(api::list_checkins))
        .route("/api/teams", get(api::list_teams).post(api::create_team))
        .route("/api/teams/:id", axum::routing::delete(api::delete_team))
        .route("/api/teams/:id/members", post(api::add_team_member))
        .route("/api/teams/:tid/members/:uid", axum::routing::delete(api::remove_team_member))
        .route("/api/shift-plans", get(api::list_shift_plans).post(api::create_shift_plan))
        .route("/api/shift-plans/:id", post(api::shift_plan_action).delete(api::delete_shift_plan))
        .route("/api/oncall", get(api::list_oncall).post(api::create_oncall))
        .route("/api/oncall/:id", axum::routing::delete(api::delete_oncall))
        .route("/api/ingest-tokens", get(api::list_ingest_tokens).post(api::create_ingest_token))
        .route("/api/ingest-tokens/:token", axum::routing::delete(api::delete_ingest_token))
        .route("/api/connectors", get(api::list_connectors))
        .route("/api/traccar/status", get(api::traccar_status))
        .route("/api/traccar/verify", post(api::traccar_verify))
        .route("/api/traccar/settings", get(api::traccar_settings_get).post(api::traccar_settings_set))
        .route("/api/events", post(api::ingest_event))
        .route("/api/ingress/traccar", post(api::ingest_traccar))
        .route("/api/ingress/traccar/position", post(api::ingest_traccar_position))
        .route("/api/devices/:id/positions", get(api::list_device_positions))
        .route("/api/geocode/reverse", get(api::reverse_geocode))
        .route("/api/devices/:id/command", post(api::device_command))
        .route("/api/devices/:id/commands", get(api::list_device_commands))
        .route("/api/devices/:id/command-types", get(api::device_command_types))
        .route("/api/geofences", get(api::list_geofences).post(api::create_geofence))
        .route("/api/geofences/:id", axum::routing::delete(api::delete_geofence))
        .route("/api/presence", get(api::list_presence))
        .route("/api/reports/route", get(api::report_route))
        .route("/api/reports/route.csv", get(api::report_route_csv))
        .route("/api/reports/presence", get(api::report_presence))
        .route("/api/reports/presence.csv", get(api::report_presence_csv))
        .route("/api/reports/alarms", get(api::report_alarms))
        .route("/api/reports/alarms.csv", get(api::report_alarms_csv))
        .route("/api/reports/alarms.txt", get(api::report_alarms_txt))
        .route("/api/alarms/map", get(api::alarms_map))
        .route("/api/beacons", get(api::list_beacons).post(api::save_beacon))
        .route("/api/beacons/:id", axum::routing::delete(api::delete_beacon))
        .route("/api/floorplans", get(api::list_floorplans).post(api::create_floorplan))
        .route("/api/floorplans/:id", post(api::update_floorplan).delete(api::delete_floorplan))
        .route("/api/floorplans/:id/image", get(api::get_floorplan_image))
        .route("/api/floorplans/:id/devices", get(api::floorplan_devices))
        .route("/api/beacons/:id/place", post(api::place_beacon))
        .route("/api/ingress/thingsboard", post(api::ingest_thingsboard))
        .route("/api/incidents", get(api::list_incidents))
        .route("/api/incidents/:id/timeline", get(api::incident_timeline))
        .route("/api/incidents/:id/ack", post(api::ack_incident))
        .route("/api/incidents/:id/close", post(api::close_incident))
        .route("/api/incidents/:id/assign", post(api::assign_incident))
        .route("/api/incidents/:id/note", post(api::add_incident_note))
        .route("/api/incidents/:id/goalert-status", get(api::goalert_status))
        .route("/api/incidents/:id/goalert-ack", post(api::goalert_ack))
        .route("/api/incidents/:id/goalert-escalate", post(api::goalert_escalate))
        .route("/api/oncall-now", get(api::goalert_oncall))
        .route("/api/goalert/routing", post(api::goalert_set_routing))
        .route("/api/sos", post(api::worker_sos))
        .route("/api/checkins", post(api::create_checkin))
        .route("/api/checkins/:id/confirm", post(api::confirm_checkin))
        .route("/api/shifts/start", post(api::shift_start))
        .route("/api/shifts/end", post(api::shift_end))
        .route("/api/shifts/current", get(api::shift_current))
        .route("/api/diary", post(api::diary_create).get(api::diary_list))
        // Prompt 9: ordini
        .route("/api/orders", post(api::create_order).get(api::list_orders))
        .route("/api/orders/:id/edit", post(api::edit_order))
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
        .layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024)) // upload piantine indoor
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
