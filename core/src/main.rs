mod state; mod clock; mod auth; mod db; mod worker; mod api; mod crypto;
#[allow(dead_code)] mod escalation; // #4 FASE 0: libreria pura (non ancora wirata; YWAlert resta il motore)
mod delivery; // #4 FASE 2: adapter di consegna in ombra (sink di prova; YWAlert resta il motore)
mod modules; // #3 FASE 1: registry manifest moduli (dipendenze + metadata)
mod teltonika; // edge nativo Teltonika Codec8/8E su TCP
mod track; // report di tracciamento nativi (percorsi, soste, riepilogo) dallo storico posizioni
mod mqtt; // broker MQTT nativo (IoT): ingest telemetria + downlink comandi su TCP 1883
mod dashboard; // servizio core: dashboard unica configurabile (widget + layout per-tenant)
mod privacy; // servizio core: GDPR, elenco utenti, ritenzione, registro attività (audit)
mod groups; // servizio core: gruppi di persone condivisi (CRUD + membri)
mod sites; // servizio core: sedi e asset/mezzi condivisi
mod iam; // servizio core: permessi (capability) e ruoli personalizzati
mod users; // servizio core: identità/utenti (CRUD anagrafica + ACL sedi); login/sessione restano in api.rs
mod scheduling; // motore regole/validazione pianificazione (puro, riusabile): turni oggi, poi reperibilità/ordini/giri

use std::sync::Arc;
use axum::{routing::{get, post, delete}, Router, Json};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::broadcast;
use uuid::Uuid;
use sqlx::Row;
use state::{AppState, TenantEvent};

/// Cifra a riposo i segreti ancora in chiaro (percorso di migrazione idempotente: salta ciò
/// che è già "enc:v1:"). L'unico segreto esterno è la chiave del provider AI. Richiede YW_SECRET_KEY.
async fn crypto_migrate(st: &AppState) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT tenant_id,api_key FROM app.ai_connections WHERE api_key<>'' AND api_key NOT LIKE 'enc:v1:%'")
        .fetch_all(&st.owner_pool).await?;
    for r in rows {
        let tid: Uuid = r.get("tenant_id");
        let enc = st.secrets.encrypt(&r.get::<String,_>("api_key"));
        if !enc.starts_with("enc:v1:") { continue; }   // YW_SECRET_KEY assente: resta in chiaro
        sqlx::query("UPDATE app.ai_connections SET api_key=$2 WHERE tenant_id=$1")
            .bind(tid).bind(enc).execute(&st.owner_pool).await?;
    }
    Ok(())
}

fn env(k: &str, d: &str) -> String { std::env::var(k).unwrap_or_else(|_| d.to_string()) }

// Sub-router IoT (telemetria/attributi/comandi/blueprint/monitor/alert): estratto dal router
// principale per contenere la profondità di codegen del tipo Router (evita lo stack-overflow di rustc).
fn iot_routes() -> Router<AppState> {
    Router::new()
        .route("/api/ingress/telemetry", post(api::ingest_telemetry))
        .route("/api/iot/devices/:id/telemetry", get(api::iot_telemetry_series))
        .route("/api/iot/devices/:id/telemetry/latest", get(api::iot_telemetry_latest))
        .route("/api/iot/devices/:id/attributes", get(api::iot_attributes_get).post(api::iot_attributes_set))
        .route("/api/iot/devices/:id/command", post(api::iot_command_post))
        .route("/api/iot/devices/:id/commands", get(api::iot_commands_list))
        .route("/api/iot/metrics", get(api::iot_metrics_list))
        .route("/api/iot/devices/:id/map", get(api::device_map_get).put(api::device_map_set))
        .route("/api/iot/blueprints", get(api::iot_blueprints_list))
        .route("/api/iot/activate", post(api::iot_activate))
        .route("/api/iot/monitor", get(api::iot_monitor))
        .route("/api/iot/alerts", get(api::iot_alerts_list))
        .route("/api/iot/alerts/summary", get(api::iot_alerts_summary))
        .route("/api/iot/alerts/:id/ack", post(api::iot_alert_ack))
        .route("/api/iot/alerts/:id/close", post(api::iot_alert_close))
        .route("/api/ingress/lorawan", post(api::ingest_lorawan))
        .route("/api/ingress/pull", get(api::iot_device_pull))
}
// Sub-router superadmin: estratto per contenere la profondità di codegen del tipo Router
// (evita lo STATUS_STACK_BUFFER_OVERRUN di rustc quando la catena principale cresce).
fn superadmin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/superadmin/login", post(api::superadmin_login))
        .route("/api/superadmin/tenants", post(api::create_tenant).get(api::list_tenants))
        .route("/api/superadmin/tenants/:id/status", post(api::set_tenant_status))
        .route("/api/superadmin/tenants/:id", axum::routing::delete(api::delete_tenant))
        .route("/api/superadmin/tenants/:id/modules", post(api::set_module).get(api::super_list_modules))
        .route("/api/superadmin/tenants/:id/ai", get(api::super_get_ai).post(api::super_set_ai))
        .route("/api/superadmin/tenants/:id/ai/verify", post(api::ai_verify))
        .route("/api/superadmin/plans", get(api::super_plans_list))
        .route("/api/superadmin/system", get(api::super_system))
        .route("/api/superadmin/system/alerts", get(api::super_system_alerts))
        .route("/api/superadmin/device-attempts", get(api::super_device_attempts))
        .route("/api/superadmin/system/settings", get(api::super_system_settings_get).post(api::super_system_settings_set))
        .route("/api/superadmin/scenarios", get(api::super_scenarios_list))
        .route("/api/superadmin/tenants/:id/scenarios", get(api::super_tenant_scenarios).post(api::super_apply_scenario))
        .route("/api/superadmin/tenants/:id/scenarios/bulk", post(api::super_bulk_scenarios))
        .route("/api/superadmin/tenants/:id/map-visibility", get(api::super_map_visibility_get).post(api::super_map_visibility_set))
        .route("/api/superadmin/tenants/:id/scenarios/:code", axum::routing::delete(api::super_remove_scenario))
        .route("/api/superadmin/tenants/:id/scenarios/:code/demo", post(api::super_scenario_demo))
        .route("/api/superadmin/tenants/:id/tools", get(api::super_list_tools).post(api::super_set_tool))
        .route("/api/superadmin/tenants/:id/admin", post(api::super_set_tenant_admin))
        .route("/api/superadmin/tenants/:id/logo", get(api::super_get_logo).post(api::super_set_logo).delete(api::super_del_logo))
        .route("/api/superadmin/tenants/:id/impersonate", post(api::super_impersonate))
}
// Sub-router aggregato (GDPR, dashboard, escalation, safety, …): estratto per contenere la
// profondità di codegen del tipo Router (evita lo stack-overflow di rustc).
fn extra_routes() -> Router<AppState> {
    Router::new()
        // #7 GDPR
        .route("/api/gdpr/export/:uid", get(privacy::gdpr_export))
        .route("/api/delivery-status", get(api::delivery_status_get))
        .route("/api/_test/stale-open-watchdog", post(api::test_stale_open_watchdog))
        // CA-8/CA-14: correzione dei consuntivi
        .route("/api/timesheets/:id/lines/:lid", post(api::update_line).delete(api::delete_line))
        .route("/api/timesheets/:id/lines/:lid/rate", post(api::set_line_rate))
        .route("/api/timesheets/:id/expenses/:eid", post(api::update_expense).delete(api::delete_expense))
        .route("/api/worker/heartbeat", post(api::worker_heartbeat))
        .route("/api/gdpr/anonymize/:uid", post(privacy::gdpr_anonymize))
        .route("/api/dashboard/layout", get(dashboard::dashboard_layout_get).put(dashboard::dashboard_layout_set))
        .route("/api/gdpr/retention", get(privacy::gdpr_retention_info))
        .route("/api/gdpr/users", get(privacy::gdpr_users_list))
        .route("/api/gdpr/users/export", get(privacy::gdpr_users_export))
        .route("/api/audit-log", get(privacy::audit_log_list))
        .route("/api/audit-log/export", get(privacy::audit_log_export))
        // #4 escalation nativo FASE 2 (consegna in ombra verso sink di prova): report + trigger
        .route("/api/escalation/delivery", get(api::escalation_delivery_report))
        .route("/api/escalation/delivery/run", post(api::escalation_delivery_run))
        // Worker Safety: trigger di sicurezza (vista+config aggregata) — tenant_admin
        .route("/api/safety/triggers", get(api::safety_triggers_get).post(api::safety_triggers_set))
        // Fase C: profili di protezione (worker+device+trigger+policy escalation in un record)
        .route("/api/safety/profiles", get(api::protection_profiles_list).post(api::protection_profile_create))
        .route("/api/safety/profiles/:id", axum::routing::put(api::protection_profile_update).delete(api::protection_profile_delete))
        .route("/api/safety/profiles/:id/members", post(api::protection_profile_member_add))
        .route("/api/safety/profiles/:id/members/:uid", axum::routing::delete(api::protection_profile_member_del))
        // FASE 3 cutover: lettura motore (admin) + set/rollback (superadmin)
        // ack inbound Twilio (firma validata; CSRF-exempt come /api/ingress/*)
        .route("/api/ingress/twilio/sms", post(api::twilio_ingress_sms))
        .route("/api/ingress/twilio/voice", post(api::twilio_ingress_voice))
        .route("/api/ingress/heartbeat/:token", post(api::ingest_heartbeat))
        .route("/api/ingress/email", post(api::ingest_email))
        .route("/api/heartbeat-monitors", get(api::heartbeat_list).post(api::heartbeat_create))
        .route("/api/heartbeat-monitors/:id", axum::routing::delete(api::heartbeat_del))
        .route("/api/email-inboxes", get(api::email_inbox_list).post(api::email_inbox_create))
        .route("/api/email-inboxes/:id", axum::routing::delete(api::email_inbox_del))
        .route("/api/oncall-now-native", get(api::oncall_now_native))
}

fn main() -> anyhow::Result<()> {
    // Runtime con stack worker grande: le fn async del motore escalation (runner, shifts) hanno
    // frame profondi; sotto carico concorrente il default tokio (2 MB) andava in stack-overflow.
    // Default YW 32 MB (era 16, marginale al picco di concorrenza: un worker sforava). Oltre allo
    // stack, i frame profondi sono ridotti con Box::pin sui leaf caldi (esc_dispatch) e con
    // l'esecuzione per-tenant su task dedicato in escalation_delivery_scan.
    let stack = std::env::var("YW_WORKER_STACK_MB").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(32);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack * 1024 * 1024)
        .build()?
        .block_on(run())
}
async fn run() -> anyhow::Result<()> {
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
        // SICUREZZA: test clock OFF di default. /api/_test/advance (CSRF-exempt, non autenticato)
        // è inerte in produzione; in dev/test si abilita con TEST_CLOCK=1.
        clock: Arc::new(clock::Clock::new(env("TEST_CLOCK","0")=="1")),
        bus,
        http: reqwest::Client::new(),
        control_host: env("YW_CONTROL_HOST", "tenants.loneworker.eu"),
        base_domain: env("YW_BASE_DOMAIN", "loneworker.eu"),
        secrets: Arc::new(crypto::Secrets::from_env()),
        started: std::time::Instant::now(),
        worker_heartbeat: Arc::new(std::sync::atomic::AtomicI64::new(0)),
    };
    // P3: se la chiave e' presente, cifra a riposo i segreti ancora in chiaro.
    if st.secrets.enabled() {
        if let Err(e) = crypto_migrate(&st).await { tracing::warn!("crypto migrate: {e}"); }
        tracing::info!("encryption-at-rest attiva (YW_SECRET_KEY)");
    } else {
        tracing::warn!("YW_SECRET_KEY non impostata: segreti upstream in chiaro nel DB");
    }

    tokio::spawn(worker::run(st.clone()));
    tokio::spawn(teltonika::serve(st.clone())); // edge nativo Teltonika (TCP Codec8/8E)
    tokio::spawn(mqtt::serve(st.clone())); // broker MQTT nativo IoT (TCP 1883)

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
        .route("/metrics", get(api::metrics))
        // control plane
        .merge(superadmin_routes())
        .route("/api/branding", get(api::branding))
        .route("/api/branding/logo", get(api::branding_logo))
        .route("/api/impersonate/stop", post(api::impersonate_stop))
        // tenant plane
        .route("/api/invite/check", get(api::check_invite))
        .route("/api/invite/accept", post(api::accept_invite))
        .route("/api/login", post(api::tenant_login))
        .route("/api/me", get(api::me))
        .route("/api/logout", post(api::logout))
        .route("/api/modules", get(api::list_modules))
        .route("/api/ui/features", get(api::ui_features).post(api::ui_features_set))
        .route("/api/modules/:code/tools", get(api::list_module_tools))
        .route("/api/modules/:code/tools/:tool", post(api::set_module_tool))
        .route("/api/sites", get(sites::list_sites).post(sites::create_site))
        .route("/api/sites/:id", post(sites::update_site).delete(sites::delete_site))
        .route("/api/users", get(users::list_users).post(users::create_user))
        .route("/api/users/:id", post(users::update_user).delete(users::delete_user))
        .route("/api/users/:id/sites", get(users::get_user_sites).post(users::set_user_sites))
        .route("/api/user-groups", get(groups::list_user_groups).post(groups::create_user_group))
        .route("/api/user-groups/:id", post(groups::update_user_group).delete(groups::delete_user_group))
        .route("/api/user-groups/:id/members", post(groups::user_group_member_add))
        .route("/api/user-groups/:id/members/:uid", axum::routing::delete(groups::user_group_member_del))
        .route("/api/capabilities", get(iam::list_capabilities).post(iam::set_capability).delete(iam::reset_capabilities))
        .route("/api/custom-roles", get(iam::list_custom_roles).post(iam::create_custom_role))
        .route("/api/custom-roles/:id", post(iam::update_custom_role).delete(iam::delete_custom_role))
        .route("/api/custom-roles/:id/users", post(iam::custom_role_assign_user))
        .route("/api/custom-roles/:id/users/:uid", axum::routing::delete(iam::custom_role_unassign_user))
        .route("/api/custom-roles/:id/groups", post(iam::custom_role_assign_group))
        .route("/api/custom-roles/:id/groups/:gid", axum::routing::delete(iam::custom_role_unassign_group))
        .route("/api/patrol-routes", get(api::list_patrol_routes).post(api::create_patrol_route))
        .route("/api/patrol-routes/:id", post(api::update_patrol_route).delete(api::delete_patrol_route))
        .route("/api/patrol-routes/:id/checkpoints", post(api::create_checkpoint))
        .route("/api/patrol-routes/:id/checkpoints/import", post(api::import_checkpoint))
        .route("/api/patrol-routes/:id/rounds", post(api::start_round))
        .route("/api/patrol-checkpoints/:id", post(api::update_checkpoint).delete(api::delete_checkpoint))
        .route("/api/patrol-rounds", get(api::list_rounds))
        .route("/api/patrol-rounds/:id", get(api::get_round))
        .route("/api/patrol-rounds/:id/punch", post(api::punch))
        .route("/api/patrol-rounds/:id/close", post(api::close_round))
        .route("/api/patrol-schedules", get(api::list_patrol_schedules).post(api::create_patrol_schedule))
        .route("/api/patrol-schedules/:id", delete(api::delete_patrol_schedule))
        .route("/api/patrol-dashboard", get(api::patrol_dashboard))
        .route("/api/settings/timezone", get(api::tenant_timezone_get).put(api::tenant_timezone_set))
        .route("/api/devices", get(api::list_devices).post(api::create_device))
        .route("/api/devices/provision", post(api::provision_device))
        .route("/api/devices/onboarding-kit", post(api::device_onboarding_kit))
        .route("/api/devices/onboarding-kit/bulk", post(api::device_onboarding_kit_bulk))
        .route("/api/devices/:id", post(api::update_device).delete(api::delete_device))
        .route("/api/devices/:id/assign", post(api::assign_device))
        .route("/api/devices/:id/readiness", get(api::device_readiness))
        .route("/api/devices/:id/test-sos", post(api::device_test_sos))
        .route("/api/devices/:id/gprs-command", post(api::device_gprs_command))
        .route("/api/catalog/devices", get(api::list_catalog))
        .route("/api/catalog/facets", get(api::catalog_facets))
        .route("/api/assets", get(sites::list_assets).post(sites::create_asset))
        .route("/api/assets/:id", post(sites::update_asset).delete(sites::delete_asset))
        .route("/api/groups", get(api::list_groups).post(api::create_group))
        .route("/api/escalation/fallback", get(api::escalation_fallback_get).put(api::escalation_fallback_set))
        .route("/api/groups/:id", axum::routing::delete(api::delete_group))
        .route("/api/groups/:id/members", post(api::add_group_member))
        .route("/api/groups/:id/members/import", post(api::alarm_group_import_members))
        .route("/api/groups/:id/oncall", post(api::set_group_oncall))
        .route("/api/groups/:id/policy", get(api::group_policy_get).post(api::group_policy_set))
        .route("/api/groups/:id/rotation/set-active", post(api::rotation_set_active))
        .route("/api/groups/:id/shifts", get(api::group_shifts))
        .route("/api/groups/:id/assignments", get(api::assignments_list).post(api::assignment_create))
        .route("/api/assignments/:id", axum::routing::delete(api::assignment_del))
        .route("/api/assignments/:id/members", post(api::assignment_member_add))
        .route("/api/assignments/:aid/members/:uid", axum::routing::delete(api::assignment_member_del))
        .route("/api/groups/:id/labels", post(api::group_labels_set))
        .route("/api/groups/:id/playbook", post(api::group_playbook_set))
        .route("/api/groups/:id/handoff-notes", get(api::handoff_notes_list).post(api::handoff_note_add))
        .route("/api/handoff-notes/:id", axum::routing::delete(api::handoff_note_del))
        .route("/api/groups/search", get(api::group_search))
        .route("/api/groups/:gid/members/:uid", axum::routing::delete(api::remove_group_member))
        .route("/api/routing", post(api::set_routing))
        .route("/api/sensor-rules", get(api::list_sensor_rules).post(api::create_sensor_rule))
        .route("/api/sensor-rules/:id", post(api::update_sensor_rule).delete(api::delete_sensor_rule))
        .route("/api/checkins/list", get(api::list_checkins))
        .route("/api/teams", get(api::list_teams).post(api::create_team))
        .route("/api/teams/:id", axum::routing::delete(api::delete_team))
        .route("/api/teams/:id/members", post(api::add_team_member))
        .route("/api/teams/:tid/members/:uid", axum::routing::delete(api::remove_team_member))
        .route("/api/shift-plans", get(api::list_shift_plans).post(api::create_shift_plan))
        .route("/api/shift-plans/validate", post(api::shift_plan_validate))
        .route("/api/absences", get(api::list_absences).post(api::create_absence))
        .route("/api/absences/:id", post(api::update_absence).delete(api::delete_absence))
        .route("/api/qualifications", get(api::list_qualifications).post(api::create_qualification))
        .route("/api/qualifications/:id", delete(api::delete_qualification))
        .route("/api/users/:id/qualifications", get(api::list_user_qualifications).post(api::user_qualification_add))
        .route("/api/users/:id/qualifications/:qid", delete(api::user_qualification_del))
        .route("/api/coverage", get(api::list_coverage).post(api::create_coverage))
        .route("/api/coverage/check", get(api::coverage_check))
        .route("/api/fairness", get(api::fairness_report))
        .route("/api/coverage/:id", post(api::update_coverage).delete(api::delete_coverage))
        .route("/api/shift-plans/:id", post(api::shift_plan_action).delete(api::delete_shift_plan))
        .route("/api/shift-plans/:id/substitutes", get(api::shift_substitutes))
        .route("/api/shift-plans/:id/move", post(api::shift_plan_move))
        .route("/api/shift-plans/suggest", post(api::shift_suggest))
        .route("/api/users/:id/color", post(api::user_set_color))
        // Assistenza A1: anagrafica Cliente + referenti
        .route("/api/clients", get(api::list_clients).post(api::create_client))
        .route("/api/clients/:id", get(api::get_client).post(api::update_client).delete(api::delete_client))
        .route("/api/clients/:id/contacts", post(api::create_contact))
        .route("/api/contacts/:id", delete(api::delete_contact))
        // Interventi A2: listino + valorizzazione visita
        .route("/api/price-items", get(api::list_price_items).post(api::create_price_item))
        .route("/api/price-items/:id", post(api::update_price_item).delete(api::delete_price_item))
        .route("/api/orders/:id/lines", get(api::list_order_lines).post(api::add_order_line))
        .route("/api/order-lines/:id", delete(api::delete_order_line))
        .route("/api/orders/:id/suggest-tech", get(api::suggest_order_tech))
        // Interventi A4: contratti/SLA + piani manutenzione
        .route("/api/contracts", get(api::list_contracts).post(api::create_contract))
        .route("/api/contracts/:id", post(api::update_contract).delete(api::delete_contract))
        .route("/api/maintenance-plans", get(api::list_plans).post(api::create_plan))
        .route("/api/maintenance-plans/:id", post(api::update_plan).delete(api::delete_plan))
        .route("/api/maintenance-plans/:id/generate", post(api::generate_plan_order))
        // Interventi A5: richieste/ticket
        .route("/api/requests", get(api::list_requests).post(api::create_request))
        .route("/api/requests/:id", post(api::update_request).delete(api::delete_request))
        .route("/api/requests/:id/convert", post(api::convert_request))
        // Modulo Cantieri: commessa -> lavorazioni (WBS) -> SAL
        .route("/api/commesse", get(api::list_commesse).post(api::create_commessa))
        .route("/api/commesse/:id", get(api::get_commessa).post(api::update_commessa).delete(api::delete_commessa))
        .route("/api/commesse/:id/lavorazioni", post(api::create_lavorazione))
        .route("/api/commesse/:id/sal", post(api::create_sal))
        .route("/api/commesse/:id/teams", post(api::commessa_assign_team))
        .route("/api/commesse/:cid/teams/:tid", delete(api::commessa_remove_team))
        .route("/api/commesse/:id/presenze/in", post(api::commessa_clock_in))
        .route("/api/commesse/:id/presenze/out", post(api::commessa_clock_out))
        .route("/api/commesse/:id/costi", post(api::add_commessa_costo))
        .route("/api/commessa-costi/:id", delete(api::delete_commessa_costo))
        .route("/api/commesse/:id/diario", post(api::add_commessa_diario))
        .route("/api/commessa-diario/:id", delete(api::delete_commessa_diario))
        .route("/api/commesse/:id/fatture", post(api::add_commessa_fattura))
        .route("/api/commessa-fatture/:id", post(api::update_commessa_fattura).delete(api::delete_commessa_fattura))
        .route("/api/commesse/:id/attachments", post(api::commessa_attachment_add))
        .route("/api/commessa-attachments/:id", get(api::commessa_attachment_get).delete(api::commessa_attachment_del))
        .route("/api/lavorazioni/:id", post(api::update_lavorazione).delete(api::delete_lavorazione))
        ;
    // catena spezzata in più statement: una singola espressione .route()* troppo lunga
    // fa esplodere lo stack di rustc in codegen (STATUS_STACK_BUFFER_OVERRUN su Windows).
    let app = app
        .route("/api/oncall", get(api::list_oncall).post(api::create_oncall))
        .route("/api/oncall/:id", post(api::update_oncall).delete(api::delete_oncall))
        // YWAlert parity: impostazioni, recapiti+verifica, assenze, manutenzione, calendario
        .route("/api/oncall-settings", get(api::oncall_settings_get).post(api::oncall_settings_set))
        .route("/api/oncall-contacts", get(api::oncall_contacts))
        .route("/api/oncall-contacts/verify", post(api::oncall_contact_verify))
        .route("/api/oncall-contacts/code", post(api::oncall_contact_code))
        .route("/api/oncall-contacts/test", post(api::oncall_contact_test))
        .route("/api/oncall-contacts/:uid/methods", get(api::user_cm_list).post(api::user_cm_add))
        .route("/api/oncall-contacts/methods/:id", axum::routing::delete(api::user_cm_del))
        .route("/api/oncall-contacts/methods/:id/verify", post(api::user_cm_verify))
        .route("/api/oncall-contacts/methods/:id/code", post(api::user_cm_code))
        .route("/api/oncall-contacts/methods/:id/test", post(api::user_cm_test))
        .route("/api/oncall-contacts/:uid/rules", post(api::user_rule_add))
        .route("/api/oncall-contacts/rules/:id", axum::routing::delete(api::user_rule_del))
        .route("/api/oncall-contacts/:uid/reactivate", post(api::user_reactivate))
        .route("/api/oncall-contacts/:uid/status-pref", post(api::user_status_pref_set))
        .route("/api/oncall-overrides", get(api::oncall_overrides_list).post(api::oncall_override_add))
        .route("/api/oncall-overrides/delete", post(api::oncall_override_del))
        .route("/api/oncall-overrides/update", post(api::oncall_override_update))
        .route("/api/oncall-temp-schedules", get(api::temp_schedule_list).post(api::temp_schedule_add))
        .route("/api/oncall-temp-schedules/delete", post(api::temp_schedule_del))
        .route("/api/oncall-maintenance", post(api::oncall_maintenance))
        .route("/api/oncall-calendar/url", get(api::oncall_calendar_url))
        .route("/api/oncall-calendar", delete(api::oncall_calendar_revoke))
        .route("/api/oncall-calendar.ics", get(api::oncall_calendar_ics))
        // Calendario CORE condiviso (W1)
        .route("/api/calendar/events", get(api::calendar_events))
        .route("/api/calendar/my-url", get(api::calendar_my_url))
        .route("/api/calendar.ics", get(api::calendar_ics))
        .route("/api/groups/:id/calendar/url", get(api::group_calendar_url))
        .route("/api/groups/:id/calendar", delete(api::group_calendar_revoke))
        .route("/api/group-calendar.ics", get(api::group_calendar_ics))
        .route("/api/ingest-tokens", get(api::list_ingest_tokens).post(api::create_ingest_token))
        .route("/api/ingest-tokens/:token", axum::routing::delete(api::delete_ingest_token))
        .route("/api/ingest-tokens/:token/revoke", post(api::revoke_ingest_token))
        // Dispositivi GPS/badge: policy eventi + report percorsi/soste (nativi)
        .route("/api/gps/event-policy", get(api::gps_event_policy_get).post(api::gps_event_policy_set))
        .route("/api/devices/:id/report/:kind", get(track::device_report))
        .route("/api/events", post(api::ingest_event))
        .route("/api/ingress/gps", post(api::ingest_gps_event))
        .route("/api/ingress/gps/position", post(api::ingest_gps_position))
        .route("/api/ingress/osmand", get(api::ingest_osmand).post(api::ingest_osmand))
        .route("/api/devices/:id/positions", get(api::list_device_positions))
        .route("/api/geocode/reverse", get(api::reverse_geocode))
        .route("/api/devices/:id/command", post(api::device_command))
        .route("/api/devices/:id/service", post(api::set_device_service))
        .route("/api/devices/:id/commands", get(api::list_device_commands))
        .route("/api/devices/:id/command-types", get(api::device_command_types))
        .route("/api/geofences", get(api::list_geofences).post(api::create_geofence))
        .route("/api/geofences/:id", axum::routing::delete(api::delete_geofence).put(api::update_geofence))
        .route("/api/geofences/:id/devices", axum::routing::put(api::set_geofence_devices))
        .route("/api/geofences/:id/events", axum::routing::put(api::set_geofence_events))
        .route("/api/geofences/:id/alert", axum::routing::put(api::set_geofence_alert))
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
        .route("/api/devices/:id/place", post(api::place_device))
        .merge(iot_routes())
        .route("/api/incidents", get(api::list_incidents))
        .route("/api/incidents/:id/timeline", get(api::incident_timeline))
        .route("/api/incidents/ack-all", post(api::incidents_ack_all))
        .route("/api/incidents/close-all", post(api::incidents_close_all))
        .route("/api/incidents/:id/ack", post(api::ack_incident))
        .route("/api/incidents/:id/close", post(api::close_incident))
        .route("/api/incidents/:id/assign", post(api::assign_incident))
        .route("/api/incidents/:id/note", post(api::add_incident_note))
        .route("/api/sos", post(api::worker_sos))
        .route("/api/worker/position", post(api::worker_position))
        .route("/api/checkins", post(api::create_checkin))
        .route("/api/checkins/mine", get(api::my_active_checkin))
        .route("/api/worker/immobility", get(api::worker_immobility_status))
        .route("/api/worker/immobility/ok", post(api::worker_immobility_ok))
        .route("/api/checkins/:id/confirm", post(api::confirm_checkin))
        .route("/api/shifts/start", post(api::shift_start))
        .route("/api/shifts/end", post(api::shift_end))
        .route("/api/shifts/current", get(api::shift_current))
        .route("/api/diary", post(api::diary_create).get(api::diary_list))
        // Prompt 9: ordini
        .route("/api/orders", post(api::create_order).get(api::list_orders))
        .route("/api/orders/:id/edit", post(api::edit_order))
        .route("/api/orders/:id/assign", post(api::order_assign))
        .route("/api/orders/:id/assign-group", post(api::order_assign_group))
        .route("/api/orders/:id/arrive", post(api::order_arrive))
        .route("/api/orders/:id/start", post(api::order_start))
        .route("/api/orders/:id/complete", post(api::order_complete))
        .route("/api/orders/:id/cancel", post(api::order_cancel))
        .route("/api/orders/:id/report", post(api::order_report_set))
        .route("/api/orders/:id/timesheet", post(api::generate_timesheet_from_order))
        .route("/api/orders/:id/attachments", get(api::order_attachments_list).post(api::order_attachment_add))
        .route("/api/orders/attachments/:aid", get(api::order_attachment_get).delete(api::order_attachment_del))
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
        // youworker.ai (AI di YouCo) agganciata ai processi: feature per-tenant + endpoint per-modulo
        .route("/api/ai/features", get(api::ai_features_get).post(api::ai_features_set))
        .route("/api/ai/agent", post(api::ai_agent))
        .route("/api/ai/agent/confirm", post(api::ai_agent_confirm))
        .route("/api/ai/reports", get(api::ai_reports_list))
        .route("/api/ai/reports/generate", post(api::ai_report_generate))
        .route("/api/ai/reports/:id", get(api::ai_report_get))
        .route("/api/incidents/:id/ai-summary", get(api::incident_ai_summary))
        .route("/api/sensor-rules/suggest", post(api::sensor_rules_suggest))
        ;
    let app = app
        // Sub-router aggregato (GDPR/dashboard/escalation/safety): merge.
        .merge(extra_routes())
        .route("/api/setup-status", get(api::setup_status))
        .route("/api/home-tasks", get(api::home_tasks))
        .route("/api/scenarios", get(api::tenant_scenarios_list))
        .route("/api/scenarios/:code/dashboard", get(api::scenario_dashboard))
        .route("/api/scenarios/:code/aeration", post(api::scenario_aeration_set))
        .route("/api/scenarios/:code/ai-analysis", post(api::scenario_ai_analysis))
        .route("/api/checkin/ai-anomaly", get(api::checkin_ai_anomaly))
        .route("/api/orders/:id/ai-assist", get(api::work_order_ai_assist))
        .route("/api/timesheets/:id/ai-summary", get(api::consuntivo_ai_summary))
        .route("/api/diary/transcribe", post(api::diary_transcribe))
        .route("/api/stream", get(api::stream))
        .route("/api/_test/advance", post(api::test_advance))
        .route("/api/_test/iot-watchdog", post(api::test_iot_watchdog))
        .route("/api/_test/gps-silence-watchdog", post(api::test_gps_silence_watchdog))
        .layer(axum::middleware::from_fn(api::csrf_guard))
        .layer(axum::middleware::from_fn(api::client_ip_mw)) // IP client dal peer TCP (anti-spoof XFF, SA-4)
        .layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024)) // upload piantine indoor
        .with_state(st);

    let bind = env("YW_BIND", "0.0.0.0:8080");
    tracing::info!("core listening on {bind}");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}

/// Idempotent seed. SICUREZZA: nessun superadmin/tenant demo hardcoded in produzione.
/// - Superadmin iniziale: da YW_SUPERADMIN_EMAIL/YW_SUPERADMIN_PASS; in mancanza, solo se
///   YW_SEED_DEMO=1 si usa l'admin di sviluppo; altrimenti si avvisa e non si crea nulla.
/// - Tenant demo (acme/globex con password "demo"): solo con YW_SEED_DEMO=1.
async fn bootstrap(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    let seed_demo = std::env::var("YW_SEED_DEMO").ok().as_deref() == Some("1");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM app.superadmins").fetch_one(pool).await?;
    if n == 0 {
        let email = std::env::var("YW_SUPERADMIN_EMAIL").ok().filter(|s| !s.is_empty());
        let pass = std::env::var("YW_SUPERADMIN_PASS").ok().filter(|s| !s.is_empty());
        match (email, pass) {
            (Some(e), Some(p)) => {
                sqlx::query("INSERT INTO app.superadmins(email,pass_hash) VALUES($1,$2)")
                    .bind(&e).bind(auth::hash_password(&p)).execute(pool).await?;
                tracing::info!("seeded superadmin {e} (da env)");
            }
            _ if seed_demo => {
                sqlx::query("INSERT INTO app.superadmins(email,pass_hash) VALUES($1,$2)")
                    .bind("admin@loneworker.eu").bind(auth::hash_password("admin")).execute(pool).await?;
                tracing::info!("seeded superadmin DEV admin@loneworker.eu / admin (YW_SEED_DEMO=1)");
            }
            _ => tracing::warn!("nessun superadmin: imposta YW_SUPERADMIN_EMAIL/YW_SUPERADMIN_PASS (o YW_SEED_DEMO=1 in dev)"),
        }
    }
    if !seed_demo { return Ok(()); }
    let acme: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.tenants WHERE slug='acme'")
        .fetch_optional(pool).await?;
    if acme.is_none() {
        let tid: Uuid = sqlx::query_scalar("INSERT INTO app.tenants(slug,name) VALUES('acme','Acme SpA') RETURNING id")
            .fetch_one(pool).await?;
        sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state)
            SELECT $1, code, CASE WHEN code IN ('loneworker','iot','consuntivi','interventi','workforce','assistant','giri') THEN 'attivo' ELSE 'non_abilitato' END
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
        sqlx::query("INSERT INTO app.devices(tenant_id,site_id,external_id,source) VALUES($1,$2,'BADGE-001','iot')")
            .bind(tid).bind(site).execute(pool).await?;
        sqlx::query("INSERT INTO app.ingest_tokens(token,tenant_id,source) VALUES('acme-ingest-token',$1,'iot')")
            .bind(tid).execute(pool).await?;
        // second tenant to prove isolation (T02)
        let t2: Uuid = sqlx::query_scalar("INSERT INTO app.tenants(slug,name) VALUES('globex','Globex Srl') RETURNING id")
            .fetch_one(pool).await?;
        sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state)
            SELECT $1, code, CASE WHEN code IN ('loneworker','interventi','giri') THEN 'attivo' ELSE 'non_abilitato' END FROM app.modules_catalog")
            .bind(t2).execute(pool).await?;
        sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,'admin@globex.loneworker.eu',$2,'tenant_admin')")
            .bind(t2).bind(auth::hash_password("demo")).execute(pool).await?;
        sqlx::query("INSERT INTO app.ingest_tokens(token,tenant_id,source) VALUES('globex-ingest-token',$1,'iot')")
            .bind(t2).execute(pool).await?;
        tracing::info!("seeded demo tenants acme + globex");
    }
    // Idempotente a OGNI boot: riallinea gli stati dei moduli dei tenant demo
    // all'insieme atteso. Serve perche' YW_SEED_DEMO crea i tenant solo se assenti,
    // ma un ywdb persistente puo' avere stati moduli vecchi (deriva seed). In dev i
    // tenant demo sono usa-e-getta, quindi li riportiamo sempre allo stato canonico.
    for (slug, active) in [
        ("acme",   "loneworker,iot,consuntivi,interventi,workforce,assistant,giri,cantieri,gps"),
        ("globex", "loneworker,interventi,giri"),
    ] {
        let codes: Vec<String> = active.split(',').map(|s| s.to_string()).collect();
        sqlx::query(
            "INSERT INTO app.tenant_modules(tenant_id,module_code,state)
             SELECT t.id, m.code, CASE WHEN m.code = ANY($2) THEN 'attivo' ELSE 'non_abilitato' END
             FROM app.tenants t CROSS JOIN app.modules_catalog m
             WHERE t.slug=$1
             ON CONFLICT (tenant_id,module_code) DO UPDATE SET state=EXCLUDED.state, updated_at=now()")
            .bind(slug).bind(&codes).execute(pool).await?;
    }
    Ok(())
}
