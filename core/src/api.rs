use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, Response, sse::{Sse, Event as SseEvent}}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::convert::Infallible;
use uuid::Uuid;
use futures::stream::Stream;
use crate::state::{AppState, Session, TenantEvent};
use crate::{auth, db, goalert};

type R = Result<Json<Value>, (StatusCode, String)>;
fn err(c: StatusCode, m: &str) -> (StatusCode, String) { (c, m.to_string()) }

const CRITICAL: &[&str] = &["sos", "man_down", "immobilita", "mancato_checkin", "sensore_critico",
    "manomissione_badge", "perdita_alimentazione", "jamming", "perdita_contatto", "traccar_conn_down",
    "batteria_critica", "heartbeat_down", "email_alarm"];

/// Traccar alarm -> normalized etype. Safety-first defaults for lone workers.
/// `sos_as` lets a tenant route the SOS alarm to `sos` or `man_down`.
/// Returns the normalized etype; whether it is critical is decided by CRITICAL.
fn traccar_alarm_etype(alarm: &str, sos_as: &str) -> String {
    match alarm {
        "sos" => sos_as.to_string(),
        "fallDown" | "accident" => "man_down".into(),
        "removing" | "tampering" | "bonnet" => "manomissione_badge".into(), // badge rimosso/manomesso
        "powerCut" | "powerOff" => "perdita_alimentazione".into(),
        "jamming" | "gpsAntennaCut" => "jamming".into(),
        "lowBattery" | "lowPower" => "batteria_bassa".into(),       // warning, non critico
        "" => "device_alarm".into(),
        _ => "device_alarm".into(),                                  // registrato, non critico
    }
}

/// Default handling + etype base per tipo evento Traccar (tipi reali da
/// /api/notifications/types). Safety-first per il lavoratore isolato.
/// Ritorna (etype_base, default_handling ∈ ignora|registra|critico).
fn traccar_event_default(evt_type: &str) -> (&'static str, &'static str) {
    match evt_type {
        "unaccompaniedMotion" => ("movimento_non_accompagnato", "critico"),
        "deviceInactive"      => ("dispositivo_inattivo", "critico"),
        "proximityEnter"      => ("prossimita_ingresso", "registra"),
        "proximityExit"       => ("prossimita_uscita", "registra"),
        "deviceOverspeed"     => ("eccesso_velocita", "registra"),
        "deviceFuelDrop"      => ("calo_carburante", "registra"),
        "deviceFuelIncrease"  => ("aumento_carburante", "ignora"),
        "ignitionOn"          => ("accensione_motore", "ignora"),
        "ignitionOff"         => ("spegnimento_motore", "ignora"),
        "maintenance"         => ("manutenzione_richiesta", "registra"),
        "driverChanged"       => ("cambio_conducente", "registra"),
        "media"               => ("media_ricevuto", "ignora"),
        "deviceMoving"        => ("device_moving", "ignora"),
        "geofenceCrossed"     => ("attraversamento_geofence", "registra"),
        "commandResult"       => ("esito_comando", "ignora"),
        "deviceOnline" | "deviceUnknown" | "queuedCommandSent" => ("traccar_event", "ignora"),
        _ => ("traccar_event", "registra"),
    }
}
/// Tipi evento Traccar esposti nella policy (per la UI). Esclusi quelli con
/// gestione dedicata propria (alarm, deviceOffline, deviceStopped, geofenceEnter/Exit).
pub const TRACCAR_POLICY_EVENTS: &[&str] = &[
    "unaccompaniedMotion","deviceInactive","proximityEnter","proximityExit","deviceOverspeed",
    "deviceFuelDrop","deviceFuelIncrease","ignitionOn","ignitionOff","maintenance","driverChanged",
    "media","deviceMoving","geofenceCrossed","commandResult","deviceOnline","deviceUnknown"];

/// Mappa un evento Traccar (tipo + eventuale alarm + batteria) nell'etype
/// normalizzato, applicando la config per-tenant `traccar/alarm_map` (incl. `events`).
/// Usata SIA dal webhook di forwarding SIA dal poller API, così push e pull
/// producono esattamente lo stesso incidente (idempotente a valle). Ritorna "ignore"
/// quando la policy è `ignora` (il chiamante salta la persistenza).
pub fn map_traccar_etype(amap: &Value, evt_type: &str, alarm: Option<&str>, battery: Option<f64>) -> String {
    let sos_as = amap.get("sos_as").and_then(|v| v.as_str()).unwrap_or("sos");
    let offline_critical = amap.get("offline_critical").and_then(|v| v.as_str()).map(|s| s=="si").unwrap_or(false);
    let stopped_immobilita = amap.get("stopped_as_immobilita").and_then(|v| v.as_str()).map(|s| s=="si").unwrap_or(false);
    let battery_min = amap.get("battery_min").and_then(|v| v.as_i64()).unwrap_or(15);
    // policy batteria configurabile dall'admin: off (ignora) | warning (evento, default) | critico (allarme)
    let battery_as = amap.get("battery_as").and_then(|v| v.as_str()).unwrap_or("warning");
    let mut etype: String = if let Some(a) = alarm {
        traccar_alarm_etype(a, sos_as)
    } else if evt_type == "deviceOffline" {
        if offline_critical { "perdita_contatto".into() } else { "device_offline".into() }
    } else if evt_type == "deviceStopped" && stopped_immobilita {
        "immobilita".into()
    } else if evt_type == "geofenceEnter" {
        "ingresso_sito".into()
    } else if evt_type == "geofenceExit" {
        "uscita_sito".into()
    } else if battery.map(|b| b > 0.0 && b < battery_min as f64).unwrap_or(false) {
        "batteria_bassa".into()
    } else {
        // event policy configurabile: ignora | registra | critico (etype *_critico)
        let (base, def) = traccar_event_default(evt_type);
        let handling = amap.get("events").and_then(|e| e.get(evt_type)).and_then(|v| v.as_str()).unwrap_or(def);
        match handling {
            "ignora" => "ignore".into(),
            "critico" => format!("{base}_critico"),
            _ => base.into(),
        }
    };
    // Applica la policy batteria sia al percorso alarm=lowBattery sia alla soglia %.
    if etype == "batteria_bassa" {
        etype = match battery_as {
            "off" => "traccar_event".into(),      // non segnalata
            "critico" => "batteria_critica".into(), // allarme + escalation (in CRITICAL)
            _ => "batteria_bassa".into(),          // warning registrato, non critico
        };
    }
    etype
}

// ----------------------------- helpers -----------------------------
fn get_cookie(h: &HeaderMap, name: &str) -> Option<String> {
    let raw = h.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let p = part.trim();
        if let Some(v) = p.strip_prefix(&format!("{name}=")) { return Some(v.to_string()); }
    }
    None
}

async fn session_of(state: &AppState, h: &HeaderMap) -> Option<Session> {
    let tok = get_cookie(h, "yw_session")?;
    // persistent sessions: survive core restarts; expired rows are ignored.
    let r = sqlx::query(
        "SELECT user_id, tenant_id, email, role, is_super, impersonator
         FROM app.sessions WHERE token=$1 AND expires_at > now()")
        .bind(&tok).fetch_optional(&state.owner_pool).await.ok().flatten()?;
    let tenant_id = r.try_get::<Option<Uuid>, _>("tenant_id").ok().flatten();
    let role: String = r.get("role");
    let uid: Uuid = r.get("user_id");
    let caps = match tenant_id { Some(tid) => effective_caps_user(state, tid, &role, uid).await, None => vec![] };
    Some(Session {
        user_id: r.get("user_id"),
        tenant_id,
        email: r.get("email"),
        role,
        is_super: r.get("is_super"),
        impersonator: r.try_get::<Option<String>, _>("impersonator").ok().flatten(),
        caps,
    })
}

/// Persist a new session (12h TTL). Opportunistic cleanup of expired rows.
async fn persist_session(state: &AppState, tok: &str, s: &Session) {
    let _ = sqlx::query("DELETE FROM app.sessions WHERE expires_at <= now()")
        .execute(&state.owner_pool).await;
    let _ = sqlx::query(
        "INSERT INTO app.sessions(token,user_id,tenant_id,email,role,is_super,impersonator,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7, now() + interval '12 hours')")
        .bind(tok).bind(s.user_id).bind(s.tenant_id).bind(&s.email).bind(&s.role).bind(s.is_super).bind(&s.impersonator)
        .execute(&state.owner_pool).await;
}

/// Session cookie (HttpOnly) + CSRF cookie (readable by JS for the double-submit header).
fn set_cookie_header(tok: &str, csrf: &str) -> HeaderMap {
    let mut m = HeaderMap::new();
    m.append(header::SET_COOKIE,
        format!("yw_session={tok}; Path=/; HttpOnly; SameSite=Lax").parse().unwrap());
    m.append(header::SET_COOKIE,
        format!("yw_csrf={csrf}; Path=/; SameSite=Lax").parse().unwrap());
    m
}

fn cookie_val(raw: &str, name: &str) -> Option<String> {
    for part in raw.split(';') {
        let p = part.trim();
        if let Some(v) = p.strip_prefix(&format!("{name}=")) { return Some(v.to_string()); }
    }
    None
}

/// CSRF guard (double-submit): mutating, cookie-authenticated requests must carry
/// an `X-CSRF-Token` header equal to the `yw_csrf` cookie. Machine ingress (ingest
/// token, no session cookie), login and logout are exempt.
pub async fn csrf_guard(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = req.method().clone();
    let safe = matches!(method, axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS);
    let path = req.uri().path().to_string();
    let exempt = safe
        || path == "/api/login" || path == "/api/superadmin/login" || path == "/api/logout"
        || path == "/api/events" || path.starts_with("/api/ingress/")
        || path == "/api/_test/advance" || path == "/health";
    if exempt { return next.run(req).await; }
    let raw = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if cookie_val(&raw, "yw_session").is_none() {
        return next.run(req).await; // not cookie-authenticated (machine/token) -> CSRF N/A
    }
    let cookie = cookie_val(&raw, "yw_csrf");
    let header = req.headers().get("x-csrf-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    match (cookie, header) {
        (Some(c), Some(h)) if !c.is_empty() && c == h => next.run(req).await,
        _ => (StatusCode::FORBIDDEN, Json(json!({"error":"csrf token mismatch"}))).into_response(),
    }
}

// --------------------- login rate limiting (anti brute-force, shared store) ---------------------
// State lives in app.login_throttle so the limit is enforced across all core instances.
// Fixed window: 5 failures / 300s per key -> block 300s.
fn rl_key(h: &HeaderMap, email: &str) -> String {
    let ip = h.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next()).map(|s| s.trim().to_string())
        .or_else(|| h.get("x-real-ip").and_then(|v| v.to_str().ok()).map(|s| s.to_string()))
        .unwrap_or_default();
    format!("{}|{}", email.to_lowercase(), ip)
}

/// Some(retry_after_secs) if the key is currently blocked.
async fn rl_check(st: &AppState, key: &str) -> Option<u64> {
    let b: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT blocked_until FROM app.login_throttle WHERE key=$1")
            .bind(key).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    if let Some(b) = b {
        let now = chrono::Utc::now();
        if b > now { return Some((b - now).num_seconds().max(0) as u64 + 1); }
    }
    None
}

async fn rl_fail(st: &AppState, key: &str) {
    let _ = sqlx::query(
        "INSERT INTO app.login_throttle(key,count,reset_at,blocked_until)
         VALUES($1, 1, now() + interval '300 seconds', NULL)
         ON CONFLICT(key) DO UPDATE SET
           count = CASE WHEN now() > login_throttle.reset_at THEN 1 ELSE login_throttle.count + 1 END,
           reset_at = CASE WHEN now() > login_throttle.reset_at THEN now() + interval '300 seconds' ELSE login_throttle.reset_at END,
           blocked_until = CASE WHEN (CASE WHEN now() > login_throttle.reset_at THEN 1 ELSE login_throttle.count + 1 END) >= 5
                                THEN now() + interval '300 seconds' ELSE login_throttle.blocked_until END,
           updated_at = now()")
        .bind(key).execute(&st.owner_pool).await;
}

async fn rl_clear(st: &AppState, key: &str) {
    let _ = sqlx::query("DELETE FROM app.login_throttle WHERE key=$1").bind(key).execute(&st.owner_pool).await;
}

fn too_many(retry_after: u64) -> Response {
    (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, retry_after.to_string())],
     Json(json!({"error":"too many attempts","retry_after":retry_after}))).into_response()
}

/// Publish a live event to ALL instances via Postgres NOTIFY; each instance's
/// LISTEN task re-broadcasts it into its local SSE bus.
pub async fn publish_event(st: &AppState, ev: TenantEvent) {
    let msg = json!({"tenant_id": ev.tenant_id, "payload": ev.payload}).to_string();
    let _ = sqlx::query("SELECT pg_notify('yw_events', $1)").bind(msg).execute(&st.owner_pool).await;
}

async fn require_tenant(state: &AppState, h: &HeaderMap) -> Result<Session, (StatusCode, String)> {
    let s = session_of(state, h).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no session"))?;
    if s.is_super || s.tenant_id.is_none() {
        return Err(err(StatusCode::FORBIDDEN, "tenant session required"));
    }
    Ok(s)
}

async fn require_super(state: &AppState, h: &HeaderMap) -> Result<Session, (StatusCode, String)> {
    let s = session_of(state, h).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no session"))?;
    if !s.is_super { return Err(err(StatusCode::FORBIDDEN, "superadmin required")); }
    Ok(s)
}

// ----------------------------- control plane -----------------------------
#[derive(Deserialize)]
pub struct LoginReq { pub email: String, pub password: String }

pub async fn superadmin_login(State(st): State<AppState>, h: HeaderMap, Json(b): Json<LoginReq>) -> impl IntoResponse {
    let key = rl_key(&h, &b.email);
    if let Some(ra) = rl_check(&st, &key).await { return too_many(ra); }
    let row = sqlx::query("SELECT id, pass_hash FROM app.superadmins WHERE email=$1")
        .bind(&b.email).fetch_optional(&st.owner_pool).await;
    match row {
        Ok(Some(r)) => {
            let id: Uuid = r.get("id");
            let hash: String = r.get("pass_hash");
            if auth::verify_password(&b.password, &hash) {
                rl_clear(&st, &key).await;
                if auth::needs_rehash(&hash) {
                    let _ = sqlx::query("UPDATE app.superadmins SET pass_hash=$1 WHERE id=$2")
                        .bind(auth::hash_password(&b.password)).bind(id).execute(&st.owner_pool).await;
                }
                let tok = auth::new_token();
                let csrf = auth::new_token();
                let sess = Session { user_id: id, tenant_id: None, email: b.email.clone(),
                    role: "superadmin".into(), is_super: true, impersonator: None, caps: vec![] };
                persist_session(&st, &tok, &sess).await;
                (StatusCode::OK, set_cookie_header(&tok, &csrf), Json(json!({"ok":true,"role":"superadmin","csrf":csrf}))).into_response()
            } else {
                rl_fail(&st, &key).await;
                (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response()
            }
        }
        _ => {
            rl_fail(&st, &key).await;
            (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CreateTenant { pub slug: String, pub name: String, pub admin_email: String, pub admin_password: String,
    pub brand_primary: Option<String>, pub brand_ink: Option<String>, pub brand_soft: Option<String>, pub brand_accent: Option<String> }

pub async fn create_tenant(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CreateTenant>) -> R {
    require_super(&st, &h).await?;
    let slug = b.slug.to_lowercase();
    const RESERVED: &[&str] = &["tenants","www","admin","api","app"];
    if RESERVED.contains(&slug.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "reserved slug")); }
    let (bp, bi, bs, ba) = (clean_color(&b.brand_primary)?, clean_color(&b.brand_ink)?, clean_color(&b.brand_soft)?, clean_color(&b.brand_accent)?);
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    let trow = sqlx::query("INSERT INTO app.tenants(slug,name,brand_primary,brand_ink,brand_soft,brand_accent) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(&slug).bind(&b.name).bind(bp).bind(bi).bind(bs).bind(ba).fetch_one(&mut *tx).await
        .map_err(|_| err(StatusCode::CONFLICT, "slug exists"))?;
    let tid: Uuid = trow.get("id");
    // first admin via single-use invite is the production path; demo seeds directly.
    sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,$2,$3,'tenant_admin')")
        .bind(tid).bind(&b.admin_email).bind(auth::hash_password(&b.admin_password))
        .execute(&mut *tx).await.map_err(ise)?;
    // seed module rows (all non_abilitato; core identity always on)
    sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state)
        SELECT $1, code, 'non_abilitato' FROM app.modules_catalog")
        .bind(tid).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,'superadmin','create_tenant',$1)")
        .bind(json!({"slug":slug})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":tid,"slug":slug})))
}

pub async fn list_tenants(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    // seat app mobile = utenti che usano l'app (lavoratore/reperibile); IoT = dispositivi registrati.
    let rows = sqlx::query(
        "SELECT t.id,t.slug,t.name,t.created_at,
            t.mobile_app_licenses, t.iot_device_licenses,
            t.brand_primary,t.brand_ink,t.brand_soft,t.brand_accent,(t.logo IS NOT NULL) AS has_logo,
            (SELECT count(*) FROM app.users u WHERE u.tenant_id=t.id AND u.role IN ('lavoratore','reperibile')) AS mobile_used,
            (SELECT count(*) FROM app.devices d WHERE d.tenant_id=t.id) AS iot_used
         FROM app.tenants t ORDER BY t.created_at")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "slug": r.get::<String,_>("slug"), "name": r.get::<String,_>("name"),
        "mobile_app_licenses": r.try_get::<Option<i32>,_>("mobile_app_licenses").ok().flatten(),
        "iot_device_licenses": r.try_get::<Option<i32>,_>("iot_device_licenses").ok().flatten(),
        "mobile_used": r.get::<i64,_>("mobile_used"),
        "iot_used": r.get::<i64,_>("iot_used"),
        "brand_primary": r.try_get::<Option<String>,_>("brand_primary").ok().flatten(),
        "brand_ink": r.try_get::<Option<String>,_>("brand_ink").ok().flatten(),
        "brand_soft": r.try_get::<Option<String>,_>("brand_soft").ok().flatten(),
        "brand_accent": r.try_get::<Option<String>,_>("brand_accent").ok().flatten(),
        "has_logo": r.get::<bool,_>("has_logo"),
    })).collect();
    Ok(Json(json!(out)))
}

// ---- superadmin: set licenze + branding del tenant (il form invia sempre tutti i campi) ----
// licenze: null = illimitato. colori: stringa vuota = reset al default globale YouCo.
#[derive(Deserialize)]
pub struct TenantAdminPatch {
    pub mobile_app_licenses: Option<i32>, pub iot_device_licenses: Option<i32>,
    pub brand_primary: Option<String>, pub brand_ink: Option<String>,
    pub brand_soft: Option<String>, pub brand_accent: Option<String>,
}
// Valida un colore CSS semplice (#rgb/#rrggbb/#rrggbbaa). Vuoto/None = reset al default.
fn clean_color(v: &Option<String>) -> Result<Option<String>, (StatusCode,String)> {
    match v.as_deref().map(|s| s.trim()) {
        None | Some("") => Ok(None),
        Some(s) => {
            let ok = s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
                && (s.len()==4 || s.len()==7 || s.len()==9);
            if ok { Ok(Some(s.to_string())) } else { Err(err(StatusCode::BAD_REQUEST, "colore non valido (usa #rgb o #rrggbb)")) }
        }
    }
}
pub async fn super_set_tenant_admin(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<TenantAdminPatch>) -> R {
    let s = require_super(&st, &h).await?;
    let lic = |v: Option<i32>| v.filter(|n| *n >= 0); // negativi -> illimitato
    let (bp, bi, bs, ba) = (clean_color(&b.brand_primary)?, clean_color(&b.brand_ink)?, clean_color(&b.brand_soft)?, clean_color(&b.brand_accent)?);
    sqlx::query("UPDATE app.tenants SET mobile_app_licenses=$1, iot_device_licenses=$2,
            brand_primary=$3, brand_ink=$4, brand_soft=$5, brand_accent=$6 WHERE id=$7")
        .bind(lic(b.mobile_app_licenses)).bind(lic(b.iot_device_licenses))
        .bind(bp).bind(bi).bind(bs).bind(ba).bind(tid)
        .execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'set_tenant_admin','{}')")
        .bind(tid).bind(&s.email).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---- superadmin: upload / serve logo del tenant ----
pub async fn super_set_logo(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, body: axum::body::Bytes) -> R {
    require_super(&st, &h).await?;
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("image/png").to_string();
    if body.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "file vuoto")); }
    sqlx::query("UPDATE app.tenants SET logo=$1, logo_ct=$2 WHERE id=$3")
        .bind(body.as_ref()).bind(&ct).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn super_del_logo(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    sqlx::query("UPDATE app.tenants SET logo=NULL, logo_ct=NULL WHERE id=$1").bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---- branding visibile al tenant (qualsiasi ruolo autenticato): colori + logo ----
pub async fn branding(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let r = sqlx::query("SELECT name,brand_primary,brand_ink,brand_soft,brand_accent,(logo IS NOT NULL) AS has_logo FROM app.tenants WHERE id=$1")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({
        "name": r.get::<String,_>("name"),
        "brand_primary": r.try_get::<Option<String>,_>("brand_primary").ok().flatten(),
        "brand_ink": r.try_get::<Option<String>,_>("brand_ink").ok().flatten(),
        "brand_soft": r.try_get::<Option<String>,_>("brand_soft").ok().flatten(),
        "brand_accent": r.try_get::<Option<String>,_>("brand_accent").ok().flatten(),
        "has_logo": r.get::<bool,_>("has_logo"),
    })))
}
async fn serve_logo(st: &AppState, tid: Uuid) -> Response {
    let r = sqlx::query("SELECT logo,logo_ct FROM app.tenants WHERE id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    match r.and_then(|r| { let b: Option<Vec<u8>> = r.try_get("logo").ok(); b.map(|b| (b, r.try_get::<Option<String>,_>("logo_ct").ok().flatten().unwrap_or("image/png".into()))) }) {
        Some((bytes, ct)) => ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "no-cache".into())], bytes).into_response(),
        None => (StatusCode::NOT_FOUND, "no logo").into_response(),
    }
}
pub async fn branding_logo(State(st): State<AppState>, h: HeaderMap) -> Response {
    match require_tenant(&st, &h).await { Ok(s) => serve_logo(&st, s.tenant_id.unwrap()).await, Err(e) => e.into_response() }
}
pub async fn super_get_logo(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> Response {
    match require_super(&st, &h).await { Ok(_) => serve_logo(&st, tid).await, Err(e) => e.into_response() }
}

// ---- impersonazione: il superadmin assume la sessione di un admin del tenant ----
pub async fn super_impersonate(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> impl IntoResponse {
    let s = match require_super(&st, &h).await { Ok(s) => s, Err(e) => return e.into_response() };
    // usa un admin reale del tenant (user_id valido per RLS/audit); fallback: primo utente
    let u = sqlx::query("SELECT id AS user_id,email FROM app.users WHERE tenant_id=$1 AND role='tenant_admin' ORDER BY created_at LIMIT 1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let u = match u {
        Some(u) => u,
        None => match sqlx::query("SELECT id AS user_id,email FROM app.users WHERE tenant_id=$1 ORDER BY created_at LIMIT 1")
            .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() {
            Some(u) => u, None => return err(StatusCode::BAD_REQUEST, "il tenant non ha utenti da impersonare").into_response(),
        }
    };
    let sess = Session {
        user_id: u.get("user_id"), tenant_id: Some(tid), email: u.get("email"),
        role: "tenant_admin".into(), is_super: false, impersonator: Some(s.email.clone()),
        caps: effective_caps_user(&st, tid, "tenant_admin", u.get("user_id")).await,
    };
    let tok = auth::new_token(); let csrf = auth::new_token();
    persist_session(&st, &tok, &sess).await;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'impersonate_start',$3)")
        .bind(tid).bind(&s.email).bind(json!({"as":sess.email})).execute(&st.owner_pool).await;
    (set_cookie_header(&tok, &csrf), Json(json!({"ok":true,"as":sess.email}))).into_response()
}
// stop impersonazione: riconia la sessione superadmin (senza nuovo login) e torna al control plane.
pub async fn impersonate_stop(State(st): State<AppState>, h: HeaderMap) -> impl IntoResponse {
    let s = match session_of(&st, &h).await { Some(s) => s, None => return err(StatusCode::UNAUTHORIZED, "no session").into_response() };
    let imp = match &s.impersonator { Some(e) => e.clone(), None => return err(StatusCode::BAD_REQUEST, "non sei in impersonazione").into_response() };
    // verifica che l'impersonator sia davvero un superadmin
    let sa = sqlx::query("SELECT email FROM app.superadmins WHERE email=$1").bind(&imp).fetch_optional(&st.owner_pool).await.ok().flatten();
    if sa.is_none() { return err(StatusCode::FORBIDDEN, "impersonator non valido").into_response(); }
    if let Some(t) = get_cookie(&h, "yw_session") { let _ = sqlx::query("DELETE FROM app.sessions WHERE token=$1").bind(&t).execute(&st.owner_pool).await; }
    let sess = Session { user_id: s.user_id, tenant_id: None, email: imp.clone(), role: "superadmin".into(), is_super: true, impersonator: None, caps: vec![] };
    let tok = auth::new_token(); let csrf = auth::new_token();
    persist_session(&st, &tok, &sess).await;
    if let Some(tid) = s.tenant_id {
        let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'impersonate_stop','{}')")
            .bind(tid).bind(&imp).execute(&st.owner_pool).await;
    }
    (set_cookie_header(&tok, &csrf), Json(json!({"ok":true}))).into_response()
}

pub async fn super_list_modules(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query(
        "SELECT m.code,m.name,m.kind,m.available,tm.state
         FROM app.modules_catalog m LEFT JOIN app.tenant_modules tm ON tm.module_code=m.code AND tm.tenant_id=$1
         ORDER BY m.kind,m.code").bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let code: String = r.get("code");
        let man = crate::modules::manifest(&code);
        json!({
        "code": code, "name": r.get::<String,_>("name"), "kind": r.get::<String,_>("kind"),
        "available": r.get::<bool,_>("available"), "state": r.try_get::<String,_>("state").ok().unwrap_or("non_abilitato".into()),
        // #3 manifest: dipendenze + descrizione (single source of truth in core::modules)
        "requires": man.map(|m| m.requires).unwrap_or(&[]),
        "provides": man.map(|m| m.provides).unwrap_or(&[]),
        "description": man.map(|m| m.description) }) }).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct SetModule { pub module_code: String, pub state: String }

pub async fn set_module(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetModule>) -> R {
    require_super(&st, &h).await?;
    let valid = ["non_abilitato","da_configurare","attivo","degradato","in_disattivazione"];
    if !valid.contains(&b.state.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "bad state")); }
    // Block disabling loneworker if it is the only SOS path for a tenant with open incidents.
    if b.module_code == "loneworker" && b.state == "non_abilitato" {
        let open = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state<>'chiuso'")
            .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
        if open > 0 { return Err(err(StatusCode::CONFLICT, "open incidents: disattivazione bloccata (apri piano di disattivazione)")); }
    }
    sqlx::query("UPDATE app.tenant_modules SET state=$1,updated_at=now() WHERE tenant_id=$2 AND module_code=$3")
        .bind(&b.state).bind(tid).bind(&b.module_code).execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,'superadmin','set_module',$2)")
        .bind(tid).bind(json!({"module":b.module_code,"state":b.state})).execute(&st.owner_pool).await.map_err(ise)?;
    // #3 manifest: avvisi soft sulle dipendenze (non bloccano).
    let active: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT module_code FROM app.tenant_modules WHERE tenant_id=$1 AND state='attivo'")
        .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default().into_iter().collect();
    let mut warning: Option<String> = None;
    if b.state == "attivo" {
        let miss = crate::modules::missing_deps(&b.module_code, &active);
        if !miss.is_empty() { warning = Some(format!("dipendenze non attive: {}", miss.join(", "))); }
    } else if b.state == "non_abilitato" {
        let dep = crate::modules::dependents_active(&b.module_code, &active);
        if !dep.is_empty() { warning = Some(format!("moduli attivi che dipendono da questo: {}", dep.join(", "))); }
    }
    Ok(Json(json!({"ok":true,"warning":warning})))
}

#[derive(Deserialize)]
pub struct SetTraccarConn { pub base_url: String, pub username: String, pub password: Option<String> }
pub async fn super_set_traccar(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetTraccarConn>) -> R {
    require_super(&st, &h).await?;
    let base = { let b = b.base_url.trim_end_matches('/'); b.strip_suffix("/api").unwrap_or(b).to_string() };
    let pw = b.password.unwrap_or_default();
    if pw.is_empty() {
        // password invariata: aggiorna solo URL/utente (richiede connessione esistente)
        let n = sqlx::query("UPDATE app.traccar_connections SET base_url=$2,username=$3,updated_at=now() WHERE tenant_id=$1")
            .bind(tid).bind(&base).bind(&b.username).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
        if n == 0 { return Err(err(StatusCode::BAD_REQUEST, "password richiesta per la prima configurazione")); }
    } else {
        let enc_pass = st.secrets.encrypt(&pw);   // P3: cifrato a riposo
        sqlx::query("INSERT INTO app.traccar_connections(tenant_id,base_url,username,password) VALUES($1,$2,$3,$4)
            ON CONFLICT (tenant_id) DO UPDATE SET base_url=EXCLUDED.base_url,username=EXCLUDED.username,password=EXCLUDED.password,updated_at=now()")
            .bind(tid).bind(&base).bind(&b.username).bind(&enc_pass).execute(&st.owner_pool).await.map_err(ise)?;
    }
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,'superadmin','set_traccar_conn',$2)")
        .bind(tid).bind(json!({"base_url":base})).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct SetGoalert { pub integration_key: String, pub service_id: Option<String>,
    pub gql_user: Option<String>, pub gql_pass: Option<String> }
pub async fn super_set_goalert(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetGoalert>) -> R {
    require_super(&st, &h).await?;
    let enc_pass = b.gql_pass.as_ref().map(|p| st.secrets.encrypt(p));   // P3: cifrato a riposo
    sqlx::query("INSERT INTO app.goalert_services(tenant_id,integration_key,service_id,gql_user,gql_pass) VALUES($1,$2,$3,$4,$5)
        ON CONFLICT (tenant_id) DO UPDATE SET integration_key=EXCLUDED.integration_key,
          service_id=COALESCE(EXCLUDED.service_id,goalert_services.service_id),
          gql_user=COALESCE(EXCLUDED.gql_user,goalert_services.gql_user),
          gql_pass=COALESCE(EXCLUDED.gql_pass,goalert_services.gql_pass), updated_at=now()")
        .bind(tid).bind(&b.integration_key).bind(&b.service_id).bind(&b.gql_user).bind(&enc_pass)
        .execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,'superadmin','set_goalert_key',$2)")
        .bind(tid).bind(json!({"len":b.integration_key.len(),"service_id":b.service_id})).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// Helper: sessione GoAlert per il tenant (login via credenziali salvate).
async fn goalert_session(st: &AppState, tid: Uuid) -> Option<(String, String, Option<String>)> {
    let base = st.goalert_base.clone()?;
    let row = sqlx::query("SELECT gql_user,gql_pass,service_id FROM app.goalert_services WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let user: String = row.try_get("gql_user").ok()?; let pass: String = row.try_get("gql_pass").ok()?;
    let pass = st.secrets.decrypt(&pass);   // P3: decifra a riposo
    let sid: Option<String> = row.try_get("service_id").ok();
    let cookie = goalert::login(&st.http, &base, &user, &pass).await?;
    Some((base, cookie, sid))
}

// P2: stato reale dell'alert GoAlert di un incidente (ack/close/escalation).
pub async fn goalert_status(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let aid: Option<i32> = sqlx::query_scalar("SELECT goalert_alert_id FROM app.incidents WHERE id=$1 AND tenant_id=$2")
        .bind(iid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let Some(aid) = aid else { return Ok(Json(json!({"available":false,"reason":"nessun alert GoAlert collegato"}))); };
    let Some((base, cookie, _)) = goalert_session(&st, tid).await else {
        return Ok(Json(json!({"available":false,"reason":"credenziali GoAlert non configurate"}))); };
    let q = format!("{{alert(id:{aid}){{id status summary}}}}");
    match goalert::graphql(&st.http, &base, &cookie, &q).await {
        Some(v) => { let a = v.pointer("/data/alert");
            Ok(Json(json!({"available":true,"alert_id":aid,
                "status": a.and_then(|x| x.get("status")).and_then(|x| x.as_str()),
                "summary": a.and_then(|x| x.get("summary")).and_then(|x| x.as_str())}))) }
        None => Ok(Json(json!({"available":false,"reason":"GoAlert non raggiungibile"}))),
    }
}

// (c) ack / escalate dell'alert GoAlert dalla nostra console.
async fn goalert_alert_action(st: &AppState, h: &HeaderMap, iid: Uuid, mutation: impl Fn(i64)->String) -> R {
    let s = require_tenant(st, h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let aid: Option<i32> = sqlx::query_scalar("SELECT goalert_alert_id FROM app.incidents WHERE id=$1 AND tenant_id=$2")
        .bind(iid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let aid = aid.ok_or_else(|| err(StatusCode::BAD_REQUEST, "nessun alert GoAlert collegato"))?;
    let (base, cookie, _) = goalert_session(st, tid).await
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "credenziali GoAlert non configurate"))?;
    let v = goalert::graphql(&st.http, &base, &cookie, &mutation(aid as i64)).await
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "GoAlert non raggiungibile"))?;
    if v.get("errors").is_some() { return Err(err(StatusCode::BAD_GATEWAY, &format!("GoAlert: {}", v.get("errors").unwrap()))); }
    Ok(Json(json!({"ok":true})))
}
pub async fn goalert_ack(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>) -> R {
    let r = goalert_alert_action(&st, &h, iid, |aid| format!("mutation{{updateAlerts(input:{{alertIDs:[{aid}],newStatus:StatusAcknowledged}}){{id}}}}")).await?;
    if let Ok(s)=require_tenant(&st,&h).await { let tid=s.tenant_id.unwrap();
        let _=sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'human_ack',$3)")
            .bind(tid).bind(iid).bind(json!({"by":s.email,"via":"goalert"})).execute(&st.owner_pool).await; }
    Ok(r)
}
pub async fn goalert_escalate(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>) -> R {
    goalert_alert_action(&st, &h, iid, |aid| format!("mutation{{escalateAlerts(input:[{aid}]){{id}}}}")).await
}

// P2: chi è reperibile adesso sul servizio del tenant.
pub async fn goalert_oncall(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor"|"responsabile") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let Some((base, cookie, sid)) = goalert_session(&st, tid).await else {
        return Ok(Json(json!({"available":false,"reason":"GoAlert non configurato"}))); };
    let Some(sid) = sid else { return Ok(Json(json!({"available":false,"reason":"service_id non impostato"}))); };
    let q = format!("{{service(id:\"{sid}\"){{onCallUsers{{userName stepNumber}}}}}}");
    match goalert::graphql(&st.http, &base, &cookie, &q).await {
        Some(v) => { let arr = v.pointer("/data/service/onCallUsers").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let out: Vec<Value> = arr.iter().map(|u| json!({
                "user": u.get("userName").and_then(|x| x.as_str()),
                "step": u.get("stepNumber").and_then(|x| x.as_i64())})).collect();
            Ok(Json(json!({"available":true,"oncall":out}))) }
        None => Ok(Json(json!({"available":false,"reason":"GoAlert non raggiungibile"}))),
    }
}

/// Credenziali admin GoAlert di default (il motore è integrato nel prodotto): da env
/// GOALERT_ADMIN_USER / GOALERT_ADMIN_PASS, altrimenti i default noti. Così il provisioning
/// avviene automaticamente, senza chiedere nulla all'amministratore.
pub fn goalert_admin_creds() -> (String, String) {
    (std::env::var("GOALERT_ADMIN_USER").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "admin".into()),
     std::env::var("GOALERT_ADMIN_PASS").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "goalertadmin1".into()))
}
/// Provisiona GoAlert per un tenant (EP + servizio + generic key + heartbeat). Riusabile da
/// handler, creazione tenant e worker. Ritorna i riferimenti creati o un errore parlante.
pub async fn do_goalert_provision(st: &AppState, tid: Uuid, user: &str, pass: &str, name: Option<&str>) -> Result<Value, (StatusCode,String)> {
    let base = st.goalert_base.clone().ok_or_else(|| err(StatusCode::BAD_REQUEST, "GOALERT_BASE non configurato (motore escalation non raggiungibile)"))?;
    let cookie = goalert::login(&st.http, &base, user, pass).await
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "login GoAlert fallito (credenziali/connessione)"))?;
    let b = GoalertProvision{ user: Some(user.to_string()), pass: Some(pass.to_string()), name: name.map(String::from) };
    goalert_provision_inner(st, tid, &base, &cookie, &b).await
}

// P3: provisioning GoAlert per-tenant (crea escalation policy + servizio + generic key).
#[derive(Deserialize)]
pub struct GoalertProvision { pub user: Option<String>, pub pass: Option<String>, pub name: Option<String> }
pub async fn goalert_provision(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<GoalertProvision>) -> R {
    require_super(&st, &h).await?;
    // credenziali: quelle passate oppure i default del prodotto (nessun prompt)
    let (du, dp) = goalert_admin_creds();
    let user = b.user.clone().filter(|s| !s.is_empty()).unwrap_or(du);
    let pass = b.pass.clone().filter(|s| !s.is_empty()).unwrap_or(dp);
    let out = do_goalert_provision(&st, tid, &user, &pass, b.name.as_deref()).await?;
    Ok(Json(out))
}
// ============ Auto-integrazioni: moduli integrati configurati dal prodotto, non dall'uomo ============
// I motori integrati (GoAlert, Traccar, ThingsBoard) girano sulla nostra infrastruttura: base URL e
// credenziali admin vengono dai default del prodotto (env), così l'attivazione di un modulo li
// configura DA SOLA, senza intervento dell'amministratore. I provider AI restano esterni (chiave cliente).
fn env_opt(k: &str) -> Option<String> { std::env::var(k).ok().filter(|s| !s.trim().is_empty()) }
fn norm_base(b: &str) -> String { let b=b.trim_end_matches('/'); b.strip_suffix("/api").unwrap_or(b).to_string() }

/// Provisiona GoAlert automaticamente se il modulo è attivo e non è già provisionato.
pub async fn auto_provision_goalert(st: &AppState, tid: Uuid) -> Result<bool, String> {
    if !module_active(st, tid, "goalert").await { return Ok(false); }
    if st.goalert_base.is_none() { return Err("motore GoAlert non raggiungibile (GOALERT_BASE)".into()); }
    let has: i64 = sqlx::query_scalar("SELECT count(*) FROM app.goalert_services WHERE tenant_id=$1")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(|e| e.to_string())?;
    if has > 0 { return Ok(false); }
    let (u,p) = goalert_admin_creds();
    do_goalert_provision(st, tid, &u, &p, None).await.map(|_| true).map_err(|e| e.1)
}
/// Connette Traccar automaticamente dai default del prodotto se il modulo è attivo.
pub async fn auto_connect_traccar(st: &AppState, tid: Uuid) -> Result<bool, String> {
    if !module_active(st, tid, "traccar").await { return Ok(false); }
    let base = env_opt("TRACCAR_BASE").ok_or("motore Traccar non configurato nel prodotto (TRACCAR_BASE)")?;
    let has: i64 = sqlx::query_scalar("SELECT count(*) FROM app.traccar_connections WHERE tenant_id=$1")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(|e| e.to_string())?;
    if has > 0 { return Ok(false); }
    let user = env_opt("TRACCAR_USER").unwrap_or_else(|| "admin".into());
    let pass = env_opt("TRACCAR_PASS").unwrap_or_else(|| "admin".into());
    let enc = st.secrets.encrypt(&pass);
    sqlx::query("INSERT INTO app.traccar_connections(tenant_id,base_url,username,password,poll_enabled) VALUES($1,$2,$3,$4,true)
        ON CONFLICT (tenant_id) DO NOTHING")
        .bind(tid).bind(norm_base(&base)).bind(&user).bind(&enc).execute(&st.owner_pool).await.map_err(|e| e.to_string())?;
    audit(st, tid, "system", "auto_connect_traccar", json!({"base":norm_base(&base)})).await;
    Ok(true)
}
/// Connette ThingsBoard automaticamente dai default del prodotto se il modulo è attivo.
pub async fn auto_connect_thingsboard(st: &AppState, tid: Uuid) -> Result<bool, String> {
    if !module_active(st, tid, "thingsboard").await { return Ok(false); }
    let base = env_opt("THINGSBOARD_BASE").ok_or("motore ThingsBoard non configurato nel prodotto (THINGSBOARD_BASE)")?;
    let has: i64 = sqlx::query_scalar("SELECT count(*) FROM app.thingsboard_connections WHERE tenant_id=$1")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(|e| e.to_string())?;
    if has > 0 { return Ok(false); }
    let user = env_opt("THINGSBOARD_USER").unwrap_or_else(|| "tenant@thingsboard.org".into());
    let pass = env_opt("THINGSBOARD_PASS").unwrap_or_else(|| "tenant".into());
    let enc = st.secrets.encrypt(&pass);
    sqlx::query("INSERT INTO app.thingsboard_connections(tenant_id,base_url,username,password,poll_enabled) VALUES($1,$2,$3,$4,true)
        ON CONFLICT (tenant_id) DO NOTHING")
        .bind(tid).bind(norm_base(&base)).bind(&user).bind(&enc).execute(&st.owner_pool).await.map_err(|e| e.to_string())?;
    audit(st, tid, "system", "auto_connect_thingsboard", json!({"base":norm_base(&base)})).await;
    Ok(true)
}
/// Stato delle integrazioni di un tenant (semaforo: ok / anomalia / spento). Segnala solo i problemi.
pub async fn super_integrations_status(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    // GoAlert
    let ga_on = module_active(&st, tid, "goalert").await;
    let ga_prov: i64 = sqlx::query_scalar("SELECT count(*) FROM app.goalert_services WHERE tenant_id=$1").bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    let ga = if !ga_on { json!({"module":false,"status":"spento"}) }
        else if st.goalert_base.is_none() { json!({"module":true,"status":"anomalia","detail":"motore GoAlert non raggiungibile"}) }
        else if ga_prov==0 { json!({"module":true,"status":"anomalia","detail":"provisioning in corso o fallito"}) }
        else { json!({"module":true,"status":"ok","detail":"provisionato"}) };
    // Traccar
    let tr_on = module_active(&st, tid, "traccar").await;
    let tr_cfg: i64 = sqlx::query_scalar("SELECT count(*) FROM app.traccar_connections WHERE tenant_id=$1").bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    let tr_reach: Option<bool> = sqlx::query_scalar("SELECT reachable FROM app.traccar_poll_state WHERE tenant_id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let tr = if !tr_on { json!({"module":false,"status":"spento"}) }
        else if tr_cfg==0 { json!({"module":true,"status":"anomalia","detail":"motore Traccar non configurato nel prodotto (TRACCAR_BASE)"}) }
        else if tr_reach==Some(false) { json!({"module":true,"status":"anomalia","detail":"server Traccar irraggiungibile"}) }
        else { json!({"module":true,"status":"ok","detail":"connesso"}) };
    // ThingsBoard
    let tb_on = module_active(&st, tid, "thingsboard").await;
    let tb_cfg: i64 = sqlx::query_scalar("SELECT count(*) FROM app.thingsboard_connections WHERE tenant_id=$1").bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    let tb_reach: Option<bool> = sqlx::query_scalar("SELECT reachable FROM app.thingsboard_poll_state WHERE tenant_id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let tb = if !tb_on { json!({"module":false,"status":"spento"}) }
        else if tb_cfg==0 { json!({"module":true,"status":"anomalia","detail":"motore ThingsBoard non configurato nel prodotto (THINGSBOARD_BASE)"}) }
        else if tb_reach==Some(false) { json!({"module":true,"status":"anomalia","detail":"server ThingsBoard irraggiungibile"}) }
        else { json!({"module":true,"status":"ok","detail":"connesso"}) };
    Ok(Json(json!({"goalert":ga,"traccar":tr,"thingsboard":tb})))
}

/// Elimina la configurazione di un motore SOLO quando il modulo è esplicitamente spento
/// (non_abilitato o assente). Stati intermedi (da_configurare/degradato/in_disattivazione)
/// MANTENGONO le credenziali: non si cancella un segreto su uno stato transitorio.
async fn deauto_integration(st: &AppState, tid: Uuid, module: &str) {
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM app.tenant_modules WHERE tenant_id=$1 AND module_code=$2")
        .bind(tid).bind(module).fetch_optional(&st.owner_pool).await.ok().flatten();
    let off = matches!(state.as_deref(), None | Some("non_abilitato"));
    if !off { return; }
    let (table, extra): (&str, Option<&str>) = match module {
        "goalert" => ("app.goalert_services", None),
        "traccar" => ("app.traccar_connections", Some("app.traccar_poll_state")),
        "thingsboard" => ("app.thingsboard_connections", Some("app.thingsboard_poll_state")),
        _ => return,
    };
    let n = sqlx::query(&format!("DELETE FROM {table} WHERE tenant_id=$1")).bind(tid).execute(&st.owner_pool).await.map(|r| r.rows_affected()).unwrap_or(0);
    if let Some(ps) = extra { let _ = sqlx::query(&format!("DELETE FROM {ps} WHERE tenant_id=$1")).bind(tid).execute(&st.owner_pool).await; }
    if n > 0 { audit(st, tid, "system", "deauto_integration", json!({"module":module})).await; }
}
/// Riconcilia tutte le integrazioni dei tenant (idempotente). Chiamata dal worker.
/// Auto-provisioning se il modulo è attivo (serve); eliminazione se è spento (non serve).
pub async fn reconcile_integrations(st: &AppState) {
    let tids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM app.tenants").fetch_all(&st.owner_pool).await.unwrap_or_default();
    for tid in tids {
        if let Err(e) = auto_provision_goalert(st, tid).await { tracing::debug!("auto goalert {tid}: {e}"); }
        if let Err(e) = auto_connect_traccar(st, tid).await { tracing::debug!("auto traccar {tid}: {e}"); }
        if let Err(e) = auto_connect_thingsboard(st, tid).await { tracing::debug!("auto thingsboard {tid}: {e}"); }
        for m in ["goalert","traccar","thingsboard"] { deauto_integration(st, tid, m).await; }
    }
}

async fn goalert_provision_inner(st: &AppState, tid: Uuid, base: &str, cookie: &str, b: &GoalertProvision) -> Result<Value,(StatusCode,String)> {
    let user = b.user.clone().unwrap_or_default();
    let pass = b.pass.clone().unwrap_or_default();
    // suffisso unico: i nomi EP/service devono essere univoci su GoAlert (re-provision sicuro)
    let suf = chrono::Utc::now().timestamp();
    let name = format!("{} {}", b.name.clone().unwrap_or_else(|| format!("YW {}", &tid.to_string()[..8])), suf);
    // EP (repeat 1, nessuno step: lo configura l'admin in GoAlert), servizio, generic key
    let ep = goalert::graphql(&st.http,base,cookie,
        &format!("mutation{{createEscalationPolicy(input:{{name:\"{name} EP\",repeat:1}}){{id}}}}")).await
        .and_then(|v| v.pointer("/data/createEscalationPolicy/id").and_then(|x| x.as_str().map(String::from)))
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "createEscalationPolicy fallito"))?;
    let sid = goalert::graphql(&st.http,base,cookie,
        &format!("mutation{{createService(input:{{name:\"{name}\",escalationPolicyID:\"{ep}\"}}){{id}}}}")).await
        .and_then(|v| v.pointer("/data/createService/id").and_then(|x| x.as_str().map(String::from)))
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "createService fallito"))?;
    let key = goalert::graphql(&st.http,base,cookie,
        &format!("mutation{{createIntegrationKey(input:{{serviceID:\"{sid}\",type:generic,name:\"core-outbox\"}}){{id}}}}")).await
        .and_then(|v| v.pointer("/data/createIntegrationKey/id").and_then(|x| x.as_str().map(String::from)))
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "createIntegrationKey fallito"))?;
    // heartbeat monitor (P3): avvisa se la nostra pipeline smette di pingare
    let hb = goalert::graphql(&st.http,base,cookie,
        &format!("mutation{{createHeartbeatMonitor(input:{{serviceID:\"{sid}\",name:\"yw-core\",timeoutMinutes:5}}){{href}}}}")).await
        .and_then(|v| v.pointer("/data/createHeartbeatMonitor/href").and_then(|x| x.as_str().map(String::from)));
    let enc_pass = st.secrets.encrypt(&pass);   // P3: cifrato a riposo
    sqlx::query("INSERT INTO app.goalert_services(tenant_id,integration_key,service_id,gql_user,gql_pass,heartbeat_url) VALUES($1,$2,$3,$4,$5,$6)
        ON CONFLICT (tenant_id) DO UPDATE SET integration_key=EXCLUDED.integration_key,service_id=EXCLUDED.service_id,
          gql_user=EXCLUDED.gql_user,gql_pass=EXCLUDED.gql_pass,heartbeat_url=EXCLUDED.heartbeat_url,updated_at=now()")
        .bind(tid).bind(&key).bind(&sid).bind(&user).bind(&enc_pass).bind(&hb).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "superadmin", "goalert_provision", json!({"service_id":sid})).await;
    Ok(json!({"ok":true,"escalation_policy":ep,"service_id":sid,"integration_key":key,"heartbeat_url":hb}))
}

// ---- "YouWorkforce pilota GoAlert": sincronizza Gruppi/Routing/Escalation -> GoAlert ----
// Escapa una stringa per inserirla in un literal GraphQL.
fn esc_gql(s: &str) -> String { s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ") }
fn parse_hhmm(s: &str) -> Option<(u32,u32)> {
    let mut it = s.split(':'); let h: u32 = it.next()?.trim().parse().ok()?; let m: u32 = it.next()?.trim().parse().ok()?;
    if h < 24 && m < 60 { Some((h,m)) } else { None }
}
/// Start della rotation GoAlert: l'istante determina l'ora (e per 'weekly' il giorno) di
/// handoff. 2024-01-01 e' un lunedi'; handoff_day 0=lun..6=dom mappa 2024-01-01..07.
fn rotation_start(handoff_time: Option<&str>, handoff_day: Option<i32>) -> String {
    let day = handoff_day.unwrap_or(0).clamp(0,6);
    let (h,m) = handoff_time.and_then(parse_hhmm).unwrap_or((0,0));
    format!("2024-01-{:02}T{:02}:{:02}:00Z", 1+day, h, m)
}
/// WeekdayFilter GoAlert = [Dom,Lun,Mar,Mer,Gio,Ven,Sab] (0=Dom). cov_days e' lun..dom.
fn weekday_filter(cov_days: &str) -> String {
    let b: Vec<char> = cov_days.chars().collect();
    let g = |i: usize| b.get(i).map(|c| *c == '1').unwrap_or(true);
    let order = [6usize,0,1,2,3,4,5]; // Dom,Lun..Sab -> indici lun..dom
    let arr: Vec<&str> = order.iter().map(|&i| if g(i) {"true"} else {"false"}).collect();
    format!("[{}]", arr.join(","))
}
// GoAlert consente nei nomi solo lettere/cifre/trattino/underscore/apostrofo/spazio.
fn ga_name(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c=='-' || c=='_' || c=='\'' || c==' ' { c } else { ' ' }).collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}
async fn gql_id(st: &AppState, base: &str, cookie: &str, q: &str, ptr: &str) -> Option<String> {
    goalert::graphql(&st.http, base, cookie, q).await
        .and_then(|v| v.pointer(ptr).and_then(|x| x.as_str().map(String::from)))
}
/// Impostazioni reperibilità del tenant (ladder vocale + status updates). Default sicuri.
pub struct OncallCfg { pub voice_on: bool, pub voice_min: i32, pub status_on: bool }
async fn oncall_cfg(st: &AppState, tid: Uuid) -> OncallCfg {
    let r = sqlx::query("SELECT voice_fallback_on,voice_fallback_min,status_updates_on FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    match r {
        Some(row) => OncallCfg{
            voice_on: row.try_get("voice_fallback_on").unwrap_or(true),
            voice_min: row.try_get("voice_fallback_min").unwrap_or(5),
            status_on: row.try_get("status_updates_on").unwrap_or(true) },
        None => OncallCfg{ voice_on:true, voice_min:5, status_on:true },
    }
}
/// Provisiona in GoAlert i recapiti dell'utente + la ladder di notifica.
/// EMAIL/SMS immediati (@0); VOICE di fallback @voice_min se abilitato (safety: se l'SMS
/// non riceve risposta, parte la chiamata). enableStatusUpdates sul recapito primario.
/// Crea solo i tipi con trasporto abilitato (EMAIL se SMTP, SMS/VOICE se Twilio). Idempotente.
async fn ensure_contact_methods(st: &AppState, base: &str, cookie: &str, guid: &str, email: &str, phone: Option<&str>, sms_on: bool, email_on: bool, cfg: &OncallCfg) -> i64 {
    let existing = goalert::graphql(&st.http, base, cookie,
        &format!("{{user(id:\"{guid}\"){{contactMethods{{type value}}}}}}")).await;
    let has = |ty: &str, val: &str| existing.as_ref()
        .and_then(|v| v.pointer("/data/user/contactMethods").and_then(|a| a.as_array()))
        .map(|a| a.iter().any(|c| c.get("type").and_then(|x| x.as_str()) == Some(ty)
            && c.get("value").and_then(|x| x.as_str()) == Some(val))).unwrap_or(false);
    let su = if cfg.status_on { "true" } else { "false" };
    let mut n = 0i64;
    let p = phone.unwrap_or("").trim();
    let have_sms = sms_on && !p.is_empty() && is_e164(p);
    // EMAIL immediata (status updates solo se non c'e' SMS, per non duplicare)
    if email_on && !email.is_empty() && !has("EMAIL", email) {
        let esu = if cfg.status_on && !have_sms { "true" } else { "false" };
        let q = format!("mutation{{createUserContactMethod(input:{{userID:\"{guid}\",name:\"YW email\",type:EMAIL,value:\"{}\",enableStatusUpdates:{esu},newUserNotificationRule:{{delayMinutes:0}}}}){{id}}}}", esc_gql(email));
        if gql_id(st, base, cookie, &q, "/data/createUserContactMethod/id").await.is_some() { n += 1; }
    }
    // SMS immediato (recapito primario per gli status updates)
    if have_sms && !has("SMS", p) {
        let q = format!("mutation{{createUserContactMethod(input:{{userID:\"{guid}\",name:\"YW sms\",type:SMS,value:\"{}\",enableStatusUpdates:{su},newUserNotificationRule:{{delayMinutes:0}}}}){{id}}}}", esc_gql(p));
        if gql_id(st, base, cookie, &q, "/data/createUserContactMethod/id").await.is_some() { n += 1; }
    }
    // VOICE di fallback @voice_min (chiamata se l'SMS non riceve risposta)
    if cfg.voice_on && have_sms && !has("VOICE", p) {
        let q = format!("mutation{{createUserContactMethod(input:{{userID:\"{guid}\",name:\"YW voce\",type:VOICE,value:\"{}\",newUserNotificationRule:{{delayMinutes:{}}}}}){{id}}}}", esc_gql(p), cfg.voice_min);
        if gql_id(st, base, cookie, &q, "/data/createUserContactMethod/id").await.is_some() { n += 1; }
    }
    n
}
/// Costruisce in GoAlert, in modo idempotente, la struttura che rispecchia la config
/// del tenant: ogni Gruppo allarme -> escalation policy (step sui membri, repeat da
/// sos_escalation) + servizio + integration key; ogni routing etype->gruppo popola
/// app.goalert_routing con la key del gruppo (il drain instrada gia' per etype).
pub async fn sync_goalert(st: &AppState, tid: Uuid) -> Result<Value, (StatusCode, String)> {
    let base = st.goalert_base.clone().ok_or_else(|| err(StatusCode::BAD_REQUEST, "GOALERT_BASE non configurato"))?;
    let svc = sqlx::query("SELECT gql_user,gql_pass FROM app.goalert_services WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "GoAlert non provisionato per questo tenant (chiedi al superadmin: Provisiona GoAlert)"))?;
    let gu: String = svc.get("gql_user");
    let gp_enc: String = svc.try_get("gql_pass").unwrap_or_default();
    let gp = st.secrets.decrypt(&gp_enc);
    let cookie = goalert::login(&st.http, &base, &gu, &gp).await
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "login GoAlert fallito (credenziali di provisioning)"))?;
    // parametri escalation dal tool loneworker/sos_escalation
    let (_pri, repeat) = sos_escalation(st, tid).await;
    let delay_min = if repeat > 0 { ((repeat + 59) / 60).max(1) } else { 15 };
    let ep_repeat = if repeat > 0 { 3 } else { 0 };

    // Quali destinazioni sono abilitate su GoAlert? servono per creare i recapiti reali.
    let cfg = goalert::graphql(&st.http, &base, &cookie, "{config(all:true){id value}}").await;
    let cfg_on = |id: &str| cfg.as_ref()
        .and_then(|v| v.pointer("/data/config").and_then(|a| a.as_array()))
        .map(|a| a.iter().any(|c| c.get("id").and_then(|x| x.as_str()) == Some(id)
            && c.get("value").and_then(|x| x.as_str()) == Some("true"))).unwrap_or(false);
    let sms_on = cfg_on("Twilio.Enable");
    let email_on = cfg_on("SMTP.Enable");
    let slack_on = cfg_on("Slack.Enable");
    let occfg = oncall_cfg(st, tid).await;
    let slug: String = sqlx::query_scalar("SELECT slug FROM app.tenants WHERE id=$1").bind(tid)
        .fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or_default();

    let groups = sqlx::query("SELECT id,name,goalert_ep_id,goalert_service_id,goalert_key,oncall_mode,oncall_type,oncall_shift,oncall_tz,goalert_rotation_id,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day,slack_channel_id,slack_notify,goalert_schedule_id FROM app.alarm_groups WHERE tenant_id=$1 ORDER BY name")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let (mut n_users, mut n_groups, mut skipped, mut n_cm, mut no_contact, mut n_rot) = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    for g in &groups {
        let gid: Uuid = g.get("id");
        let gname: String = g.get("name");
        let members = sqlx::query("SELECT u.id,u.email,u.phone,u.goalert_user_id FROM app.alarm_group_members m JOIN app.users u ON u.id=m.user_id WHERE m.group_id=$1 ORDER BY m.position")
            .bind(gid).fetch_all(&st.owner_pool).await.map_err(ise)?;
        if members.is_empty() { skipped += 1; continue; }
        // assicura l'utente GoAlert per ogni membro + i suoi recapiti
        let mut uids: Vec<String> = Vec::new();
        for m in &members {
            let uid: Uuid = m.get("id");
            let email: String = m.get("email");
            let phone: Option<String> = m.try_get("phone").ok().flatten();
            let mut g_uid: Option<String> = m.try_get("goalert_user_id").ok().flatten();
            if g_uid.is_none() {
                let uname = format!("yw_{}_{}", &tid.to_string()[..8], &uid.to_string()[..8]);
                let pw = auth::new_token();
                let q = format!("mutation{{createUser(input:{{username:\"{uname}\",password:\"{pw}\",name:\"{}\",email:\"{}\"}}){{id}}}}", esc_gql(&email), esc_gql(&email));
                g_uid = gql_id(st, &base, &cookie, &q, "/data/createUser/id").await;
                if let Some(ref id) = g_uid {
                    sqlx::query("UPDATE app.users SET goalert_user_id=$1 WHERE id=$2").bind(id).bind(uid).execute(&st.owner_pool).await.map_err(ise)?;
                    n_users += 1;
                }
            }
            if let Some(id) = &g_uid {
                // recapiti reali + ladder vocale: EMAIL/SMS @0 + VOICE fallback @N
                let created = ensure_contact_methods(st, &base, &cookie, id, &email, phone.as_deref(), sms_on, email_on, &occfg).await;
                n_cm += created;
                let has_any = (email_on && !email.is_empty()) || (sms_on && phone.as_deref().map(|p| !p.is_empty()).unwrap_or(false));
                if !has_any { no_contact += 1; }
            }
            if let Some(id) = g_uid { uids.push(id); }
        }
        if uids.is_empty() { skipped += 1; continue; }
        // nuova escalation policy + step (idempotenza: si ricrea la EP, si ripunta il servizio, si elimina la vecchia)
        let suf = chrono::Utc::now().timestamp_millis();
        let epname = format!("YW {} {} {}", &tid.to_string()[..8], ga_name(&gname), suf);
        let new_ep = gql_id(st, &base, &cookie,
            &format!("mutation{{createEscalationPolicy(input:{{name:\"{}\",repeat:{ep_repeat}}}){{id}}}}", esc_gql(&epname)),
            "/data/createEscalationPolicy/id").await
            .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "createEscalationPolicy fallito"))?;
        // config copertura/handoff/slack del gruppo
        let mode: String = g.get("oncall_mode");
        let coverage: String = g.try_get("coverage_mode").unwrap_or_else(|_| "24_7".into());
        let cov_start: Option<String> = g.try_get("cov_start").ok().flatten();
        let cov_end: Option<String> = g.try_get("cov_end").ok().flatten();
        let cov_days: String = g.try_get("cov_days").unwrap_or_else(|_| "1111111".into());
        let handoff_time: Option<String> = g.try_get("handoff_time").ok().flatten();
        let handoff_day: Option<i32> = g.try_get("handoff_day").ok().flatten();
        let slack_ch: Option<String> = g.try_get("slack_channel_id").ok().flatten();
        let slack_notify: String = g.try_get("slack_notify").unwrap_or_else(|_| "off".into());

        // base target(s): rotation (turni) o utenti (tutti)
        let base_targets: Vec<String> = if mode == "turni" {
            let oc_type: String = g.get("oncall_type");
            let oc_shift: i32 = g.get("oncall_shift");
            let oc_tz: String = g.get("oncall_tz");
            let start = rotation_start(handoff_time.as_deref(), handoff_day);
            let mut rot_id: Option<String> = g.try_get("goalert_rotation_id").ok().flatten();
            let rname = format!("YW {} {} turni {}", &tid.to_string()[..8], ga_name(&gname), suf);
            if rot_id.is_none() {
                rot_id = gql_id(st, &base, &cookie,
                    &format!("mutation{{createRotation(input:{{name:\"{}\",type:{oc_type},start:\"{start}\",shiftLength:{oc_shift},timeZone:\"{}\"}}){{id}}}}", esc_gql(&rname), esc_gql(&oc_tz)),
                    "/data/createRotation/id").await;
            }
            if let Some(ref rid) = rot_id {
                let uidlist = uids.iter().map(|u| format!("\"{u}\"")).collect::<Vec<_>>().join(",");
                let _ = goalert::graphql(&st.http, &base, &cookie,
                    &format!("mutation{{updateRotation(input:{{id:\"{rid}\",type:{oc_type},start:\"{start}\",shiftLength:{oc_shift},timeZone:\"{}\",userIDs:[{uidlist}]}})}}", esc_gql(&oc_tz))).await;
                sqlx::query("UPDATE app.alarm_groups SET goalert_rotation_id=$1 WHERE id=$2").bind(rid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?;
                n_rot += 1;
                vec![format!("{{type:rotation,id:\"{rid}\"}}")]
            } else {
                uids.iter().map(|id| format!("{{type:user,id:\"{id}\"}}")).collect()
            }
        } else {
            uids.iter().map(|id| format!("{{type:user,id:\"{id}\"}}")).collect()
        };

        // Avvolgi sempre on-call in una Schedule (modello nativo GoAlert): abilita copertura
        // a fasce (#3), override/assenze (#4) e on-call-slack (#9) in modo uniforme. Fallback
        // sicuro: se la Schedule non si crea, lo step punta direttamente a rotation/utenti.
        let want_schedule = true;
        let mut step_targets: Vec<String> = base_targets.clone();
        if want_schedule {
            let oc_tz: String = g.get("oncall_tz");
            let mut sched_id: Option<String> = g.try_get("goalert_schedule_id").ok().flatten();
            if sched_id.is_none() {
                let scname = format!("YW {} {} sched {}", &tid.to_string()[..8], ga_name(&gname), suf);
                sched_id = gql_id(st, &base, &cookie,
                    &format!("mutation{{createSchedule(input:{{name:\"{}\",timeZone:\"{}\"}}){{id}}}}", esc_gql(&scname), esc_gql(&oc_tz)),
                    "/data/createSchedule/id").await;
            }
            if let Some(ref scid) = sched_id {
                // regole di copertura: 'orari' = finestra+giorni; altrimenti 24/7
                let rules = if coverage == "orari" {
                    let s = cov_start.as_deref().unwrap_or("00:00");
                    let e = cov_end.as_deref().unwrap_or("00:00");
                    format!("{{start:\"{s}\",end:\"{e}\",weekdayFilter:{}}}", weekday_filter(&cov_days))
                } else {
                    "{start:\"00:00\",end:\"00:00\",weekdayFilter:[true,true,true,true,true,true,true]}".to_string()
                };
                // ogni target base diventa un target della schedule con le stesse regole
                for bt in &base_targets {
                    let tgt = bt.trim_start_matches('{').trim_end_matches('}');
                    let _ = goalert::graphql(&st.http, &base, &cookie,
                        &format!("mutation{{updateScheduleTarget(input:{{scheduleID:\"{scid}\",target:{{{tgt}}},rules:[{rules}]}})}}")).await;
                }
                sqlx::query("UPDATE app.alarm_groups SET goalert_schedule_id=$1 WHERE id=$2").bind(scid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?;
                step_targets = vec![format!("{{type:schedule,id:\"{scid}\"}}")];
                // #9 on-call notifications su Slack
                if slack_on { if let Some(ref ch) = slack_ch {
                    let rule = if slack_notify == "onchange" || slack_notify == "off" {
                        format!("{{target:{{type:slackChannel,id:\"{}\"}}}}", esc_gql(ch))
                    } else {
                        format!("{{target:{{type:slackChannel,id:\"{}\"}},time:\"{}\",weekdayFilter:[true,true,true,true,true,true,true]}}", esc_gql(ch), esc_gql(&slack_notify))
                    };
                    if slack_notify != "off" {
                        let _ = goalert::graphql(&st.http, &base, &cookie,
                            &format!("mutation{{setScheduleOnCallNotificationRules(input:{{scheduleID:\"{scid}\",rules:[{rule}]}})}}")).await;
                    }
                }}
            }
        }
        // #10 canale Slack come ulteriore target dello step di escalation
        if slack_on { if let Some(ref ch) = slack_ch {
            step_targets.push(format!("{{type:slackChannel,id:\"{}\"}}", esc_gql(ch)));
        }}
        let targets = step_targets.join(",");
        let _ = goalert::graphql(&st.http, &base, &cookie,
            &format!("mutation{{createEscalationPolicyStep(input:{{escalationPolicyID:\"{new_ep}\",delayMinutes:{delay_min},targets:[{targets}]}}){{id}}}}")).await;
        // servizio + integration key (la key resta stabile tra le sync)
        let mut sid: Option<String> = g.try_get("goalert_service_id").ok().flatten();
        let mut key: Option<String> = g.try_get("goalert_key").ok().flatten();
        let old_ep: Option<String> = g.try_get("goalert_ep_id").ok().flatten();
        if sid.is_none() {
            let sname = format!("YW {} {} {}", &tid.to_string()[..8], ga_name(&gname), suf);
            sid = gql_id(st, &base, &cookie,
                &format!("mutation{{createService(input:{{name:\"{}\",escalationPolicyID:\"{new_ep}\"}}){{id}}}}", esc_gql(&sname)),
                "/data/createService/id").await;
            if let Some(ref s) = sid {
                key = gql_id(st, &base, &cookie,
                    &format!("mutation{{createIntegrationKey(input:{{serviceID:\"{s}\",type:generic,name:\"yw-group\"}}){{id}}}}"),
                    "/data/createIntegrationKey/id").await;
            }
        } else if let Some(ref s) = sid {
            let _ = goalert::graphql(&st.http, &base, &cookie,
                &format!("mutation{{updateService(input:{{id:\"{s}\",escalationPolicyID:\"{new_ep}\"}})}}")).await;
            if let Some(oep) = &old_ep {
                let _ = goalert::graphql(&st.http, &base, &cookie,
                    &format!("mutation{{deleteAll(input:[{{type:escalationPolicy,id:\"{oep}\"}}])}}")).await;
            }
        }
        sqlx::query("UPDATE app.alarm_groups SET goalert_ep_id=$1, goalert_service_id=$2, goalert_key=COALESCE($3,goalert_key) WHERE id=$4")
            .bind(&new_ep).bind(&sid).bind(&key).bind(gid).execute(&st.owner_pool).await.map_err(ise)?;
        // #12 labels sul servizio: ritrovabilita' per tenant/gruppo nella console GoAlert
        if let Some(ref s) = sid {
            if !slug.is_empty() {
                let _ = goalert::graphql(&st.http, &base, &cookie,
                    &format!("mutation{{setLabel(input:{{target:{{type:service,id:\"{s}\"}},key:\"youworkforce/tenant\",value:\"{}\"}})}}", esc_gql(&slug))).await;
            }
            let _ = goalert::graphql(&st.http, &base, &cookie,
                &format!("mutation{{setLabel(input:{{target:{{type:service,id:\"{s}\"}},key:\"youworkforce/group\",value:\"{}\"}})}}", esc_gql(&ga_name(&gname)))).await;
        }
        n_groups += 1;
    }
    // routing etype -> integration key del gruppo responsabile (il drain legge goalert_routing)
    let routes = sqlx::query("SELECT r.etype,g.goalert_key,g.goalert_service_id FROM app.alarm_routing r JOIN app.alarm_groups g ON g.id=r.group_id WHERE r.tenant_id=$1 AND g.goalert_key IS NOT NULL")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut n_routes = 0i64;
    for r in &routes {
        let et: String = r.get("etype");
        let k: String = r.get("goalert_key");
        let s: Option<String> = r.try_get("goalert_service_id").ok().flatten();
        sqlx::query("INSERT INTO app.goalert_routing(tenant_id,etype,integration_key,service_id) VALUES($1,$2,$3,$4)
             ON CONFLICT (tenant_id,etype) DO UPDATE SET integration_key=EXCLUDED.integration_key, service_id=EXCLUDED.service_id")
            .bind(tid).bind(&et).bind(&k).bind(&s).execute(&st.owner_pool).await.map_err(ise)?;
        n_routes += 1;
    }
    // avviso onesto: se nessun trasporto e' attivo, GoAlert non puo' consegnare a nessuno
    let warning = if !sms_on && !email_on {
        Some("Nessun trasporto attivo in GoAlert: gli alert NON raggiungono nessuno. Il superadmin deve configurare Twilio (SMS/voce) o SMTP (email) in Connettori -> Trasporto notifiche.".to_string())
    } else if no_contact > 0 {
        Some(format!("{no_contact} membro/i senza recapito utilizzabile (manca il telefono per SMS, o l'email con SMTP attivo): non verranno avvisati finche' non aggiungi il recapito in Utenti e sedi."))
    } else { None };
    audit(st, tid, "youworkforce", "goalert_sync", json!({"groups":n_groups,"users_created":n_users,"routes":n_routes,"skipped":skipped,"contact_methods":n_cm})).await;
    Ok(json!({"ok":true,"groups_synced":n_groups,"users_created":n_users,"routes":n_routes,"groups_skipped_no_members":skipped,
        "contact_methods_created":n_cm,"members_without_contact":no_contact,"rotations":n_rot,
        "transport":{"sms":sms_on,"email":email_on},
        "repeat_seconds":repeat,"step_delay_minutes":delay_min,"ep_repeat":ep_repeat,"warning":warning}))
}
pub async fn goalert_sync_handler(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo l'admin del tenant")); }
    require_module(&st, s.tenant_id.unwrap(), "loneworker").await?;
    let out = sync_goalert(&st, s.tenant_id.unwrap()).await?;
    Ok(Json(out))
}

// ====== Config server Traccar, gestita DA YouWorkforce (PUT /api/server) ======
// Porte/forwarding = config d'avvio (nostra compose/env). I filtri posizione sono
// già nel nostro strumento traccar/alarm_map. Qui le impostazioni runtime del server.
const TR_CFG_BOOL: &[&str] = &["registration","readonly","deviceReadonly","limitCommands","disableReports","forceSettings"];
const TR_CFG_STR:  &[&str] = &["mapUrl","overlayUrl","coordinateFormat","poiLayer","announcement"];
const TR_CFG_NUM:  &[&str] = &["zoom","latitude","longitude"];
pub async fn traccar_conn(st: &AppState, tid: Uuid) -> Option<(String,String,String)> {
    let r = sqlx::query("SELECT base_url,username,password FROM app.traccar_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let pass: String = r.get("password");
    Some((r.get("base_url"), r.get("username"), st.secrets.decrypt(&pass)))   // P3: decifra a riposo
}
/// Compone base_url Traccar + path tollerando il caso comune in cui l'utente include "/api"
/// nel base URL (es. http://host:8082/api) oppure lo slash finale.
pub fn traccar_url(base: &str, path: &str) -> String {
    let b = base.trim_end_matches('/');
    let b = b.strip_suffix("/api").unwrap_or(b);
    let p = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    format!("{b}{p}")
}
/// Login con diagnostica: distingue irraggiungibile / credenziali / HTTP / cookie assente.
pub async fn traccar_login_diag(http: &reqwest::Client, base: &str, user: &str, pass: &str) -> Result<String, String> {
    let resp = http.post(traccar_url(base, "/api/session"))
        .form(&[("email", user), ("password", pass)]).send().await
        .map_err(|e| format!("server Traccar irraggiungibile: {e}"))?;
    let code = resp.status();
    if !code.is_success() {
        return Err(match code.as_u16() {
            401 | 403 => "credenziali Traccar non valide (controlla utente/password)".into(),
            404 => "endpoint Traccar non trovato (controlla il base URL, senza /api finale)".into(),
            c => format!("Traccar HTTP {c}"),
        });
    }
    let mut c = Vec::new();
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        if let Ok(s)=v.to_str() { if let Some(p)=s.split(';').next() { c.push(p.to_string()); } } }
    if c.is_empty() { Err("Traccar: nessun cookie di sessione (versione/endpoint inatteso)".into()) } else { Ok(c.join("; ")) }
}
pub async fn traccar_login(http: &reqwest::Client, base: &str, user: &str, pass: &str) -> Option<String> {
    traccar_login_diag(http, base, user, pass).await.ok()
}
// ====== Connessione ThingsBoard, gestita DA YouWorkforce (superadmin) ======
pub async fn tb_conn(st: &AppState, tid: Uuid) -> Option<(String,String,String)> {
    let r = sqlx::query("SELECT base_url,username,password FROM app.thingsboard_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let pass: String = r.get("password");
    Some((r.get("base_url"), r.get("username"), st.secrets.decrypt(&pass)))
}
/// Login ThingsBoard (JWT). POST /api/auth/login {username,password} -> {token}.
pub async fn tb_login(http: &reqwest::Client, base: &str, user: &str, pass: &str) -> Option<String> {
    let resp = http.post(format!("{base}/api/auth/login"))
        .json(&json!({"username":user,"password":pass})).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    resp.json::<Value>().await.ok()?.get("token").and_then(|v| v.as_str().map(String::from))
}
#[derive(Deserialize)]
pub struct SetTbConn { pub base_url: String, pub username: String, pub password: Option<String> }
pub async fn super_set_thingsboard(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetTbConn>) -> R {
    require_super(&st, &h).await?;
    let base = b.base_url.trim_end_matches('/').to_string();
    let pw = b.password.unwrap_or_default();
    if pw.is_empty() {
        let n = sqlx::query("UPDATE app.thingsboard_connections SET base_url=$2,username=$3,updated_at=now() WHERE tenant_id=$1")
            .bind(tid).bind(&base).bind(&b.username).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
        if n == 0 { return Err(err(StatusCode::BAD_REQUEST, "password richiesta per la prima configurazione")); }
    } else {
        let enc = st.secrets.encrypt(&pw);
        sqlx::query("INSERT INTO app.thingsboard_connections(tenant_id,base_url,username,password) VALUES($1,$2,$3,$4)
            ON CONFLICT (tenant_id) DO UPDATE SET base_url=EXCLUDED.base_url,username=EXCLUDED.username,password=EXCLUDED.password,updated_at=now()")
            .bind(tid).bind(&base).bind(&b.username).bind(&enc).execute(&st.owner_pool).await.map_err(ise)?;
    }
    audit(&st, tid, "superadmin", "set_thingsboard_conn", json!({"base_url":base})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn super_get_thingsboard(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let r = sqlx::query("SELECT base_url,username,password,poll_enabled,poll_interval_s FROM app.thingsboard_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match r {
        Some(r) => Ok(Json(json!({"configured":true,"base_url":r.get::<String,_>("base_url"),
            "username":r.get::<String,_>("username"),"has_password":!r.get::<String,_>("password").is_empty(),
            "poll_enabled":r.get::<bool,_>("poll_enabled"),"poll_interval_s":r.get::<i32,_>("poll_interval_s")}))),
        None => Ok(Json(json!({"configured":false}))),
    }
}
/// Verifica collegamento ThingsBoard: login + conteggio device (specchio).
pub async fn thingsboard_verify(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let (base,user,pass) = tb_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione ThingsBoard non configurata"))?;
    let token = match tb_login(&st.http, &base, &user, &pass).await {
        Some(t) => t,
        None => return Ok(Json(json!({"reachable":false,"verdict":"login fallito o server irraggiungibile"}))),
    };
    let dev = st.http.get(format!("{base}/api/tenant/devices?pageSize=1&page=0"))
        .header("X-Authorization", format!("Bearer {token}")).send().await;
    match dev {
        Ok(r) if r.status().is_success() => {
            let count = r.json::<Value>().await.ok().and_then(|v| v.get("totalElements").and_then(|x| x.as_i64())).unwrap_or(0);
            audit(&st, tid, "superadmin", "thingsboard_verify", json!({"devices":count})).await;
            Ok(Json(json!({"reachable":true,"verdict":"collegato","device_count":count})))
        }
        Ok(r) => Ok(Json(json!({"reachable":false,"verdict":format!("ThingsBoard HTTP {}", r.status())}))),
        Err(e) => Ok(Json(json!({"reachable":false,"verdict":format!("irraggiungibile: {e}")}))),
    }
}

// ============ ThingsBoard PULL: specchio device + telemetria -> regole/incidenti ============
// Completa il connettore TB (prima solo push Rule Engine): YouWorkforce fa login JWT, pulla
// i device nel proprio inventario e l'ultima telemetria, applicando eval_sensor_rules per
// alzare incidenti critici sullo stesso percorso del push (persist_ingest). On-demand dal
// pannello o in background dal worker (cadenza per-tenant).

/// Dall'ultima telemetria TB (timeseries {chiave:[{ts,value}]}) costruisce un payload piatto
/// {chiave: valore} usando il campione piu' recente; prova numero, poi booleano, poi stringa.
fn tb_latest_payload(ts: &Value) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(obj) = ts.as_object() {
        for (k, v) in obj {
            let raw = v.as_array().and_then(|a| a.get(0)).and_then(|p| p.get("value"));
            if let Some(val) = raw {
                let parsed = match val {
                    Value::String(s) => s.parse::<f64>().map(|n| json!(n))
                        .or_else(|_| s.parse::<bool>().map(|b| json!(b)))
                        .unwrap_or_else(|_| json!(s)),
                    other => other.clone(),
                };
                m.insert(k.clone(), parsed);
            }
        }
    }
    Value::Object(m)
}

/// Policy per severità degli alarm nativi ThingsBoard. Default safety-first.
/// Ritorna handling ∈ ignora|registra|critico per una severità TB.
fn tb_sev_default(sev: &str) -> &'static str {
    match sev {
        "CRITICAL" | "MAJOR" => "critico",
        "MINOR" | "WARNING" => "registra",
        _ => "ignora", // INDETERMINATE
    }
}
async fn tb_alarm_policy(st: &AppState, tid: Uuid) -> serde_json::Map<String,Value> {
    let cur: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='thingsboard' AND tool_code='severity_gate'")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    cur.and_then(|v| v.get("tb_alarm_severity").and_then(|s| s.as_object().cloned())).unwrap_or_default()
}
/// Pull degli alarm nativi TB attivi -> incidenti, applicando la policy per severità.
/// Dedup via idempotenza persist_ingest (idem = tb-alarm-{alarmId}).
async fn tb_pull_alarms(st: &AppState, tid: Uuid, base: &str, hdr: &str) -> i64 {
    let pol = tb_alarm_policy(st, tid).await;
    let url = format!("{base}/api/v2/alarms?pageSize=200&page=0&sortProperty=createdTime&sortOrder=DESC&statusList=ACTIVE");
    let resp = match st.http.get(&url).header("X-Authorization", hdr).send().await { Ok(r) => r, Err(_) => return 0 };
    if !resp.status().is_success() { return 0; }
    let v: Value = match resp.json().await { Ok(v) => v, Err(_) => return 0 };
    let empty = vec![];
    let data = v.get("data").and_then(|d| d.as_array()).unwrap_or(&empty);
    let mut n = 0i64;
    for a in data {
        let sev = a.get("severity").and_then(|x| x.as_str()).unwrap_or("INDETERMINATE");
        let handling = pol.get(sev).and_then(|x| x.as_str()).unwrap_or_else(|| tb_sev_default(sev));
        if handling == "ignora" { continue; }
        let atype = a.get("type").and_then(|x| x.as_str()).unwrap_or("alarm");
        let dev_name = a.get("originatorName").and_then(|x| x.as_str()).unwrap_or("");
        let alarm_id = a.get("id").and_then(|i| i.get("id")).and_then(|x| x.as_str()).unwrap_or("");
        if alarm_id.is_empty() { continue; }
        let etype = if handling == "critico" { "sensore_critico" } else { "allarme_sensore" };
        let idem = format!("tb-alarm-{alarm_id}");
        let payload = json!({"tb_alarm_type":atype,"tb_severity":sev,"device":dev_name,"source":"thingsboard_alarm"});
        let r = persist_ingest(st, tid, Normalized{
            source:"thingsboard".into(), source_event:idem.clone(), idem:idem.clone(),
            etype:etype.into(), device_external_id:if dev_name.is_empty(){None}else{Some(dev_name.to_string())},
            site_name:None, lat:None, lon:None, accuracy_m:None, correlation:idem, payload,
            indoor:false, beacon_uid:None, zone:None }).await;
        if r.is_ok() { n += 1; }
    }
    n
}

/// Esegue un pull completo TB per il tenant: specchio device + telemetria -> regole + alarm TB.
/// Ritorna (device specchiati, incidenti alzati). Non critico: errori -> Err stringa.
pub async fn tb_pull_once(st: &AppState, tid: Uuid) -> Result<(i64, i64), String> {
    let (base, user, pass) = tb_conn(st, tid).await.ok_or("connessione ThingsBoard non configurata")?;
    let token = tb_login(&st.http, &base, &user, &pass).await.ok_or("login ThingsBoard fallito")?;
    let hdr = format!("Bearer {token}");
    let dresp = st.http.get(format!("{base}/api/tenant/devices?pageSize=1000&page=0"))
        .header("X-Authorization", &hdr).send().await.map_err(|e| format!("device irraggiungibili: {e}"))?;
    if !dresp.status().is_success() { return Err(format!("ThingsBoard /devices HTTP {}", dresp.status())); }
    let dv: Value = dresp.json().await.map_err(|e| format!("device json non valido: {e}"))?;
    let empty = vec![];
    let data = dv.get("data").and_then(|d| d.as_array()).unwrap_or(&empty);
    let mut dev_count = 0i64; let mut alarms = 0i64;
    for d in data {
        let name = d.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() { continue; }
        let tbid = d.get("id").and_then(|i| i.get("id")).and_then(|v| v.as_str()).unwrap_or("");
        let dtype = d.get("type").and_then(|v| v.as_str()).unwrap_or("");
        dev_count += 1;
        // specchio nell'inventario (source=thingsboard); id/tipo TB in config.
        let _ = sqlx::query(
            "INSERT INTO app.devices(tenant_id,external_id,source,label,config,updated_at)
             VALUES($1,$2,'thingsboard',$3,$4,now())
             ON CONFLICT (tenant_id,source,external_id) DO UPDATE SET
               label=COALESCE(app.devices.label, EXCLUDED.label),
               config=EXCLUDED.config, updated_at=now()")
            .bind(tid).bind(name).bind(name).bind(json!({"tb_id":tbid,"type":dtype}))
            .execute(&st.owner_pool).await;
        // ultima telemetria -> regole sensori -> incidente critico (stesso percorso del push)
        if tbid.is_empty() { continue; }
        let tresp = st.http.get(format!("{base}/api/plugins/telemetry/DEVICE/{tbid}/values/timeseries"))
            .header("X-Authorization", &hdr).send().await;
        if let Ok(r) = tresp { if r.status().is_success() {
            if let Ok(ts) = r.json::<Value>().await {
                let payload = tb_latest_payload(&ts);
                if payload.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                    if let Some(etype) = eval_sensor_rules(st, tid, &payload).await {
                        if CRITICAL.contains(&etype.as_str()) {
                            let corr = format!("tbpull-{}-{}", name, Uuid::new_v4());
                            let _ = persist_ingest(st, tid, Normalized{
                                source:"thingsboard".into(), source_event:corr.clone(), idem:corr.clone(),
                                etype, device_external_id:Some(name.to_string()), site_name:None,
                                lat:None, lon:None, accuracy_m:None, correlation:corr, payload,
                                indoor:false, beacon_uid:None, zone:None }).await;
                            alarms += 1;
                        }
                    }
                }
            }
        }}
    }
    // #1 alarm nativi TB -> incidenti (policy per severità)
    alarms += tb_pull_alarms(st, tid, &base, &hdr).await;
    Ok((dev_count, alarms))
}

async fn tb_mark_poll(st: &AppState, tid: Uuid, reachable: bool, err: Option<&str>, dev: Option<i64>, alarms: Option<i64>, now: chrono::DateTime<chrono::Utc>) {
    let _ = sqlx::query(
        "INSERT INTO app.thingsboard_poll_state(tenant_id,last_poll_at,last_ok_at,reachable,last_error,device_count,alarm_count,updated_at)
         VALUES($1,$2,CASE WHEN $3 THEN $2 ELSE NULL END,$3,$4,$5,$6,now())
         ON CONFLICT (tenant_id) DO UPDATE SET last_poll_at=$2,
           last_ok_at=CASE WHEN $3 THEN $2 ELSE app.thingsboard_poll_state.last_ok_at END,
           reachable=$3, last_error=$4,
           device_count=COALESCE($5, app.thingsboard_poll_state.device_count),
           alarm_count=COALESCE($6, app.thingsboard_poll_state.alarm_count), updated_at=now()")
        .bind(tid).bind(now).bind(reachable).bind(err).bind(dev).bind(alarms).execute(&st.owner_pool).await;
}
/// On-demand: forza un pull e aggiorna lo stato salute. Usato da pannello e agente AI.
pub async fn tb_poll_now(st: &AppState, tid: Uuid) -> Result<(i64,i64), String> {
    let now = st.clock.now();
    match tb_pull_once(st, tid).await {
        Ok((d,a)) => { tb_mark_poll(st, tid, true, None, Some(d), Some(a), now).await; Ok((d,a)) }
        Err(e) => { tb_mark_poll(st, tid, false, Some(&e), None, None, now).await; Err(e) }
    }
}

#[derive(Deserialize)]
pub struct TbSyncReq { pub poll_enabled: Option<bool>, pub poll_interval_s: Option<i32> }
/// Superadmin: imposta poll on/off + cadenza, e forza un sync immediato (reachable-aware).
pub async fn thingsboard_sync(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, body: Option<Json<TbSyncReq>>) -> R {
    require_super(&st, &h).await?;
    let b = body.map(|Json(b)| b).unwrap_or(TbSyncReq{ poll_enabled:None, poll_interval_s:None });
    if let Some(v) = b.poll_enabled {
        sqlx::query("UPDATE app.thingsboard_connections SET poll_enabled=$2 WHERE tenant_id=$1").bind(tid).bind(v).execute(&st.owner_pool).await.map_err(ise)?;
    }
    if let Some(v) = b.poll_interval_s {
        sqlx::query("UPDATE app.thingsboard_connections SET poll_interval_s=$2 WHERE tenant_id=$1").bind(tid).bind(v.clamp(15,3600)).execute(&st.owner_pool).await.map_err(ise)?;
    }
    match tb_poll_now(&st, tid).await {
        Ok((d,a)) => { audit(&st, tid, "superadmin", "thingsboard_sync", json!({"devices":d,"alarms":a})).await;
            Ok(Json(json!({"reachable":true,"device_count":d,"alarm_count":a}))) }
        Err(e) => Ok(Json(json!({"reachable":false,"verdict":e}))),
    }
}
/// Stato salute poller TB (operatore/admin/auditor).
pub async fn thingsboard_status(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let cfg = sqlx::query("SELECT poll_enabled,poll_interval_s FROM app.thingsboard_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let (poll_enabled, poll_interval_s) = cfg.as_ref().map(|r| (r.get::<bool,_>("poll_enabled"), r.get::<i32,_>("poll_interval_s"))).unwrap_or((false,60));
    let st_row = sqlx::query("SELECT last_poll_at,last_ok_at,reachable,last_error,device_count,alarm_count FROM app.thingsboard_poll_state WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({
        "configured": cfg.is_some(), "poll_enabled": poll_enabled, "poll_interval_s": poll_interval_s,
        "reachable": st_row.as_ref().and_then(|r| r.try_get::<bool,_>("reachable").ok()),
        "last_poll_at": st_row.as_ref().and_then(|r| r.try_get::<Dt,_>("last_poll_at").ok()).map(|t| t.to_rfc3339()),
        "last_ok_at": st_row.as_ref().and_then(|r| r.try_get::<Dt,_>("last_ok_at").ok()).map(|t| t.to_rfc3339()),
        "last_error": st_row.as_ref().and_then(|r| r.try_get::<String,_>("last_error").ok()),
        "device_count": st_row.as_ref().and_then(|r| r.try_get::<i32,_>("device_count").ok()),
        "alarm_count": st_row.as_ref().and_then(|r| r.try_get::<i32,_>("alarm_count").ok()),
    })))
}
/// Telemetria live di un device TB (per nome), via il suo tb_id specchiato. Per l'agente AI.
pub async fn tb_device_telemetry(st: &AppState, tid: Uuid, name: &str) -> Result<Value, String> {
    let (base, user, pass) = tb_conn(st, tid).await.ok_or("connessione ThingsBoard non configurata")?;
    let tbid: Option<String> = sqlx::query_scalar("SELECT config->>'tb_id' FROM app.devices WHERE tenant_id=$1 AND source='thingsboard' AND external_id=$2")
        .bind(tid).bind(name).fetch_optional(&st.owner_pool).await.ok().flatten();
    let tbid = tbid.filter(|s| !s.is_empty()).ok_or("device TB sconosciuto (sincronizza prima i device)")?;
    let token = tb_login(&st.http, &base, &user, &pass).await.ok_or("login ThingsBoard fallito")?;
    let r = st.http.get(format!("{base}/api/plugins/telemetry/DEVICE/{tbid}/values/timeseries"))
        .header("X-Authorization", format!("Bearer {token}")).send().await.map_err(|e| format!("irraggiungibile: {e}"))?;
    if !r.status().is_success() { return Err(format!("ThingsBoard HTTP {}", r.status())); }
    let ts: Value = r.json().await.map_err(|e| format!("json non valido: {e}"))?;
    Ok(tb_latest_payload(&ts))
}

// ================= Ambienti / casi d'uso (ispirati a thingsboard.io/use-cases) =================
// Scenari attivabili dal superadmin su QUALSIASI tenant. Applicare uno scenario provisiona,
// riusando le primitive esistenti: abilita i moduli giusti, inserisce un bundle di regole
// sensori (metrica/operatore/soglia -> etype) e, se richiesto, crea device + sito demo.
// Reversibile (rimozione pulita di regole e demo). Catalogo parametrico nel codice.

struct ScnRule { metric: &'static str, op: &'static str, threshold: Option<f64>, etype: &'static str }
struct ScnDev { suffix: &'static str, label: &'static str }
struct Scenario { code: &'static str, name: &'static str, sector: &'static str, desc: &'static str,
    modules: Vec<&'static str>, rules: Vec<ScnRule>, devices: Vec<ScnDev> }

fn scenario_catalog() -> Vec<Scenario> {
    // operatori numerici con soglia, oppure is_true/is_false (threshold None).
    let r = |metric, op, threshold, etype| ScnRule{ metric, op, threshold, etype };
    let d = |suffix, label| ScnDev{ suffix, label };
    let crit = "sensore_critico"; let tele = "telemetria";
    vec![
      Scenario{ code:"smart_energy", name:"Smart Energy", sector:"Energia", desc:"Consumo elettrico per piano con uno smart meter per piano: tensione, corrente, frequenza, energia; allarmi su sovra/sotto-tensione e calo di frequenza.",
        modules:vec!["iot","thingsboard"],
        rules:vec![ r("voltage",">=",Some(253.0),crit), r("voltage","<=",Some(207.0),crit), r("frequency","<=",Some(49.5),crit), r("current",">=",Some(32.0),tele), r("energy_total",">=",Some(1000.0),tele) ],
        devices:vec![ d("meter-a","Smart Meter A (Piano 1)"), d("meter-b","Smart Meter B (Piano 2)"), d("meter-c","Smart Meter C (Piano 3)") ] },
      Scenario{ code:"environment_monitoring", name:"Environment Monitoring", sector:"Ambiente", desc:"Monitoraggio ambientale indoor/outdoor con sensori multipli.",
        modules:vec!["iot"],
        rules:vec![ r("temperature",">=",Some(35.0),crit), r("humidity",">=",Some(85.0),tele), r("co2",">=",Some(1500.0),crit) ],
        devices:vec![ d("env-1","Sensore ambientale 1") ] },
      Scenario{ code:"smart_office", name:"Smart Office", sector:"Edifici", desc:"Clima indoor ufficio, salute/sicurezza dipendenti, consumo risorse.",
        modules:vec!["iot"],
        rules:vec![ r("co2",">=",Some(1200.0),crit), r("temperature",">=",Some(30.0),tele), r("smoke","is_true",None,crit) ],
        devices:vec![ d("office-1","Sensore ufficio 1") ] },
      Scenario{ code:"water_metering", name:"Water Metering", sector:"Utility", desc:"Consumo acqua dai contatori con rilevazione perdite/frodi.",
        modules:vec!["iot","thingsboard"],
        rules:vec![ r("leak","is_true",None,crit), r("flow",">=",Some(100.0),tele), r("fraud","is_true",None,crit) ],
        devices:vec![ d("water-1","Contatore acqua 1") ] },
      Scenario{ code:"smart_retail", name:"Smart Retail (catena freddo)", sector:"Retail", desc:"Frigo/freezer e asset di supermercato: qualita' alimentare e disponibilita'.",
        modules:vec!["iot"],
        rules:vec![ r("temperature",">=",Some(8.0),crit), r("temperature","<=",Some(-25.0),crit), r("door_open","is_true",None,tele) ],
        devices:vec![ d("chiller-1","Frigo 1"), d("freezer-1","Freezer 1") ] },
      Scenario{ code:"smart_farming", name:"Smart Farming", sector:"Agricoltura", desc:"Stoccaggio cereali in silos: livello, temperatura, umidità grano, aerazione. Allarmi su surriscaldamento, grano umido (muffa) e scorta bassa.",
        modules:vec!["iot","thingsboard"],
        rules:vec![ r("temperature",">=",Some(30.0),crit), r("moisture",">=",Some(25.0),crit), r("humidity",">=",Some(80.0),crit), r("crop_level","<=",Some(15.0),tele) ],
        devices:vec![ d("silo-a","Silo A"), d("silo-b","Silo B"), d("silo-c","Silo C") ] },
      Scenario{ code:"site_fleet_tracking", name:"Site Fleet Tracking", sector:"Mezzi", desc:"Escavatori, camion e mezzi in cantiere/miniera: posizione e carburante.",
        modules:vec!["traccar","loneworker","iot"],
        rules:vec![ r("fuel","<=",Some(10.0),crit), r("engine_temp",">=",Some(110.0),crit) ],
        devices:vec![ d("vehicle-1","Mezzo 1") ] },
      Scenario{ code:"smart_metering", name:"Smart Metering", sector:"Utility", desc:"Dati smart meter per consumo risorse: perdite, anomalie, frodi.",
        modules:vec!["iot","thingsboard"],
        rules:vec![ r("leak","is_true",None,crit), r("fraud","is_true",None,crit), r("consumption",">=",Some(1000.0),tele) ],
        devices:vec![ d("smartmeter-1","Smart meter 1") ] },
      Scenario{ code:"health_care", name:"Health Care (assisted living)", sector:"Sanita'", desc:"Metriche salute paziente da device connessi, allerta su variazioni critiche.",
        modules:vec!["loneworker","iot","goalert"],
        rules:vec![ r("heart_rate",">=",Some(120.0),crit), r("heart_rate","<=",Some(40.0),crit), r("fall","is_true",None,"man_down") ],
        devices:vec![ d("wearable-1","Wearable paziente 1") ] },
      Scenario{ code:"air_quality", name:"Air Quality Monitoring", sector:"Ambiente", desc:"Qualita' dell'aria su piu' sedi: PM2.5, CO2, umidita', temperatura.",
        modules:vec!["iot"],
        rules:vec![ r("pm25",">=",Some(35.0),crit), r("co2",">=",Some(1000.0),crit), r("humidity",">=",Some(90.0),tele), r("temperature",">=",Some(40.0),tele) ],
        devices:vec![ d("air-1","Centralina aria 1") ] },
      Scenario{ code:"smart_irrigation", name:"Smart Irrigation", sector:"Agricoltura", desc:"Impianto di irrigazione e campi: umidita' suolo, posizione, statistiche.",
        modules:vec!["iot"],
        rules:vec![ r("soil_moisture","<=",Some(15.0),crit), r("flow","<=",Some(1.0),tele) ],
        devices:vec![ d("irrig-1","Nodo irrigazione 1") ] },
      Scenario{ code:"waste_management", name:"Waste Management", sector:"Utility", desc:"Cassonetti e raccolta: livello di riempimento e pattern di raccolta.",
        modules:vec!["iot"],
        rules:vec![ r("fill_level",">=",Some(90.0),crit), r("fill_level",">=",Some(70.0),tele) ],
        devices:vec![ d("bin-1","Cassonetto 1") ] },
      Scenario{ code:"tank_level", name:"Tank Level Monitoring", sector:"Industria", desc:"Serbatoi carburante/acqua/chimici: livelli e trend di consumo.",
        modules:vec!["iot","thingsboard"],
        rules:vec![ r("level","<=",Some(10.0),crit), r("level",">=",Some(95.0),crit) ],
        devices:vec![ d("tank-1","Serbatoio 1") ] },
      Scenario{ code:"scada_pool", name:"SCADA Swimming Pool", sector:"SCADA", desc:"Sistemi piscina e processi industriali in tempo reale.",
        modules:vec!["iot","thingsboard"],
        rules:vec![ r("ph","<=",Some(6.8),crit), r("ph",">=",Some(7.8),crit), r("chlorine","<=",Some(0.5),crit), r("temperature",">=",Some(32.0),tele) ],
        devices:vec![ d("pool-1","Centralina piscina 1") ] },
      Scenario{ code:"scada_oil_gas", name:"SCADA Oil & Gas Drilling", sector:"SCADA", desc:"Perforazione: parametri chiave (rig, pompe, BOP) con focus sicurezza.",
        modules:vec!["iot","thingsboard","goalert"],
        rules:vec![ r("pressure",">=",Some(350.0),crit), r("gas",">=",Some(20.0),crit), r("vibration",">=",Some(80.0),crit) ],
        devices:vec![ d("rig-1","Rig 1") ] },
      Scenario{ code:"scada_energy", name:"SCADA Energy Management", sector:"SCADA", desc:"Flussi energetici e infrastruttura: efficienza, picchi di domanda, costi.",
        modules:vec!["iot","thingsboard"],
        rules:vec![ r("demand_kw",">=",Some(500.0),crit), r("power_factor","<=",Some(0.8),tele) ],
        devices:vec![ d("grid-1","Nodo rete 1") ] },
    ]
}

pub async fn super_scenarios_list(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let out: Vec<Value> = scenario_catalog().iter().map(|s| json!({
        "code":s.code, "name":s.name, "sector":s.sector, "desc":s.desc,
        "modules":s.modules, "rule_count":s.rules.len(), "device_count":s.devices.len() })).collect();
    Ok(Json(json!(out)))
}
pub async fn super_tenant_scenarios(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query("SELECT scenario_code,with_demo,applied_by,applied_at FROM app.tenant_scenarios WHERE tenant_id=$1 ORDER BY applied_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cat = scenario_catalog();
    let out: Vec<Value> = rows.iter().map(|r| { let code: String = r.get("scenario_code");
        let name = cat.iter().find(|s| s.code==code).map(|s| s.name).unwrap_or("?");
        json!({"code":code,"name":name,"with_demo":r.get::<bool,_>("with_demo"),
            "applied_by":r.get::<String,_>("applied_by"),"applied_at":r.get::<Dt,_>("applied_at").to_rfc3339()}) }).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct ApplyScenario { pub code: String, pub with_demo: Option<bool> }
/// Applica uno scenario a un tenant: abilita moduli + inserisce regole + (opz.) demo.
pub async fn super_apply_scenario(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<ApplyScenario>) -> R {
    require_super(&st, &h).await?;
    let (code, rules_added, devices_added, modules) = scenario_apply_inner(&st, tid, &b.code, b.with_demo.unwrap_or(false)).await?;
    Ok(Json(json!({"ok":true,"code":code,"modules":modules,"rules_added":rules_added,"devices_added":devices_added})))
}
/// Logica pura di applicazione di uno scenario (riusata da apply singolo e bulk).
/// Ritorna (code, rules_added, devices_added, modules).
async fn scenario_apply_inner(st: &AppState, tid: Uuid, code: &str, with_demo: bool) -> Result<(String, i64, i64, Vec<&'static str>), (StatusCode, String)> {
    let cat = scenario_catalog();
    let scn = cat.iter().find(|s| s.code==code).ok_or_else(|| err(StatusCode::BAD_REQUEST, "scenario sconosciuto"))?;
    // 1) moduli
    for m in &scn.modules {
        sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES($1,$2,'attivo')
            ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo', updated_at=now()")
            .bind(tid).bind(m).execute(&st.owner_pool).await.map_err(ise)?;
    }
    // 2) regole (idempotenti per label scn:<code>:<metric>:<op>)
    let mut rules_added = 0i64;
    for ru in &scn.rules {
        let label = format!("scn:{}:{}:{}", scn.code, ru.metric, ru.op);
        let n = sqlx::query("INSERT INTO app.sensor_rules(tenant_id,metric,op,threshold,etype,label,sort)
            SELECT $1,$2,$3,$4,$5,$6,100 WHERE NOT EXISTS(SELECT 1 FROM app.sensor_rules WHERE tenant_id=$1 AND label=$6)")
            .bind(tid).bind(ru.metric).bind(ru.op).bind(ru.threshold).bind(ru.etype).bind(&label)
            .execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
        rules_added += n as i64;
    }
    // 3) demo: sito + device
    let mut devices_added = 0i64;
    if with_demo {
        let site_name = format!("{} (demo)", scn.name);
        let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.sites WHERE tenant_id=$1 AND name=$2")
            .bind(tid).bind(&site_name).fetch_optional(&st.owner_pool).await.map_err(ise)?;
        let site_id: Uuid = match existing {
            Some(id) => id,
            None => sqlx::query_scalar("INSERT INTO app.sites(tenant_id,name) VALUES($1,$2) RETURNING id")
                .bind(tid).bind(&site_name).fetch_one(&st.owner_pool).await.map_err(ise)?,
        };
        // coordinate demo sparse attorno a una base (Milano) per popolare la mappa del cruscotto
        for (i, dev) in scn.devices.iter().enumerate() {
            let ext = format!("scn-{}-{}", scn.code, dev.suffix);
            let lat = 45.464 + ((i as f64)*0.9 + (scn.code.len() as f64)*0.13).sin() * 0.03;
            let lon = 9.190 + ((i as f64)*1.3 + (scn.code.len() as f64)*0.21).cos() * 0.03;
            let n = sqlx::query("INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,enabled,config,last_lat,last_lon,updated_at)
                VALUES($1,$2,'scenario',$3,$4,true,$5,$6,$7,now())
                ON CONFLICT (tenant_id,source,external_id) DO NOTHING")
                .bind(tid).bind(&ext).bind(dev.label).bind(site_id).bind(json!({"scenario":scn.code})).bind(lat).bind(lon)
                .execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
            devices_added += n as i64;
        }
    }
    // 4) stato
    sqlx::query("INSERT INTO app.tenant_scenarios(tenant_id,scenario_code,with_demo,applied_by) VALUES($1,$2,$3,'superadmin')
        ON CONFLICT (tenant_id,scenario_code) DO UPDATE SET with_demo=EXCLUDED.with_demo, applied_by='superadmin', applied_at=now()")
        .bind(tid).bind(&scn.code).bind(with_demo).execute(&st.owner_pool).await.map_err(ise)?;
    audit(st, tid, "superadmin", "scenario_apply", json!({"code":scn.code,"with_demo":with_demo,"rules_added":rules_added,"devices_added":devices_added})).await;
    Ok((scn.code.to_string(), rules_added, devices_added, scn.modules.clone()))
}
/// Rimuove uno scenario: regole del bundle + device demo (se non referenziati). I moduli
/// restano abilitati (potrebbero servire ad altri scenari/uso del tenant).
pub async fn super_remove_scenario(State(st): State<AppState>, h: HeaderMap, Path((tid,code)): Path<(Uuid,String)>) -> R {
    require_super(&st, &h).await?;
    let (rules, devs) = scenario_remove_inner(&st, tid, &code).await?;
    Ok(Json(json!({"ok":true,"rules_removed":rules,"devices_removed":devs,"note":"moduli lasciati abilitati"})))
}
/// Logica pura di rimozione di uno scenario (riusata da remove singolo e bulk). Rimozione COMPLETA
/// dei dati demo generati dallo scenario: incidenti -> eventi -> device (in quest'ordine per i FK),
/// più regole e riga di stato. I moduli restano abilitati. Ritorna (regole, device).
async fn scenario_remove_inner(st: &AppState, tid: Uuid, code: &str) -> Result<(u64, u64), (StatusCode, String)> {
    let dev_like = format!("scn-{}-%", code);
    let corr_like = format!("scndemo-{}-%", code);
    // 1) incidenti demo: da eventi dei device demo OPPURE con correlation della demo (timeline in cascade).
    let _inc = sqlx::query("DELETE FROM app.incidents WHERE tenant_id=$1 AND (
            correlation LIKE $2
            OR event_id IN (SELECT e.id FROM app.events e JOIN app.devices d ON d.id=e.device_id
                            WHERE d.tenant_id=$1 AND d.source='scenario' AND d.external_id LIKE $3))")
        .bind(tid).bind(&corr_like).bind(&dev_like).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    // 2) eventi demo: dei device demo OPPURE con correlation della demo.
    let _ev = sqlx::query("DELETE FROM app.events WHERE tenant_id=$1 AND (
            correlation LIKE $2
            OR device_id IN (SELECT id FROM app.devices WHERE tenant_id=$1 AND source='scenario' AND external_id LIKE $3))")
        .bind(tid).bind(&corr_like).bind(&dev_like).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    // 3) device demo (ora senza eventi che li referenziano).
    let devs = sqlx::query("DELETE FROM app.devices WHERE tenant_id=$1 AND source='scenario' AND external_id LIKE $2")
        .bind(tid).bind(&dev_like).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    // 4) sito demo, se rimasto vuoto.
    let _ = sqlx::query("DELETE FROM app.sites s WHERE s.tenant_id=$1 AND s.name LIKE '% (demo)'
            AND NOT EXISTS (SELECT 1 FROM app.devices d WHERE d.site_id=s.id)
            AND NOT EXISTS (SELECT 1 FROM app.incidents i WHERE i.site_id=s.id)")
        .bind(tid).execute(&st.owner_pool).await;
    // 5) regole del bundle + riga di stato.
    let rules = sqlx::query("DELETE FROM app.sensor_rules WHERE tenant_id=$1 AND label LIKE $2")
        .bind(tid).bind(format!("scn:{}:%", code)).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    sqlx::query("DELETE FROM app.tenant_scenarios WHERE tenant_id=$1 AND scenario_code=$2")
        .bind(tid).bind(code).execute(&st.owner_pool).await.map_err(ise)?;
    audit(st, tid, "superadmin", "scenario_remove", json!({"code":code,"rules_removed":rules,"devices_removed":devs,"incidents_removed":_inc,"events_removed":_ev})).await;
    Ok((rules, devs))
}

/// Bulk: attiva TUTTI gli ambienti del catalogo, o li rimuove tutti. Chiude il buco "non si
/// riesce ad attivarli/disattivarli tutti". `action`: apply|remove; `with_demo` solo per apply.
#[derive(Deserialize)] pub struct BulkScenarios { pub action: String, pub with_demo: Option<bool> }
pub async fn super_bulk_scenarios(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<BulkScenarios>) -> R {
    require_super(&st, &h).await?;
    match b.action.as_str() {
        "apply" => {
            let with_demo = b.with_demo.unwrap_or(false);
            let (mut n, mut rules, mut devs) = (0i64, 0i64, 0i64);
            for s in scenario_catalog() {
                let (_c, r, d, _m) = scenario_apply_inner(&st, tid, s.code, with_demo).await?;
                n += 1; rules += r; devs += d;
            }
            Ok(Json(json!({"ok":true,"action":"apply","applied":n,"rules_added":rules,"devices_added":devs})))
        }
        "remove" => {
            // rimuove solo quelli effettivamente applicati al tenant
            let codes: Vec<String> = sqlx::query_scalar("SELECT scenario_code FROM app.tenant_scenarios WHERE tenant_id=$1")
                .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
            let (mut n, mut rules, mut devs) = (0i64, 0u64, 0u64);
            for code in &codes {
                let (r, d) = scenario_remove_inner(&st, tid, code).await?;
                n += 1; rules += r; devs += d;
            }
            Ok(Json(json!({"ok":true,"action":"remove","removed":n,"rules_removed":rules,"devices_removed":devs,"note":"moduli lasciati abilitati"})))
        }
        _ => Err(err(StatusCode::BAD_REQUEST, "action deve essere 'apply' o 'remove'")),
    }
}

// Valori demo derivati da una regola: uno che la fa scattare, uno "normale" che non scatta.
fn demo_alarm_value(op: &str, thr: Option<f64>) -> Value {
    match op {
        ">=" | ">" => json!(thr.map(|t| t + (t.abs()*0.1).max(1.0)).unwrap_or(1.0)),
        "<=" | "<" => json!(thr.map(|t| t - (t.abs()*0.1).max(1.0)).unwrap_or(-1.0)),
        "==" => thr.map(|t| json!(t)).unwrap_or(json!(0.0)),
        "is_true" => json!(true),
        "is_false" => json!(false),
        _ => json!(thr.unwrap_or(0.0)),
    }
}
fn demo_normal_value(op: &str, thr: Option<f64>) -> Value {
    match op {
        ">=" | ">" => json!(thr.map(|t| if t > 0.0 { t*0.5 } else { t - 5.0 }).unwrap_or(0.0)),
        "<=" | "<" => json!(thr.map(|t| t + (t.abs()*0.5).max(5.0)).unwrap_or(1.0)),
        "==" => thr.map(|t| json!(t + 1.0)).unwrap_or(json!(1.0)),
        "is_true" => json!(false),
        "is_false" => json!(true),
        _ => json!(0.0),
    }
}
/// Genera telemetria demo per uno scenario: 1 payload tutto-normale (telemetria) + un payload
/// d'allarme per ciascuna regola critica (max 3), iniettati per i device demo sullo STESSO
/// percorso del push (eval_sensor_rules -> persist_ingest). Così lo scenario mostra subito
/// incidenti reali nella centrale.
pub async fn super_scenario_demo(State(st): State<AppState>, h: HeaderMap, Path((tid,code)): Path<(Uuid,String)>) -> R {
    require_super(&st, &h).await?;
    let cat = scenario_catalog();
    let scn = cat.iter().find(|s| s.code==code).ok_or_else(|| err(StatusCode::BAD_REQUEST, "scenario sconosciuto"))?;
    // nomi device demo (se lo scenario non ha device demo, usa un nome sintetico)
    let devs: Vec<String> = if scn.devices.is_empty() { vec![format!("scn-{}-demo", scn.code)] }
        else { scn.devices.iter().map(|d| format!("scn-{}-{}", scn.code, d.suffix)).collect() };
    // Telemetria demo: spec-driven (realistica, multi-metrica) se esiste la spec generata,
    // altrimenti derivata dalle regole (comportamento storico).
    let spec = scenario_spec(scn.code);
    let mut payloads: Vec<Value> = Vec::new();
    if let Some(ms) = spec.as_ref().and_then(|s| s.get("metrics")).and_then(|m| m.as_array()) {
        use rand::Rng as _;
        let mut rng = rand::thread_rng();
        let normal_val = |m: &Value, rng: &mut rand::rngs::ThreadRng| -> Value {
            if m.get("widget").and_then(|v| v.as_str()) == Some("status") { return json!(false); }
            if let Some(a) = m.get("normal").and_then(|v| v.as_array()) {
                let lo = a.get(0).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let hi = a.get(1).and_then(|v| v.as_f64()).unwrap_or(lo);
                let v = if hi > lo { lo + rng.gen::<f64>()*(hi-lo) } else { lo };
                return json!((v*100.0).round()/100.0);
            }
            let mn = m.get("min").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let mx = m.get("max").and_then(|v| v.as_f64()).unwrap_or(1.0);
            json!((((mn+mx)/2.0)*100.0).round()/100.0)
        };
        // 12 campioni normali (variati) -> serie/grafici realistici
        for _ in 0..12 {
            let mut p = serde_json::Map::new();
            for m in ms { if let Some(k)=m.get("key").and_then(|v| v.as_str()) { p.insert(k.into(), normal_val(m, &mut rng)); } }
            payloads.push(Value::Object(p));
        }
        // un allarme per metrica con regola critica (max 3)
        let mut na = 0;
        for m in ms {
            if na >= 3 { break; }
            let k = m.get("key").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(rule) = scn.rules.iter().find(|r| r.metric==k && CRITICAL.contains(&r.etype)) {
                let mut p = serde_json::Map::new();
                for mm in ms { if let Some(kk)=mm.get("key").and_then(|v| v.as_str()) { p.insert(kk.into(), normal_val(mm, &mut rng)); } }
                let av = m.get("alarm").and_then(|v| v.as_f64()).map(|x| json!(x)).unwrap_or_else(|| demo_alarm_value(rule.op, rule.threshold));
                p.insert(k.into(), av);
                payloads.push(Value::Object(p));
                na += 1;
            }
        }
    } else {
        let mut normal = serde_json::Map::new();
        for ru in &scn.rules { normal.insert(ru.metric.to_string(), demo_normal_value(ru.op, ru.threshold)); }
        payloads.push(Value::Object(normal.clone()));
        for ru in scn.rules.iter().filter(|r| CRITICAL.contains(&r.etype)).take(3) {
            let mut p = normal.clone();
            p.insert(ru.metric.to_string(), demo_alarm_value(ru.op, ru.threshold));
            payloads.push(Value::Object(p));
        }
    }
    let mut events = 0i64; let mut incidents = 0i64;
    for (i, payload) in payloads.iter().enumerate() {
        let device = devs[i % devs.len()].clone();
        let etype = eval_sensor_rules(&st, tid, payload).await.unwrap_or_else(|| "telemetria".into());
        let is_crit = CRITICAL.contains(&etype.as_str());
        let corr = format!("scndemo-{}-{}", scn.code, Uuid::new_v4());
        let _ = persist_ingest(&st, tid, Normalized{
            source:"scenario".into(), source_event:corr.clone(), idem:corr.clone(),
            etype, device_external_id:Some(device), site_name:None,
            lat:None, lon:None, accuracy_m:None, correlation:corr, payload:payload.clone(),
            indoor:false, beacon_uid:None, zone:None }).await;
        events += 1; if is_crit { incidents += 1; }
    }
    audit(&st, tid, "superadmin", "scenario_demo", json!({"code":scn.code,"events":events,"incidents":incidents})).await;
    Ok(Json(json!({"ok":true,"events":events,"incidents":incidents})))
}

// ---- Motore dashboard parametrico per scenario (widget derivati dalle regole) ----
fn metric_unit(m: &str) -> &'static str {
    match m {
        "temperature"|"engine_temp"|"temp_out" => "°C", "humidity"|"soil_moisture"|"moisture"|"crop_level"|"fill_level"|"power_factor" => "%",
        "co2" => "ppm", "pm25" => "µg/m³", "power_kw"|"demand_kw" => "kW", "energy_kwh"|"consumption" => "kWh",
        "voltage" => "V", "pressure" => "bar", "flow" => "L/min", "level" => "%", "fuel" => "%",
        "current" => "A", "frequency" => "Hz", "energy_total" => "kWh",
        "heart_rate" => "bpm", "gas" => "%LEL", "chlorine" => "mg/L", "ph" => "pH", "vibration" => "mm/s",
        _ => "",
    }
}
fn metric_label(m: &str) -> String {
    let base = match m {
        "temperature" => "Temperatura", "humidity" => "Umidità", "co2" => "CO₂", "pm25" => "PM2.5",
        "power_kw" => "Potenza", "demand_kw" => "Domanda", "energy_kwh" => "Energia", "voltage" => "Tensione",
        "soil_moisture" => "Umidità suolo", "moisture" => "Umidità grano", "crop_level" => "Livello cereale",
        "aeration" => "Aerazione", "temp_out" => "Temp. esterna",
        "fill_level" => "Riempimento", "level" => "Livello", "fuel" => "Carburante",
        "heart_rate" => "Battito", "gas" => "Gas", "chlorine" => "Cloro", "ph" => "pH", "pressure" => "Pressione",
        "flow" => "Flusso", "vibration" => "Vibrazione", "power_factor" => "Fattore di potenza", "consumption" => "Consumo",
        "current" => "Corrente", "frequency" => "Frequenza", "energy_total" => "Energia",
        "engine_temp" => "Temp. motore", "leak" => "Perdita", "fraud" => "Frode", "smoke" => "Fumo",
        "door_open" => "Porta aperta", "fall" => "Caduta",
        _ => m,
    };
    base.to_string()
}
/// Deriva i widget di uno scenario dalle sue regole: una card per metrica (gauge numerico
/// o stato booleano), con soglia/direzione per colorare l'allarme.
fn scenario_widgets(scn: &Scenario) -> Vec<Value> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for ru in &scn.rules {
        if seen.contains(&ru.metric) { continue; }
        seen.push(ru.metric);
        let rules_m: Vec<&ScnRule> = scn.rules.iter().filter(|r| r.metric==ru.metric).collect();
        let is_bool = rules_m.iter().any(|r| matches!(r.op, "is_true"|"is_false"));
        let crit = rules_m.iter().find(|r| CRITICAL.contains(&r.etype)).and_then(|r| r.threshold);
        let any_thr: Vec<f64> = rules_m.iter().filter_map(|r| r.threshold).collect();
        let op = rules_m.iter().find(|r| CRITICAL.contains(&r.etype)).map(|r| r.op).unwrap_or(rules_m[0].op);
        let (min, max) = if is_bool { (0.0, 1.0) } else {
            let maxt = any_thr.iter().cloned().fold(f64::MIN, f64::max);
            let mint = any_thr.iter().cloned().fold(f64::MAX, f64::min);
            let lo = if mint < 0.0 { mint - mint.abs()*0.3 } else { 0.0 };
            let hi = if maxt > 0.0 { maxt * 1.4 } else { maxt * 0.6 + 10.0 };
            (lo, hi)
        };
        out.push(json!({
            "metric": ru.metric, "label": metric_label(ru.metric), "unit": metric_unit(ru.metric),
            "kind": if is_bool { "status" } else { "gauge" }, "op": op, "threshold": crit,
            "min": min, "max": max,
            "critical": rules_m.iter().any(|r| CRITICAL.contains(&r.etype)),
        }));
    }
    out
}

/// Spec di scenario GENERATA (dal progetto multiagentico da descrizione+immagini del sito):
/// JSON opzionale in SCENARIO_SPECS_DIR/<code>.json che arricchisce widget/narrativa/demo.
/// Additiva: se assente, si usa la derivazione dalle regole. Mai copia di asset del sito.
fn scenario_spec(code: &str) -> Option<Value> {
    let dir = std::env::var("SCENARIO_SPECS_DIR").unwrap_or_else(|_| "scenario_specs".into());
    let path = std::path::Path::new(&dir).join(format!("{code}.json"));
    let txt = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&txt).ok()
}
/// Widget dal spec (se presente) altrimenti derivati dalle regole. Il spec definisce
/// metrica/etichetta/unità/tipo/range; la soglia/direzione d'allarme viene dalle regole.
fn scenario_widgets_spec(scn: &Scenario, spec: Option<&Value>) -> Vec<Value> {
    if let Some(ms) = spec.and_then(|s| s.get("metrics")).and_then(|m| m.as_array()) {
        return ms.iter().map(|m| {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let rules_m: Vec<&ScnRule> = scn.rules.iter().filter(|r| r.metric==key).collect();
            let critr = rules_m.iter().find(|r| CRITICAL.contains(&r.etype));
            let op = critr.map(|r| r.op).or_else(|| rules_m.first().map(|r| r.op)).unwrap_or("");
            json!({
                "metric": key,
                "label": m.get("label").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| metric_label(key)),
                "unit": m.get("unit").and_then(|v| v.as_str()).unwrap_or(metric_unit(key)),
                "kind": m.get("widget").and_then(|v| v.as_str()).unwrap_or("gauge"),
                "op": op, "threshold": critr.and_then(|r| r.threshold),
                "min": m.get("min").and_then(|v| v.as_f64()).unwrap_or(0.0),
                "max": m.get("max").and_then(|v| v.as_f64()).unwrap_or(100.0),
                "critical": critr.is_some(),
            })
        }).collect();
    }
    scenario_widgets(scn)
}

/// Scenari applicati al tenant corrente (per il cruscotto in-app; qualsiasi ruolo).
pub async fn tenant_scenarios_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let rows = sqlx::query("SELECT scenario_code FROM app.tenant_scenarios WHERE tenant_id=$1 ORDER BY applied_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cat = scenario_catalog();
    let out: Vec<Value> = rows.iter().filter_map(|r| { let code: String = r.get("scenario_code");
        cat.iter().find(|s| s.code==code).map(|s| json!({"code":s.code,"name":s.name,"sector":s.sector})) }).collect();
    Ok(Json(json!(out)))
}

/// Angolo di analisi AI specifico per scenario (preset).
fn scenario_ai_focus(code: &str) -> &'static str {
    match code {
        "smart_energy" => "Valuta picchi di consumo, sovratensioni e opportunità di risparmio energetico.",
        "environment_monitoring" => "Valuta comfort e salubrità ambientale (temperatura/umidità/CO₂) e soglie superate.",
        "smart_office" => "Valuta clima ufficio, qualità dell'aria e sicurezza (fumo), con azioni per il facility.",
        "water_metering" => "Evidenzia perdite, frodi e consumi anomali; stima l'impatto e le verifiche prioritarie.",
        "smart_retail" => "Valuta il rischio catena del freddo: escursioni di temperatura e integrità del prodotto.",
        "smart_farming" => "Valuta stress colturale (umidità suolo, gelo) e interventi agronomici consigliati.",
        "site_fleet_tracking" => "Valuta carburante, surriscaldamento motori e rischi operativi dei mezzi in cantiere.",
        "smart_metering" => "Evidenzia perdite/frodi e pattern di consumo anomali sui contatori.",
        "health_care" => "Valuta rischio clinico dei pazienti (battito, cadute) e priorità di allerta, SENZA sostituire il personale sanitario.",
        "air_quality" => "Valuta la qualità dell'aria (PM2.5/CO₂) rispetto alle soglie di salute e le azioni di mitigazione.",
        "smart_irrigation" => "Valuta fabbisogno idrico e stato impianto; suggerisci la programmazione irrigua.",
        "waste_management" => "Valuta i cassonetti prossimi alla saturazione e ottimizza i giri di raccolta.",
        "tank_level" => "Valuta rischi di esaurimento e troppopieno dei serbatoi e i rifornimenti prioritari.",
        "scada_pool" => "Valuta la qualità dell'acqua (pH/cloro) e la conformità dei parametri della piscina.",
        "scada_oil_gas" => "Valuta i rischi di sicurezza della perforazione (pressione/gas/vibrazioni) con priorità operative.",
        "scada_energy" => "Valuta picchi di domanda, fattore di potenza e costi; suggerisci ottimizzazioni.",
        _ => "Valuta anomalie, priorità di intervento e azioni consigliate.",
    }
}

/// Guardia: scenario esistente e attivo per il tenant. Ritorna il riferimento allo scenario.
async fn scenario_applied<'a>(st: &AppState, tid: Uuid, code: &str, cat: &'a [Scenario]) -> Result<&'a Scenario, (StatusCode,String)> {
    let scn = cat.iter().find(|x| x.code==code).ok_or_else(|| err(StatusCode::NOT_FOUND, "scenario sconosciuto"))?;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM app.tenant_scenarios WHERE tenant_id=$1 AND scenario_code=$2")
        .bind(tid).bind(code).fetch_one(&st.owner_pool).await.map_err(ise)?;
    if applied == 0 { return Err(err(StatusCode::FORBIDDEN, "scenario non attivo per questo tenant")); }
    Ok(scn)
}

/// Dati completi del cruscotto: widget (valore/serie), KPI aggregati, device (coord mappa),
/// multi-metrica, incidenti recenti. Riusato dal cruscotto e dall'analisi AI.
async fn scenario_dash_json(st: &AppState, tid: Uuid, scn: &Scenario) -> Result<Value, (StatusCode,String)> {
    let spec = scenario_spec(scn.code);
    let widgets = scenario_widgets_spec(scn, spec.as_ref());
    let narrative = spec.as_ref().and_then(|s| s.get("narrative")).and_then(|v| v.as_str()).map(|s| s.to_string());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let devs = sqlx::query("SELECT id,external_id,label,last_lat,last_lon,config FROM app.devices WHERE source='scenario' AND external_id LIKE $1 ORDER BY external_id")
        .bind(format!("scn-{}-%", scn.code)).fetch_all(&mut *tx).await.map_err(ise)?;
    let ids: Vec<Uuid> = devs.iter().map(|r| r.get::<Uuid,_>("id")).collect();
    let evs = if ids.is_empty() { Vec::new() } else {
        sqlx::query("SELECT device_id,payload_ref,origin_ts FROM app.events WHERE device_id = ANY($1) AND payload_ref IS NOT NULL ORDER BY origin_ts DESC LIMIT 400")
            .bind(&ids).fetch_all(&mut *tx).await.map_err(ise)? };
    let incs = if ids.is_empty() { Vec::new() } else {
        sqlx::query("SELECT i.etype,i.state,i.opened_at FROM app.incidents i JOIN app.events e ON e.id=i.event_id
            WHERE e.device_id = ANY($1) ORDER BY i.opened_at DESC LIMIT 10").bind(&ids).fetch_all(&mut *tx).await.map_err(ise)? };
    let open_inc: i64 = if ids.is_empty() { 0 } else {
        sqlx::query_scalar("SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id
            WHERE e.device_id = ANY($1) AND i.state<>'chiuso'").bind(&ids).fetch_one(&mut *tx).await.map_err(ise)? };
    let ev24: i64 = if ids.is_empty() { 0 } else {
        sqlx::query_scalar("SELECT count(*) FROM app.events WHERE device_id = ANY($1) AND origin_ts > now()-interval '24 hours'")
            .bind(&ids).fetch_one(&mut *tx).await.map_err(ise)? };
    tx.commit().await.ok();
    let payloads: Vec<(Value, Dt)> = evs.iter().map(|r| (r.try_get::<Value,_>("payload_ref").unwrap_or(json!({})), r.get::<Dt,_>("origin_ts"))).collect();
    // storico per-device (per il raggruppamento a silos)
    let mut by_dev: std::collections::HashMap<Uuid, Vec<(Value, Dt)>> = std::collections::HashMap::new();
    for r in &evs { if let Ok(did) = r.try_get::<Uuid,_>("device_id") {
        by_dev.entry(did).or_default().push((r.try_get::<Value,_>("payload_ref").unwrap_or(json!({})), r.get::<Dt,_>("origin_ts"))); } }
    let mut alarms_now = 0i64;
    let widgets_out: Vec<Value> = widgets.into_iter().map(|mut w| {
        let metric = w["metric"].as_str().unwrap_or("").to_string();
        let op = w["op"].as_str().unwrap_or("").to_string();
        let thr = w["threshold"].as_f64();
        let (mut value, mut series): (Option<Value>, Vec<Value>) = (None, Vec::new());
        for (p, ts) in &payloads { if let Some(v) = p.get(&metric) {
            if value.is_none() { value = Some(v.clone()); }
            series.push(json!({"t": ts.to_rfc3339(), "v": v})); } }
        series.reverse();
        if series.len() > 30 { series = series.split_off(series.len()-30); }
        // stato d'allarme corrente (per KPI)
        if let (Some(v), Some(t)) = (value.as_ref().and_then(|x| x.as_f64()), thr) {
            let trip = match op.as_str() { ">="=>v>=t, ">"=>v>t, "<="=>v<=t, "<"=>v<t, _=>false };
            if trip { alarms_now += 1; }
        }
        if let Value::Object(ref mut m) = w { m.insert("value".into(), value.unwrap_or(Value::Null)); m.insert("series".into(), json!(series)); }
        w
    }).collect();
    let incidents: Vec<Value> = incs.iter().map(|r| json!({"etype":r.get::<String,_>("etype"),
        "state":r.get::<String,_>("state"),"at":r.get::<Dt,_>("opened_at").to_rfc3339()})).collect();
    let devices: Vec<Value> = devs.iter().map(|r| json!({"external_id":r.get::<String,_>("external_id"),
        "label":r.try_get::<String,_>("label").ok(),"lat":r.try_get::<f64,_>("last_lat").ok(),"lon":r.try_get::<f64,_>("last_lon").ok()})).collect();
    // --- Raggruppamento a "silos" (device): una scheda per device con tutte le metriche spec ---
    let grouped = spec.as_ref().and_then(|s| s.get("grouped")).and_then(|v| v.as_str()) == Some("device");
    let spec_metrics: Vec<(String, Value)> = spec.as_ref().and_then(|s| s.get("metrics")).and_then(|m| m.as_array())
        .map(|a| a.iter().filter_map(|m| m.get("key").and_then(|k| k.as_str()).map(|k| (k.to_string(), m.clone()))).collect()).unwrap_or_default();
    let primary = spec.as_ref().and_then(|s| s.get("primary_metric")).and_then(|v| v.as_str()).unwrap_or("temperature").to_string();
    let group_label = spec.as_ref().and_then(|s| s.get("group_label")).and_then(|v| v.as_str()).unwrap_or("Gruppo").to_string();
    let silos: Vec<Value> = if !grouped { vec![] } else { devs.iter().map(|dr| {
        let did: Uuid = dr.get("id");
        let cfg: Value = dr.try_get::<Value,_>("config").unwrap_or(json!({}));
        let hist = by_dev.get(&did);
        let mut metrics = serde_json::Map::new();
        let mut silo_alarm = false;
        for (k, mspec) in &spec_metrics {
            let mut val: Option<Value> = None;
            if let Some(h) = hist { for (p, _ts) in h { if let Some(v) = p.get(k) { val = Some(v.clone()); break; } } }
            if k == "aeration" && val.is_none() { if let Some(a) = cfg.get("aeration") { val = Some(a.clone()); } }
            let rule = scn.rules.iter().find(|rr| rr.metric==k.as_str() && CRITICAL.contains(&rr.etype));
            let tripped = if let (Some(v), Some(ru)) = (val.as_ref().and_then(|x| x.as_f64()), rule) {
                match ru.op { ">="=>ru.threshold.map(|t|v>=t).unwrap_or(false), ">"=>ru.threshold.map(|t|v>t).unwrap_or(false),
                    "<="=>ru.threshold.map(|t|v<=t).unwrap_or(false), "<"=>ru.threshold.map(|t|v<t).unwrap_or(false), _=>false }
            } else { false };
            if tripped { silo_alarm = true; }
            metrics.insert(k.clone(), json!({
                "label": mspec.get("label").cloned().unwrap_or_else(|| json!(metric_label(k))),
                "unit": mspec.get("unit").cloned().unwrap_or_else(|| json!(metric_unit(k))),
                "widget": mspec.get("widget").cloned().unwrap_or_else(|| json!("gauge")),
                "min": mspec.get("min").cloned().unwrap_or_else(|| json!(0)),
                "max": mspec.get("max").cloned().unwrap_or_else(|| json!(100)),
                "alarm": mspec.get("alarm").cloned(),
                "value": val.unwrap_or(Value::Null), "tripped": tripped,
            }));
        }
        let mut series: Vec<Value> = vec![];
        if let Some(h) = hist { for (p, ts) in h.iter().rev() { if let Some(v) = p.get(&primary) { series.push(json!({"t":ts.to_rfc3339(),"v":v})); } } }
        if series.len() > 30 { series = series.split_off(series.len()-30); }
        let aer = metrics.get("aeration").and_then(|m| m.get("value")).and_then(|v| v.as_bool());
        json!({ "device_id": did, "external_id": dr.get::<String,_>("external_id"), "label": dr.try_get::<String,_>("label").ok(),
            "lat": dr.try_get::<f64,_>("last_lat").ok(), "lon": dr.try_get::<f64,_>("last_lon").ok(),
            "metrics": metrics, "alarm": silo_alarm, "aeration": aer, "primary_series": series })
    }).collect() };
    Ok(json!({
        "code":scn.code, "name":scn.name, "sector":scn.sector, "ai_focus":scenario_ai_focus(scn.code),
        "narrative": narrative, "generated": spec.is_some(),
        "grouped": grouped, "group_label": group_label, "primary_metric": primary, "silos": silos,
        "device_count": devs.len(), "widgets": widgets_out, "incidents": incidents, "devices": devices,
        "kpis": {"device_count":devs.len(),"open_incidents":open_inc,"events_24h":ev24,"metrics_tracked":widgets_out.len(),"alarms_now":alarms_now}
    }))
}

/// Dati del cruscotto di uno scenario (tenant, scenario attivo).
pub async fn scenario_dashboard(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let cat = scenario_catalog();
    let scn = scenario_applied(&st, tid, &code, &cat).await?;
    Ok(Json(scenario_dash_json(&st, tid, scn).await?))
}

#[derive(Deserialize)] pub struct AerationReq { pub device_id: Uuid, pub on: bool }
/// Aziona l'aerazione di un silo (device dello scenario). Aggiorna lo stato nativo (devices.config),
/// inietta un campione di telemetria (così il cruscotto riflette il cambio) e, se il device è
/// mappato su ThingsBoard, inoltra il comando via RPC. operatore/tenant_admin.
pub async fn scenario_aeration_set(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>, Json(b): Json<AerationReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let cat = scenario_catalog();
    let _scn = scenario_applied(&st, tid, &code, &cat).await?;
    // device deve appartenere a questo scenario
    let dev = sqlx::query("SELECT external_id, config FROM app.devices WHERE id=$1 AND tenant_id=$2 AND source='scenario' AND external_id LIKE $3")
        .bind(b.device_id).bind(tid).bind(format!("scn-{}-%", code)).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "device non appartiene allo scenario"))?;
    let ext: String = dev.get("external_id");
    let mut cfg: Value = dev.try_get::<Value,_>("config").unwrap_or(json!({}));
    if let Value::Object(ref mut m) = cfg { m.insert("aeration".into(), json!(b.on)); }
    sqlx::query("UPDATE app.devices SET config=$2, updated_at=now() WHERE id=$1").bind(b.device_id).bind(&cfg).execute(&st.owner_pool).await.map_err(ise)?;
    // telemetria: il cruscotto mostra subito il nuovo stato (serie/widget aerazione)
    let corr = format!("aeration-{}-{}", code, Uuid::new_v4());
    let _ = persist_ingest(&st, tid, Normalized{
        source:"scenario".into(), source_event:corr.clone(), idem:corr.clone(), etype:"telemetria".into(),
        device_external_id:Some(ext), site_name:None, lat:None, lon:None, accuracy_m:None,
        correlation:corr, payload:json!({"aeration": b.on}), indoor:false, beacon_uid:None, zone:None }).await;
    // NB: i silos demo non sono mappati su ThingsBoard. Per un device reale mappato, il comando RPC
    // va inoltrato via l'endpoint dedicato (tb_device_rpc). Qui l'azione è nativa (stato+telemetria).
    audit(&st, tid, &s.email, "scenario_aeration", json!({"code":code,"device":b.device_id,"on":b.on})).await;
    Ok(Json(json!({"ok":true,"on":b.on})))
}

#[derive(Deserialize)]
pub struct ScnAnalysisReq { pub save: Option<bool> }
/// Analisi AI dello scenario: grounding sui dati reali del cruscotto + preset specifico.
pub async fn scenario_ai_analysis(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>, Json(b): Json<ScnAnalysisReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let cat = scenario_catalog();
    let scn = scenario_applied(&st, tid, &code, &cat).await?;
    if !module_active(&st, tid, "assistant").await { return Err(err(StatusCode::FORBIDDEN, "modulo assistant non abilitato")); }
    let c = ai_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "provider AI non configurato (superadmin)"))?;
    let facts = scenario_dash_json(&st, tid, scn).await?;
    let sys = format!("Sei un analista IoT di YouWorkforce per lo scenario «{}» (settore {}). {} \
        Usa SOLO i fatti forniti (widget con valori/serie, KPI, incidenti). Non inventare numeri. \
        Rispondi in italiano, breve e strutturato (stato, anomalie, azioni consigliate). Non agire \
        sulla catena di sicurezza: resta manuale.", scn.name, scn.sector, scenario_ai_focus(scn.code));
    let user = format!("Dati cruscotto: {}", serde_json::to_string(&facts).unwrap_or_default());
    let analysis = ai_chat(&st, &c, &sys, &user).await.map_err(|e| err(StatusCode::BAD_GATEWAY, &e))?;
    let mut report_id: Option<Uuid> = None;
    if b.save.unwrap_or(false) {
        let title = format!("Analisi {} — {}", scn.name, st.clock.now().format("%Y-%m-%d %H:%M"));
        report_id = Some(ai_report_save(&st, &s, scn.code, &title, &analysis, json!({"scenario":scn.code})).await?);
    }
    audit(&st, tid, &s.email, "scenario_ai_analysis", json!({"code":scn.code,"saved":report_id.is_some()})).await;
    Ok(Json(json!({"provider":c.provider,"analysis":analysis,"ai_focus":scenario_ai_focus(scn.code),"report_id":report_id})))
}

// ======================= youworker.ai — piattaforma AI di YouCo =======================
// Integrazione AI DI TUTTA la suite, configurata DA YouWorkforce (mai da sistemi a monte).
// - Connessione per-tenant (base URL + API key cifrata enc:v1) impostata dal superadmin.
// - Abilitazione per-processo pilotata dall'admin del tenant (app.ai_features).
// - Provider intercambiabile: contratto OpenAI-compatibile parametrico (path/model).
// Invarianti di sicurezza: tenant/identita' SEMPRE dal contesto autenticato (mai dal
// modello); l'API key non e' mai esposta ne' inviata come dato tenant; i processi AI sono
// in SOLA LETTURA / proposta (nessuna azione critica eseguita dal modello); se la
// connessione manca o fallisce, si degrada al piano deterministico locale senza mai
// togliere l'inserimento manuale, il check-in o l'SOS.

pub const AI_FEATURES: &[&str] = &["assistant","agent","incident_summary","sensor_suggest","checkin_anomaly","interventi_assist","consuntivi_summary","voice_diary"];

#[derive(Clone)]
pub struct AiConn { pub provider: String, pub base_url: String, pub api_key: String, pub model: String, pub chat_path: String, pub stt_path: String }
impl AiConn {
    /// L'adattatore da usare: anthropic nativo, altrimenti OpenAI-compatibile (youworker/openai/deepseek/custom).
    pub fn is_anthropic(&self) -> bool { self.provider.eq_ignore_ascii_case("anthropic") }
}
pub const AI_PROVIDERS: &[&str] = &["youworker","openai","deepseek","anthropic","custom"];

/// Connessione AI del tenant (API key decifrata a riposo). None se non configurata.
pub async fn ai_conn(st: &AppState, tid: Uuid) -> Option<AiConn> {
    let r = sqlx::query("SELECT provider,base_url,api_key,model,chat_path,stt_path FROM app.ai_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let key: String = r.get("api_key");
    Some(AiConn{ provider: r.get("provider"), base_url: r.get("base_url"), api_key: st.secrets.decrypt(&key),
        model: r.get("model"), chat_path: r.get("chat_path"), stt_path: r.get("stt_path") })
}

/// Un processo AI e' attivo per il tenant SOLO se: modulo assistant attivo + connessione
/// configurata + feature abilitata dall'admin. Qualsiasi mancanza => degrada (false).
pub async fn ai_feature_on(st: &AppState, tid: Uuid, feature: &str) -> bool {
    if !module_active(st, tid, "assistant").await { return false; }
    if ai_conn(st, tid).await.is_none() { return false; }
    // Auto: con un provider AI configurato le funzioni sono ATTIVE di default; l'admin può
    // comunque disattivarne esplicitamente qualcuna (riga con enabled=false).
    sqlx::query_scalar::<_, bool>("SELECT enabled FROM app.ai_features WHERE tenant_id=$1 AND feature=$2")
        .bind(tid).bind(feature).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or(true)
}

/// Compone base_url + path per l'endpoint AI, tollerando il caso comune in cui l'utente
/// mette "/v1" sia nel base URL sia nel path (server OpenAI-compatibili tipo llama.cpp/vLLM):
/// collassa i segmenti "/v1" duplicati così entrambe le forme funzionano.
pub fn ai_url(base: &str, path: &str) -> String {
    let b = base.trim_end_matches('/');
    let p = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    let mut url = format!("{b}{p}");
    while let Some(i) = url.find("/v1/v1/") { url.replace_range(i..i+7, "/v1/"); }
    if url.ends_with("/v1/v1") { let n = url.len(); url.replace_range(n-6..n, "/v1"); }
    url
}

/// Chiamata chat semplice (solo testo), provider-aware. system = istruzioni/guardrail,
/// user = richiesta+contesto (gia' ristretto al tenant). Ritorna il testo o Err diagnostico.
pub async fn ai_chat(st: &AppState, c: &AiConn, system: &str, user: &str) -> Result<String, String> {
    let url = ai_url(&c.base_url, &c.chat_path);
    if c.is_anthropic() {
        let body = json!({"model":c.model,"max_tokens":800,"system":system,
            "messages":[{"role":"user","content":user}]});
        let resp = st.http.post(&url).header("x-api-key", &c.api_key).header("anthropic-version","2023-06-01")
            .json(&body).send().await.map_err(|e| format!("provider AI irraggiungibile: {e}"))?;
        if !resp.status().is_success() { return Err(format!("provider AI HTTP {}", resp.status())); }
        let v: Value = resp.json().await.map_err(|e| format!("risposta non valida: {e}"))?;
        return anthropic_text(&v).ok_or_else(|| "provider AI: nessun contenuto".to_string());
    }
    let body = json!({"model":c.model,"temperature":0.1,"max_tokens":800,
        "messages":[{"role":"system","content":system},{"role":"user","content":user}]});
    let resp = st.http.post(&url).bearer_auth(&c.api_key).json(&body).send().await
        .map_err(|e| format!("provider AI irraggiungibile: {e}"))?;
    if !resp.status().is_success() { return Err(format!("provider AI HTTP {}", resp.status())); }
    let v: Value = resp.json().await.map_err(|e| format!("risposta non valida: {e}"))?;
    v.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message"))
        .and_then(|m| m.get("content")).and_then(|t| t.as_str()).map(|s| s.trim().to_string())
        .ok_or_else(|| "provider AI: nessun contenuto nella risposta".to_string())
}
fn anthropic_text(v: &Value) -> Option<String> {
    let arr = v.get("content")?.as_array()?;
    let mut out = String::new();
    for b in arr { if b.get("type").and_then(|t| t.as_str())==Some("text") {
        if let Some(t)=b.get("text").and_then(|t| t.as_str()) { out.push_str(t); } } }
    let out = out.trim().to_string();
    if out.is_empty() { None } else { Some(out) }
}

/// Speech-to-text youworker.ai (diario vocale lato server). Audio multipart -> testo.
pub async fn ai_stt(st: &AppState, c: &AiConn, audio: Vec<u8>, filename: &str, mime: &str) -> Result<String, String> {
    let url = ai_url(&c.base_url, &c.stt_path);
    let part = reqwest::multipart::Part::bytes(audio).file_name(filename.to_string())
        .mime_str(mime).map_err(|e| format!("audio non valido: {e}"))?;
    let form = reqwest::multipart::Form::new().text("model", c.model.clone()).part("file", part);
    let resp = st.http.post(&url).bearer_auth(&c.api_key).multipart(form).send().await
        .map_err(|e| format!("youworker.ai irraggiungibile: {e}"))?;
    if !resp.status().is_success() { return Err(format!("youworker.ai STT HTTP {}", resp.status())); }
    let v: Value = resp.json().await.map_err(|e| format!("risposta non valida: {e}"))?;
    v.get("text").and_then(|t| t.as_str()).map(|s| s.trim().to_string())
        .ok_or_else(|| "youworker.ai STT: nessun testo".to_string())
}

// ---- Connessione youworker.ai: set/get mascherato/verifica (superadmin) ----
#[derive(Deserialize)]
pub struct SetAiConn { pub provider: Option<String>, pub base_url: String, pub api_key: Option<String>, pub model: Option<String>,
    pub chat_path: Option<String>, pub stt_path: Option<String> }

/// Default di path/model sensati per provider (l'admin puo' sovrascrivere).
fn ai_provider_defaults(p: &str) -> (&'static str, &'static str, &'static str) {
    // (model, chat_path, stt_path)
    match p {
        "openai"    => ("gpt-4o-mini", "/v1/chat/completions", "/v1/audio/transcriptions"),
        "deepseek"  => ("deepseek-chat", "/chat/completions", "/v1/audio/transcriptions"),
        "anthropic" => ("claude-3-5-sonnet-latest", "/v1/messages", "/v1/audio/transcriptions"),
        _            => ("youworker-1", "/v1/chat/completions", "/v1/audio/transcriptions"),
    }
}

pub async fn super_set_ai(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetAiConn>) -> R {
    require_super(&st, &h).await?;
    let provider = b.provider.map(|p| p.trim().to_lowercase()).filter(|p| AI_PROVIDERS.contains(&p.as_str())).unwrap_or_else(|| "youworker".into());
    let (dm, dc, ds) = ai_provider_defaults(&provider);
    let base = b.base_url.trim_end_matches('/').to_string();
    let model = b.model.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| dm.into());
    let chat = b.chat_path.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| dc.into());
    let stt = b.stt_path.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| ds.into());
    let key = b.api_key.unwrap_or_default();
    if key.is_empty() {
        let n = sqlx::query("UPDATE app.ai_connections SET provider=$2,base_url=$3,model=$4,chat_path=$5,stt_path=$6,updated_at=now() WHERE tenant_id=$1")
            .bind(tid).bind(&provider).bind(&base).bind(&model).bind(&chat).bind(&stt).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
        if n == 0 { return Err(err(StatusCode::BAD_REQUEST, "API key richiesta per la prima configurazione")); }
    } else {
        let enc = st.secrets.encrypt(&key);
        sqlx::query("INSERT INTO app.ai_connections(tenant_id,provider,base_url,api_key,model,chat_path,stt_path) VALUES($1,$2,$3,$4,$5,$6,$7)
            ON CONFLICT (tenant_id) DO UPDATE SET provider=EXCLUDED.provider,base_url=EXCLUDED.base_url,api_key=EXCLUDED.api_key,model=EXCLUDED.model,chat_path=EXCLUDED.chat_path,stt_path=EXCLUDED.stt_path,updated_at=now()")
            .bind(tid).bind(&provider).bind(&base).bind(&enc).bind(&model).bind(&chat).bind(&stt).execute(&st.owner_pool).await.map_err(ise)?;
    }
    audit(&st, tid, "superadmin", "set_ai_conn", json!({"provider":provider,"base_url":base,"model":model})).await;
    Ok(Json(json!({"ok":true})))
}

pub async fn super_get_ai(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let r = sqlx::query("SELECT provider,base_url,api_key,model,chat_path,stt_path FROM app.ai_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match r {
        Some(r) => Ok(Json(json!({"configured":true,"provider":r.get::<String,_>("provider"),"base_url":r.get::<String,_>("base_url"),
            "model":r.get::<String,_>("model"),"chat_path":r.get::<String,_>("chat_path"),
            "stt_path":r.get::<String,_>("stt_path"),"has_api_key":!r.get::<String,_>("api_key").is_empty()}))),
        None => Ok(Json(json!({"configured":false,"provider":"youworker","model":"youworker-1","chat_path":"/v1/chat/completions","stt_path":"/v1/audio/transcriptions","providers":AI_PROVIDERS}))),
    }
}

/// Verifica collegamento youworker.ai: ping chat minimale, reachable-aware.
pub async fn ai_verify(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let c = ai_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione youworker.ai non configurata"))?;
    match ai_chat(&st, &c, "Rispondi con una sola parola: ok.", "ping").await {
        Ok(ans) => { audit(&st, tid, "superadmin", "ai_verify", json!({"model":c.model})).await;
            Ok(Json(json!({"reachable":true,"verdict":"collegato","sample":ans.chars().take(40).collect::<String>()}))) }
        Err(e) => Ok(Json(json!({"reachable":false,"verdict":e}))),
    }
}

// ---- Feature AI per-processo, pilotate dall'admin del tenant ----
pub async fn ai_features_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let configured = ai_conn(&st, tid).await.is_some();
    let rows = sqlx::query("SELECT feature,enabled FROM app.ai_features WHERE tenant_id=$1")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    // default: ON quando il provider è configurato (altrimenti OFF); l'override esplicito vince.
    let mut map = serde_json::Map::new();
    for f in AI_FEATURES { map.insert((*f).to_string(), json!(configured)); }
    for r in rows { map.insert(r.get::<String,_>("feature"), json!(r.get::<bool,_>("enabled"))); }
    Ok(Json(json!({"connection_configured":configured,"features":map})))
}

#[derive(Deserialize)]
pub struct SetAiFeature { pub feature: String, pub enabled: bool }
pub async fn ai_features_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SetAiFeature>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo l'admin del tenant")); }
    let tid = s.tenant_id.unwrap();
    if !AI_FEATURES.contains(&b.feature.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "feature AI sconosciuta")); }
    sqlx::query("INSERT INTO app.ai_features(tenant_id,feature,enabled) VALUES($1,$2,$3)
        ON CONFLICT (tenant_id,feature) DO UPDATE SET enabled=EXCLUDED.enabled,updated_at=now()")
        .bind(tid).bind(&b.feature).bind(b.enabled).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "set_ai_feature", json!({"feature":b.feature,"enabled":b.enabled})).await;
    Ok(Json(json!({"ok":true})))
}

pub async fn traccar_get_config(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let (base,user,pass) = traccar_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione Traccar non configurata"))?;
    let cookie = traccar_login_diag(&st.http, &base, &user, &pass).await.map_err(|e| err(StatusCode::BAD_GATEWAY, &e))?;
    let srv: Value = st.http.get(traccar_url(&base, "/api/server")).header("Cookie", &cookie).send().await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar irraggiungibile"))?.json().await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar json"))?;
    let mut out = Vec::new();
    for id in TR_CFG_BOOL { out.push(json!({"id":id,"type":"boolean","value":srv.get(id).and_then(|v|v.as_bool()).unwrap_or(false)})); }
    for id in TR_CFG_STR  { out.push(json!({"id":id,"type":"string","value":srv.get(id).and_then(|v|v.as_str()).unwrap_or("")})); }
    for id in TR_CFG_NUM  { out.push(json!({"id":id,"type":"number","value":srv.get(id).and_then(|v|v.as_f64()).unwrap_or(0.0)})); }
    Ok(Json(json!(out)))
}
pub async fn traccar_set_config(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<GaCfgSet>) -> R {
    require_super(&st, &h).await?;
    let (base,user,pass) = traccar_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione Traccar non configurata"))?;
    let cookie = traccar_login_diag(&st.http, &base, &user, &pass).await.map_err(|e| err(StatusCode::BAD_GATEWAY, &e))?;
    let mut srv: Value = st.http.get(traccar_url(&base, "/api/server")).header("Cookie", &cookie).send().await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar irraggiungibile"))?.json().await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar json"))?;
    let o = srv.as_object_mut().ok_or_else(|| err(StatusCode::BAD_GATEWAY, "server model"))?;
    for e in &b.entries {
        if TR_CFG_BOOL.contains(&e.id.as_str()) { o.insert(e.id.clone(), json!(e.value=="true")); }
        else if TR_CFG_STR.contains(&e.id.as_str()) { o.insert(e.id.clone(), json!(e.value)); }
        else if TR_CFG_NUM.contains(&e.id.as_str()) { o.insert(e.id.clone(), json!(e.value.parse::<f64>().unwrap_or(0.0))); }
    }
    let put = st.http.put(traccar_url(&base, "/api/server")).header("Cookie", &cookie).json(&srv).send().await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar PUT fallito"))?;
    if !put.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, &format!("Traccar PUT {}", put.status()))); }
    audit(&st, tid, "superadmin", "traccar_set_config", json!({"n":b.entries.len()})).await;
    Ok(Json(json!({"ok":true})))
}

/// Stato salute della connessione/poller Traccar del tenant (card Connettori).
/// Lo alimenta il worker::traccar_poll; qui lo leggiamo per operatore/admin/auditor.
pub async fn traccar_status(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor") {
        return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato"));
    }
    let configured = traccar_conn(&st, tid).await.is_some();
    let cfg: Option<(bool,i32,i32,i32)> = sqlx::query_as("SELECT poll_enabled,poll_interval_s,silence_after_s,conn_down_after_s FROM app.traccar_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let (poll_enabled, poll_interval_s, silence_after_s, conn_down_after_s) = cfg.unwrap_or((false,30,600,180));
    let row = sqlx::query("SELECT last_event_id,last_poll_at,last_ok_at,reachable,last_error,device_count,online_count FROM app.traccar_poll_state WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let now = st.clock.now();
    let (reachable, last_ok_at, last_poll_at, last_error, device_count, online_count, last_event_id) = match &row {
        Some(r) => (
            r.get::<bool,_>("reachable"),
            r.try_get::<chrono::DateTime<chrono::Utc>,_>("last_ok_at").ok(),
            r.try_get::<chrono::DateTime<chrono::Utc>,_>("last_poll_at").ok(),
            r.try_get::<String,_>("last_error").ok(),
            r.get::<i32,_>("device_count"),
            r.get::<i32,_>("online_count"),
            r.get::<i64,_>("last_event_id"),
        ),
        None => (false, None, None, None, 0, 0, 0),
    };
    let lag_seconds = last_ok_at.map(|t| (now - t).num_seconds());
    // #6 — invii di sicurezza bloccati: notifiche/chiusure GoAlert che non passano
    // (oltre soglia, ancora in coda a ritentare). Visibile all'admin come degrado.
    let outbox_stuck: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM app.outbox WHERE tenant_id=$1 AND kind IN ('goalert_notify','goalert_close') AND state<>'done' AND attempts>=6")
        .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    let outbox_last_error: Option<String> = sqlx::query_scalar(
        "SELECT last_error FROM app.outbox WHERE tenant_id=$1 AND kind IN ('goalert_notify','goalert_close') AND state<>'done' AND attempts>=6 ORDER BY attempts DESC LIMIT 1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    Ok(Json(json!({
        "configured": configured,
        "outbox_stuck": outbox_stuck,
        "outbox_last_error": outbox_last_error,
        "poll_enabled": poll_enabled,
        "poll_interval_s": poll_interval_s,
        "silence_after_s": silence_after_s,
        "conn_down_after_s": conn_down_after_s,
        "reachable": reachable,
        "last_ok_at": last_ok_at,
        "last_poll_at": last_poll_at,
        "lag_seconds": lag_seconds,
        "last_error": last_error,
        "device_count": device_count,
        "online_count": online_count,
        "last_event_id": last_event_id,
    })))
}

/// Lettura connessione Traccar del tenant per la UI superadmin (password MAI
/// restituita, solo has_password). Include le impostazioni poll/watchdog.
pub async fn super_get_traccar(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let row = sqlx::query("SELECT base_url,username,password,poll_enabled,poll_interval_s,silence_after_s,conn_down_after_s
         FROM app.traccar_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match row {
        None => Ok(Json(json!({"configured":false}))),
        Some(r) => Ok(Json(json!({
            "configured": true,
            "base_url": r.get::<String,_>("base_url"),
            "username": r.get::<String,_>("username"),
            "has_password": !r.get::<String,_>("password").is_empty(),
            "poll_enabled": r.get::<bool,_>("poll_enabled"),
            "poll_interval_s": r.get::<i32,_>("poll_interval_s"),
            "silence_after_s": r.get::<i32,_>("silence_after_s"),
            "conn_down_after_s": r.get::<i32,_>("conn_down_after_s"),
        }))),
    }
}

#[derive(Deserialize)]
pub struct TraccarSettings {
    pub poll_enabled: Option<bool>,
    pub poll_interval_s: Option<i32>,
    pub silence_after_s: Option<i32>,
    pub conn_down_after_s: Option<i32>,
}
/// GAP C — impostazioni operative Traccar configurabili dall'admin del tenant:
/// cadenza poll, soglia silenzio badge (watchdog), soglia down connessione.
/// Sono parametri di SICUREZZA: l'admin decide quando un badge silente o un
/// Traccar irraggiungibile diventano allarme.
pub async fn traccar_settings_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    let row = sqlx::query("SELECT poll_enabled,poll_interval_s,silence_after_s,conn_down_after_s FROM app.traccar_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match row {
        None => Ok(Json(json!({"configured":false}))),
        Some(r) => Ok(Json(json!({
            "configured": true,
            "poll_enabled": r.get::<bool,_>("poll_enabled"),
            "poll_interval_s": r.get::<i32,_>("poll_interval_s"),
            "silence_after_s": r.get::<i32,_>("silence_after_s"),
            "conn_down_after_s": r.get::<i32,_>("conn_down_after_s"),
        }))),
    }
}
pub async fn traccar_settings_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<TraccarSettings>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    if traccar_conn(&st, tid).await.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "configura prima la connessione Traccar (superadmin)"));
    }
    // bound di sicurezza: no poll troppo aggressivo, no soglie fuori scala
    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi);
    if let Some(v) = b.poll_enabled {
        sqlx::query("UPDATE app.traccar_connections SET poll_enabled=$2 WHERE tenant_id=$1").bind(tid).bind(v).execute(&st.owner_pool).await.map_err(ise)?;
    }
    if let Some(v) = b.poll_interval_s {
        sqlx::query("UPDATE app.traccar_connections SET poll_interval_s=$2 WHERE tenant_id=$1").bind(tid).bind(clamp(v,5,600)).execute(&st.owner_pool).await.map_err(ise)?;
    }
    if let Some(v) = b.silence_after_s {
        sqlx::query("UPDATE app.traccar_connections SET silence_after_s=$2 WHERE tenant_id=$1").bind(tid).bind(clamp(v,0,86400)).execute(&st.owner_pool).await.map_err(ise)?;
    }
    if let Some(v) = b.conn_down_after_s {
        sqlx::query("UPDATE app.traccar_connections SET conn_down_after_s=$2 WHERE tenant_id=$1").bind(tid).bind(clamp(v,30,3600)).execute(&st.owner_pool).await.map_err(ise)?;
    }
    audit(&st, tid, &s.email, "traccar_settings", json!({})).await;
    Ok(Json(json!({"ok":true})))
}

/// P2 — "Verifica & collega" dal pannello: forza un poll immediato, verifica la
/// catena end-to-end (Traccar raggiungibile -> device sincronizzati nel nostro DB)
/// e restituisce la config di forwarding (fast-path opzionale) pronta da copiare.
/// Il pull non richiede config lato Traccar oltre alle credenziali: quindi il
/// collegamento e' "nostro" anche senza toccare la UI di Traccar.
pub async fn traccar_verify(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    if traccar_conn(&st, tid).await.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "connessione Traccar non configurata (superadmin)"));
    }
    // garantisce un token di ingest 'traccar' (serve per il forward fast-path)
    let token: String = match sqlx::query_scalar::<_,String>("SELECT token FROM app.ingest_tokens WHERE tenant_id=$1 AND source='traccar' ORDER BY created_at LIMIT 1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)? {
        Some(t) => t,
        None => {
            let t = format!("traccar-{}", &auth::new_token()[..24]);
            sqlx::query("INSERT INTO app.ingest_tokens(token,tenant_id,source) VALUES($1,$2,'traccar')")
                .bind(&t).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
            t
        }
    };
    // poll immediato (aggiorna raggiungibilita' + sync device)
    let poll_err = crate::worker::poll_now(&st, tid).await.err().map(|e| e.to_string());
    // stato fresco
    let row = sqlx::query("SELECT reachable,last_ok_at,device_count,online_count,last_error FROM app.traccar_poll_state WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let reachable = row.as_ref().map(|r| r.get::<bool,_>("reachable")).unwrap_or(false);
    let device_count = row.as_ref().map(|r| r.get::<i32,_>("device_count")).unwrap_or(0);
    let online_count = row.as_ref().map(|r| r.get::<i32,_>("online_count")).unwrap_or(0);
    let last_error = row.as_ref().and_then(|r| r.try_get::<String,_>("last_error").ok());
    // specchio: quanti nostri device traccar hanno una posizione recente nel DB
    let mirror = sqlx::query("SELECT count(*)::int total, count(*) FILTER (WHERE last_pos_at > now() - interval '15 minutes')::int fresh
         FROM app.devices WHERE tenant_id=$1 AND source='traccar'")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    let mirror_total: i32 = mirror.get("total");
    let mirror_fresh: i32 = mirror.get("fresh");
    let ingress = std::env::var("YW_INGRESS_BASE").unwrap_or_else(|_| "http://host.docker.internal:8080".into());
    let ev_url = format!("{ingress}/api/ingress/traccar?token={token}");
    let pos_url = format!("{ingress}/api/ingress/traccar/position?token={token}");
    let verdict = if reachable { "collegato" } else { "irraggiungibile" };
    audit(&st, tid, &s.email, "traccar_verify", json!({"reachable":reachable,"devices":device_count})).await;
    Ok(Json(json!({
        "verdict": verdict,
        "reachable": reachable,
        "device_count": device_count,
        "online_count": online_count,
        "mirror_total": mirror_total,
        "mirror_fresh": mirror_fresh,
        "last_error": last_error,
        "poll_error": poll_err,
        "pull_note": "Il pull via API non richiede config lato Traccar: basta la connessione (base URL + credenziali) impostata dal superadmin.",
        "forward": {
            "ingest_token": token,
            "event_forward_url": ev_url,
            "position_forward_url": pos_url,
            "env": {
                "EVENT_FORWARD_URL": ev_url,
                "FORWARD_URL": pos_url,
                "FORWARD_ENABLE": "true"
            },
            "note": "Fast-path opzionale a bassa latenza: queste env vanno impostate all'avvio del container Traccar (il forward non e' riconfigurabile a runtime via API)."
        }
    })))
}

// ====== Config trasporto notifiche GoAlert, gestita DA YouWorkforce ======
// L'admin configura tutto dal nostro pannello; noi spingiamo la config a GoAlert via GraphQL.
// NB: in GoAlert OSS la config è per-istanza (globale), non per-tenant.
const GA_CFG_IDS: &[&str] = &[
    "Twilio.Enable","Twilio.AccountSID","Twilio.AuthToken","Twilio.FromNumber","Twilio.MessagingServiceSID",
    "SMTP.Enable","SMTP.From","SMTP.Address","SMTP.Username","SMTP.Password","SMTP.DisableTLS","SMTP.SkipVerify",
    "Slack.Enable","Slack.ClientID","Slack.ClientSecret","Slack.AccessToken","Slack.SigningSecret",
    "Mailgun.Enable","Mailgun.APIKey","Mailgun.EmailDomain",
    "Webhook.Enable","Webhook.AllowedURLs","General.PublicURL",
];
const GA_CFG_SECRETS: &[&str] = &["Twilio.AuthToken","SMTP.Password","Slack.ClientSecret","Slack.SigningSecret","Slack.AccessToken","Mailgun.APIKey"];
/// Validazione E.164: '+' seguito da 8-15 cifre, prima cifra 1-9.
fn is_e164(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 9 || b.len() > 16 || b[0] != b'+' { return false; }
    if !(b'1'..=b'9').contains(&b[1]) { return false; }
    b[1..].iter().all(|c| c.is_ascii_digit())
}

pub async fn goalert_get_config(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let Some((base, cookie, _)) = goalert_session(&st, tid).await else {
        return Err(err(StatusCode::BAD_REQUEST, "credenziali GoAlert non configurate per il tenant (prima: Provisiona/Imposta)")); };
    let v = goalert::graphql(&st.http, &base, &cookie, "{config(all:true){id type value}}").await
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "GoAlert non raggiungibile"))?;
    let all = v.pointer("/data/config").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let out: Vec<Value> = all.iter().filter(|c| {
        c.get("id").and_then(|i| i.as_str()).map(|i| GA_CFG_IDS.contains(&i)).unwrap_or(false)
    }).map(|c| {
        let id = c.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let ty = c.get("type").and_then(|x| x.as_str()).unwrap_or("string");
        let val = c.get("value").and_then(|x| x.as_str()).unwrap_or("");
        let secret = GA_CFG_SECRETS.contains(&id);
        json!({"id":id,"type":ty,"secret":secret,
            "value": if secret { Value::Null } else { json!(val) },
            "set": if secret { Some(!val.is_empty()) } else { None }})
    }).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct GaCfgSet { pub entries: Vec<GaCfgEntry> }
#[derive(Deserialize)]
pub struct GaCfgEntry { pub id: String, pub value: String }
pub async fn goalert_set_config(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<GaCfgSet>) -> R {
    require_super(&st, &h).await?;
    let Some((base, cookie, _)) = goalert_session(&st, tid).await else {
        return Err(err(StatusCode::BAD_REQUEST, "credenziali GoAlert non configurate")); };
    // whitelist + segreti vuoti = non sovrascrivere
    let mut items = Vec::new();
    for e in &b.entries {
        if !GA_CFG_IDS.contains(&e.id.as_str()) { continue; }
        if GA_CFG_SECRETS.contains(&e.id.as_str()) && e.value.is_empty() { continue; }
        // validazione lato nostro (prima di inoltrare a GoAlert)
        if e.id == "Twilio.FromNumber" && !e.value.is_empty() && !is_e164(&e.value) {
            return Err(err(StatusCode::BAD_REQUEST, "Twilio.FromNumber: formato E.164 richiesto (es. +14155552671)"));
        }
        if e.id == "Twilio.MessagingServiceSID" && !e.value.is_empty()
            && !(e.value.len()==34 && e.value.starts_with("MG")) {
            return Err(err(StatusCode::BAD_REQUEST, "Twilio.MessagingServiceSID: formato 'MG' + 32 esadecimali"));
        }
        if e.id == "Twilio.AccountSID" && !e.value.is_empty()
            && !(e.value.len()==34 && e.value.starts_with("AC")) {
            return Err(err(StatusCode::BAD_REQUEST, "Twilio.AccountSID: formato 'AC' + 32 esadecimali"));
        }
        let v = e.value.replace('\\', "\\\\").replace('"', "\\\"");
        items.push(format!("{{id:\"{}\",value:\"{}\"}}", e.id, v));
    }
    if items.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nessun campo valido")); }
    let q = format!("mutation{{setConfig(input:[{}])}}", items.join(","));
    let v = goalert::graphql(&st.http, &base, &cookie, &q).await
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "GoAlert non raggiungibile"))?;
    if v.get("errors").is_some() {
        return Err(err(StatusCode::BAD_GATEWAY, &format!("GoAlert: {}", v.get("errors").unwrap())));
    }
    audit(&st, tid, "superadmin", "goalert_set_config", json!({"n":items.len()})).await;
    Ok(Json(json!({"ok":true,"updated":items.len()})))
}

// P3: routing per tipo evento -> key GoAlert specifica.
#[derive(Deserialize)]
pub struct GoalertRoute { pub etype: String, pub integration_key: Option<String> }
pub async fn goalert_set_routing(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GoalertRoute>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    match &b.integration_key {
        Some(k) if !k.is_empty() => { sqlx::query("INSERT INTO app.goalert_routing(tenant_id,etype,integration_key) VALUES($1,$2,$3)
            ON CONFLICT (tenant_id,etype) DO UPDATE SET integration_key=EXCLUDED.integration_key")
            .bind(tid).bind(&b.etype).bind(k).execute(&st.owner_pool).await.map_err(ise)?; }
        _ => { sqlx::query("DELETE FROM app.goalert_routing WHERE tenant_id=$1 AND etype=$2")
            .bind(tid).bind(&b.etype).execute(&st.owner_pool).await.map_err(ise)?; }
    }
    Ok(Json(json!({"ok":true})))
}

// ----------------------------- tenant plane -----------------------------
pub async fn tenant_login(State(st): State<AppState>, h: HeaderMap, Json(b): Json<LoginReq>) -> impl IntoResponse {
    // tenant resolved from Host (or X-Tenant-Slug for local dev)
    let slug = match tenant_slug(&st, &h) {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no tenant host"}))).into_response(),
    };
    let trow = sqlx::query("SELECT id FROM app.tenants WHERE slug=$1").bind(&slug)
        .fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(trow) = trow else { return (StatusCode::NOT_FOUND, Json(json!({"error":"tenant not found"}))).into_response() };
    let tid: Uuid = trow.get("id");
    let key = format!("{}|{}", slug, rl_key(&h, &b.email));
    if let Some(ra) = rl_check(&st, &key).await { return too_many(ra); }
    // user lookup under tenant context
    let mut tx = match db::begin_tenant(&st.rt_pool, tid).await { Ok(t)=>t, Err(_)=>return ise_resp() };
    let urow = sqlx::query("SELECT id,pass_hash,role FROM app.users WHERE email=$1")
        .bind(&b.email).fetch_optional(&mut *tx).await.ok().flatten();
    let _ = tx.commit().await;
    match urow {
        Some(r) if auth::verify_password(&b.password, &r.get::<String,_>("pass_hash")) => {
            rl_clear(&st, &key).await;
            let tok = auth::new_token();
            let uid: Uuid = r.get("id"); let role: String = r.get("role");
            if auth::needs_rehash(&r.get::<String,_>("pass_hash")) {
                if let Ok(mut tx2) = db::begin_tenant(&st.rt_pool, tid).await {
                    let _ = sqlx::query("UPDATE app.users SET pass_hash=$1 WHERE id=$2")
                        .bind(auth::hash_password(&b.password)).bind(uid).execute(&mut *tx2).await;
                    let _ = tx2.commit().await;
                }
            }
            let csrf = auth::new_token();
            let sess = Session { user_id: uid, tenant_id: Some(tid), email: b.email.clone(), role: role.clone(), is_super: false, impersonator: None, caps: effective_caps_user(&st, tid, &role, uid).await };
            persist_session(&st, &tok, &sess).await;
            (StatusCode::OK, set_cookie_header(&tok, &csrf), Json(json!({"ok":true,"role":role,"tenant":slug,"csrf":csrf}))).into_response()
        }
        _ => {
            rl_fail(&st, &key).await;
            (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response()
        }
    }
}

pub async fn me(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = session_of(&st, &h).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no session"))?;
    Ok(Json(json!({"email":s.email,"role":s.role,"is_super":s.is_super,"tenant_id":s.tenant_id,"impersonator":s.impersonator,"caps":s.caps})))
}

pub async fn logout(State(st): State<AppState>, h: HeaderMap) -> impl IntoResponse {
    if let Some(tok) = get_cookie(&h, "yw_session") {
        let _ = sqlx::query("DELETE FROM app.sessions WHERE token=$1").bind(&tok).execute(&st.owner_pool).await;
    }
    let mut m = HeaderMap::new();
    m.append(header::SET_COOKIE, "yw_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".parse().unwrap());
    m.append(header::SET_COOKIE, "yw_csrf=; Path=/; SameSite=Lax; Max-Age=0".parse().unwrap());
    (StatusCode::OK, m, Json(json!({"ok":true})))
}

pub async fn list_modules(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT m.code,m.name,m.kind,m.available,tm.state
         FROM app.modules_catalog m
         JOIN app.tenant_modules tm ON tm.module_code=m.code
         ORDER BY m.kind,m.code")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "code": r.get::<String,_>("code"), "name": r.get::<String,_>("name"),
        "kind": r.get::<String,_>("kind"), "available": r.get::<bool,_>("available"),
        "state": r.get::<String,_>("state") })).collect();
    Ok(Json(json!(out)))
}

pub async fn list_sites(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,name,address,lat,lon,access_notes FROM app.sites ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // access_notes (indicazioni d'accesso) solo agli autorizzati (pianificatore/admin)
    let can_access = is_planner(&s);
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"), "name":r.get::<String,_>("name"),
        "address": r.try_get::<String,_>("address").ok(),
        "lat": r.try_get::<f64,_>("lat").ok(), "lon": r.try_get::<f64,_>("lon").ok(),
        "access_notes": if can_access { r.try_get::<String,_>("access_notes").ok() } else { None }
    })).collect();
    Ok(Json(json!(out)))
}

// ---- Gruppi di persone CONDIVISI (primitiva comune, riusabile da tutti i moduli) ----
pub async fn list_user_groups(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let groups = sqlx::query("SELECT id,name,kind,labels FROM app.user_groups ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    let mems = sqlx::query("SELECT m.group_id,m.user_id,u.email FROM app.user_group_members m JOIN app.users u ON u.id=m.user_id ORDER BY u.email")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = groups.iter().map(|g| {
        let gid: Uuid = g.get("id");
        let members: Vec<Value> = mems.iter().filter(|m| m.get::<Uuid,_>("group_id")==gid)
            .map(|m| json!({"user_id":m.get::<Uuid,_>("user_id"),"email":m.get::<String,_>("email")})).collect();
        json!({"id":gid,"name":g.get::<String,_>("name"),"kind":g.get::<String,_>("kind"),
            "labels":g.try_get::<Value,_>("labels").unwrap_or(json!({})),"members":members})
    }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct UserGroupReq { pub name: String, #[serde(default)] pub kind: Option<String>, #[serde(default)] pub labels: Option<Value> }
pub async fn create_user_group(State(st): State<AppState>, h: HeaderMap, Json(b): Json<UserGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.user_groups(tenant_id,name,kind,labels) VALUES($1,$2,COALESCE($3,'squadra'),COALESCE($4,'{}'::jsonb)) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.kind.as_deref()).bind(b.labels.as_ref())
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "gruppo già esistente (nome)"))?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_user_group", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn update_user_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<UserGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.user_groups SET name=$2, kind=COALESCE($3,kind), labels=COALESCE($4,labels) WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(b.kind.as_deref()).bind(b.labels.as_ref())
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_user_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.user_groups WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    audit(&st, tid, &s.email, "delete_user_group", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn user_group_member_add(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // verifica appartenenza al tenant del gruppo e dell'utente (RLS già scopa, ma espliciti i 404)
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.user_groups WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"gruppo inesistente")); }
    sqlx::query("INSERT INTO app.user_group_members(tenant_id,group_id,user_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(id).bind(b.user_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn user_group_member_del(State(st): State<AppState>, h: HeaderMap, Path((id,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_group_members WHERE group_id=$1 AND user_id=$2").bind(id).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Import "serve a tutti i moduli": copia i membri di un gruppo comune in un GRUPPO DI ALLARME
/// (escalation). Dimostra il riuso senza accoppiare i modelli. Richiede il modulo escalation.
#[derive(Deserialize)] pub struct ImportGroupReq { pub user_group_id: Uuid }
pub async fn alarm_group_import_members(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<ImportGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1").bind(gid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"gruppo allarme inesistente")); }
    // posizione iniziale = dopo gli attuali membri
    let base: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(position),0)::int FROM app.alarm_group_members WHERE group_id=$1").bind(gid).fetch_one(&mut *tx).await.map_err(ise)?;
    let n = sqlx::query(
        "INSERT INTO app.alarm_group_members(tenant_id,group_id,user_id,position)
         SELECT $1,$2,m.user_id, ($4 + row_number() OVER (ORDER BY u.email))::int
         FROM app.user_group_members m JOIN app.users u ON u.id=m.user_id
         WHERE m.group_id=$3
         ON CONFLICT (group_id,user_id) DO NOTHING")
        .bind(tid).bind(gid).bind(b.user_group_id).bind(base).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "alarm_group_import_members", json!({"group":gid,"from":b.user_group_id,"added":n})).await;
    Ok(Json(json!({"ok":true,"added":n})))
}

pub async fn list_users(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,email,role,phone FROM app.users ORDER BY role,email").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "email": r.get::<String,_>("email"), "role": r.get::<String,_>("role"),
        "phone": r.try_get::<String,_>("phone").ok() })).collect();
    Ok(Json(json!(out)))
}

// ============================ Gestione (Fase 1) ============================
// Anagrafiche e operazioni che mancavano: dispositivi, sedi, utenti, gruppi di
// allarme + routing, assegnazione/note incidenti, check-in in centrale.
// ---- Fase 2: capability layer -------------------------------------------------
// Catalogo capability (coarse, allineato ai controlli concentrati di oggi).
pub const CAPABILITIES: &[(&str,&str)] = &[
    ("admin",   "Amministrazione: impostazioni, utenti, dispositivi, gruppi, configurazione escalation"),
    ("plan",    "Pianificazione: ordini/interventi, turni"),
    ("approve", "Approvazione consuntivi"),
];
/// Mappa di DEFAULT ruolo -> capability (riproduce ESATTAMENTE il comportamento storico).
fn default_role_caps(role: &str) -> Vec<&'static str> {
    match role {
        "tenant_admin" => vec!["admin","plan","approve"],
        "pianificatore" => vec!["plan"],
        "responsabile" => vec!["approve"],
        _ => vec![],
    }
}
/// Capability effettive = default del ruolo ± override per-tenant (granted true/false).
pub async fn effective_caps(st: &AppState, tid: Uuid, role: &str) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> =
        default_role_caps(role).into_iter().map(|c| c.to_string()).collect();
    if let Ok(rows) = sqlx::query("SELECT capability, granted FROM app.role_capabilities WHERE tenant_id=$1 AND role=$2")
        .bind(tid).bind(role).fetch_all(&st.owner_pool).await {
        for r in rows {
            let cap: String = r.get("capability");
            if r.get::<bool,_>("granted") { set.insert(cap); } else { set.remove(&cap); }
        }
    }
    set.into_iter().collect()
}
/// Fase 3: capability effettive dell'UTENTE = livello ruolo (default ± override) UNITE alle
/// capability dei RUOLI PERSONALIZZATI assegnati all'utente (direttamente o via gruppi condivisi).
/// I ruoli custom sono SOLO additivi (aggiungono capability, non le tolgono).
pub async fn effective_caps_user(st: &AppState, tid: Uuid, role: &str, user_id: Uuid) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = effective_caps(st, tid, role).await.into_iter().collect();
    if let Ok(rows) = sqlx::query(
        "SELECT DISTINCT crc.capability FROM app.custom_role_capabilities crc
         WHERE crc.tenant_id=$1 AND crc.role_id IN (
             SELECT role_id FROM app.user_custom_roles WHERE tenant_id=$1 AND user_id=$2
             UNION
             SELECT ugcr.role_id FROM app.user_group_custom_roles ugcr
               JOIN app.user_group_members m ON m.group_id=ugcr.group_id
               WHERE ugcr.tenant_id=$1 AND m.user_id=$2)")
        .bind(tid).bind(user_id).fetch_all(&st.owner_pool).await {
        for r in rows { set.insert(r.get::<String,_>("capability")); }
    }
    set.into_iter().collect()
}
fn has_cap(s: &Session, cap: &str) -> bool { s.caps.iter().any(|c| c == cap) }

fn admin_only(s: &Session) -> Result<(), (StatusCode, String)> {
    if !has_cap(s, "admin") { return Err(err(StatusCode::FORBIDDEN, "riservato all'admin del tenant")); }
    Ok(())
}
const USER_ROLES: &[&str] = &["tenant_admin","operatore","pianificatore","responsabile","lavoratore","reperibile","auditor"];

/// #8 ACL: sedi visibili all'utente. None = tutte (admin/auditor/pianificatore o
/// operatore senza restrizioni); Some(set) = solo quelle sedi.
async fn allowed_sites(st: &AppState, s: &Session) -> Option<std::collections::HashSet<Uuid>> {
    if !matches!(s.role.as_str(), "operatore"|"lavoratore"|"reperibile") { return None; }
    let rows = sqlx::query_scalar::<_, Uuid>("SELECT site_id FROM app.operator_sites WHERE user_id=$1")
        .bind(s.user_id).fetch_all(&st.owner_pool).await.ok()?;
    if rows.is_empty() { None } else { Some(rows.into_iter().collect()) }
}
fn site_allowed(set: &Option<std::collections::HashSet<Uuid>>, site_id: Option<Uuid>) -> bool {
    match set { None => true, Some(s) => site_id.map(|x| s.contains(&x)).unwrap_or(false) }
}
/// ACL per-sito sulle AZIONI di incidente: un operatore ristretto a certe sedi non può
/// ack/chiudere/assegnare/annotare/leggere la timeline di incidenti di altre sedi (coerente
/// col filtro di lettura su list_incidents). Nessun effetto per ruoli non ristretti.
async fn guard_incident_site(st: &AppState, s: &Session, iid: Uuid) -> Result<(), (StatusCode,String)> {
    let allowed = allowed_sites(st, s).await;
    if allowed.is_none() { return Ok(()); }
    let site_id: Option<Uuid> = sqlx::query_scalar("SELECT site_id FROM app.incidents WHERE tenant_id=$1 AND id=$2")
        .bind(s.tenant_id.unwrap()).bind(iid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    if site_allowed(&allowed, site_id) { Ok(()) } else { Err(err(StatusCode::FORBIDDEN, "incidente fuori dalle tue sedi")) }
}

#[derive(Deserialize)]
pub struct UserSitesReq { pub site_ids: Vec<Uuid> }
pub async fn get_user_sites(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query_scalar::<_, Uuid>("SELECT site_id FROM app.operator_sites WHERE user_id=$1")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"site_ids": rows})))
}
pub async fn set_user_sites(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<UserSitesReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.operator_sites WHERE user_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    for sid in &b.site_ids {
        sqlx::query("INSERT INTO app.operator_sites(tenant_id,user_id,site_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(tid).bind(id).bind(sid).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "set_user_sites", json!({"user":id,"n":b.site_ids.len()})).await;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Dispositivi ----------------
pub async fn list_devices(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
    let acl = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT d.id,d.external_id,d.source,d.label,d.enabled,d.config,d.site_id,d.assigned_user,
                d.last_lat,d.last_lon,d.last_battery,d.last_pos_at,d.last_attributes,d.model_code,d.assigned_asset,
                d.traccar_status,d.floorplan_id,d.fp_x,d.fp_y,
                s.name AS site_name, u.email AS user_email, a.name AS asset_name,
                m.manufacturer, m.model_name, m.category, m.capabilities,
                pr.step AS prov_step,
                (SELECT max(received_ts) FROM app.events e WHERE e.device_id=d.id) AS last_seen,
                (SELECT e.etype FROM app.events e WHERE e.device_id=d.id ORDER BY e.received_ts DESC LIMIT 1) AS last_etype
         FROM app.devices d LEFT JOIN app.sites s ON s.id=d.site_id LEFT JOIN app.users u ON u.id=d.assigned_user
              LEFT JOIN app.assets a ON a.id=d.assigned_asset
              LEFT JOIN app.device_models m ON m.model_code=d.model_code
              LEFT JOIN app.device_provisioning pr ON pr.device_id=d.id
         ORDER BY d.source,d.external_id").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter()
        .filter(|r| site_allowed(&acl, r.try_get::<Uuid,_>("site_id").ok()))
        .map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "external_id": r.get::<String,_>("external_id"), "source": r.get::<String,_>("source"),
        "label": r.try_get::<String,_>("label").ok(), "enabled": r.get::<bool,_>("enabled"), "config": r.get::<Value,_>("config"),
        "site_id": r.try_get::<Uuid,_>("site_id").ok(), "site_name": r.try_get::<String,_>("site_name").ok(),
        "assigned_user": r.try_get::<Uuid,_>("assigned_user").ok(), "user_email": r.try_get::<String,_>("user_email").ok(),
        "last_seen": r.try_get::<chrono::DateTime<chrono::Utc>,_>("last_seen").ok(),
        "last_etype": r.try_get::<String,_>("last_etype").ok(),
        "last_lat": r.try_get::<f64,_>("last_lat").ok(), "last_lon": r.try_get::<f64,_>("last_lon").ok(),
        "last_battery": r.try_get::<f64,_>("last_battery").ok(),
        "last_pos_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("last_pos_at").ok(),
        "last_attributes": r.try_get::<Value,_>("last_attributes").ok(),
        "model_code": r.try_get::<String,_>("model_code").ok(),
        "manufacturer": r.try_get::<String,_>("manufacturer").ok(),
        "model_name": r.try_get::<String,_>("model_name").ok(),
        "category": r.try_get::<String,_>("category").ok(),
        "capabilities": r.try_get::<Vec<String>,_>("capabilities").ok(),
        "assigned_asset": r.try_get::<Uuid,_>("assigned_asset").ok(),
        "asset_name": r.try_get::<String,_>("asset_name").ok(),
        "traccar_status": r.try_get::<String,_>("traccar_status").ok(),
        "floorplan_id": r.try_get::<Uuid,_>("floorplan_id").ok(),
        "fp_x": r.try_get::<f64,_>("fp_x").ok(), "fp_y": r.try_get::<f64,_>("fp_y").ok(),
        "prov_step": r.try_get::<String,_>("prov_step").ok() })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct DeviceReq { pub external_id: String, pub source: String, pub label: Option<String>,
    pub site_id: Option<Uuid>, pub assigned_user: Option<Uuid>, pub enabled: Option<bool>, pub config: Option<Value> }
pub async fn create_device(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DeviceReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.external_id.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "external_id richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,assigned_user,enabled,config)
         VALUES($1,$2,$3,$4,$5,$6,COALESCE($7,true),COALESCE($8,'{}'::jsonb)) RETURNING id")
        .bind(tid).bind(b.external_id.trim()).bind(&b.source).bind(&b.label).bind(b.site_id).bind(b.assigned_user)
        .bind(b.enabled).bind(b.config.clone())
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "dispositivo gia' presente (source+external_id)"))?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "dispositivo gia' presente"))?.get::<Uuid,_>("id");
    if b.assigned_user.is_some() { record_device_assignment(&mut tx, tid, id, None, b.assigned_user, st.clock.now()).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_device", json!({"external_id":b.external_id,"source":b.source})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn update_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<DeviceReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let old: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.devices WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
    let n = sqlx::query(
        "UPDATE app.devices SET external_id=$2,source=$3,label=$4,site_id=$5,assigned_user=$6,
            enabled=COALESCE($7,enabled),config=COALESCE($8,config),updated_at=now() WHERE id=$1")
        .bind(id).bind(b.external_id.trim()).bind(&b.source).bind(&b.label).bind(b.site_id).bind(b.assigned_user)
        .bind(b.enabled).bind(b.config.clone()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n>0 { record_device_assignment(&mut tx, tid, id, old, b.assigned_user, st.clock.now()).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente")); }
    audit(&st, tid, &s.email, "update_device", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.devices WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|_| err(StatusCode::CONFLICT, "dispositivo referenziato da eventi: disabilitalo invece di eliminarlo"))?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente")); }
    audit(&st, tid, &s.email, "delete_device", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Ingest token (credenziali macchina dei connettori) ----------------
// Il tenant deriva SEMPRE dal token di ingest; qui il tenant_admin li vede e li ruota.
/// Worker Safety Fase A — hash a riposo delle credenziali di ingest (sha256 hex).
pub fn hash_token(tok: &str) -> String {
    use sha2::{Sha256, Digest};
    let d = Sha256::digest(tok.as_bytes());
    d.iter().map(|b| format!("{:02x}", b)).collect()
}
/// Credenziale di ingest risolta dal token (header X-Ingest-Token / querystring legacy).
pub struct IngestCred { pub tenant_id: Uuid, pub source: String, pub device_id: Option<Uuid> }
/// Risolve il token di ingest con ciclo di vita e binding al dispositivo.
/// Match per hash (nuovi token) o per token in chiaro (legacy non ancora ruotati).
/// Rifiuta token revocati o scaduti; aggiorna last_used_at best-effort.
pub async fn resolve_ingest_token(st: &AppState, token: &str) -> Option<IngestCred> {
    let hash = hash_token(token);
    // SICUREZZA: match per hash (token nuovi). Il match in chiaro vale SOLO per i token legacy
    // non ancora ruotati (token_hash IS NULL): per i token hashati la colonna `token` contiene un
    // handle NON segreto mostrato in UI, che non deve mai autenticare.
    let row = sqlx::query(
        "SELECT tenant_id,source,device_id,expires_at,revoked_at FROM app.ingest_tokens \
         WHERE token_hash=$1 OR (token=$2 AND token_hash IS NULL) LIMIT 1")
        .bind(&hash).bind(token).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let revoked: Option<chrono::DateTime<chrono::Utc>> = row.try_get("revoked_at").ok().flatten();
    if revoked.is_some() { return None; }
    let exp: Option<chrono::DateTime<chrono::Utc>> = row.try_get("expires_at").ok().flatten();
    if let Some(e) = exp { if e < st.clock.now() { return None; } }
    let _ = sqlx::query("UPDATE app.ingest_tokens SET last_used_at=now() WHERE token_hash=$1 OR (token=$2 AND token_hash IS NULL)")
        .bind(&hash).bind(token).execute(&st.owner_pool).await;
    Some(IngestCred { tenant_id: row.get("tenant_id"), source: row.get("source"), device_id: row.try_get("device_id").ok().flatten() })
}
/// external_id del dispositivo legato a un token device-bound (per attribuire/forzare il fix).
pub async fn bound_device_external(st: &AppState, device_id: Uuid) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT external_id FROM app.devices WHERE id=$1")
        .bind(device_id).fetch_optional(&st.owner_pool).await.ok().flatten().flatten()
}

pub async fn list_ingest_tokens(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, CONNECTOR_MODS).await?;
    let rows = sqlx::query(
        "SELECT t.token,t.token_hash,t.source,t.created_at,t.device_id,t.label,t.expires_at,t.revoked_at,t.last_used_at, COALESCE(d.label, d.external_id) AS device_name \
         FROM app.ingest_tokens t LEFT JOIN app.devices d ON d.id=t.device_id \
         WHERE t.tenant_id=$1 ORDER BY t.source,t.created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let now = st.clock.now();
    let out: Vec<Value> = rows.iter().map(|r| {
        let hashed: Option<String> = r.try_get("token_hash").ok().flatten();
        let raw: String = r.get("token");
        let revoked: Option<chrono::DateTime<chrono::Utc>> = r.try_get("revoked_at").ok().flatten();
        let exp: Option<chrono::DateTime<chrono::Utc>> = r.try_get("expires_at").ok().flatten();
        let expired = exp.map(|e| e < now).unwrap_or(false);
        // i token nuovi (hashed) non espongono più il segreto: solo un handle/prefisso.
        json!({
            "token": raw, "hashed": hashed.is_some(), "source": r.get::<String,_>("source"),
            "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
            "device_id": r.try_get::<Option<Uuid>,_>("device_id").ok().flatten(),
            "device_name": r.try_get::<Option<String>,_>("device_name").ok().flatten(),
            "label": r.try_get::<Option<String>,_>("label").ok().flatten(),
            "expires_at": exp, "revoked": revoked.is_some(), "expired": expired,
            "last_used_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("last_used_at").ok().flatten(),
        })
    }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct TokenReq {
    pub source: String,
    #[serde(default)] pub device_id: Option<Uuid>,
    #[serde(default)] pub label: Option<String>,
    #[serde(default)] pub expires_days: Option<i64>,
}
pub async fn create_ingest_token(State(st): State<AppState>, h: HeaderMap, Json(b): Json<TokenReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    const SRC: &[&str] = &["traccar","thingsboard","iot"];
    if !SRC.contains(&b.source.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "sorgente non valida")); }
    // device opzionale: deve appartenere al tenant
    if let Some(did) = b.device_id {
        let ok = sqlx::query_scalar::<_, Uuid>("SELECT id FROM app.devices WHERE id=$1 AND tenant_id=$2")
            .bind(did).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "device non valido per questo tenant")); }
    }
    // segreto mostrato UNA volta; a riposo salviamo solo l'hash + un handle non segreto (prefisso).
    let secret = format!("{}-{}", b.source, &auth::new_token()[..24]);
    let hash = hash_token(&secret);
    let handle = format!("{}-{}", b.source, &secret[b.source.len()+1..b.source.len()+9]);
    let expires_at = b.expires_days.filter(|d| *d > 0).map(|d| st.clock.now() + chrono::Duration::days(d));
    sqlx::query("INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id,label,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(&handle).bind(&hash).bind(tid).bind(&b.source).bind(b.device_id).bind(&b.label).bind(expires_at)
        .execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_ingest_token", json!({"source":b.source,"device_id":b.device_id,"expires_at":expires_at})).await;
    // il token completo torna SOLO ora (non più recuperabile dopo)
    Ok(Json(json!({"token":secret,"handle":handle,"note":"Copia il token adesso: non sarà più mostrato."})))
}
/// Rotazione/revoca: revoca per handle (token column) del tenant.
pub async fn revoke_ingest_token(State(st): State<AppState>, h: HeaderMap, Path(tok): Path<String>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let n = sqlx::query("UPDATE app.ingest_tokens SET revoked_at=now() WHERE token=$1 AND tenant_id=$2 AND revoked_at IS NULL")
        .bind(&tok).bind(tid).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "token inesistente o già revocato")); }
    audit(&st, tid, &s.email, "revoke_ingest_token", json!({})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_ingest_token(State(st): State<AppState>, h: HeaderMap, Path(tok): Path<String>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    // il filtro per tenant_id impedisce di toccare token di altri tenant
    let n = sqlx::query("DELETE FROM app.ingest_tokens WHERE token=$1 AND tenant_id=$2")
        .bind(&tok).bind(tid).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "token inesistente")); }
    audit(&st, tid, &s.email, "delete_ingest_token", json!({})).await;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Connettori (riepilogo) ----------------
pub async fn list_connectors(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, CONNECTOR_MODS).await?;
    let defs = [("traccar","Traccar (badge)",5055usize),("thingsboard","ThingsBoard (IoT)",0),("goalert","GoAlert (escalation)",0),("iot","IoT generico",0)];
    let has_key: i64 = sqlx::query_scalar("SELECT count(*) FROM app.goalert_services WHERE tenant_id=$1")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    let mut out = Vec::new();
    for (code,name,_p) in defs {
        let state: Option<String> = sqlx::query_scalar("SELECT state FROM app.tenant_modules WHERE tenant_id=$1 AND module_code=$2")
            .bind(tid).bind(code).fetch_optional(&st.owner_pool).await.map_err(ise)?;
        let toks: i64 = sqlx::query_scalar("SELECT count(*) FROM app.ingest_tokens WHERE tenant_id=$1 AND source=$2")
            .bind(tid).bind(code).fetch_one(&st.owner_pool).await.map_err(ise)?;
        out.push(json!({"code":code,"name":name,"state":state.unwrap_or("non_abilitato".into()),
            "tokens":toks,"has_key": if code=="goalert" { Some(has_key>0) } else { None::<bool> }}));
    }
    Ok(Json(json!(out)))
}

// ================= Fase A — Catalogo modelli dispositivo =================
pub async fn list_catalog(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
    let term = q.get("q").map(|s| format!("%{}%", s.to_lowercase()));
    let manu = q.get("manufacturer").filter(|s| !s.is_empty()).cloned();
    let cat = q.get("category").filter(|s| !s.is_empty()).cloned();
    let rows = sqlx::query(
        "SELECT model_code,manufacturer,model_name,protocol,default_port,category,capabilities,osmand,recommended,notes
         FROM app.device_models
         WHERE ($1::text IS NULL OR lower(manufacturer||' '||model_name) LIKE $1)
           AND ($2::text IS NULL OR manufacturer=$2)
           AND ($3::text IS NULL OR category=$3)
         ORDER BY recommended DESC, manufacturer, model_name
         LIMIT 300")
        .bind(term).bind(manu).bind(cat).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "model_code":r.get::<String,_>("model_code"),"manufacturer":r.get::<String,_>("manufacturer"),
        "model_name":r.get::<String,_>("model_name"),"protocol":r.get::<String,_>("protocol"),
        "default_port":r.try_get::<i32,_>("default_port").ok(),"category":r.get::<String,_>("category"),
        "capabilities":r.get::<Vec<String>,_>("capabilities"),"osmand":r.get::<bool,_>("osmand"),
        "recommended":r.get::<bool,_>("recommended"),"notes":r.try_get::<String,_>("notes").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn catalog_facets(State(st): State<AppState>, h: HeaderMap) -> R {
    let _ = require_tenant(&st, &h).await?;
    let manu: Vec<String> = sqlx::query_scalar("SELECT DISTINCT manufacturer FROM app.device_models ORDER BY manufacturer")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cats: Vec<String> = sqlx::query_scalar("SELECT DISTINCT category FROM app.device_models ORDER BY category")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"manufacturers":manu,"categories":cats})))
}

// ================= Fase C — Asset =================
pub async fn list_assets(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT a.id,a.name,a.kind,a.identifier,a.site_id,si.name AS site,a.note
        FROM app.assets a LEFT JOIN app.sites si ON si.id=a.site_id ORDER BY a.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"kind":r.get::<String,_>("kind"),
        "identifier":r.try_get::<String,_>("identifier").ok(),"site_id":r.try_get::<Uuid,_>("site_id").ok(),
        "site":r.try_get::<String,_>("site").ok(),"note":r.try_get::<String,_>("note").ok() })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct AssetReq { pub name: String, pub kind: Option<String>, pub identifier: Option<String>, pub site_id: Option<Uuid>, pub note: Option<String> }
pub async fn create_asset(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AssetReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let kind = b.kind.clone().unwrap_or("attrezzatura".into());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.assets(tenant_id,name,kind,identifier,site_id,note) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&kind).bind(&b.identifier).bind(b.site_id).bind(&b.note)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "nome gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id")})))
}
pub async fn delete_asset(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.assets WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|_| err(StatusCode::CONFLICT, "asset con dispositivi collegati"))?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "asset inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct AssignReq2 { pub user_id: Option<Uuid>, pub asset_id: Option<Uuid> }
pub async fn assign_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<AssignReq2>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    if b.user_id.is_some() && b.asset_id.is_some() { return Err(err(StatusCode::BAD_REQUEST, "assegna a utente OPPURE asset, non entrambi")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let old: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.devices WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
    let n = sqlx::query("UPDATE app.devices SET assigned_user=$2, assigned_asset=$3, updated_at=now() WHERE id=$1")
        .bind(id).bind(b.user_id).bind(b.asset_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n>0 { record_device_assignment(&mut tx, tid, id, old, b.user_id, st.clock.now()).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "device inesistente")); }
    audit(&st, tid, &s.email, "assign_device", json!({"device":id,"user":b.user_id,"asset":b.asset_id})).await;
    Ok(Json(json!({"ok":true})))
}

// ================= Device Onboarding Kit (DOK) =================
// Da un modello a catalogo (qualsiasi hardware censito) genera in un colpo tutto il lato-YW:
// device + credenziale di ingest (token device-bound) + routing degli allarmi + (per i device
// personali) profilo di protezione, differenziando per TIPOLOGIA D'USO (device_models.category).

/// Mappa una capacità dichiarata del modello all'etype YW (solo segnali di sicurezza diretti).
fn dok_cap_to_etype(cap: &str) -> Option<&'static str> {
    match cap.to_ascii_lowercase().as_str() {
        "sos" => Some("sos"),
        "fall" | "falldown" | "man_down" | "mandown" => Some("man_down"),
        "movement" | "motion" | "immobility" | "immobilita" => Some("immobilita"),
        "battery" => Some("batteria_bassa"),
        _ => None, // gps/ignition/temperature/immobilizer/ecc.: non sono allarmi di sicurezza diretti
    }
}
struct DokPlan { needs_checkin: bool, suggest_profile: bool, label: &'static str }
fn dok_category_plan(cat: &str) -> DokPlan {
    match cat {
        "personale"  => DokPlan{needs_checkin:true,  suggest_profile:true,  label:"Lavoratore isolato"},
        "smartphone" => DokPlan{needs_checkin:true,  suggest_profile:true,  label:"Smartphone / app"},
        "veicolo"    => DokPlan{needs_checkin:false, suggest_profile:false, label:"Veicolo / flotta"},
        "asset"      => DokPlan{needs_checkin:false, suggest_profile:false, label:"Asset / attrezzatura"},
        "sensore"    => DokPlan{needs_checkin:false, suggest_profile:false, label:"Sensore IoT"},
        _            => DokPlan{needs_checkin:false, suggest_profile:false, label:"Generico"},
    }
}
fn dok_ingress_base() -> String { std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_else(|_| "http://localhost:8080".into()) }

#[derive(Deserialize)]
pub struct DokReq {
    pub model_code: String,
    pub external_id: String,
    pub label: Option<String>,
    pub site_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub group_id: Option<Uuid>,      // gruppo di escalation per gli allarmi del device
    pub make_profile: Option<bool>,  // crea/attacca un profilo di protezione (device personali)
}
pub async fn device_onboarding_kit(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DokReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    dok_build(&st, tid, &s.email, b).await.map(Json)
}

// ---- DOK massivo (import CSV/lista): censisce N device con UN'unica chiamata ----
#[derive(Deserialize)]
pub struct DokBulkRow {
    pub external_id: String,
    pub model_code: Option<String>,   // override per-riga; altrimenti default_model_code
    pub label: Option<String>,
    pub worker_email: Option<String>,
    pub group: Option<String>,        // nome gruppo, override; altrimenti default_group
}
#[derive(Deserialize)]
pub struct DokBulkReq {
    pub default_model_code: Option<String>,
    pub default_group: Option<String>,     // nome gruppo di default per tutte le righe
    pub make_profile: Option<bool>,
    pub rows: Vec<DokBulkRow>,
}
pub async fn device_onboarding_kit_bulk(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DokBulkReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if b.rows.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nessuna riga")); }
    if b.rows.len() > 500 { return Err(err(StatusCode::BAD_REQUEST, "massimo 500 righe per import")); }
    let mut results: Vec<Value> = Vec::with_capacity(b.rows.len());
    let mut created = 0i64; let mut failed = 0i64;
    for row in &b.rows {
        let ext = row.external_id.trim();
        if ext.is_empty() { failed+=1; results.push(json!({"external_id":"","ok":false,"error":"external_id vuoto"})); continue; }
        let model_code = row.model_code.clone().or_else(|| b.default_model_code.clone());
        let Some(model_code) = model_code else { failed+=1; results.push(json!({"external_id":ext,"ok":false,"error":"model_code mancante (nessun default)"})); continue; };
        // lavoratore
        let user_id = if let Some(em) = &row.worker_email {
            let u = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE tenant_id=$1 AND lower(email)=lower($2)")
                .bind(tid).bind(em.trim()).fetch_optional(&st.owner_pool).await.map_err(ise)?;
            if u.is_none() { failed+=1; results.push(json!({"external_id":ext,"ok":false,"error":format!("lavoratore non trovato: {}", em)})); continue; }
            u
        } else { None };
        // gruppo (riga o default)
        let gname = row.group.clone().or_else(|| b.default_group.clone());
        let group_id = if let Some(gn) = &gname {
            sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE tenant_id=$1 AND lower(name)=lower($2)")
                .bind(tid).bind(gn.trim()).fetch_optional(&st.owner_pool).await.map_err(ise)?
        } else { None };
        if gname.is_some() && group_id.is_none() { failed+=1; results.push(json!({"external_id":ext,"ok":false,"error":format!("gruppo non trovato: {}", gname.unwrap())})); continue; }
        let req = DokReq{ model_code, external_id: ext.to_string(), label: row.label.clone(), site_id:None, user_id, group_id, make_profile: b.make_profile };
        match dok_build(&st, tid, &s.email, req).await {
            Ok(v) => { created+=1; results.push(json!({
                "external_id": ext, "ok": true, "device_id": v.get("device_id"),
                "category": v.get("category_label"), "etypes": v.get("etypes"),
                "token": v.get("token"), "ingress_url": v.get("ingress_url"), "profile_id": v.get("profile_id") })); }
            Err(e) => { failed+=1; results.push(json!({"external_id":ext,"ok":false,"error":e.1})); }
        }
    }
    audit(&st, tid, &s.email, "device_onboarding_kit_bulk", json!({"total":b.rows.len(),"created":created,"failed":failed})).await;
    Ok(Json(json!({"total": b.rows.len(), "created": created, "failed": failed, "results": results})))
}
/// Logica riusabile del Device Onboarding Kit (usata dall'endpoint e dall'agente AI).
pub async fn dok_build(st: &AppState, tid: Uuid, actor: &str, b: DokReq) -> Result<Value,(StatusCode,String)> {
    require_any_module(st, tid, DEVICE_MODS).await?;
    if b.external_id.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "IMEI/uniqueId richiesto")); }
    // modello dal catalogo
    let m = sqlx::query("SELECT manufacturer,model_name,protocol,capabilities,category FROM app.device_models WHERE model_code=$1")
        .bind(&b.model_code).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "modello inesistente nel catalogo"))?;
    let manufacturer: String = m.get("manufacturer");
    let model_name: String = m.get("model_name");
    let protocol: String = m.get("protocol");
    let caps: Vec<String> = m.get("capabilities");
    let category: String = m.try_get("category").unwrap_or_else(|_| "altro".into());
    let plan = dok_category_plan(&category);
    // etype derivati dalle capacità (segnali di sicurezza)
    let mut etypes: Vec<String> = vec![];
    for c in &caps { if let Some(e) = dok_cap_to_etype(c) { if !etypes.iter().any(|x| x==e) { etypes.push(e.to_string()); } } }

    let now = st.clock.now();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // 1) device
    let dcfg = json!({"model_code": b.model_code, "capabilities": caps, "protocol": protocol, "category": category});
    let label = b.label.clone().unwrap_or_else(|| format!("{manufacturer} {model_name}"));
    let drow = sqlx::query(
        "INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,assigned_user,enabled,config,model_code)
         VALUES($1,$2,'traccar',$3,$4,$5,true,$6,$7) RETURNING id")
        .bind(tid).bind(b.external_id.trim()).bind(&label).bind(b.site_id).bind(b.user_id).bind(&dcfg).bind(&b.model_code)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "dispositivo gia' presente (uniqueId)"))?;
    let dev_id = drow.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id");
    if b.user_id.is_some() { record_device_assignment(&mut tx, tid, dev_id, None, b.user_id, now).await.map_err(ise)?; }
    // 2) routing degli etype -> gruppo (se fornito)
    let mut routed: Vec<String> = vec![];
    if let Some(g) = b.group_id {
        let okg = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(g).bind(tid).fetch_optional(&mut *tx).await.map_err(ise)?.is_some();
        if !okg { return Err(err(StatusCode::BAD_REQUEST, "gruppo inesistente")); }
        for et in &etypes {
            sqlx::query("INSERT INTO app.alarm_routing(tenant_id,etype,group_id) VALUES($1,$2,$3)
                 ON CONFLICT (tenant_id,etype) DO UPDATE SET group_id=EXCLUDED.group_id")
                .bind(tid).bind(et).bind(g).execute(&mut *tx).await.map_err(ise)?;
            routed.push(et.clone());
        }
    }
    // 3) profilo di protezione (device personali/smartphone, se richiesto e con lavoratore)
    let mut profile_id: Option<Uuid> = None;
    if plan.suggest_profile && b.make_profile.unwrap_or(false) {
        if let Some(uid) = b.user_id {
            let triggers = json!({
                "sos": etypes.iter().any(|e|e=="sos"),
                "man_down": etypes.iter().any(|e|e=="man_down"),
                "immobilita": etypes.iter().any(|e|e=="immobilita"),
                "mancato_checkin": plan.needs_checkin,
                "geofence": false });
            let pname = format!("DOK {} · {}", plan.label, label);
            let prow = sqlx::query("INSERT INTO app.protection_profiles(tenant_id,name,descr,escalation_group_id,triggers,checkin_minutes,enabled)
                 VALUES($1,$2,$3,$4,$5,$6,true)
                 ON CONFLICT (tenant_id,name) DO UPDATE SET escalation_group_id=EXCLUDED.escalation_group_id, triggers=EXCLUDED.triggers RETURNING id")
                .bind(tid).bind(&pname).bind("Generato dal Device Onboarding Kit").bind(b.group_id).bind(&triggers)
                .bind(if plan.needs_checkin { Some(30i32) } else { None })
                .fetch_one(&mut *tx).await.map_err(ise)?;
            let pid: Uuid = prow.get("id"); profile_id = Some(pid);
            // membro (sposta dal profilo precedente per via dell'UNIQUE tenant_id,user_id)
            sqlx::query("DELETE FROM app.protection_profile_members WHERE tenant_id=$1 AND user_id=$2").bind(tid).bind(uid).execute(&mut *tx).await.map_err(ise)?;
            sqlx::query("INSERT INTO app.protection_profile_members(tenant_id,profile_id,user_id) VALUES($1,$2,$3)").bind(tid).bind(pid).bind(uid).execute(&mut *tx).await.map_err(ise)?;
        }
    }
    tx.commit().await.map_err(ise)?;

    // 4) credenziale di ingest device-bound (segreto one-time)
    let secret = format!("traccar-{}", &auth::new_token()[..24]);
    let hash = hash_token(&secret);
    let handle = format!("traccar-{}", &secret[8..16]);
    sqlx::query("INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id,label) VALUES($1,$2,$3,'traccar',$4,$5)")
        .bind(&handle).bind(&hash).bind(tid).bind(dev_id).bind(&label).execute(&st.owner_pool).await.map_err(ise)?;

    audit(st, tid, actor, "device_onboarding_kit", json!({"device":dev_id,"model":b.model_code,"category":category,"etypes":etypes,"profile":profile_id})).await;
    let base = dok_ingress_base();
    Ok(json!({
        "device_id": dev_id, "external_id": b.external_id.trim(), "model_code": b.model_code,
        "manufacturer": manufacturer, "model_name": model_name, "protocol": protocol,
        "category": category, "category_label": plan.label, "capabilities": caps,
        "etypes": etypes, "routed_etypes": routed, "routed_group": b.group_id, "profile_id": profile_id,
        "token": secret, "handle": handle,
        "ingress_url": format!("{base}/api/ingress/traccar?token={secret}"),
        "osmand_url": format!("{base}/api/ingress/osmand?token={secret}&id={}&lat={{lat}}&lon={{lon}}&timestamp={{unix}}", b.external_id.trim()),
        "note": "Copia il token adesso: non sarà più mostrato. Due canali: Traccar (event forwarding verso ingress_url) OPPURE nativo OsmAnd/HTTP (osmand_url: smartphone/tracker che parlano OsmAnd, senza server Traccar)."
    }))
}

/// Readiness/idoneità del device per i suoi segnali: dichiarato (dalle capacità del modello) vs
/// verificato (abbiamo mai ricevuto quel segnale). Stati: registrazione · assegnazione · connessione · protezione.
pub async fn device_readiness(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    device_readiness_value(&st, tid, id).await.map(Json)
}
/// Readiness come Value (riusabile da endpoint e agente AI).
pub async fn device_readiness_value(st: &AppState, tid: Uuid, id: Uuid) -> Result<Value,(StatusCode,String)> {
    let d = sqlx::query("SELECT external_id,label,assigned_user,enabled,config,model_code FROM app.devices WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "device inesistente"))?;
    let cfg: Value = d.try_get("config").unwrap_or(json!({}));
    let caps: Vec<String> = cfg.get("capabilities").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
    let category = cfg.get("category").and_then(|v| v.as_str()).unwrap_or("altro").to_string();
    let plan = dok_category_plan(&category);
    let assigned: Option<Uuid> = d.try_get("assigned_user").ok().flatten();
    // ultimo contatto = max(received_ts) degli eventi del device (la tabella devices non lo tiene)
    let last_seen: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT max(received_ts) FROM app.events WHERE tenant_id=$1 AND device_id=$2")
        .bind(tid).bind(id).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let online = last_seen.map(|t| (st.clock.now() - t) < chrono::Duration::minutes(15)).unwrap_or(false);
    // per capacità -> etype -> verificato se esiste almeno un evento di quell'etype per il device
    let mut signals = vec![]; let mut missing = vec![];
    for c in &caps {
        if let Some(et) = dok_cap_to_etype(c) {
            let seen: bool = sqlx::query_scalar::<_,Option<Uuid>>("SELECT id FROM app.events WHERE tenant_id=$1 AND device_id=$2 AND etype=$3 LIMIT 1")
                .bind(tid).bind(id).bind(et).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten().is_some();
            signals.push(json!({"capability":c,"etype":et,"verified":seen}));
            if !seen { missing.push(et.to_string()); }
        }
    }
    let protection_ok = assigned.is_some() && online;
    Ok(json!({
        "device_id": id, "category": category, "category_label": plan.label, "capabilities": caps,
        "states": {
            "registrazione": true,
            "assegnazione": assigned.is_some(),
            "connessione": if online {"online"} else if last_seen.is_some() {"offline"} else {"mai_visto"},
            "protezione": if protection_ok {"attiva"} else {"degradata"}
        },
        "last_seen": last_seen.map(|t| t.to_rfc3339()),
        "signals": signals, "missing_signals": missing,
        "ready": missing.is_empty() && assigned.is_some(),
        "needs_checkin": plan.needs_checkin
    }))
}

/// SOS di prova dalla console: inietta un SOS attraverso il PERCORSO REALE (ingress->incidente->
/// escalation), marcato come test. Serve a validare il profilo end-to-end (spec §3.4).
pub async fn device_test_sos(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let ext: Option<String> = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let Some(ext) = ext else { return Err(err(StatusCode::NOT_FOUND, "device inesistente")); };
    let corr = format!("TEST-SOS-{}", &Uuid::new_v4().to_string()[..8]);
    let src_evt = format!("test-sos-{}", Uuid::new_v4());
    let r = persist_ingest(&st, tid, Normalized{
        source:"traccar".into(), source_event: src_evt.clone(), idem: src_evt,
        etype:"sos".into(), device_external_id: Some(ext.clone()),
        site_name:None, lat:None, lon:None, accuracy_m:None, correlation: corr.clone(),
        payload: json!({"test":true,"by":s.email,"source":"test_sos_console"}),
        indoor:false, beacon_uid:None, zone:None }).await?;
    audit(&st, tid, &s.email, "device_test_sos", json!({"device":id,"correlation":corr})).await;
    Ok(Json(json!({"ok":true,"correlation":corr,"result":r.0,"note":"SOS di PROVA inviato sul percorso reale (incidente + escalation). Chiudilo in Centrale."})))
}

// ================= Fase B — Provisioning guidato =================
#[derive(Deserialize)]
pub struct ProvisionReq {
    pub model_code: String, pub external_id: String, pub label: Option<String>, pub site_id: Option<Uuid>,
    pub user_id: Option<Uuid>, pub asset_id: Option<Uuid>, pub register_traccar: Option<bool> }
pub async fn provision_device(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ProvisionReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if b.external_id.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "IMEI/uniqueId richiesto")); }
    if b.user_id.is_some() && b.asset_id.is_some() { return Err(err(StatusCode::BAD_REQUEST, "assegna a utente OPPURE asset")); }
    // modello dal catalogo
    let m = sqlx::query("SELECT protocol,default_port,capabilities,default_config,osmand FROM app.device_models WHERE model_code=$1")
        .bind(&b.model_code).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "modello inesistente"))?;
    let protocol: String = m.get("protocol");
    let port: Option<i32> = m.try_get("default_port").ok();
    let caps: Vec<String> = m.get("capabilities");
    let def_cfg: Value = m.get("default_config");
    // crea device (source 'traccar': tutti i protocolli arrivano via forwarding Traccar)
    let dcfg = json!({"model_code": b.model_code, "capabilities": caps, "protocol": protocol});
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let drow = sqlx::query(
        "INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,assigned_user,assigned_asset,enabled,config,model_code)
         VALUES($1,$2,'traccar',$3,$4,$5,$6,true,$7,$8) RETURNING id")
        .bind(tid).bind(b.external_id.trim()).bind(&b.label).bind(b.site_id).bind(b.user_id).bind(b.asset_id)
        .bind(&dcfg).bind(&b.model_code)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "dispositivo gia' presente (uniqueId)"))?;
    let dev_id = drow.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id");
    if b.user_id.is_some() { record_device_assignment(&mut tx, tid, dev_id, None, b.user_id, st.clock.now()).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    // default alarm_map dal modello SOLO se il tenant non ha gia' una config propria
    let has_cfg: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='traccar' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    if has_cfg.is_none() && def_cfg.is_object() && !def_cfg.as_object().unwrap().is_empty() {
        let mut ttx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let _ = sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'traccar','alarm_map',$2,$3) ON CONFLICT DO NOTHING")
            .bind(tid).bind(&def_cfg).bind(s.user_id).execute(&mut *ttx).await;
        let _ = ttx.commit().await;
    }
    // registrazione su Traccar (opzionale, se connessione configurata)
    let mut step = "in_attesa_primo_fix".to_string();
    let mut tdid: Option<i64> = None; let mut last_err: Option<String> = None;
    if b.register_traccar.unwrap_or(false) {
        if let Some(conn) = sqlx::query("SELECT base_url,username,password FROM app.traccar_connections WHERE tenant_id=$1")
            .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)? {
            let base: String = conn.get("base_url"); let user: String = conn.get("username");
            let pass: String = st.secrets.decrypt(&conn.get::<String,_>("password"));   // P3
            let name = b.label.clone().unwrap_or_else(|| b.external_id.clone());
            match st.http.post(format!("{base}/api/devices")).basic_auth(&user, Some(&pass))
                .json(&json!({"name":name,"uniqueId":b.external_id})).send().await {
                Ok(r) if r.status().is_success() => {
                    if let Ok(v) = r.json::<Value>().await { tdid = v.get("id").and_then(|x| x.as_i64()); }
                    step = "traccar_registrato".into();
                }
                Ok(r) => last_err = Some(format!("Traccar {}", r.status())),
                Err(e) => last_err = Some(format!("Traccar irraggiungibile: {e}")),
            }
        } else { last_err = Some("connessione Traccar non configurata".into()); }
    }
    sqlx::query("INSERT INTO app.device_provisioning(device_id,tenant_id,step,traccar_device_id,last_error) VALUES($1,$2,$3,$4,$5)
        ON CONFLICT (device_id) DO UPDATE SET step=EXCLUDED.step,traccar_device_id=EXCLUDED.traccar_device_id,last_error=EXCLUDED.last_error,updated_at=now()")
        .bind(dev_id).bind(tid).bind(&step).bind(tdid).bind(&last_err).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "provision_device", json!({"model":b.model_code,"external_id":b.external_id})).await;
    Ok(Json(json!({"device_id":dev_id,"step":step,"traccar_device_id":tdid,"error":last_err,
        "install":{"protocol":protocol,"port":port,"note":"Configura il device verso host:porta del server Traccar (o OsmAnd per smartphone)."}})))
}

// ---------------- Sedi ----------------
#[derive(Deserialize)]
pub struct SiteReq { pub name: String, #[serde(default)] pub address: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>, #[serde(default)] pub access_notes: Option<String> }
pub async fn create_site(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SiteReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.sites(tenant_id,name,address,lat,lon,access_notes) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.address.as_deref()).bind(b.lat).bind(b.lon).bind(b.access_notes.as_deref())
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_site", json!({"name":b.name})).await;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn update_site(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<SiteReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // COALESCE: aggiorna solo i campi passati; address/access_notes vuoti ("") azzerano.
    let n = sqlx::query("UPDATE app.sites SET name=$2,
            address=COALESCE($3,address), lat=COALESCE($4,lat), lon=COALESCE($5,lon), access_notes=COALESCE($6,access_notes)
         WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(b.address.as_deref()).bind(b.lat).bind(b.lon).bind(b.access_notes.as_deref())
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "sede inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_site(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.sites WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|_| err(StatusCode::CONFLICT, "sede referenziata: rimuovi prima i collegamenti"))?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "sede inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---------------- Utenti ----------------
#[derive(Deserialize)]
pub struct NewUser { pub email: String, pub role: String, pub password: String }
pub async fn create_user(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewUser>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if !USER_ROLES.contains(&b.role.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo non valido")); }
    if b.password.len() < 4 { return Err(err(StatusCode::BAD_REQUEST, "password troppo corta")); }
    if b.email.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "email richiesta")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(b.email.trim()).bind(auth::hash_password(&b.password)).bind(&b.role)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "email gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "email gia' presente"))?.get::<Uuid,_>("id");
    audit(&st, tid, &s.email, "create_user", json!({"email":b.email,"role":b.role})).await;
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)]
pub struct UpdateUser { pub role: Option<String>, pub password: Option<String>, pub phone: Option<String> }
pub async fn update_user(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<UpdateUser>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if let Some(r) = &b.role {
        if !USER_ROLES.contains(&r.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo non valido")); }
        // non togliere l'ultimo admin
        if id == s.user_id && r != "tenant_admin" {
            let admins: i64 = sqlx::query_scalar("SELECT count(*) FROM app.users WHERE tenant_id=$1 AND role='tenant_admin'")
                .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
            if admins <= 1 { return Err(err(StatusCode::CONFLICT, "non puoi rimuovere l'ultimo admin")); }
        }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if let Some(r) = &b.role {
        sqlx::query("UPDATE app.users SET role=$2 WHERE id=$1").bind(id).bind(r).execute(&mut *tx).await.map_err(ise)?;
    }
    if let Some(p) = &b.password {
        if p.len() < 4 { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, "password troppo corta")); }
        sqlx::query("UPDATE app.users SET pass_hash=$2 WHERE id=$1").bind(id).bind(auth::hash_password(p)).execute(&mut *tx).await.map_err(ise)?;
    }
    if let Some(ph) = &b.phone {
        let ph = ph.trim();
        if ph.is_empty() {
            sqlx::query("UPDATE app.users SET phone=NULL WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
        } else {
            if !is_e164(ph) { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, "telefono: formato E.164 richiesto (es. +393331234567)")); }
            sqlx::query("UPDATE app.users SET phone=$2 WHERE id=$1").bind(id).bind(ph).execute(&mut *tx).await.map_err(ise)?;
        }
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "update_user", json!({"id":id,"role":b.role})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_user(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if id == s.user_id { return Err(err(StatusCode::CONFLICT, "non puoi eliminare te stesso")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.users WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|_| err(StatusCode::CONFLICT, "utente referenziato (ordini/consuntivi): cambia ruolo invece di eliminarlo"))?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "utente inesistente")); }
    audit(&st, tid, &s.email, "delete_user", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Gruppi di allarme + routing ----------------
pub async fn list_groups(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let groups = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day,slack_channel_id,slack_notify,labels,playbook FROM app.alarm_groups ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    let members = sqlx::query(
        "SELECT m.group_id,m.user_id,m.position,u.email,u.role FROM app.alarm_group_members m
         JOIN app.users u ON u.id=m.user_id ORDER BY m.position").fetch_all(&mut *tx).await.map_err(ise)?;
    let routing = sqlx::query("SELECT etype,group_id FROM app.alarm_routing").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = groups.iter().map(|g| {
        let gid: Uuid = g.get("id");
        let mem: Vec<Value> = members.iter().filter(|m| m.get::<Uuid,_>("group_id")==gid).map(|m| json!({
            "user_id":m.get::<Uuid,_>("user_id"),"email":m.get::<String,_>("email"),
            "role":m.get::<String,_>("role"),"position":m.get::<i32,_>("position")})).collect();
        json!({"id":gid,"name":g.get::<String,_>("name"),"members":mem,
            "oncall_mode":g.get::<String,_>("oncall_mode"),"oncall_type":g.get::<String,_>("oncall_type"),
            "oncall_shift":g.get::<i32,_>("oncall_shift"),"oncall_tz":g.get::<String,_>("oncall_tz"),
            "coverage_mode":g.get::<String,_>("coverage_mode"),
            "cov_start":g.try_get::<Option<String>,_>("cov_start").ok().flatten(),
            "cov_end":g.try_get::<Option<String>,_>("cov_end").ok().flatten(),
            "cov_days":g.get::<String,_>("cov_days"),
            "handoff_time":g.try_get::<Option<String>,_>("handoff_time").ok().flatten(),
            "handoff_day":g.try_get::<Option<i32>,_>("handoff_day").ok().flatten(),
            "slack_channel_id":g.try_get::<Option<String>,_>("slack_channel_id").ok().flatten(),
            "slack_notify":g.get::<String,_>("slack_notify"),
            "labels":g.try_get::<Value,_>("labels").unwrap_or_else(|_| json!({})),
            "playbook":g.try_get::<String,_>("playbook").unwrap_or_default()})
    }).collect();
    let routes: Vec<Value> = routing.iter().map(|r| json!({"etype":r.get::<String,_>("etype"),"group_id":r.get::<Uuid,_>("group_id")})).collect();
    Ok(Json(json!({"groups":out,"routing":routes})))
}
#[derive(Deserialize)]
pub struct GroupReq { pub name: String }
pub async fn create_group(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.alarm_groups(tenant_id,name) VALUES($1,$2) RETURNING id")
        .bind(tid).bind(b.name.trim()).fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "gruppo gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "gruppo gia' presente"))?.get::<Uuid,_>("id");
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)]
pub struct GroupOncallReq {
    pub mode: String, pub oncall_type: Option<String>, pub shift: Option<i32>, pub tz: Option<String>,
    // copertura / handoff / slack (tutti opzionali: default sicuri)
    pub coverage_mode: Option<String>, pub cov_start: Option<String>, pub cov_end: Option<String>,
    pub cov_days: Option<String>, pub handoff_time: Option<String>, pub handoff_day: Option<i32>,
    pub slack_channel_id: Option<String>, pub slack_notify: Option<String>,
}
fn valid_hhmm(s: &str) -> bool { parse_hhmm(s).is_some() }
// Turni + copertura + slack del gruppo (effetto alla prossima Sincronizza con GoAlert).
pub async fn set_group_oncall(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GroupOncallReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if !matches!(b.mode.as_str(), "tutti"|"turni") { return Err(err(StatusCode::BAD_REQUEST, "mode: 'tutti' o 'turni'")); }
    let ty = b.oncall_type.as_deref().unwrap_or("weekly");
    if !matches!(ty, "hourly"|"daily"|"weekly"|"monthly") { return Err(err(StatusCode::BAD_REQUEST, "tipo turno non valido")); }
    let shift = b.shift.unwrap_or(1).clamp(1, 999);
    let tz = b.tz.as_deref().unwrap_or("Europe/Rome");
    let coverage = b.coverage_mode.as_deref().unwrap_or("24_7");
    if !matches!(coverage, "24_7"|"orari") { return Err(err(StatusCode::BAD_REQUEST, "coverage_mode: '24_7' o 'orari'")); }
    if coverage == "orari" {
        for t in [b.cov_start.as_deref(), b.cov_end.as_deref()] {
            if let Some(v) = t { if !v.is_empty() && !valid_hhmm(v) { return Err(err(StatusCode::BAD_REQUEST, "orario copertura: formato HH:MM")); } }
        }
    }
    if let Some(ht) = b.handoff_time.as_deref() { if !ht.is_empty() && !valid_hhmm(ht) { return Err(err(StatusCode::BAD_REQUEST, "handoff: formato HH:MM")); } }
    let cov_days = b.cov_days.as_deref().filter(|d| d.len()==7 && d.chars().all(|c| c=='0'||c=='1')).unwrap_or("1111111");
    let hday = b.handoff_day.map(|d| d.clamp(0,6));
    let slack_notify = b.slack_notify.as_deref().unwrap_or("off");
    if !(slack_notify=="off"||slack_notify=="onchange"||valid_hhmm(slack_notify)) { return Err(err(StatusCode::BAD_REQUEST, "slack_notify: 'off','onchange' o HH:MM")); }
    let slack_ch = b.slack_channel_id.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.alarm_groups SET oncall_mode=$2,oncall_type=$3,oncall_shift=$4,oncall_tz=$5,coverage_mode=$6,cov_start=$7,cov_end=$8,cov_days=$9,handoff_time=$10,handoff_day=$11,slack_channel_id=$12,slack_notify=$13 WHERE id=$1")
        .bind(id).bind(&b.mode).bind(ty).bind(shift).bind(tz)
        .bind(coverage).bind(&b.cov_start).bind(&b.cov_end).bind(cov_days)
        .bind(&b.handoff_time).bind(hday).bind(slack_ch).bind(slack_notify)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    Ok(Json(json!({"ok":true})))
}
// ---- F3: policy di escalation multi-step del gruppo (parità GoAlert) ----
pub async fn group_policy_get(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let steps = sqlx::query("SELECT idx,delay_s,channel,target_kind,target_group_id,target_user_id,target_slack FROM app.esc_policy_steps WHERE group_id=$1 ORDER BY idx")
        .bind(gid).fetch_all(&mut *tx).await.map_err(ise)?;
    let rep = sqlx::query_scalar::<_,Option<i32>>("SELECT esc_repeat FROM app.alarm_groups WHERE id=$1")
        .bind(gid).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = steps.iter().map(|r| json!({
        "idx":r.get::<i32,_>("idx"),"delay_min":(r.get::<i32,_>("delay_s")/60),"channel":r.get::<String,_>("channel"),
        "target_kind":r.try_get::<String,_>("target_kind").unwrap_or_else(|_|"oncall".into()),
        "target_group_id":r.try_get::<Option<Uuid>,_>("target_group_id").ok().flatten(),
        "target_user_id":r.try_get::<Option<Uuid>,_>("target_user_id").ok().flatten(),
        "target_slack":r.try_get::<Option<String>,_>("target_slack").ok().flatten()})).collect();
    Ok(Json(json!({"steps":out,"repeat":rep})))
}
#[derive(Deserialize)] pub struct PolicyStepReq { pub delay_min: Option<i64>, pub channel: String,
    #[serde(default)] pub target_kind: Option<String>, #[serde(default)] pub target_group_id: Option<Uuid>,
    #[serde(default)] pub target_user_id: Option<Uuid>, #[serde(default)] pub target_slack: Option<String> }
#[derive(Deserialize)] pub struct PolicySetReq { pub steps: Vec<PolicyStepReq>, #[serde(default)] pub repeat: Option<i32> }
/// Sostituisce (replace-all) gli step della policy del gruppo + il repeat. steps vuoto = torna alla
/// ladder derivata (default). L'ordine dell'array è l'ordine degli step (reorder lato client).
pub async fn group_policy_set(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<PolicySetReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.steps.len() > 20 { return Err(err(StatusCode::BAD_REQUEST, "massimo 20 step")); }
    for stp in &b.steps {
        if !matches!(stp.channel.as_str(), "sms"|"voice"|"email"|"slack") { return Err(err(StatusCode::BAD_REQUEST, "canale non valido (sms|voice|email|slack)")); }
        let dm = stp.delay_min.unwrap_or(0);
        if !(0..=1440).contains(&dm) { return Err(err(StatusCode::BAD_REQUEST, "attesa step: 0..1440 minuti")); }
        let kind = stp.target_kind.as_deref().unwrap_or("oncall");
        if !matches!(kind, "oncall"|"group"|"user"|"slack") { return Err(err(StatusCode::BAD_REQUEST, "target non valido (oncall|group|user|slack)")); }
        // il destinatario del target deve appartenere al tenant
        if kind=="group" {
            let g = stp.target_group_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "target group richiede target_group_id"))?;
            if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(g).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::BAD_REQUEST, "gruppo target inesistente")); }
        }
        if kind=="user" {
            let u = stp.target_user_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "target user richiede target_user_id"))?;
            if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(u).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::BAD_REQUEST, "utente target inesistente")); }
        }
        if kind=="slack" && stp.target_slack.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true) { return Err(err(StatusCode::BAD_REQUEST, "target slack richiede un canale")); }
    }
    let repeat = b.repeat.map(|r| r.clamp(0, 10));
    // il gruppo deve esistere nel tenant
    let exists = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !exists { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.esc_policy_steps WHERE group_id=$1").bind(gid).execute(&mut *tx).await.map_err(ise)?;
    for (i, stp) in b.steps.iter().enumerate() {
        let kind = stp.target_kind.as_deref().unwrap_or("oncall");
        sqlx::query("INSERT INTO app.esc_policy_steps(tenant_id,group_id,idx,delay_s,channel,target_kind,target_group_id,target_user_id,target_slack) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(tid).bind(gid).bind(i as i32).bind((stp.delay_min.unwrap_or(0)*60) as i32).bind(&stp.channel)
            .bind(kind).bind(stp.target_group_id).bind(stp.target_user_id)
            .bind(stp.target_slack.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()))
            .execute(&mut *tx).await.map_err(ise)?;
    }
    sqlx::query("UPDATE app.alarm_groups SET esc_repeat=$2 WHERE id=$1").bind(gid).bind(repeat).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, "youworkforce", "group_policy_set", json!({"group":gid,"steps":b.steps.len(),"repeat":repeat})).await;
    Ok(Json(json!({"ok":true,"steps":b.steps.len()})))
}

// ---- F3: label/metadata sui gruppi (parità GoAlert labels) + ricerca ----
#[derive(Deserialize)] pub struct LabelsReq { pub labels: std::collections::HashMap<String,String> }
pub async fn group_labels_set(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<LabelsReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.labels.len() > 30 { return Err(err(StatusCode::BAD_REQUEST, "massimo 30 label")); }
    for (k,v) in &b.labels {
        if k.trim().is_empty() || k.len() > 60 || v.len() > 200 { return Err(err(StatusCode::BAD_REQUEST, "label: chiave 1..60, valore <=200")); }
    }
    let lv = serde_json::to_value(&b.labels).unwrap_or_else(|_| json!({}));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.alarm_groups SET labels=$2 WHERE id=$1").bind(gid).bind(&lv).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    audit(&st, tid, "youworkforce", "group_labels_set", json!({"group":gid,"labels":b.labels.len()})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct LabelSearchQuery { pub key: Option<String>, pub value: Option<String>, pub q: Option<String> }
/// Ricerca gruppi per label (key, key+value) o testo nel nome. Parità GoAlert search.
pub async fn group_search(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<LabelSearchQuery>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,name,labels FROM app.alarm_groups ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let key = q.key.as_deref().map(|s| s.to_lowercase());
    let val = q.value.as_deref();
    let txt = q.q.as_deref().map(|s| s.to_lowercase());
    let out: Vec<Value> = rows.iter().filter_map(|r| {
        let name: String = r.get("name");
        let labels: Value = r.try_get("labels").unwrap_or_else(|_| json!({}));
        if let Some(t) = &txt { if !name.to_lowercase().contains(t) { return None; } }
        if let Some(k) = &key {
            let found = labels.as_object().map(|o| o.iter().any(|(kk,vv)|
                kk.to_lowercase()==*k && val.map(|v| vv.as_str()==Some(v)).unwrap_or(true))).unwrap_or(false);
            if !found { return None; }
        }
        Some(json!({"id":r.get::<Uuid,_>("id"),"name":name,"labels":labels}))
    }).collect();
    Ok(Json(json!({"groups":out})))
}

// ---- F3/G4: note di passaggio turno (handoff) + playbook del gruppo ----
pub async fn handoff_notes_list(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let rows = sqlx::query("SELECT id,author,body,created_at FROM app.esc_handoff_notes WHERE tenant_id=$1 AND group_id=$2 ORDER BY created_at DESC LIMIT 50")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"author":r.get::<String,_>("author"),
        "body":r.get::<String,_>("body"),"at":r.get::<chrono::DateTime<chrono::Utc>,_>("created_at")})).collect();
    Ok(Json(json!({"notes":out})))
}
#[derive(Deserialize)] pub struct HandoffReq { pub body: String }
pub async fn handoff_note_add(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<HandoffReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"reperibile"|"responsabile") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let body = b.body.trim();
    if body.is_empty() || body.len() > 4000 { return Err(err(StatusCode::BAD_REQUEST, "nota 1..4000 caratteri")); }
    let exists = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !exists { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.esc_handoff_notes(tenant_id,group_id,author,body) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(gid).bind(&s.email).bind(body).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn handoff_note_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_handoff_notes WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "nota inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct PlaybookReq { pub playbook: String }
pub async fn group_playbook_set(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<PlaybookReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.playbook.len() > 8000 { return Err(err(StatusCode::BAD_REQUEST, "playbook troppo lungo (max 8000)")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.alarm_groups SET playbook=$2 WHERE id=$1").bind(gid).bind(b.playbook.trim()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- R1: Set Active (parità Rotations GoAlert) — imposta SUBITO il reperibile di turno ----
#[derive(Deserialize)] pub struct SetActiveReq { pub user_id: Uuid }
pub async fn rotation_set_active(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<SetActiveReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // indice del membro nell'ordine della rotation (ORDER BY position)
    let members: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM app.alarm_group_members WHERE group_id=$1 ORDER BY position")
        .bind(gid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let Some(idx) = members.iter().position(|u| *u == b.user_id) else {
        return Err(err(StatusCode::BAD_REQUEST, "l'utente non è membro del gruppo"));
    };
    let now = st.clock.now();
    sqlx::query("INSERT INTO app.esc_rotation_state(tenant_id,group_id,position,shift_start,seeded_from) VALUES($1,$2,$3,$4,'manual')
        ON CONFLICT (tenant_id,group_id) DO UPDATE SET position=EXCLUDED.position, shift_start=EXCLUDED.shift_start, seeded_from='manual', updated_at=now()")
        .bind(tid).bind(gid).bind(idx as i32).bind(now).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "rotation_set_active", json!({"group":gid,"user":b.user_id,"position":idx})).await;
    Ok(Json(json!({"ok":true,"position":idx})))
}

// ---- G-A: assegnazioni multiple del gruppo (coperture aggiuntive con finestra per-assegnazione) ----
pub async fn assignments_list(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let rows = sqlx::query("SELECT id,label,cov_days,cov_start,cov_end FROM app.esc_assignments WHERE tenant_id=$1 AND group_id=$2 ORDER BY created_at")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut out = Vec::new();
    for a in &rows {
        let aid: Uuid = a.get("id");
        let mem = sqlx::query("SELECT u.id,u.email FROM app.esc_assignment_members am JOIN app.users u ON u.id=am.user_id WHERE am.assignment_id=$1 ORDER BY am.position")
            .bind(aid).fetch_all(&st.owner_pool).await.map_err(ise)?;
        let mj: Vec<Value> = mem.iter().map(|m| json!({"user_id":m.get::<Uuid,_>("id"),"email":m.get::<String,_>("email")})).collect();
        out.push(json!({"id":aid,"label":a.get::<String,_>("label"),"cov_days":a.get::<String,_>("cov_days"),
            "cov_start":a.try_get::<Option<String>,_>("cov_start").ok().flatten(),
            "cov_end":a.try_get::<Option<String>,_>("cov_end").ok().flatten(),
            "mode":a.try_get::<String,_>("mode").unwrap_or_else(|_|"tutti".into()),
            "oncall_type":a.try_get::<String,_>("oncall_type").unwrap_or_else(|_|"weekly".into()),
            "oncall_shift":a.try_get::<i32,_>("oncall_shift").unwrap_or(1),
            "members":mj}));
    }
    Ok(Json(json!({"assignments":out})))
}
#[derive(Deserialize)] pub struct AssignmentReq { #[serde(default)] pub label: String, #[serde(default)] pub cov_days: Option<String>, #[serde(default)] pub cov_start: Option<String>, #[serde(default)] pub cov_end: Option<String>,
    #[serde(default)] pub mode: Option<String>, #[serde(default)] pub oncall_type: Option<String>, #[serde(default)] pub oncall_shift: Option<i32>,
    #[serde(default)] pub handoff_time: Option<String>, #[serde(default)] pub handoff_day: Option<i32> }
pub async fn assignment_create(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<AssignmentReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    let days = b.cov_days.as_deref().filter(|d| d.len()==7 && d.chars().all(|c| c=='0'||c=='1')).unwrap_or("1111111");
    for t in [b.cov_start.as_deref(), b.cov_end.as_deref(), b.handoff_time.as_deref()].into_iter().flatten() { if !t.is_empty() && parse_hhmm(t).is_none() { return Err(err(StatusCode::BAD_REQUEST, "orario: HH:MM")); } }
    let mode = b.mode.as_deref().unwrap_or("tutti");
    if !matches!(mode, "tutti"|"turni") { return Err(err(StatusCode::BAD_REQUEST, "mode: tutti|turni")); }
    let oc_type = b.oncall_type.as_deref().unwrap_or("weekly");
    if !matches!(oc_type, "hourly"|"daily"|"weekly"|"monthly") { return Err(err(StatusCode::BAD_REQUEST, "tipo turno non valido")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.esc_assignments(tenant_id,group_id,label,cov_days,cov_start,cov_end,mode,oncall_type,oncall_shift,handoff_time,handoff_day) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id")
        .bind(tid).bind(gid).bind(b.label.trim()).bind(days)
        .bind(b.cov_start.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty()))
        .bind(b.cov_end.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty()))
        .bind(mode).bind(oc_type).bind(b.oncall_shift.unwrap_or(1).clamp(1,999))
        .bind(b.handoff_time.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty()))
        .bind(b.handoff_day.map(|d| d.clamp(0,6)))
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn assignment_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_assignments WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "assegnazione inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn assignment_member_add(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.esc_assignments WHERE id=$1 AND tenant_id=$2").bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND, "assegnazione inesistente")); }
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::BAD_REQUEST, "utente non nel tenant")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.esc_assignment_members(tenant_id,assignment_id,user_id,position) VALUES($1,$2,$3,COALESCE($4,1)) ON CONFLICT (assignment_id,user_id) DO UPDATE SET position=EXCLUDED.position")
        .bind(tid).bind(aid).bind(b.user_id).bind(b.position).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn assignment_member_del(State(st): State<AppState>, h: HeaderMap, Path((aid,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.esc_assignment_members WHERE assignment_id=$1 AND user_id=$2 AND tenant_id=$3").bind(aid).bind(uid).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

pub async fn delete_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.alarm_groups WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct MemberReq { pub user_id: Uuid, pub position: Option<i32> }
pub async fn add_group_member(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.alarm_group_members(tenant_id,group_id,user_id,position) VALUES($1,$2,$3,COALESCE($4,1))
        ON CONFLICT (group_id,user_id) DO UPDATE SET position=EXCLUDED.position")
        .bind(tid).bind(gid).bind(b.user_id).bind(b.position).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn remove_group_member(State(st): State<AppState>, h: HeaderMap, Path((gid,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.alarm_group_members WHERE group_id=$1 AND user_id=$2").bind(gid).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct RouteReq { pub etype: String, pub group_id: Option<Uuid> }
pub async fn set_routing(State(st): State<AppState>, h: HeaderMap, Json(b): Json<RouteReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    match b.group_id {
        Some(g) => { sqlx::query("INSERT INTO app.alarm_routing(tenant_id,etype,group_id) VALUES($1,$2,$3)
            ON CONFLICT (tenant_id,etype) DO UPDATE SET group_id=EXCLUDED.group_id")
            .bind(tid).bind(&b.etype).bind(g).execute(&mut *tx).await.map_err(ise)?; }
        None => { sqlx::query("DELETE FROM app.alarm_routing WHERE tenant_id=$1 AND etype=$2").bind(tid).bind(&b.etype).execute(&mut *tx).await.map_err(ise)?; }
    }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Centrale: assegnazione + note ----------------
#[derive(Deserialize)]
pub struct AssignReq { pub user_id: Option<Uuid> }
pub async fn assign_incident(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>, Json(b): Json<AssignReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    guard_incident_site(&st, &s, iid).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.incidents SET assigned_user=$2 WHERE id=$1").bind(iid).bind(b.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n>0 {
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'assegnazione',$3)")
            .bind(tid).bind(iid).bind(json!({"assigned_user":b.user_id,"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "incidente inesistente")); }
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_assigned","incident_id":iid})}).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct NoteReq { pub body: String }
pub async fn add_incident_note(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>, Json(b): Json<NoteReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    guard_incident_site(&st, &s, iid).await?;
    let tid = s.tenant_id.unwrap();
    let body = b.body.trim();
    if body.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nota vuota")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail)
        SELECT $1,$2,'nota',$3 WHERE EXISTS(SELECT 1 FROM app.incidents WHERE id=$2)")
        .bind(tid).bind(iid).bind(json!({"body":body,"by":s.email})).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "incidente inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---------------- Check-in in centrale ----------------
pub async fn list_checkins(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    require_module(&st, s.tenant_id.unwrap(), "loneworker").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT c.id,c.due_at,c.state,u.email,s.name AS site FROM app.checkins c
         JOIN app.users u ON u.id=c.user_id LEFT JOIN app.sites s ON s.id=c.site_id
         WHERE c.state IN ('attivo','scaduto') ORDER BY c.due_at").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"due_at":r.get::<chrono::DateTime<chrono::Utc>,_>("due_at"),
        "state":r.get::<String,_>("state"),"email":r.get::<String,_>("email"),"site":r.try_get::<String,_>("site").ok() })).collect();
    Ok(Json(json!(out)))
}

pub(crate) async fn audit(st: &AppState, tid: Uuid, actor: &str, action: &str, detail: Value) {
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,$3,$4)")
        .bind(tid).bind(actor).bind(action).bind(detail).execute(&st.owner_pool).await;
}

// ---- Fase 2: gestione capability per ruolo (override per-tenant) ----
pub async fn list_capabilities(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let catalog: Vec<Value> = CAPABILITIES.iter().map(|(c,d)| json!({"code":c,"desc":d})).collect();
    // matrice effettiva ruolo x capability + flag "override" (diverso dal default)
    let mut matrix = serde_json::Map::new();
    for role in USER_ROLES {
        let eff = effective_caps(&st, tid, role).await;
        let def: Vec<&str> = default_role_caps(role);
        let row: Vec<Value> = CAPABILITIES.iter().map(|(c,_)| {
            let has = eff.iter().any(|x| x==c);
            let is_def = def.contains(c);
            json!({"cap":c,"granted":has,"default":is_def,"override":has!=is_def})
        }).collect();
        matrix.insert(role.to_string(), json!(row));
    }
    Ok(Json(json!({"catalog":catalog,"roles":USER_ROLES,"matrix":matrix})))
}
#[derive(Deserialize)] pub struct CapReq { pub role: String, pub capability: String, pub granted: bool }
pub async fn set_capability(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CapReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if !USER_ROLES.contains(&b.role.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo sconosciuto")); }
    if !CAPABILITIES.iter().any(|(c,_)| *c==b.capability) { return Err(err(StatusCode::BAD_REQUEST, "capability sconosciuta")); }
    // guardia anti-lockout: non togliere 'admin' a tenant_admin
    if b.role=="tenant_admin" && b.capability=="admin" && !b.granted {
        return Err(err(StatusCode::BAD_REQUEST, "non si puo' togliere 'admin' a tenant_admin"));
    }
    // se granted coincide col default -> rimuovi l'override (torna al default)
    let is_def = default_role_caps(&b.role).contains(&b.capability.as_str());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if b.granted == is_def {
        sqlx::query("DELETE FROM app.role_capabilities WHERE tenant_id=$1 AND role=$2 AND capability=$3")
            .bind(tid).bind(&b.role).bind(&b.capability).execute(&mut *tx).await.map_err(ise)?;
    } else {
        sqlx::query("INSERT INTO app.role_capabilities(tenant_id,role,capability,granted) VALUES($1,$2,$3,$4)
                     ON CONFLICT (tenant_id,role,capability) DO UPDATE SET granted=EXCLUDED.granted, updated_at=now()")
            .bind(tid).bind(&b.role).bind(&b.capability).bind(b.granted).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "set_capability", json!({"role":b.role,"capability":b.capability,"granted":b.granted})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn reset_capabilities(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.role_capabilities WHERE tenant_id=$1").bind(tid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "reset_capabilities", json!({})).await;
    Ok(Json(json!({"ok":true})))
}

// ---- Fase 3: ruoli personalizzati per tenant (bundle di capability, assegnabili) ----
pub async fn list_custom_roles(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let roles = sqlx::query("SELECT id,name,description FROM app.custom_roles ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    let caps = sqlx::query("SELECT role_id,capability FROM app.custom_role_capabilities").fetch_all(&mut *tx).await.map_err(ise)?;
    let ur = sqlx::query("SELECT r.role_id,u.id as user_id,u.email FROM app.user_custom_roles r JOIN app.users u ON u.id=r.user_id ORDER BY u.email").fetch_all(&mut *tx).await.map_err(ise)?;
    let gr = sqlx::query("SELECT r.role_id,g.id as group_id,g.name FROM app.user_group_custom_roles r JOIN app.user_groups g ON g.id=r.group_id ORDER BY g.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = roles.iter().map(|r| {
        let rid: Uuid = r.get("id");
        let caps: Vec<String> = caps.iter().filter(|c| c.get::<Uuid,_>("role_id")==rid).map(|c| c.get::<String,_>("capability")).collect();
        let users: Vec<Value> = ur.iter().filter(|x| x.get::<Uuid,_>("role_id")==rid).map(|x| json!({"user_id":x.get::<Uuid,_>("user_id"),"email":x.get::<String,_>("email")})).collect();
        let groups: Vec<Value> = gr.iter().filter(|x| x.get::<Uuid,_>("role_id")==rid).map(|x| json!({"group_id":x.get::<Uuid,_>("group_id"),"name":x.get::<String,_>("name")})).collect();
        json!({"id":rid,"name":r.get::<String,_>("name"),"description":r.try_get::<String,_>("description").ok(),"capabilities":caps,"users":users,"groups":groups})
    }).collect();
    let catalog: Vec<Value> = CAPABILITIES.iter().map(|(c,d)| json!({"code":c,"desc":d})).collect();
    Ok(Json(json!({"roles":out,"catalog":catalog})))
}
#[derive(Deserialize)] pub struct CustomRoleReq { pub name: String, #[serde(default)] pub description: Option<String>, #[serde(default)] pub capabilities: Vec<String> }
fn validate_caps(caps: &[String]) -> Result<(), (StatusCode,String)> {
    for c in caps { if !CAPABILITIES.iter().any(|(k,_)| k==c) { return Err(err(StatusCode::BAD_REQUEST, "capability sconosciuta")); } }
    Ok(())
}
async fn set_role_caps(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, rid: Uuid, caps: &[String]) -> Result<(), (StatusCode,String)> {
    sqlx::query("DELETE FROM app.custom_role_capabilities WHERE role_id=$1").bind(rid).execute(&mut **tx).await.map_err(ise)?;
    for c in caps {
        sqlx::query("INSERT INTO app.custom_role_capabilities(tenant_id,role_id,capability) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(tid).bind(rid).bind(c).execute(&mut **tx).await.map_err(ise)?;
    }
    Ok(())
}
pub async fn create_custom_role(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CustomRoleReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    validate_caps(&b.capabilities)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.custom_roles(tenant_id,name,description) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.description.as_deref())
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "ruolo già esistente (nome)"))?;
    let rid = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    set_role_caps(&mut tx, tid, rid, &b.capabilities).await?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_custom_role", json!({"name":b.name,"caps":b.capabilities})).await;
    Ok(Json(json!({"id":rid})))
}
pub async fn update_custom_role(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<CustomRoleReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    validate_caps(&b.capabilities)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.custom_roles SET name=$2, description=$3 WHERE id=$1")
        .bind(rid).bind(b.name.trim()).bind(b.description.as_deref()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "ruolo inesistente")); }
    set_role_caps(&mut tx, tid, rid, &b.capabilities).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_custom_role(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.custom_roles WHERE id=$1").bind(rid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "ruolo inesistente")); }
    audit(&st, tid, &s.email, "delete_custom_role", json!({"id":rid})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct AssignUserReq { pub user_id: Uuid }
pub async fn custom_role_assign_user(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<AssignUserReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.custom_roles WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"ruolo inesistente")); }
    sqlx::query("INSERT INTO app.user_custom_roles(tenant_id,user_id,role_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(b.user_id).bind(rid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn custom_role_unassign_user(State(st): State<AppState>, h: HeaderMap, Path((rid,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_custom_roles WHERE role_id=$1 AND user_id=$2").bind(rid).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct AssignGroupReq { pub group_id: Uuid }
pub async fn custom_role_assign_group(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<AssignGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.custom_roles WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"ruolo inesistente")); }
    sqlx::query("INSERT INTO app.user_group_custom_roles(tenant_id,group_id,role_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(b.group_id).bind(rid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn custom_role_unassign_group(State(st): State<AppState>, h: HeaderMap, Path((rid,gid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_group_custom_roles WHERE role_id=$1 AND group_id=$2").bind(rid).bind(gid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ============================ Campo Slice 2 — Ronde e punzonature (giri) ============================
const PATROL_KINDS: &[&str] = &["qr","nfc","beacon","gps"];

// ---- Configurazione (pianificatore) ----
pub async fn list_patrol_routes(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let routes = sqlx::query("SELECT r.id,r.name,r.site_id,r.window_minutes,r.active,s.name as site_name FROM app.patrol_routes r LEFT JOIN app.sites s ON s.id=r.site_id ORDER BY r.name").fetch_all(&mut *tx).await.map_err(ise)?;
    let cps = sqlx::query("SELECT id,route_id,name,position,kind,code,lat,lon,radius_m FROM app.patrol_checkpoints ORDER BY position,name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = routes.iter().map(|r| {
        let rid: Uuid = r.get("id");
        let checkpoints: Vec<Value> = cps.iter().filter(|c| c.get::<Uuid,_>("route_id")==rid).map(|c| json!({
            "id":c.get::<Uuid,_>("id"),"name":c.get::<String,_>("name"),"position":c.get::<i32,_>("position"),
            "kind":c.get::<String,_>("kind"),"code":c.try_get::<String,_>("code").ok(),
            "lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),"radius_m":c.get::<i32,_>("radius_m")})).collect();
        json!({"id":rid,"name":r.get::<String,_>("name"),"site_id":r.try_get::<Uuid,_>("site_id").ok(),
            "site_name":r.try_get::<String,_>("site_name").ok(),"window_minutes":r.try_get::<i32,_>("window_minutes").ok(),
            "active":r.get::<bool,_>("active"),"checkpoints":checkpoints})
    }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct PatrolRouteReq { pub name: String, #[serde(default)] pub site_id: Option<Uuid>, #[serde(default)] pub window_minutes: Option<i32>, #[serde(default)] pub active: Option<bool> }
pub async fn create_patrol_route(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PatrolRouteReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.patrol_routes(tenant_id,name,site_id,window_minutes) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.site_id).bind(b.window_minutes)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "giro già esistente (nome)"))?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_patrol_route", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn update_patrol_route(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PatrolRouteReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.patrol_routes SET name=$2, site_id=$3, window_minutes=$4, active=COALESCE($5,active) WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(b.site_id).bind(b.window_minutes).bind(b.active).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "giro inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_patrol_route(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.patrol_routes WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "giro inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct CheckpointReq { pub name: String, pub kind: String, #[serde(default)] pub code: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>, #[serde(default)] pub radius_m: Option<i32>, #[serde(default)] pub position: Option<i32> }
pub async fn create_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<CheckpointReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if !PATROL_KINDS.contains(&b.kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo checkpoint sconosciuto (qr/nfc/beacon/gps)")); }
    if b.kind=="gps" && (b.lat.is_none()||b.lon.is_none()) { return Err(err(StatusCode::BAD_REQUEST, "checkpoint GPS richiede lat/lon")); }
    if (b.kind=="qr"||b.kind=="nfc"||b.kind=="beacon") && b.code.as_deref().unwrap_or("").trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "checkpoint qr/nfc/beacon richiede un codice")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.patrol_routes WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"giro inesistente")); }
    let pos = match b.position { Some(p) => p, None => {
        let m: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(position),0)::int FROM app.patrol_checkpoints WHERE route_id=$1").bind(rid).fetch_one(&mut *tx).await.map_err(ise)?;
        m + 1 } };
    let row = sqlx::query("INSERT INTO app.patrol_checkpoints(tenant_id,route_id,name,position,kind,code,lat,lon,radius_m) VALUES($1,$2,$3,$4,$5,$6,$7,$8,COALESCE($9,50)) RETURNING id")
        .bind(tid).bind(rid).bind(b.name.trim()).bind(pos).bind(&b.kind).bind(b.code.as_deref()).bind(b.lat).bind(b.lon).bind(b.radius_m)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    let id = row.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<CheckpointReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if !PATROL_KINDS.contains(&b.kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo checkpoint sconosciuto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.patrol_checkpoints SET name=$2, kind=$3, code=$4, lat=$5, lon=$6, radius_m=COALESCE($7,radius_m), position=COALESCE($8,position) WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(&b.kind).bind(b.code.as_deref()).bind(b.lat).bind(b.lon).bind(b.radius_m).bind(b.position).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "checkpoint inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.patrol_checkpoints WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "checkpoint inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- Esecuzione (qualunque utente del tenant col modulo attivo) ----
async fn round_view(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, rid: Uuid) -> Result<Value, (StatusCode,String)> {
    let r = sqlx::query("SELECT ro.id,ro.route_id,ro.user_id,ro.started_at,ro.ended_at,ro.status,rt.name as route_name,rt.window_minutes FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id WHERE ro.id=$1")
        .bind(rid).fetch_optional(&mut **tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"ronda inesistente"))?;
    let route_id: Uuid = r.get("route_id");
    let cps = sqlx::query("SELECT id,name,position,kind,code,lat,lon,radius_m FROM app.patrol_checkpoints WHERE route_id=$1 ORDER BY position,name").bind(route_id).fetch_all(&mut **tx).await.map_err(ise)?;
    let punches = sqlx::query("SELECT checkpoint_id,method,punched_at,ok,note FROM app.patrol_punches WHERE round_id=$1").bind(rid).fetch_all(&mut **tx).await.map_err(ise)?;
    let cpv: Vec<Value> = cps.iter().map(|c| {
        let cid: Uuid = c.get("id");
        let p = punches.iter().find(|p| p.get::<Uuid,_>("checkpoint_id")==cid);
        json!({"id":cid,"name":c.get::<String,_>("name"),"position":c.get::<i32,_>("position"),"kind":c.get::<String,_>("kind"),
            "code":c.try_get::<String,_>("code").ok(),"lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),"radius_m":c.get::<i32,_>("radius_m"),
            "punched": p.map(|p| json!({"method":p.get::<String,_>("method"),"at":p.get::<chrono::DateTime<chrono::Utc>,_>("punched_at").to_rfc3339(),"ok":p.get::<bool,_>("ok"),"note":p.try_get::<String,_>("note").ok()}))})
    }).collect();
    let total = cps.len();
    let done = cpv.iter().filter(|c| c["punched"].get("ok").and_then(|v| v.as_bool())==Some(true)).count();
    Ok(json!({"id":rid,"route_id":route_id,"route_name":r.get::<String,_>("route_name"),"user_id":r.get::<Uuid,_>("user_id"),
        "started_at":r.get::<chrono::DateTime<chrono::Utc>,_>("started_at").to_rfc3339(),
        "ended_at":r.try_get::<chrono::DateTime<chrono::Utc>,_>("ended_at").ok().map(|d| d.to_rfc3339()),
        "status":r.get::<String,_>("status"),"window_minutes":r.try_get::<i32,_>("window_minutes").ok(),
        "checkpoints":cpv,"total":total,"done":done}))
}
pub async fn start_round(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let act = sqlx::query_scalar::<_,bool>("SELECT active FROM app.patrol_routes WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?;
    match act { None => return Err(err(StatusCode::NOT_FOUND,"giro inesistente")), Some(false) => return Err(err(StatusCode::BAD_REQUEST,"giro non attivo")), _ => {} }
    let row = sqlx::query("INSERT INTO app.patrol_rounds(tenant_id,route_id,user_id) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(rid).bind(s.user_id).fetch_one(&mut *tx).await.map_err(ise)?;
    let round_id = row.get::<Uuid,_>("id");
    let v = round_view(&mut tx, round_id).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(v))
}
pub async fn list_rounds(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // pianificatore vede tutte; altri vedono le proprie
    let rows = if is_planner(&s) {
        sqlx::query("SELECT ro.id,ro.route_id,ro.user_id,ro.started_at,ro.ended_at,ro.status,rt.name as route_name,u.email FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id JOIN app.users u ON u.id=ro.user_id ORDER BY ro.started_at DESC LIMIT 200").fetch_all(&mut *tx).await.map_err(ise)?
    } else {
        sqlx::query("SELECT ro.id,ro.route_id,ro.user_id,ro.started_at,ro.ended_at,ro.status,rt.name as route_name,u.email FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id JOIN app.users u ON u.id=ro.user_id WHERE ro.user_id=$1 ORDER BY ro.started_at DESC LIMIT 200").bind(s.user_id).fetch_all(&mut *tx).await.map_err(ise)?
    };
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"route_id":r.get::<Uuid,_>("route_id"),
        "route_name":r.get::<String,_>("route_name"),"email":r.get::<String,_>("email"),
        "started_at":r.get::<chrono::DateTime<chrono::Utc>,_>("started_at").to_rfc3339(),
        "ended_at":r.try_get::<chrono::DateTime<chrono::Utc>,_>("ended_at").ok().map(|d| d.to_rfc3339()),
        "status":r.get::<String,_>("status")})).collect();
    Ok(Json(json!(out)))
}
pub async fn get_round(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let v = round_view(&mut tx, id).await?;
    if !is_planner(&s) && v["user_id"].as_str() != Some(&s.user_id.to_string()) { return Err(err(StatusCode::FORBIDDEN, "non è la tua ronda")); }
    tx.commit().await.map_err(ise)?;
    Ok(Json(v))
}
#[derive(Deserialize)] pub struct PunchReq { pub checkpoint_id: Uuid, #[serde(default)] pub code: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>, #[serde(default)] pub note: Option<String> }
pub async fn punch(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PunchReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let round = sqlx::query("SELECT user_id,route_id,status FROM app.patrol_rounds WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"ronda inesistente"))?;
    if round.get::<Uuid,_>("user_id") != s.user_id && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "non è la tua ronda")); }
    if round.get::<String,_>("status") != "in_corso" { return Err(err(StatusCode::BAD_REQUEST, "ronda già chiusa")); }
    let route_id: Uuid = round.get("route_id");
    let cp = sqlx::query("SELECT kind,code,lat,lon,radius_m FROM app.patrol_checkpoints WHERE id=$1 AND route_id=$2")
        .bind(b.checkpoint_id).bind(route_id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST,"checkpoint non appartiene al giro"))?;
    let kind: String = cp.get("kind");
    // verifica per tipo
    let (ok, method, reason) = match kind.as_str() {
        "qr"|"nfc"|"beacon" => {
            let exp = cp.try_get::<String,_>("code").ok().unwrap_or_default();
            let got = b.code.clone().unwrap_or_default();
            (!exp.is_empty() && exp==got, kind.clone(), if exp==got {""} else {"codice non corrispondente"})
        }
        "gps" => {
            match (cp.try_get::<f64,_>("lat").ok(), cp.try_get::<f64,_>("lon").ok(), b.lat, b.lon) {
                (Some(clat),Some(clon),Some(ulat),Some(ulon)) => {
                    let d = haversine_m(clat,clon,ulat,ulon);
                    let rad = cp.get::<i32,_>("radius_m") as f64;
                    (d<=rad, "gps".to_string(), if d<=rad {""} else {"fuori raggio"})
                }
                _ => (false, "gps".to_string(), "posizione mancante")
            }
        }
        _ => (false, "manual".to_string(), "tipo ignoto")
    };
    sqlx::query("INSERT INTO app.patrol_punches(tenant_id,round_id,checkpoint_id,user_id,method,lat,lon,ok,note) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)
                 ON CONFLICT (round_id,checkpoint_id) DO UPDATE SET method=EXCLUDED.method,lat=EXCLUDED.lat,lon=EXCLUDED.lon,ok=EXCLUDED.ok,note=EXCLUDED.note,punched_at=now()")
        .bind(tid).bind(id).bind(b.checkpoint_id).bind(s.user_id).bind(&method).bind(b.lat).bind(b.lon).bind(ok).bind(b.note.as_deref())
        .execute(&mut *tx).await.map_err(ise)?;
    let v = round_view(&mut tx, id).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":ok,"reason":reason,"round":v})))
}
pub async fn close_round(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let round = sqlx::query("SELECT ro.user_id,ro.route_id,ro.started_at,ro.status,rt.window_minutes FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id WHERE ro.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"ronda inesistente"))?;
    if round.get::<Uuid,_>("user_id") != s.user_id && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "non è la tua ronda")); }
    if round.get::<String,_>("status") != "in_corso" { return Err(err(StatusCode::BAD_REQUEST, "ronda già chiusa")); }
    let route_id: Uuid = round.get("route_id");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM app.patrol_checkpoints WHERE route_id=$1").bind(route_id).fetch_one(&mut *tx).await.map_err(ise)?;
    let done: i64 = sqlx::query_scalar("SELECT count(*) FROM app.patrol_punches WHERE round_id=$1 AND ok=true").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    // finestra: se impostata, controlla che la chiusura sia entro i minuti dall'avvio
    let mut in_window = true;
    if let Ok(win) = round.try_get::<i32,_>("window_minutes") {
        let started: chrono::DateTime<chrono::Utc> = round.get("started_at");
        let elapsed = (chrono::Utc::now() - started).num_minutes();
        in_window = elapsed <= win as i64;
    }
    let status = if total>0 && done==total && in_window { "completato" } else { "incompleto" };
    sqlx::query("UPDATE app.patrol_rounds SET ended_at=$2, status=$3 WHERE id=$1")
        .bind(id).bind(chrono::Utc::now()).bind(status).execute(&mut *tx).await.map_err(ise)?;
    let v = round_view(&mut tx, id).await?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "close_round", json!({"round":id,"status":status,"done":done,"total":total})).await;
    Ok(Json(json!({"status":status,"in_window":in_window,"round":v})))
}

// ============================ Fase 2 — Forza lavoro + turni ============================
fn parse_ts(s: &str) -> Result<chrono::DateTime<chrono::Utc>, (StatusCode, String)> {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|_| err(StatusCode::BAD_REQUEST, "data/ora non valida (usa ISO 8601)"))
}
async fn need_workforce(st: &AppState, tid: Uuid) -> Result<(), (StatusCode, String)> {
    if !module_active(st, tid, "workforce").await { return Err(err(StatusCode::FORBIDDEN, "modulo workforce non abilitato")); }
    Ok(())
}

// ---------------- Squadre ----------------
pub async fn list_teams(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let teams = sqlx::query("SELECT id,name FROM app.teams ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    let mem = sqlx::query("SELECT m.team_id,m.user_id,u.email FROM app.team_members m JOIN app.users u ON u.id=m.user_id")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = teams.iter().map(|t| {
        let id: Uuid = t.get("id");
        let ms: Vec<Value> = mem.iter().filter(|m| m.get::<Uuid,_>("team_id")==id)
            .map(|m| json!({"user_id":m.get::<Uuid,_>("user_id"),"email":m.get::<String,_>("email")})).collect();
        json!({"id":id,"name":t.get::<String,_>("name"),"members":ms})
    }).collect();
    Ok(Json(json!(out)))
}
pub async fn create_team(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.teams(tenant_id,name) VALUES($1,$2) RETURNING id")
        .bind(tid).bind(b.name.trim()).fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "squadra gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id")})))
}
pub async fn delete_team(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.teams WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "squadra inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn add_team_member(State(st): State<AppState>, h: HeaderMap, Path(tid_p): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.team_members(tenant_id,team_id,user_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(tid_p).bind(b.user_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn remove_team_member(State(st): State<AppState>, h: HeaderMap, Path((team,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.team_members WHERE team_id=$1 AND user_id=$2").bind(team).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Turni pianificati ----------------
#[derive(Deserialize)]
pub struct ShiftPlanReq { pub user_id: Uuid, pub site_id: Option<Uuid>, pub starts_at: String, pub ends_at: String, pub note: Option<String> }
pub async fn create_shift_plan(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ShiftPlanReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.shift_plans(tenant_id,user_id,site_id,starts_at,ends_at,note,created_by) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.user_id).bind(b.site_id).bind(a).bind(z).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let iid: Uuid = row.get("id");
    audit(&st, tid, &s.email, "create_shift_plan", json!({"user":b.user_id})).await;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"shift_planned","id":iid})}).await;
    Ok(Json(json!({"id":iid})))
}
pub async fn list_shift_plans(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let from = q.get("from").and_then(|x| parse_ts(x).ok());
    let to = q.get("to").and_then(|x| parse_ts(x).ok());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // worker sees own; planner/admin/auditor see all
    let own = matches!(s.role.as_str(), "lavoratore"|"reperibile");
    let rows = sqlx::query(
        "SELECT p.id,p.user_id,p.site_id,p.starts_at,p.ends_at,p.note,p.state,u.email,si.name AS site
         FROM app.shift_plans p JOIN app.users u ON u.id=p.user_id LEFT JOIN app.sites si ON si.id=p.site_id
         WHERE ($1=false OR p.user_id=$2)
           AND ($3::timestamptz IS NULL OR p.ends_at>=$3)
           AND ($4::timestamptz IS NULL OR p.starts_at<=$4)
         ORDER BY p.starts_at")
        .bind(own).bind(s.user_id).bind(from).bind(to).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"user_id":r.get::<Uuid,_>("user_id"),"email":r.get::<String,_>("email"),
        "site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok(),
        "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
        "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at"),
        "note":r.try_get::<String,_>("note").ok(),"state":r.get::<String,_>("state") })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct ShiftPlanAction { pub action: String } // confirm|cancel
pub async fn shift_plan_action(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ShiftPlanAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = match b.action.as_str() {
        // il lavoratore conferma il PROPRIO turno pianificato
        "confirm" => sqlx::query("UPDATE app.shift_plans SET state='confermato' WHERE id=$1 AND user_id=$2 AND state='pianificato'")
            .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected(),
        // pianificatore/admin annulla
        "cancel" => { if !is_planner(&s) { let _=tx.rollback().await; return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
            sqlx::query("UPDATE app.shift_plans SET state='annullato' WHERE id=$1 AND state<>'annullato'")
                .bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected() }
        _ => { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, "azione non valida")); }
    };
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::CONFLICT, "turno non aggiornabile (stato o proprieta')")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_shift_plan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.shift_plans WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "turno inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---------------- Reperibilita' (on-call) ----------------
#[derive(Deserialize)]
pub struct OncallReq { pub user_id: Uuid, pub starts_at: String, pub ends_at: String, pub note: Option<String> }
pub async fn list_oncall(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "workforce").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT o.id,o.user_id,o.starts_at,o.ends_at,o.note,u.email FROM app.oncall o JOIN app.users u ON u.id=o.user_id ORDER BY o.starts_at")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"user_id":r.get::<Uuid,_>("user_id"),"email":r.get::<String,_>("email"),
        "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
        "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at"),"note":r.try_get::<String,_>("note").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_oncall(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OncallReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.oncall(tenant_id,user_id,starts_at,ends_at,note) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(b.user_id).bind(a).bind(z).bind(&b.note).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn delete_oncall(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.oncall WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "reperibilita' inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ----------------------------- ingress -----------------------------
#[derive(Deserialize)]
pub struct IngestReq {
    pub etype: String,
    pub source_event: String,
    pub idem_key: Option<String>,
    pub device_external_id: Option<String>,
    pub site_name: Option<String>,
    pub lat: Option<f64>, pub lon: Option<f64>, pub accuracy_m: Option<f64>,
    pub correlation: Option<String>,
    pub payload: Option<Value>,
}

/// Machine ingress. Tenant comes from the credential (X-Ingest-Token), never the body.
pub async fn ingest_event(State(st): State<AppState>, h: HeaderMap, Json(b): Json<IngestReq>) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, token).await
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    let source = cred.source.clone();
    // modular per-tenant: a disabled module refuses new ingress too (history stays readable)
    if !module_active(&st, tid, "loneworker").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo loneworker non abilitato"));
    }
    // Fase A: token device-bound -> il fix è attribuito a QUEL device (anti-spoof dell'external_id).
    let bound_ext = match cred.device_id { Some(did) => bound_device_external(&st, did).await, None => None };

    let idem = b.idem_key.clone().unwrap_or_else(|| b.source_event.clone());
    if idem.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "idem key required")); }
    let correlation = b.correlation.clone().unwrap_or_else(|| Uuid::new_v4().to_string());

    // Motore regole sensori: se il payload telemetria sfora una regola critica, l'etype
    // viene elevato (il dispositivo generico non deve decidere da solo la criticita').
    let mut etype = b.etype.clone();
    if let Some(p) = &b.payload {
        if let Some(e) = eval_sensor_rules(&st, tid, p).await {
            if CRITICAL.contains(&e.as_str()) { etype = e; }
        }
    }

    persist_ingest(&st, tid, Normalized {
        source,
        source_event: b.source_event.clone(),
        idem,
        etype,
        device_external_id: bound_ext.or_else(|| b.device_external_id.clone()),
        site_name: b.site_name.clone(),
        lat: b.lat, lon: b.lon, accuracy_m: b.accuracy_m,
        correlation,
        payload: b.payload.clone().unwrap_or(json!({})),
        indoor: false, beacon_uid: None, zone: None,
    }).await
}

/// A source-neutral normalized ingress. Every machine source (direct badge HTTP,
/// Traccar adapter, …) maps onto this shape so the SOS chain stays single-path.
pub struct Normalized {
    pub source: String,
    pub source_event: String,
    pub idem: String,
    pub etype: String,
    pub device_external_id: Option<String>,
    pub site_name: Option<String>,
    pub lat: Option<f64>, pub lon: Option<f64>, pub accuracy_m: Option<f64>,
    pub correlation: String,
    pub payload: Value,
    pub indoor: bool,
    pub beacon_uid: Option<String>,
    pub zone: Option<String>,
}

/// Estrae l'identificatore beacon dagli attributi di una posizione/evento Traccar.
/// Supporta: beaconId (stringa), major+minor (iBeacon), beacon (stringa generica).
pub fn extract_beacon_uid(attrs: &Value) -> Option<String> {
    if let Some(b) = attrs.get("beaconId").and_then(|v| v.as_str()) { return Some(b.to_string()); }
    let as_i = |v: &Value| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()));
    let major = attrs.get("major").and_then(as_i);
    let minor = attrs.get("minor").and_then(as_i);
    if let (Some(ma), Some(mi)) = (major, minor) { return Some(format!("{}-{}", ma, mi)); }
    if let Some(b) = attrs.get("beacon").and_then(|v| v.as_str()) { return Some(b.to_string()); }
    None
}
/// Risolve se un allarme e' INDOOR dai suoi attributi: se contiene un beacon lo marca
/// indoor e risolve la zona (e l'ancora sulla mappa) dalla tabella beacons del tenant.
/// Un beacon visto ma non ancora mappato resta indoor con zona ignota (l'admin lo registra).
pub async fn resolve_indoor(st: &AppState, tid: Uuid, attrs: Option<&Value>)
    -> (bool, Option<String>, Option<String>, Option<(f64,f64)>) {
    let Some(a) = attrs else { return (false, None, None, None); };
    let Some(uid) = extract_beacon_uid(a) else { return (false, None, None, None); };
    let row = sqlx::query("SELECT zone_name,lat,lon FROM app.beacons WHERE tenant_id=$1 AND beacon_uid=$2")
        .bind(tid).bind(&uid).fetch_optional(&st.owner_pool).await.ok().flatten();
    match row {
        Some(r) => {
            let zone: String = r.get("zone_name");
            let anchor = match (r.try_get::<f64,_>("lat").ok(), r.try_get::<f64,_>("lon").ok()) {
                (Some(la), Some(lo)) => Some((la, lo)), _ => None };
            (true, Some(uid), Some(zone), anchor)
        }
        None => (true, Some(uid), None, None),
    }
}

/// Persist a normalized event idempotently; open an incident + outbox notify
/// (one transaction, contract line 48) when the mapped etype is critical.
/// Registra un cambio di assegnazione device->worker nello storico (chiude l'aperta, apre la nuova).
/// No-op se l'assegnatario non cambia. Da chiamare dentro la stessa tx dell'UPDATE su devices.
async fn record_device_assignment(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, device_id: Uuid,
    old_user: Option<Uuid>, new_user: Option<Uuid>, at: chrono::DateTime<chrono::Utc>) -> Result<(), sqlx::Error> {
    if old_user == new_user { return Ok(()); }
    sqlx::query("UPDATE app.device_assignments SET unassigned_at=$3 WHERE tenant_id=$1 AND device_id=$2 AND unassigned_at IS NULL")
        .bind(tid).bind(device_id).bind(at).execute(&mut **tx).await?;
    if let Some(u) = new_user {
        sqlx::query("INSERT INTO app.device_assignments(tenant_id,device_id,user_id,assigned_at) VALUES($1,$2,$3,$4)")
            .bind(tid).bind(device_id).bind(u).bind(at).execute(&mut **tx).await?;
    }
    Ok(())
}
/// Attribuzione temporale: risolve il lavoratore assegnato al device all'ISTANTE `at`.
/// Ritorna (worker, incerta, nota). `origin_known` = l'orario deriva dal dispositivo (affidabile).
async fn resolve_worker_at(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, device_id: Uuid,
    at: chrono::DateTime<chrono::Utc>, origin_known: bool) -> (Option<Uuid>, bool, Option<String>) {
    let hist: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM app.device_assignments
          WHERE tenant_id=$1 AND device_id=$2 AND assigned_at<=$3 AND (unassigned_at IS NULL OR unassigned_at>$3)
          ORDER BY assigned_at DESC LIMIT 1")
        .bind(tid).bind(device_id).bind(at).fetch_optional(&mut **tx).await.ok().flatten();
    let current: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.devices WHERE id=$1")
        .bind(device_id).fetch_optional(&mut **tx).await.ok().flatten();
    let changes: i64 = sqlx::query_scalar("SELECT count(*) FROM app.device_assignments WHERE tenant_id=$1 AND device_id=$2")
        .bind(tid).bind(device_id).fetch_one(&mut **tx).await.unwrap_or(0);
    match hist {
        Some(u) => {
            if !origin_known && changes > 1 {
                (Some(u), true, Some("orario evento non fornito dal dispositivo e device con più assegnatari nello storico: attribuzione da verificare".into()))
            } else { (Some(u), false, None) }
        }
        None => {
            if current.is_some() {
                (current, true, Some("nessuna assegnazione valida all'orario dell'evento: usato l'assegnatario corrente".into()))
            } else {
                (None, false, None)
            }
        }
    }
}
/// Estrae un orario-evento affidabile dal payload (device/fix time) se presente.
fn payload_origin_time(p: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let keys = ["fixTime","deviceTime","eventTime","occurred_at","occurredAt","ts","time"];
    let try_obj = |o: &Value| -> Option<chrono::DateTime<chrono::Utc>> {
        for k in keys { if let Some(s) = o.get(k).and_then(|v| v.as_str()) {
            if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) { return Some(d.with_timezone(&chrono::Utc)); }
        }}
        None
    };
    try_obj(p)
        .or_else(|| p.get("attributes").and_then(try_obj))
        .or_else(|| p.get("position").and_then(try_obj))
}

pub async fn persist_ingest(st: &AppState, tid: Uuid, n: Normalized) -> R {
    // policy "ignora": evento non significativo per questo tenant -> nessuna persistenza.
    if n.etype == "ignore" { return Ok(Json(json!({"ignored":true,"correlation":n.correlation}))); }
    let now = st.clock.now();
    // Attribuzione temporale: se il payload porta un orario-evento affidabile (device/fix time)
    // lo usiamo come origin_ts; altrimenti ripieghiamo su `now` (evento considerato "live").
    let origin_opt = payload_origin_time(&n.payload);
    let origin_known = origin_opt.is_some();
    let origin_ts = origin_opt.unwrap_or(now);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;

    // resolve device/site within tenant (optional)
    let device_id: Option<Uuid> = if let Some(ext) = &n.device_external_id {
        sqlx::query_scalar("SELECT id FROM app.devices WHERE source=$1 AND external_id=$2")
            .bind(&n.source).bind(ext).fetch_optional(&mut *tx).await.map_err(ise)?
    } else { None };
    // site: explicit name wins; otherwise inherit the device's home site
    let mut site_id: Option<Uuid> = if let Some(sn) = &n.site_name {
        sqlx::query_scalar("SELECT id FROM app.sites WHERE name=$1")
            .bind(sn).fetch_optional(&mut *tx).await.map_err(ise)?
    } else { None };
    if site_id.is_none() {
        if let Some(did) = device_id {
            site_id = sqlx::query_scalar("SELECT site_id FROM app.devices WHERE id=$1")
                .bind(did).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
        }
    }

    // idempotent insert
    let ev = sqlx::query(
        "INSERT INTO app.events(tenant_id,source,source_event,idem_key,site_id,device_id,etype,origin_ts,received_ts,lat,lon,accuracy_m,payload_ref,correlation,indoor,beacon_uid,zone)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)
         ON CONFLICT (tenant_id,source,idem_key) DO NOTHING
         RETURNING id")
        .bind(tid).bind(&n.source).bind(&n.source_event).bind(&n.idem).bind(site_id).bind(device_id)
        .bind(&n.etype).bind(origin_ts).bind(now).bind(n.lat).bind(n.lon).bind(n.accuracy_m)
        .bind(n.payload.clone()).bind(&n.correlation).bind(n.indoor).bind(&n.beacon_uid).bind(&n.zone)
        .fetch_optional(&mut *tx).await.map_err(ise)?;

    let Some(ev) = ev else {
        tx.commit().await.map_err(ise)?;
        return Ok(Json(json!({"deduplicated":true,"correlation":n.correlation})));
    };
    let eid: Uuid = ev.get("id");

    let mut incident_id: Option<Uuid> = None;
    // Fase C gating: un profilo di protezione attivo può disabilitare questo trigger per il
    // lavoratore (risolto via assegnatario del device). Fail-open: senza profilo non sopprime.
    let prof_suppressed = trigger_suppressed(st, tid, None, device_id, &n.etype).await;
    if (CRITICAL.contains(&n.etype.as_str()) || n.etype.ends_with("_critico")) && prof_suppressed {
        tx.commit().await.map_err(ise)?;
        audit(st, tid, "system", "trigger_suppressed_by_profile", json!({"etype":n.etype,"device_id":device_id,"correlation":n.correlation})).await;
        return Ok(Json(json!({"event_id":eid,"suppressed_by_profile":true,"etype":n.etype,"correlation":n.correlation})));
    }
    if CRITICAL.contains(&n.etype.as_str()) || n.etype.ends_with("_critico") {
        // DEDUP esplicito (parità GoAlert): se esiste già un incidente APERTO con la stessa chiave
        // di dedup (correlation), NON se ne apre un altro — l'evento resta a trail e si annota sulla
        // timeline dell'incidente esistente. La correlation di default è casuale (nessun dedup
        // involontario); il dedup scatta solo quando il chiamante fornisce una chiave stabile.
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM app.incidents WHERE tenant_id=$1 AND correlation=$2 AND state<>'chiuso' ORDER BY opened_at DESC LIMIT 1")
            .bind(tid).bind(&n.correlation).fetch_optional(&mut *tx).await.map_err(ise)?;
        if let Some(exid) = existing {
            sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'dedup_suppressed',$3)")
                .bind(tid).bind(exid).bind(json!({"etype":n.etype,"source":n.source,"event_id":eid})).execute(&mut *tx).await.map_err(ise)?;
            tx.commit().await.map_err(ise)?;
            return Ok(Json(json!({"event_id":eid,"incident_id":exid,"deduplicated":true,"correlation":n.correlation})));
        }
        // SOS priority/repeat follow the per-tenant escalation policy; other criticals = 2.
        let (pri, repeat) = if n.etype=="sos" { sos_escalation(st, tid).await } else { (2, 0) };
        let irow = sqlx::query(
            "INSERT INTO app.incidents(tenant_id,event_id,site_id,etype,correlation,priority)
             VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
            .bind(tid).bind(eid).bind(site_id).bind(&n.etype).bind(&n.correlation)
            .bind(pri)
            .fetch_one(&mut *tx).await.map_err(ise)?;
        let iid: Uuid = irow.get("id");
        incident_id = Some(iid);
        // timeline + outbox notify request in the SAME transaction (contract line 48)
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_requested',$3)")
            .bind(tid).bind(iid).bind(json!({"etype":n.etype,"source":n.source,"priority":pri,"repeat_seconds":repeat})).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_notify',$2)")
            .bind(tid).bind(json!({
                "incident_id": iid, "summary": format!("{} [{}] @ {} (src:{})", n.etype, n.correlation, n.site_name.clone().unwrap_or("-".into()), n.source),
                "dedup": n.correlation, "integration_key": "", "repeat_seconds": repeat, "etype": n.etype,
                "details": format!("etype={}; sede={}; sorgente={}; lat={:?}; lon={:?}; correlazione={}",
                    n.etype, n.site_name.clone().unwrap_or("-".into()), n.source, n.lat, n.lon, n.correlation)}))
            .execute(&mut *tx).await.map_err(ise)?;
        // Attribuzione temporale del LAVORATORE: snapshot sull'incidente, risolto all'orario
        // dell'evento (origin_ts) via storico assegnazioni, con marcatura di incertezza.
        if let Some(did) = device_id {
            let (worker, uncertain, note) = resolve_worker_at(&mut tx, tid, did, origin_ts, origin_known).await;
            sqlx::query("UPDATE app.incidents SET worker_user_id=$2, attribution_uncertain=$3, attribution_note=$4 WHERE id=$1")
                .bind(iid).bind(worker).bind(uncertain).bind(&note).execute(&mut *tx).await.map_err(ise)?;
            if uncertain {
                sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'attribution_uncertain',$3)")
                    .bind(tid).bind(iid).bind(json!({"note":note,"origin_ts":origin_ts.to_rfc3339(),"origin_known":origin_known})).execute(&mut *tx).await.map_err(ise)?;
            }
        }
    }
    tx.commit().await.map_err(ise)?;

    if let Some(iid) = incident_id {
        publish_event(st, TenantEvent { tenant_id: tid, payload: json!({
            "type":"incident_opened","incident_id":iid,"etype":n.etype,"correlation":n.correlation}) }).await;
    }
    Ok(Json(json!({"event_id":eid,"incident_id":incident_id,"correlation":n.correlation})))
}

// ----------------------------- Traccar adapter (UPSTREAM REALE) -----------------------------
/// Receives Traccar *event forwarding* webhooks and maps them onto the same
/// normalized SOS chain. Tenant is derived from the ingest token (header
/// `X-Ingest-Token` or `?token=`), NEVER from the forwarded body. Traccar's
/// device `uniqueId` must match a registered device's external_id (source
/// `traccar`). Alarm mapping:
///   alarm=sos      -> sos       (critical)
///   alarm=fallDown -> man_down  (critical, lone-worker fall detection)
///   other alarm    -> device_alarm (recorded, non-critical)
///   deviceOffline  -> device_offline
///   else           -> traccar_event
pub async fn ingest_traccar(
    State(st): State<AppState>,
    h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
    Json(b): Json<Value>,
) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| q.get("token").cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, &token).await
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    let source = cred.source.clone();
    if !module_active(&st, tid, "traccar").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo traccar non abilitato"));
    }
    let bound_ext = match cred.device_id { Some(did) => bound_device_external(&st, did).await, None => None };

    let ev = b.get("event");
    let dev = b.get("device");
    let pos = b.get("position");

    let unique_id = dev.and_then(|d| d.get("uniqueId")).and_then(|v| v.as_str()).map(|s| s.to_string());
    let lat = pos.and_then(|p| p.get("latitude")).and_then(|v| v.as_f64());
    let lon = pos.and_then(|p| p.get("longitude")).and_then(|v| v.as_f64());
    let evt_type = ev.and_then(|e| e.get("type")).and_then(|v| v.as_str()).unwrap_or("");
    let alarm = ev.and_then(|e| e.get("attributes")).and_then(|a| a.get("alarm")).and_then(|v| v.as_str())
        .or_else(|| pos.and_then(|p| p.get("attributes")).and_then(|a| a.get("alarm")).and_then(|v| v.as_str()));
    let evt_id = ev.and_then(|e| e.get("id")).map(|v| v.to_string())
        .or_else(|| pos.and_then(|p| p.get("id")).map(|v| v.to_string()))
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Per-tenant mapping (traccar/alarm_map): sos routing, offline severity, soglia batteria.
    let amap = tool_config(&st, tid, "traccar", "alarm_map").await;
    // battery level dagli attributi (posizione o evento)
    let battery = pos.and_then(|p| p.get("attributes")).and_then(|a| a.get("batteryLevel")).and_then(|v| v.as_f64())
        .or_else(|| ev.and_then(|e| e.get("attributes")).and_then(|a| a.get("batteryLevel")).and_then(|v| v.as_f64()));
    let etype = map_traccar_etype(&amap, evt_type, alarm, battery);

    // Indoor/beacon: se la posizione (o l'evento) porta un beacon, l'allarme e' indoor
    // e lo localizziamo per zona. L'ancora del beacon vince sul GPS (in fabbrica inaffidabile).
    let attrs = pos.and_then(|p| p.get("attributes"))
        .or_else(|| ev.and_then(|e| e.get("attributes")));
    let (indoor, beacon_uid, zone, anchor) = resolve_indoor(&st, tid, attrs).await;
    let (lat, lon) = if indoor { if let Some((a,o)) = anchor { (Some(a), Some(o)) } else { (lat, lon) } } else { (lat, lon) };

    let source_event = format!("traccar-evt-{}", evt_id);
    persist_ingest(&st, tid, Normalized {
        source,
        source_event: source_event.clone(),
        idem: source_event.clone(),
        etype,
        device_external_id: bound_ext.or(unique_id),
        site_name: None,
        lat, lon, accuracy_m: None,
        correlation: source_event,
        payload: b.clone(),
        indoor, beacon_uid, zone,
    }).await
}

// ----------------------------- #2 Comandi al device (via API Traccar) -----------------------------
#[derive(Deserialize)]
pub struct DeviceCmd { #[serde(rename="type")] pub kind: String, pub data: Option<String> }
/// Inoltra un comando al device tramite l'API Traccar del tenant (es. richiesta posizione).
/// Risolve uniqueId -> id numerico Traccar, poi POST /api/commands/send. operatore/admin.
pub async fn device_command(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<DeviceCmd>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    // device (deve essere sorgente traccar)
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ext: Option<String> = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE id=$1 AND source='traccar'")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let ext = ext.ok_or_else(|| err(StatusCode::NOT_FOUND, "device traccar inesistente"))?;
    // connessione Traccar per-tenant
    let conn = sqlx::query("SELECT base_url,username,password FROM app.traccar_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione Traccar non configurata (superadmin)"))?;
    let base: String = conn.get("base_url"); let user: String = conn.get("username");
    let pass: String = st.secrets.decrypt(&conn.get::<String,_>("password"));   // P3
    // risolvi uniqueId -> id numerico
    let dl = st.http.get(format!("{base}/api/devices?uniqueId={ext}"))
        .basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar non raggiungibile: {e}")))?;
    if !dl.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, "Traccar: auth/lookup fallito")); }
    let arr: Value = dl.json().await.map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar: json device non valido"))?;
    let dev_id = arr.as_array().and_then(|a| a.first()).and_then(|d| d.get("id")).and_then(|v| v.as_i64())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "device non trovato su Traccar"))?;
    // invia comando
    let mut payload = json!({"deviceId": dev_id, "type": b.kind});
    if let Some(d) = &b.data { payload["attributes"] = json!({"data": d}); }
    let cr = st.http.post(format!("{base}/api/commands/send"))
        .basic_auth(&user, Some(&pass)).json(&payload).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar send: {e}")))?;
    let status = cr.status().as_u16();
    let body = cr.text().await.unwrap_or_default();
    // P4: storicizza il comando con esito
    let result = if (200..300).contains(&status) { "inviato".to_string() }
        else { format!("errore {}: {}", status, body.chars().take(140).collect::<String>()) };
    let _ = sqlx::query("INSERT INTO app.device_commands(tenant_id,device_id,ctype,actor,traccar_status,result) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(tid).bind(id).bind(&b.kind).bind(&s.email).bind(status as i32).bind(&result)
        .execute(&st.owner_pool).await;
    audit(&st, tid, &s.email, "device_command", json!({"device":id,"type":b.kind,"traccar_status":status})).await;
    // relay esito Traccar (anche se il device non supporta il comando)
    Ok(Json(json!({"resolved_device_id":dev_id,"traccar_status":status,"traccar_body":body,"result":result})))
}

/// P4: storico comandi inviati a un device (operatore/admin/auditor).
pub async fn list_device_commands(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let rows = sqlx::query("SELECT ctype,actor,traccar_status,result,created_at FROM app.device_commands
         WHERE tenant_id=$1 AND device_id=$2 ORDER BY created_at DESC LIMIT 50")
        .bind(tid).bind(id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "ctype": r.get::<String,_>("ctype"),
        "actor": r.get::<String,_>("actor"),
        "traccar_status": r.try_get::<i32,_>("traccar_status").ok(),
        "result": r.try_get::<String,_>("result").ok(),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
    })).collect();
    Ok(Json(json!(out)))
}

/// P4: tipi di comando supportati dal device secondo Traccar (per popolare la UI).
pub async fn device_command_types(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let ext: Option<String> = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE id=$1 AND tenant_id=$2 AND source='traccar'")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let ext = ext.ok_or_else(|| err(StatusCode::NOT_FOUND, "device traccar inesistente"))?;
    let (base,user,pass) = traccar_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione Traccar non configurata"))?;
    // risolvi id numerico
    let dl = st.http.get(format!("{base}/api/devices?uniqueId={ext}")).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let arr: Value = dl.json().await.unwrap_or(json!([]));
    let dev_id = arr.as_array().and_then(|a| a.first()).and_then(|d| d.get("id")).and_then(|v| v.as_i64());
    let types: Value = match dev_id {
        Some(did) => match st.http.get(format!("{base}/api/commands/types?deviceId={did}"))
            .basic_auth(&user, Some(&pass)).send().await {
            Ok(r) => r.json::<Value>().await.unwrap_or(json!([])),
            Err(_) => json!([]),
        },
        None => json!([]),
    };
    Ok(Json(types))
}

// ----------------------------- #3 Geofence + presenza -----------------------------
fn haversine_m(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    let r = 6_371_000.0_f64;
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dphi = (b_lat - a_lat).to_radians();
    let dlmb = (b_lon - a_lon).to_radians();
    let h = (dphi/2.0).sin().powi(2) + p1.cos()*p2.cos()*(dlmb/2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}
fn point_in_polygon(lat: f64, lon: f64, pts: &[[f64;2]]) -> bool {
    // ray casting; punti come [lat, lon]
    let mut inside = false; let n = pts.len();
    if n < 3 { return false; }
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = (pts[i][0], pts[i][1]);
        let (yj, xj) = (pts[j][0], pts[j][1]);
        if ((yi > lat) != (yj > lat)) && (lon < (xj - xi) * (lat - yi) / (yj - yi) + xi) { inside = !inside; }
        j = i;
    }
    inside
}
fn geofence_contains(geom: &Value, lat: f64, lon: f64) -> bool {
    if let Some(radius) = geom.get("radius").and_then(|v| v.as_f64()) {
        let (clat, clon) = (geom.get("lat").and_then(|v| v.as_f64()), geom.get("lon").and_then(|v| v.as_f64()));
        if let (Some(clat), Some(clon)) = (clat, clon) { return haversine_m(lat, lon, clat, clon) <= radius; }
        false
    } else if let Some(points) = geom.get("points").and_then(|v| v.as_array()) {
        let pts: Vec<[f64;2]> = points.iter().filter_map(|p| {
            let a = p.as_array()?; Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?])
        }).collect();
        point_in_polygon(lat, lon, &pts)
    } else { false }
}

pub async fn list_geofences(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "traccar").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT g.id,g.name,g.kind,g.geom,g.site_id,g.floorplan_id,g.color,si.name AS site
        FROM app.geofences g LEFT JOIN app.sites si ON si.id=g.site_id ORDER BY g.name").fetch_all(&mut *tx).await.map_err(ise)?;
    let gd = sqlx::query("SELECT geofence_id,device_id FROM app.geofence_devices").fetch_all(&mut *tx).await.map_err(ise)?;
    let ge = sqlx::query("SELECT geofence_id,etype FROM app.geofence_events").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| { let gid: Uuid = r.get("id");
        let devs: Vec<Uuid> = gd.iter().filter(|x| x.get::<Uuid,_>("geofence_id")==gid).map(|x| x.get::<Uuid,_>("device_id")).collect();
        let evs: Vec<String> = ge.iter().filter(|x| x.get::<Uuid,_>("geofence_id")==gid).map(|x| x.get::<String,_>("etype")).collect();
        json!({
        "id":gid,"name":r.get::<String,_>("name"),"kind":r.get::<String,_>("kind"),
        "geom":r.get::<Value,_>("geom"),"site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok(),
        "color":r.try_get::<String,_>("color").ok(),
        "floorplan_id":r.try_get::<Uuid,_>("floorplan_id").ok(),"device_ids":devs,"etypes":evs }) }).collect();
    Ok(Json(json!(out)))
}
fn clean_hex_color(c: Option<&str>) -> String {
    let c = c.unwrap_or("").trim();
    let ok = c.starts_with('#') && (c.len()==7 || c.len()==4) && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if ok { c.to_string() } else { "#16a34a".to_string() }
}
#[derive(Deserialize)]
pub struct GeofenceReq { pub name: String, pub kind: String, pub geom: Value, pub site_id: Option<Uuid>, pub floorplan_id: Option<Uuid>, pub color: Option<String> }
pub async fn create_geofence(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GeofenceReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    // indoor (geom in coordinate immagine): cerchio={cx,cy,radius}, poligono={points}
    let indoor = b.floorplan_id.is_some();
    match b.kind.as_str() {
        "circle" => {
            let has_geo = b.geom.get("radius").and_then(|v|v.as_f64()).is_some() && b.geom.get("lat").is_some();
            let has_xy  = b.geom.get("radius").and_then(|v|v.as_f64()).is_some() && b.geom.get("cx").is_some();
            if !(if indoor { has_xy } else { has_geo }) { return Err(err(StatusCode::BAD_REQUEST, "cerchio: coordinate/raggio richiesti")); }
        }
        "polygon" => { if b.geom.get("points").and_then(|v|v.as_array()).map(|a| a.len()<3).unwrap_or(true) { return Err(err(StatusCode::BAD_REQUEST, "poligono: almeno 3 punti")); } }
        _ => return Err(err(StatusCode::BAD_REQUEST, "kind non valido (circle|polygon)")),
    }
    let color = clean_hex_color(b.color.as_deref());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.geofences(tenant_id,name,kind,geom,site_id,floorplan_id,color) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.kind).bind(&b.geom).bind(b.site_id).bind(b.floorplan_id).bind(&color)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "nome gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id");
    audit(&st, tid, &s.email, "create_geofence", json!({"name":b.name,"kind":b.kind})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_geofence(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.geofences WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "geofence inesistente")); }
    audit(&st, s.tenant_id.unwrap(), &s.email, "delete_geofence", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}

// Etype associabili a un'area geofence (eventi rilevanti/monitorati nella zona).
pub const ASSOC_ETYPES: &[&str] = &["sos","man_down","immobilita","mancato_checkin","sensore_critico",
    "batteria_critica","batteria_bassa","perdita_alimentazione","device_offline","manomissione_badge",
    "jamming","presenza_ingresso","presenza_uscita","telemetria"];

#[derive(Deserialize)]
pub struct GeofenceUpdate { pub name: Option<String>, pub geom: Option<Value>, pub color: Option<String> }
/// Modifica nome, geometria e/o colore di una geofence (rinomina, sposta, ridimensiona, colore).
pub async fn update_geofence(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GeofenceUpdate>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let name = b.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    let color = b.color.as_deref().map(|c| clean_hex_color(Some(c)));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.geofences SET name=COALESCE($2,name), geom=COALESCE($3,geom), color=COALESCE($4,color) WHERE id=$1")
        .bind(id).bind(&name).bind(&b.geom).bind(&color).execute(&mut *tx).await
        .map_err(|_| err(StatusCode::CONFLICT, "nome gia' presente"))?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "geofence inesistente")); }
    audit(&st, tid, &s.email, "update_geofence", json!({"id":id,"renamed":name.is_some(),"moved":b.geom.is_some(),"recolored":color.is_some()})).await;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct GeofenceDevicesReq { pub device_ids: Vec<Uuid> }
/// Associa (sostituisce) i dispositivi IoT appartenenti all'area.
pub async fn set_geofence_devices(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GeofenceDevicesReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // la geofence deve esistere (RLS la limita al tenant)
    let ok: i64 = sqlx::query_scalar("SELECT count(*) FROM app.geofences WHERE id=$1").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    if ok==0 { let _=tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "geofence inesistente")); }
    sqlx::query("DELETE FROM app.geofence_devices WHERE geofence_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    for d in &b.device_ids {
        sqlx::query("INSERT INTO app.geofence_devices(tenant_id,geofence_id,device_id) VALUES($1,$2,$3)
            ON CONFLICT DO NOTHING").bind(tid).bind(id).bind(d).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "set_geofence_devices", json!({"id":id,"n":b.device_ids.len()})).await;
    Ok(Json(json!({"ok":true,"count":b.device_ids.len()})))
}

#[derive(Deserialize)]
pub struct GeofenceEventsReq { pub etypes: Vec<String> }
/// Associa (sostituisce) i tipi di evento rilevanti per l'area.
pub async fn set_geofence_events(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GeofenceEventsReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    for e in &b.etypes { if !ASSOC_ETYPES.contains(&e.as_str()) { return Err(err(StatusCode::BAD_REQUEST, &format!("etype non ammesso: {e}"))); } }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ok: i64 = sqlx::query_scalar("SELECT count(*) FROM app.geofences WHERE id=$1").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    if ok==0 { let _=tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "geofence inesistente")); }
    sqlx::query("DELETE FROM app.geofence_events WHERE geofence_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    for e in &b.etypes {
        sqlx::query("INSERT INTO app.geofence_events(tenant_id,geofence_id,etype) VALUES($1,$2,$3)
            ON CONFLICT DO NOTHING").bind(tid).bind(id).bind(e).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "set_geofence_events", json!({"id":id,"n":b.etypes.len()})).await;
    Ok(Json(json!({"ok":true,"count":b.etypes.len()})))
}
/// Presenza corrente: chi e' dentro quale geofence adesso.
pub async fn list_presence(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    require_module(&st, s.tenant_id.unwrap(), "traccar").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT p.device_id,p.geofence_id,p.inside,p.changed_at,d.label,d.external_id,u.email,g.name AS geofence
         FROM app.device_presence p JOIN app.devices d ON d.id=p.device_id JOIN app.geofences g ON g.id=p.geofence_id
         LEFT JOIN app.users u ON u.id=d.assigned_user WHERE p.inside=true ORDER BY g.name,d.external_id")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "device_id":r.get::<Uuid,_>("device_id"),"geofence_id":r.get::<Uuid,_>("geofence_id"),
        "geofence":r.get::<String,_>("geofence"),"device":r.try_get::<String,_>("label").ok(),
        "external_id":r.get::<String,_>("external_id"),"user":r.try_get::<String,_>("email").ok(),
        "since":r.get::<chrono::DateTime<chrono::Utc>,_>("changed_at") })).collect();
    Ok(Json(json!(out)))
}

// ----------------------------- #5 Report (percorso/soste/presenza) -----------------------------
fn rep_dates(q: &std::collections::HashMap<String,String>) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), (StatusCode,String)> {
    let to = q.get("to").and_then(|x| parse_ts(x).ok()).unwrap_or_else(chrono::Utc::now);
    let from = q.get("from").and_then(|x| parse_ts(x).ok()).unwrap_or(to - chrono::Duration::days(7));
    Ok((from, to))
}
fn report_role(s: &Session) -> Result<(), (StatusCode,String)> {
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor"|"responsabile") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    Ok(())
}
/// Report percorso: posizioni in intervallo -> distanza totale, durata, soste.
pub async fn report_route(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","traccar"]).await?;
    let dev = q.get("device").and_then(|x| Uuid::parse_str(x).ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "device richiesto"))?;
    let (from, to) = rep_dates(&q)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT lat,lon,speed,battery,fixed_at FROM app.device_positions
        WHERE device_id=$1 AND fixed_at BETWEEN $2 AND $3 ORDER BY fixed_at").bind(dev).bind(from).bind(to)
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let pts: Vec<(f64,f64,chrono::DateTime<chrono::Utc>)> = rows.iter().map(|r| (r.get("lat"),r.get("lon"),r.get("fixed_at"))).collect();
    let mut total = 0.0;
    for w in pts.windows(2) { total += haversine_m(w[0].0,w[0].1,w[1].0,w[1].1); }
    // soste: cluster di punti entro 50 m per >= 5 min
    let mut stops = Vec::new();
    let mut i = 0;
    while i < pts.len() {
        let anchor = pts[i]; let mut j = i;
        while j+1 < pts.len() && haversine_m(anchor.0,anchor.1,pts[j+1].0,pts[j+1].1) <= 50.0 { j += 1; }
        let mins = (pts[j].2 - anchor.2).num_minutes();
        if j > i && mins >= 5 {
            stops.push(json!({"lat":anchor.0,"lon":anchor.1,"from":anchor.2,"to":pts[j].2,"minutes":mins}));
            i = j + 1;
        } else { i += 1; }
    }
    let dur = pts.first().map(|f| (pts.last().unwrap().2 - f.2).num_minutes()).unwrap_or(0);
    Ok(Json(json!({"device":dev,"from":from,"to":to,"points":pts.len(),"total_m":total.round(),
        "duration_min":dur,"stops":stops})))
}
pub async fn report_route_csv(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    let dev = q.get("device").and_then(|x| Uuid::parse_str(x).ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "device richiesto"))?;
    let (from, to) = rep_dates(&q)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(|_| ise_resp2())?;
    let rows = sqlx::query("SELECT lat,lon,speed,battery,fixed_at FROM app.device_positions
        WHERE device_id=$1 AND fixed_at BETWEEN $2 AND $3 ORDER BY fixed_at").bind(dev).bind(from).bind(to)
        .fetch_all(&mut *tx).await.map_err(|_| ise_resp2())?;
    let _ = tx.commit().await;
    let mut csv = String::from("data_ora,lat,lon,velocita,batteria\n");
    for r in &rows {
        csv.push_str(&format!("{},{},{},{},{}\n",
            r.get::<chrono::DateTime<chrono::Utc>,_>("fixed_at").to_rfc3339(),
            r.get::<f64,_>("lat"), r.get::<f64,_>("lon"),
            r.try_get::<f64,_>("speed").map(|x|x.to_string()).unwrap_or_default(),
            r.try_get::<f64,_>("battery").map(|x|x.to_string()).unwrap_or_default()));
    }
    Ok(csv_response(csv, "percorso.csv"))
}
/// Report presenza: ingressi/uscite nelle geofence in intervallo.
pub async fn report_presence(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","traccar"]).await?;
    let (from, to) = rep_dates(&q)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT l.inside,l.at,d.external_id,d.label,g.name AS geofence,u.email
         FROM app.presence_log l JOIN app.devices d ON d.id=l.device_id JOIN app.geofences g ON g.id=l.geofence_id
         LEFT JOIN app.users u ON u.id=d.assigned_user
         WHERE l.at BETWEEN $1 AND $2 ORDER BY l.at DESC").bind(from).bind(to).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "at":r.get::<chrono::DateTime<chrono::Utc>,_>("at"),"inside":r.get::<bool,_>("inside"),
        "device":r.try_get::<String,_>("label").ok(),"external_id":r.get::<String,_>("external_id"),
        "geofence":r.get::<String,_>("geofence"),"user":r.try_get::<String,_>("email").ok() })).collect();
    Ok(Json(json!({"from":from,"to":to,"events":out})))
}
pub async fn report_presence_csv(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    let (from, to) = rep_dates(&q)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(|_| ise_resp2())?;
    let rows = sqlx::query(
        "SELECT l.inside,l.at,d.external_id,g.name AS geofence,u.email
         FROM app.presence_log l JOIN app.devices d ON d.id=l.device_id JOIN app.geofences g ON g.id=l.geofence_id
         LEFT JOIN app.users u ON u.id=d.assigned_user
         WHERE l.at BETWEEN $1 AND $2 ORDER BY l.at DESC").bind(from).bind(to).fetch_all(&mut *tx).await.map_err(|_| ise_resp2())?;
    let _ = tx.commit().await;
    let mut csv = String::from("data_ora,evento,dispositivo,geofence,assegnatario\n");
    for r in &rows {
        csv.push_str(&format!("{},{},{},{},{}\n",
            r.get::<chrono::DateTime<chrono::Utc>,_>("at").to_rfc3339(),
            if r.get::<bool,_>("inside") {"ingresso"} else {"uscita"},
            csv_esc(r.get::<String,_>("external_id").as_str()),
            csv_esc(r.get::<String,_>("geofence").as_str()),
            csv_esc(r.try_get::<String,_>("email").unwrap_or_default().as_str())));
    }
    Ok(csv_response(csv, "presenza.csv"))
}
fn ise_resp2() -> (StatusCode,String) { (StatusCode::INTERNAL_SERVER_ERROR, "db".into()) }
fn csv_response(csv: String, filename: &str) -> Response {
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
        (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))], csv).into_response()
}
fn text_response(body: String, filename: &str) -> Response {
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
        (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))], body).into_response()
}

// ============ Report Allarmi (DVR) — guidato dal modulo allarmi ============
// Quando il tenant ha un modulo che genera allarmi (loneworker/traccar), il report
// principale e' quello degli ALLARMI/incidenti, non percorsi/soste/presenze.
// Descrizioni leggibili per ogni tipo di allarme (DVR, operatori non tecnici).
pub fn etype_label(e: &str) -> &'static str {
    match e {
        "sos" => "SOS / richiesta soccorso",
        "man_down" => "Uomo a terra (man down)",
        "immobilita" => "Immobilita' prolungata",
        "mancato_checkin" => "Check-in mancato",
        "sensore_critico" => "Sensore critico",
        "manomissione_badge" => "Manomissione / rimozione badge",
        "perdita_alimentazione" => "Perdita alimentazione",
        "jamming" => "Jamming / disturbo del segnale",
        "perdita_contatto" => "Perdita contatto (badge silente)",
        "traccar_conn_down" => "Sistema di tracciamento irraggiungibile",
        "batteria_critica" => "Batteria in esaurimento (critico)",
        "batteria_bassa" => "Batteria bassa",
        _ => "Altro evento",
    }
}
struct AlarmRow {
    etype: String, state: String, priority: i32,
    opened_at: chrono::DateTime<chrono::Utc>,
    closed_at: Option<chrono::DateTime<chrono::Utc>>,
    ack_at: Option<chrono::DateTime<chrono::Utc>>,
    site: Option<String>, assignee: Option<String>, outcome: Option<String>,
}
impl AlarmRow {
    fn response_s(&self) -> Option<i64> { self.ack_at.map(|a| (a - self.opened_at).num_seconds().max(0)) }
    fn is_critical(&self) -> bool { CRITICAL.contains(&self.etype.as_str()) }
}
async fn alarms_rows(st: &AppState, s: &Session, q: &std::collections::HashMap<String,String>)
    -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Vec<AlarmRow>), (StatusCode,String)> {
    let tid = s.tenant_id.unwrap();
    let (from, to) = rep_dates(q)?;
    let etype_f = q.get("etype").filter(|x| !x.is_empty()).cloned();
    // stato: "chiuso" = chiusi, "aperto" = tutti i non-chiusi (include preso_in_carico)
    let state_cond = match q.get("state").map(|x| x.as_str()) {
        Some("chiuso") => " AND i.state='chiuso'",
        Some("aperto") => " AND i.state<>'chiuso'",
        _ => "",
    };
    let acl = allowed_sites(st, s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(|_| ise_resp2())?;
    let sql = format!(
        "SELECT i.etype,i.state,i.priority,i.opened_at,i.closed_at,i.outcome,i.site_id,
                s.name AS site, u.email AS assignee,
                (SELECT min(t.at) FROM app.incident_timeline t WHERE t.incident_id=i.id AND t.kind='human_ack') AS ack_at
         FROM app.incidents i
         LEFT JOIN app.sites s ON s.id=i.site_id
         LEFT JOIN app.users u ON u.id=i.assigned_user
         WHERE i.opened_at BETWEEN $1 AND $2
           AND ($3::text IS NULL OR i.etype=$3){}
         ORDER BY i.opened_at DESC", state_cond);
    let rows = sqlx::query(&sql)
        .bind(from).bind(to).bind(&etype_f)
        .fetch_all(&mut *tx).await.map_err(|_| ise_resp2())?;
    let _ = tx.commit().await;
    let out: Vec<AlarmRow> = rows.iter()
        .filter(|r| site_allowed(&acl, r.try_get::<Uuid,_>("site_id").ok()))
        .map(|r| AlarmRow{
            etype: r.get("etype"), state: r.get("state"), priority: r.get("priority"),
            opened_at: r.get("opened_at"), closed_at: r.try_get("closed_at").ok(),
            ack_at: r.try_get("ack_at").ok(),
            site: r.try_get::<String,_>("site").ok(),
            assignee: r.try_get::<String,_>("assignee").ok(),
            outcome: r.try_get::<String,_>("outcome").ok(),
        }).collect();
    Ok((from, to, out))
}
async fn tenant_name(st: &AppState, tid: Uuid) -> String {
    sqlx::query_scalar::<_,String>("SELECT name FROM app.tenants WHERE id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or_else(|| "—".into())
}
fn fmt_local(t: &chrono::DateTime<chrono::Utc>) -> String { t.format("%d/%m/%Y %H:%M:%S").to_string() }
fn fmt_resp(sec: Option<i64>) -> String {
    match sec { Some(s) if s >= 60 => format!("{}m {}s", s/60, s%60), Some(s) => format!("{}s", s), None => "—".into() }
}

/// JSON: report allarmi + sintesi (per la UI e per il PDF lato client).
pub async fn report_alarms(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","traccar"]).await?;
    let (from, to, rows) = alarms_rows(&st, &s, &q).await?;
    let total = rows.len();
    let aperti = rows.iter().filter(|r| r.state != "chiuso").count();
    let critici = rows.iter().filter(|r| r.is_critical()).count();
    let resp: Vec<i64> = rows.iter().filter_map(|r| r.response_s()).collect();
    let ack_medio_s = if resp.is_empty() { None } else { Some(resp.iter().sum::<i64>() / resp.len() as i64) };
    let presi = resp.len();
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "etype": r.etype, "label": etype_label(&r.etype), "state": r.state, "priority": r.priority,
        "opened_at": r.opened_at, "closed_at": r.closed_at, "ack_at": r.ack_at,
        "response_s": r.response_s(), "site": r.site, "assignee": r.assignee, "outcome": r.outcome,
    })).collect();
    Ok(Json(json!({
        "tenant": tenant_name(&st, s.tenant_id.unwrap()).await,
        "from": from, "to": to,
        "summary": {"totale": total, "aperti": aperti, "critici": critici, "presi_in_carico": presi, "ack_medio_s": ack_medio_s},
        "rows": out,
    })))
}
/// CSV: dati grezzi macchina-leggibili (colonna tecnica + descrizione).
pub async fn report_alarms_csv(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    let (_from, _to, rows) = alarms_rows(&st, &s, &q).await?;
    let mut csv = String::from("aperto_il,tipo,descrizione,stato,priorita,preso_in_carico_il,tempo_risposta_s,chiuso_il,sede,assegnatario,esito\n");
    for r in &rows {
        csv.push_str(&format!("{},{},{},{},{},{},{},{},{},{},{}\n",
            r.opened_at.to_rfc3339(),
            csv_esc(&r.etype), csv_esc(etype_label(&r.etype)),
            csv_esc(&r.state), r.priority,
            r.ack_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            r.response_s().map(|x| x.to_string()).unwrap_or_default(),
            r.closed_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            csv_esc(r.site.as_deref().unwrap_or("")),
            csv_esc(r.assignee.as_deref().unwrap_or("")),
            csv_esc(r.outcome.as_deref().unwrap_or(""))));
    }
    Ok(csv_response(csv, "allarmi.csv"))
}
/// TXT: report leggibile formattato (intestazione tenant + intervallo + sintesi).
pub async fn report_alarms_txt(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    let tid = s.tenant_id.unwrap();
    let (from, to, rows) = alarms_rows(&st, &s, &q).await?;
    let name = tenant_name(&st, tid).await;
    let total = rows.len();
    let aperti = rows.iter().filter(|r| r.state != "chiuso").count();
    let critici = rows.iter().filter(|r| r.is_critical()).count();
    let resp: Vec<i64> = rows.iter().filter_map(|r| r.response_s()).collect();
    let ack_medio = if resp.is_empty() { "—".to_string() } else { fmt_resp(Some(resp.iter().sum::<i64>()/resp.len() as i64)) };
    let mut t = String::new();
    t.push_str("========================================================\n");
    t.push_str("           REPORT ALLARMI — YouWorkforce\n");
    t.push_str("========================================================\n");
    t.push_str(&format!("Tenant       : {}\n", name));
    t.push_str(&format!("Periodo      : {} — {}\n", fmt_local(&from), fmt_local(&to)));
    t.push_str(&format!("Generato il  : {}\n", fmt_local(&st.clock.now())));
    t.push_str("--------------------------------------------------------\n");
    t.push_str(&format!("Totale allarmi: {}   Critici: {}   Aperti: {}\n", total, critici, aperti));
    t.push_str(&format!("Presi in carico: {}/{}   Tempo medio risposta: {}\n", resp.len(), total, ack_medio));
    t.push_str("========================================================\n\n");
    if rows.is_empty() { t.push_str("Nessun allarme nel periodo selezionato.\n"); }
    for (i, r) in rows.iter().enumerate() {
        t.push_str(&format!("#{:<3} {}  [{}]\n", i+1, etype_label(&r.etype), if r.is_critical() {"CRITICO"} else {"evento"}));
        t.push_str(&format!("     Aperto il   : {}\n", fmt_local(&r.opened_at)));
        t.push_str(&format!("     Preso in car.: {}   (risposta: {})\n",
            r.ack_at.map(|a| fmt_local(&a)).unwrap_or_else(|| "non preso in carico".into()), fmt_resp(r.response_s())));
        t.push_str(&format!("     Chiuso il   : {}\n", r.closed_at.map(|c| fmt_local(&c)).unwrap_or_else(|| "aperto".into())));
        t.push_str(&format!("     Stato       : {}   Sede: {}\n", r.state, r.site.as_deref().unwrap_or("—")));
        t.push_str(&format!("     Assegnato a : {}\n", r.assignee.as_deref().unwrap_or("—")));
        if let Some(o) = &r.outcome { if !o.is_empty() { t.push_str(&format!("     Esito       : {}\n", o)); } }
        t.push('\n');
    }
    Ok(text_response(t, "allarmi.txt"))
}

// ============ Mappa Allarmi — dove si sono verificati (indoor/outdoor) ============
/// Allarmi georeferenziati: per ognuno tipo, device, persona, e indoor(zona)/outdoor(GPS).
pub async fn alarms_map(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","traccar"]).await?;
    let tid = s.tenant_id.unwrap();
    let (from, to) = rep_dates(&q)?;
    let etype_f = q.get("etype").filter(|x| !x.is_empty()).cloned();
    let acl = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT i.id,i.etype,i.state,i.opened_at,i.site_id,
                e.lat,e.lon,e.accuracy_m,e.indoor,e.zone,e.beacon_uid,
                d.label AS device_label, d.external_id AS device_ext,
                u.email AS person, s.name AS site
         FROM app.incidents i
         JOIN app.events e ON e.id=i.event_id
         LEFT JOIN app.devices d ON d.id=e.device_id
         LEFT JOIN app.users u ON u.id=d.assigned_user
         LEFT JOIN app.sites s ON s.id=i.site_id
         WHERE i.opened_at BETWEEN $1 AND $2
           AND ($3::text IS NULL OR i.etype=$3)
         ORDER BY i.opened_at DESC LIMIT 2000")
        .bind(from).bind(to).bind(&etype_f)
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter()
        .filter(|r| site_allowed(&acl, r.try_get::<Uuid,_>("site_id").ok()))
        .map(|r| json!({
            "id": r.get::<Uuid,_>("id"), "etype": r.get::<String,_>("etype"),
            "label": etype_label(&r.get::<String,_>("etype")),
            "state": r.get::<String,_>("state"),
            "opened_at": r.get::<chrono::DateTime<chrono::Utc>,_>("opened_at"),
            "lat": r.try_get::<f64,_>("lat").ok(), "lon": r.try_get::<f64,_>("lon").ok(),
            "accuracy_m": r.try_get::<f64,_>("accuracy_m").ok(),
            "indoor": r.try_get::<bool,_>("indoor").unwrap_or(false),
            "zone": r.try_get::<String,_>("zone").ok(),
            "beacon_uid": r.try_get::<String,_>("beacon_uid").ok(),
            "device": r.try_get::<String,_>("device_label").ok()
                .or_else(|| r.try_get::<String,_>("device_ext").ok()),
            "person": r.try_get::<String,_>("person").ok(),
            "site": r.try_get::<String,_>("site").ok(),
        })).collect();
    Ok(Json(json!({"from":from,"to":to,"alarms":out})))
}

// ============ Beacon indoor — CRUD (admin configura tutto) ============
#[derive(Deserialize)]
pub struct BeaconReq {
    pub id: Option<Uuid>, pub beacon_uid: String, pub zone_name: String,
    pub site_id: Option<Uuid>, pub lat: Option<f64>, pub lon: Option<f64>, pub floor: Option<String>,
    pub floorplan_id: Option<Uuid>, pub x: Option<f64>, pub y: Option<f64>,
}
pub async fn list_beacons(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor"|"responsabile") {
        return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato"));
    }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT b.id,b.beacon_uid,b.zone_name,b.site_id,b.lat,b.lon,b.floor,b.floorplan_id,b.x,b.y,s.name AS site
         FROM app.beacons b LEFT JOIN app.sites s ON s.id=b.site_id ORDER BY b.zone_name")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "beacon_uid": r.get::<String,_>("beacon_uid"),
        "zone_name": r.get::<String,_>("zone_name"),
        "site_id": r.try_get::<Uuid,_>("site_id").ok(), "site": r.try_get::<String,_>("site").ok(),
        "lat": r.try_get::<f64,_>("lat").ok(), "lon": r.try_get::<f64,_>("lon").ok(),
        "floor": r.try_get::<String,_>("floor").ok(),
        "floorplan_id": r.try_get::<Uuid,_>("floorplan_id").ok(),
        "x": r.try_get::<f64,_>("x").ok(), "y": r.try_get::<f64,_>("y").ok(),
    })).collect();
    Ok(Json(json!(out)))
}
pub async fn save_beacon(State(st): State<AppState>, h: HeaderMap, Json(b): Json<BeaconReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo tenant_admin")); }
    if b.beacon_uid.trim().is_empty() || b.zone_name.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "beacon_uid e zona richiesti"));
    }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // upsert per (tenant, beacon_uid): un beacon = una zona
    let row = sqlx::query(
        "INSERT INTO app.beacons(tenant_id,beacon_uid,zone_name,site_id,lat,lon,floor,floorplan_id,x,y)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
         ON CONFLICT (tenant_id,beacon_uid) DO UPDATE SET
            zone_name=EXCLUDED.zone_name, site_id=EXCLUDED.site_id,
            lat=EXCLUDED.lat, lon=EXCLUDED.lon, floor=EXCLUDED.floor,
            floorplan_id=EXCLUDED.floorplan_id, x=EXCLUDED.x, y=EXCLUDED.y
         RETURNING id")
        .bind(tid).bind(b.beacon_uid.trim()).bind(b.zone_name.trim())
        .bind(b.site_id).bind(b.lat).bind(b.lon).bind(b.floor.as_deref())
        .bind(b.floorplan_id).bind(b.x).bind(b.y)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "beacon_save", json!({"beacon_uid":b.beacon_uid,"zone":b.zone_name})).await;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id")})))
}
pub async fn delete_beacon(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo tenant_admin")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.beacons WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "beacon inesistente")); }
    audit(&st, tid, &s.email, "beacon_delete", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}

// ============ Piantine indoor — upload immagine + CRUD ============
pub async fn list_floorplans(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor"|"responsabile") {
        return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato"));
    }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT f.id,f.name,f.site_id,f.width,f.height,f.content_type,s.name AS site
        FROM app.floorplans f LEFT JOIN app.sites s ON s.id=f.site_id ORDER BY f.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "name": r.get::<String,_>("name"),
        "site_id": r.try_get::<Uuid,_>("site_id").ok(), "site": r.try_get::<String,_>("site").ok(),
        "width": r.get::<i32,_>("width"), "height": r.get::<i32,_>("height"),
        "content_type": r.get::<String,_>("content_type"),
    })).collect();
    Ok(Json(json!(out)))
}
/// Upload piantina: body = bytes immagine (PDF rasterizzato a PNG lato client).
/// Metadati in query (name, width, height, site_id); content-type dall'header.
pub async fn create_floorplan(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
    body: axum::body::Bytes) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo tenant_admin")); }
    let tid = s.tenant_id.unwrap();
    let name = q.get("name").map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "nome richiesto"))?;
    let width: i32 = q.get("width").and_then(|x| x.parse().ok()).unwrap_or(0);
    let height: i32 = q.get("height").and_then(|x| x.parse().ok()).unwrap_or(0);
    if width <= 0 || height <= 0 { return Err(err(StatusCode::BAD_REQUEST, "dimensioni immagine richieste")); }
    if body.len() < 10 { return Err(err(StatusCode::BAD_REQUEST, "immagine vuota")); }
    if body.len() > 20_000_000 { return Err(err(StatusCode::BAD_REQUEST, "immagine troppo grande (max 20MB)")); }
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("image/png").to_string();
    let site_id = q.get("site_id").and_then(|x| Uuid::parse_str(x).ok());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.floorplans(tenant_id,name,site_id,content_type,width,height,image)
        VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(&name).bind(site_id).bind(&ct).bind(width).bind(height).bind(body.as_ref())
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "floorplan_create", json!({"name":name,"bytes":body.len()})).await;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id")})))
}
pub async fn get_floorplan_image(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>)
    -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("SELECT image,content_type FROM app.floorplans WHERE id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let _ = tx.commit().await;
    let row = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "piantina inesistente"))?;
    let img: Vec<u8> = row.get("image"); let ct: String = row.get("content_type");
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "private, max-age=3600".to_string())], img).into_response())
}
#[derive(Deserialize)]
pub struct FloorplanPatch { pub name: Option<String>, pub site_id: Option<Uuid> }
/// Rinomina / riassegna sede di una piantina (l'immagine resta). tenant_admin.
pub async fn update_floorplan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<FloorplanPatch>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo tenant_admin")); }
    let tid = s.tenant_id.unwrap();
    let name = b.name.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.floorplans SET name=COALESCE($2,name), site_id=$3 WHERE id=$1")
        .bind(id).bind(name).bind(b.site_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_floorplan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo tenant_admin")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.floorplans WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "inesistente")); }
    audit(&st, tid, &s.email, "floorplan_delete", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
/// Dispositivi la cui ultima posizione INDOOR (24h) cade su un beacon di questa piantina.
/// Serve a mostrare sulla mappa indoor i device che si trovano al chiuso (senza GPS).
pub async fn floorplan_devices(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    let tid = s.tenant_id.unwrap();
    let acl = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT DISTINCT ON (e.device_id) e.device_id, e.etype, e.origin_ts, e.site_id,
                b.x, b.y, b.zone_name, d.label AS device_label, d.external_id, u.email AS person
         FROM app.events e
         JOIN app.beacons b ON b.beacon_uid=e.beacon_uid AND b.floorplan_id=$1 AND b.x IS NOT NULL
         JOIN app.devices d ON d.id=e.device_id
         LEFT JOIN app.users u ON u.id=d.assigned_user
         WHERE e.indoor=true AND e.device_id IS NOT NULL AND e.origin_ts > now() - interval '24 hours'
         ORDER BY e.device_id, e.origin_ts DESC")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter()
        .filter(|r| site_allowed(&acl, r.try_get::<Uuid,_>("site_id").ok()))
        .map(|r| json!({
            "device": r.try_get::<String,_>("device_label").ok().or_else(|| r.try_get::<String,_>("external_id").ok()),
            "person": r.try_get::<String,_>("person").ok(),
            "zone": r.get::<String,_>("zone_name"),
            "x": r.try_get::<f64,_>("x").ok(), "y": r.try_get::<f64,_>("y").ok(),
            "at": r.get::<chrono::DateTime<chrono::Utc>,_>("origin_ts"),
            "last_etype": r.get::<String,_>("etype"),
        })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct BeaconPlace { pub floorplan_id: Option<Uuid>, pub x: Option<f64>, pub y: Option<f64> }
/// Posiziona un beacon GIA' REGISTRATO su una piantina (o lo rimuove con floorplan_id null).
/// Non crea beacon: aggiorna solo la posizione di uno esistente del tenant. tenant_admin.
pub async fn place_beacon(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<BeaconPlace>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo tenant_admin")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.beacons SET floorplan_id=$2, x=$3, y=$4 WHERE id=$1")
        .bind(id).bind(b.floorplan_id).bind(b.x).bind(b.y).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "beacon inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// Posiziona (o rimuove: floorplan_id=null) un DISPOSITIVO del tenant su una piantina, in pixel.
pub async fn place_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<BeaconPlace>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo tenant_admin")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.devices SET floorplan_id=$2, fp_x=$3, fp_y=$4 WHERE id=$1")
        .bind(id).bind(b.floorplan_id).bind(b.x).bind(b.y).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ----------------------------- #4 Geocoding inverso (Nominatim/OSM) -----------------------------
/// Indirizzo da coordinate. Cache globale (coordinate arrotondate) per rispettare
/// la usage policy di Nominatim. Richiede sessione tenant.
pub async fn reverse_geocode(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let _ = require_tenant(&st, &h).await?;
    let lat: f64 = q.get("lat").and_then(|x| x.parse().ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "lat mancante"))?;
    let lon: f64 = q.get("lon").and_then(|x| x.parse().ok()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "lon mancante"))?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) { return Err(err(StatusCode::BAD_REQUEST, "coordinate fuori range")); }
    let (lk, ok) = (format!("{:.4}", lat), format!("{:.4}", lon));
    if let Some(name) = sqlx::query_scalar::<_, String>("SELECT display_name FROM app.geocode_cache WHERE lat_key=$1::numeric AND lon_key=$2::numeric")
        .bind(&lk).bind(&ok).fetch_optional(&st.owner_pool).await.map_err(ise)? {
        return Ok(Json(json!({"address":name,"cached":true})));
    }
    let url = format!("https://nominatim.openstreetmap.org/reverse?format=jsonv2&lat={lat}&lon={lon}&zoom=18&addressdetails=0");
    let resp = st.http.get(&url)
        .header("User-Agent", "YouWorkforce/1.0 (lone-worker safety platform)")
        .header("Accept-Language", "it")
        .send().await.map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("geocoder non raggiungibile: {e}")))?;
    if !resp.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, "geocoder: risposta non valida")); }
    let body: Value = resp.json().await.map_err(|_| err(StatusCode::BAD_GATEWAY, "geocoder: json non valido"))?;
    let name = body.get("display_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if name.is_empty() { return Ok(Json(json!({"address":null,"cached":false}))); }
    let _ = sqlx::query("INSERT INTO app.geocode_cache(lat_key,lon_key,display_name) VALUES($1::numeric,$2::numeric,$3) ON CONFLICT DO NOTHING")
        .bind(&lk).bind(&ok).bind(&name).execute(&st.owner_pool).await;
    Ok(Json(json!({"address":name,"cached":false})))
}

// ----------------------------- Traccar position forwarding (P2) -----------------------------
/// Riceve il *position forwarding* di Traccar ({position, device}) e aggiorna
/// l'ultima posizione nota del device + lo storico. Tenant dal token di ingest.
/// Alta frequenza: idempotente per id posizione, nessun incidente qui.
pub async fn ingest_traccar_position(
    State(st): State<AppState>,
    h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
    Json(b): Json<Value>,
) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| q.get("token").cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, &token).await
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    if !module_active(&st, tid, "traccar").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo traccar non abilitato"));
    }
    let pos = b.get("position").or(Some(&b)).unwrap();
    let dev = b.get("device");
    // Fase A: token device-bound -> external_id forzato (il payload non può spacciarsi per un altro device).
    let bound_ext = match cred.device_id { Some(did) => bound_device_external(&st, did).await, None => None };
    let unique_id = bound_ext.as_deref()
        .or_else(|| dev.and_then(|d| d.get("uniqueId")).and_then(|v| v.as_str()))
        .or_else(|| q.get("id").map(|s| s.as_str()));
    let lat = pos.get("latitude").and_then(|v| v.as_f64());
    let lon = pos.get("longitude").and_then(|v| v.as_f64());
    if lat.is_none() || lon.is_none() { return Err(err(StatusCode::BAD_REQUEST, "posizione senza coordinate")); }
    let Some(ext) = unique_id else { return Err(err(StatusCode::BAD_REQUEST, "device uniqueId mancante")); };
    let out = process_traccar_position(&st, tid, pos, ext).await.map_err(ise)?;
    Ok(Json(out))
}

/// Ingest NATIVO OsmAnd/HTTP (senza server Traccar): smartphone e tracker che parlano il
/// protocollo OsmAnd inviano posizioni e allarmi via HTTP (GET/POST con query string).
/// Autenticazione con token device-bound (anti-spoof, come gli altri ingress). Riusa la pipeline
/// nativa: process_traccar_position (posizione/presenza) + persist_ingest (allarme->incidente),
/// con orario-evento affidabile (timestamp del device) per l'attribuzione temporale.
pub async fn ingest_osmand(
    State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| q.get("token").cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, &token).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    // gating: almeno un modulo dispositivi attivo sul tenant
    let mut mod_ok=false; for m in DEVICE_MODS { if module_active(&st, tid, m).await { mod_ok=true; break; } }
    if !mod_ok { return Err(err(StatusCode::FORBIDDEN, "nessun modulo dispositivi attivo")); }
    // device-bound: external_id forzato (il payload non può spacciarsi per un altro device)
    let ext = match cred.device_id { Some(did) => bound_device_external(&st, did).await, None => None }
        .or_else(|| q.get("id").cloned()).or_else(|| q.get("deviceid").cloned())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "device id mancante"))?;
    // sorgente reale del device (per far combaciare la risoluzione in persist_ingest/position)
    let dev_source: String = match cred.device_id {
        Some(did) => sqlx::query_scalar("SELECT source FROM app.devices WHERE id=$1").bind(did).fetch_optional(&st.owner_pool).await.map_err(ise)?.unwrap_or_else(|| "traccar".into()),
        None => "traccar".into(),
    };
    let getf = |keys: &[&str]| -> Option<f64> { for k in keys { if let Some(v)=q.get(*k){ if let Ok(x)=v.parse::<f64>(){ return Some(x); } } } None };
    let lat = getf(&["lat","latitude"]); let lon = getf(&["lon","longitude"]);
    let battery = getf(&["batt","battery","batteryLevel"]);
    let speed = getf(&["speed"]); let course = getf(&["bearing","heading","course"]);
    let accuracy = getf(&["accuracy","hdop"]);
    // timestamp -> RFC3339 (accetta unix secondi o ISO)
    let ts_rfc = q.get("timestamp").or_else(|| q.get("time")).and_then(|s| {
        if let Ok(secs)=s.parse::<i64>() { chrono::DateTime::<chrono::Utc>::from_timestamp(secs,0).map(|d| d.to_rfc3339()) }
        else { chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&chrono::Utc).to_rfc3339()) }
    });
    let alarm = q.get("alarm").map(|s| s.to_string());
    // 1) posizione (pipeline nativa) se coordinate presenti
    let mut position = json!({"skipped":"no_coords"});
    if let (Some(la),Some(lo)) = (lat,lon) {
        let pos = json!({"latitude":la,"longitude":lo,"speed":speed,"course":course,
            "attributes":{"batteryLevel":battery,"accuracy":accuracy},
            "id": format!("osmand-{}-{}", ext, ts_rfc.clone().unwrap_or_default()),
            "fixTime": ts_rfc.clone(), "source": dev_source});
        // process_traccar_position filtra source='traccar': per device con quella sorgente aggiorna,
        // altrimenti ignora (la posizione resta comunque nel payload dell'eventuale allarme).
        if dev_source=="traccar" { position = process_traccar_position(&st, tid, &pos, &ext).await.map_err(ise)?; }
        else { position = json!({"skipped":"source_non_traccar"}); }
    }
    // 2) allarme -> incidente (etype mappato; orario affidabile -> attribuzione temporale)
    let mut incident = json!(null);
    if let Some(al) = alarm.as_deref() {
        let amap = tool_config(&st, tid, "traccar", "alarm_map").await;
        let etype = map_traccar_etype(&amap, "", Some(al), battery);
        let corr = format!("osmand-{}-{}", ext, &Uuid::new_v4().to_string()[..8]);
        let src_evt = format!("osmand-evt-{}", Uuid::new_v4());
        let payload = json!({"channel":"osmand","alarm":al,"lat":lat,"lon":lon,"battery":battery,"occurred_at":ts_rfc});
        let r = persist_ingest(&st, tid, Normalized{
            source: dev_source.clone(), source_event: src_evt.clone(), idem: src_evt,
            etype, device_external_id: Some(ext.clone()), site_name:None,
            lat, lon, accuracy_m: accuracy, correlation: corr, payload,
            indoor:false, beacon_uid:None, zone:None }).await?;
        incident = r.0;
    }
    Ok(Json(json!({"ok":true,"external_id":ext,"position":position,"incident":incident})))
}

// ---- Teltonika edge nativo (Codec8/8E su TCP): helper d'ingest riusabili ----
/// Risolve device+tenant+source dall'IMEI (= external_id). owner_pool (bypassa RLS).
pub async fn teltonika_resolve(st: &AppState, imei: &str) -> Option<(Uuid, Uuid, String)> {
    let row = sqlx::query("SELECT id, tenant_id, source FROM app.devices WHERE external_id=$1 LIMIT 1")
        .bind(imei).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    Some((row.get::<Uuid,_>("tenant_id"), row.get::<Uuid,_>("id"),
          row.try_get::<String,_>("source").unwrap_or_else(|_| "traccar".into())))
}
/// Almeno un modulo dispositivi attivo sul tenant?
pub async fn any_device_module(st: &AppState, tid: Uuid) -> bool {
    for m in DEVICE_MODS { if module_active(st, tid, m).await { return true; } }
    false
}
/// Ingest di un record AVL Teltonika decodificato: posizione (pipeline nativa) + eventuale allarme.
#[allow(clippy::too_many_arguments)]
pub async fn teltonika_ingest_one(st: &AppState, tid: Uuid, ext: &str, source: &str, ts_rfc: &str,
    lat: f64, lon: f64, speed: Option<f64>, course: Option<f64>, battery: Option<f64>, alarm: Option<String>,
    io_attrs: Value) {
    if source == "traccar" {
        let mut attrs = match io_attrs { Value::Object(m) => m, _ => serde_json::Map::new() };
        attrs.insert("batteryLevel".into(), json!(battery));
        let pos = json!({"latitude":lat,"longitude":lon,"speed":speed,"course":course,
            "attributes": Value::Object(attrs),
            "id": format!("telt-{}-{}", ext, ts_rfc), "fixTime": ts_rfc, "source": source});
        let _ = process_traccar_position(st, tid, &pos, ext).await;
    }
    if let Some(al) = alarm {
        let amap = tool_config(st, tid, "traccar", "alarm_map").await;
        let etype = map_traccar_etype(&amap, "", Some(&al), battery);
        let src_evt = format!("telt-evt-{}", Uuid::new_v4());
        let corr = format!("telt-{}-{}", ext, &Uuid::new_v4().to_string()[..8]);
        let _ = persist_ingest(st, tid, Normalized{
            source: source.into(), source_event: src_evt.clone(), idem: src_evt, etype,
            device_external_id: Some(ext.into()), site_name: None,
            lat: Some(lat), lon: Some(lon), accuracy_m: None, correlation: corr,
            payload: json!({"channel":"teltonika","alarm":al,"lat":lat,"lon":lon,"battery":battery,"occurred_at":ts_rfc}),
            indoor: false, beacon_uid: None, zone: None }).await;
    }
}

/// Pipeline posizione condivisa da webhook (push) e poller API (pull): filtri #7,
/// storico, ultima posizione, presenza geofence #3, primo-fix->attivo. Idempotente
/// per (tenant,device,source_id) cosi' push e pull convergono senza duplicare.
pub async fn process_traccar_position(st: &AppState, tid: Uuid, pos: &Value, ext: &str) -> anyhow::Result<Value> {
    let (Some(lat), Some(lon)) = (pos.get("latitude").and_then(|v| v.as_f64()), pos.get("longitude").and_then(|v| v.as_f64()))
        else { return Ok(json!({"ignored":"no_coords"})); };
    let speed = pos.get("speed").and_then(|v| v.as_f64());
    let course = pos.get("course").and_then(|v| v.as_f64());
    let battery = pos.get("attributes").and_then(|a| a.get("batteryLevel")).and_then(|v| v.as_f64());
    let src_id = pos.get("id").map(|v| v.to_string());
    let fixed_at = pos.get("fixTime").and_then(|v| v.as_str())
        .or_else(|| pos.get("deviceTime").and_then(|v| v.as_str()))
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| st.clock.now());
    // #7 filtri posizione (config per-tenant)
    let fcfg = tool_config(st, tid, "traccar", "alarm_map").await;
    let max_jump = fcfg.get("max_jump_kmh").and_then(|v| v.as_f64()).unwrap_or(300.0);
    let min_move = fcfg.get("min_move_m").and_then(|v| v.as_f64()).unwrap_or(5.0);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await?;
    let drow = sqlx::query("SELECT id,last_lat,last_lon,last_pos_at FROM app.devices WHERE source='traccar' AND external_id=$1")
        .bind(ext).fetch_optional(&mut *tx).await?;
    let Some(drow) = drow else {
        tx.commit().await?;
        return Ok(json!({"ignored":"device non registrato","external_id":ext}));
    };
    let dev_id: Uuid = drow.get("id");
    // confronto con l'ultima posizione nota per scartare salti/duplicati
    if let (Some(plat), Some(plon), Some(pat)) = (drow.try_get::<f64,_>("last_lat").ok(), drow.try_get::<f64,_>("last_lon").ok(), drow.try_get::<chrono::DateTime<chrono::Utc>,_>("last_pos_at").ok()) {
        let dist = haversine_m(plat, plon, lat, lon);
        let dt = (fixed_at - pat).num_seconds();
        if max_jump > 0.0 && dt > 0 {
            let kmh = dist / dt as f64 * 3.6;
            if kmh > max_jump {
                tx.commit().await?;
                return Ok(json!({"filtered":"salto implausibile","speed_kmh":kmh.round()}));
            }
        }
        if min_move > 0.0 && dist < min_move && dt >= 0 && dt < 300 {
            tx.commit().await?;
            return Ok(json!({"filtered":"spostamento sotto soglia","dist_m":dist.round()}));
        }
    }
    let dedup = sqlx::query(
        "INSERT INTO app.device_positions(tenant_id,device_id,source_id,lat,lon,speed,course,battery,attributes,fixed_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
         ON CONFLICT (tenant_id,device_id,source_id) DO NOTHING RETURNING id")
        .bind(tid).bind(dev_id).bind(&src_id).bind(lat).bind(lon).bind(speed).bind(course).bind(battery)
        .bind(pos.get("attributes").cloned()).bind(fixed_at)
        .fetch_optional(&mut *tx).await?;
    // aggiorna l'ultima posizione solo se piu' recente
    sqlx::query(
        "UPDATE app.devices SET last_lat=$2,last_lon=$3,last_speed=$4,last_course=$5,last_battery=COALESCE($6,last_battery),
            last_attributes=COALESCE($8,last_attributes),last_pos_at=$7,updated_at=now()
         WHERE id=$1 AND (last_pos_at IS NULL OR last_pos_at <= $7)")
        .bind(dev_id).bind(lat).bind(lon).bind(speed).bind(course).bind(battery).bind(fixed_at)
        .bind(pos.get("attributes").cloned())
        .execute(&mut *tx).await?;
    // #3 presenza: valuta le geofence del tenant e registra i cambi dentro/fuori
    let mut presence_changes: Vec<(Uuid, bool)> = Vec::new();
    let gfs = sqlx::query("SELECT id,geom FROM app.geofences").fetch_all(&mut *tx).await?;
    for g in gfs {
        let gid: Uuid = g.get("id");
        let inside = geofence_contains(&g.get::<Value,_>("geom"), lat, lon);
        let prev: Option<bool> = sqlx::query_scalar("SELECT inside FROM app.device_presence WHERE device_id=$1 AND geofence_id=$2")
            .bind(dev_id).bind(gid).fetch_optional(&mut *tx).await?;
        if prev != Some(inside) {
            sqlx::query("INSERT INTO app.device_presence(tenant_id,device_id,geofence_id,inside) VALUES($1,$2,$3,$4)
                ON CONFLICT (device_id,geofence_id) DO UPDATE SET inside=EXCLUDED.inside, changed_at=now()")
                .bind(tid).bind(dev_id).bind(gid).bind(inside).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO app.presence_log(tenant_id,device_id,geofence_id,inside,at) VALUES($1,$2,$3,$4,$5)")
                .bind(tid).bind(dev_id).bind(gid).bind(inside).bind(fixed_at).execute(&mut *tx).await?;
            presence_changes.push((gid, inside));
        }
    }
    tx.commit().await?;
    // Fase B: al primo fix il device diventa "attivo"
    let _ = sqlx::query("UPDATE app.device_provisioning SET step='attivo',updated_at=now() WHERE device_id=$1 AND step<>'attivo'")
        .bind(dev_id).execute(&st.owner_pool).await;
    publish_event(st, TenantEvent{tenant_id:tid,payload:json!({"type":"device_position","device_id":dev_id,"lat":lat,"lon":lon})}).await;
    for (gid, inside) in &presence_changes {
        publish_event(st, TenantEvent{tenant_id:tid,payload:json!({"type":"presence","device_id":dev_id,"geofence_id":gid,"inside":inside})}).await;
    }
    Ok(json!({"ok":true,"device_id":dev_id,"deduplicated":dedup.is_none(),"presence_changes":presence_changes.len()}))
}

/// Storico posizioni di un device (trail/mappa). operatore/admin/auditor.
pub async fn list_device_positions(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "traccar").await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let limit: i64 = q.get("limit").and_then(|x| x.parse().ok()).unwrap_or(50).clamp(1,500);
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT lat,lon,speed,course,battery,attributes,fixed_at FROM app.device_positions
        WHERE device_id=$1 ORDER BY fixed_at DESC LIMIT $2").bind(id).bind(limit).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "lat":r.get::<f64,_>("lat"),"lon":r.get::<f64,_>("lon"),
        "speed":r.try_get::<f64,_>("speed").ok(),"course":r.try_get::<f64,_>("course").ok(),
        "battery":r.try_get::<f64,_>("battery").ok(),"attributes":r.try_get::<Value,_>("attributes").ok(),
        "fixed_at":r.get::<chrono::DateTime<chrono::Utc>,_>("fixed_at") })).collect();
    Ok(Json(json!(out)))
}

// ----------------------------- ThingsBoard adapter (UPSTREAM REALE) -----------------------------
/// Receives forwards from a ThingsBoard Rule Engine "REST API Call" node and maps
/// them onto the same normalized chain. Tenant from the ingest token (header or
/// `?token=`), NEVER from the body. `?device=<name>` carries the TB device name.
/// The ThingsBoard rule chain decides WHAT to forward; the core decides severity:
/// an explicit alarm flag or a temperature threshold breach -> `sensore_critico`
/// (critical -> incident -> GoAlert); anything else is stored as `telemetria`.
// ---------------- Motore regole sensori (parametrico, da YouWorkforce) ----------------
fn rule_num(v: &Value) -> Option<f64> { v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())) }
fn rule_truthy(v: &Value) -> bool {
    v.as_bool().unwrap_or_else(|| v.as_f64().map(|n| n != 0.0)
        .unwrap_or_else(|| matches!(v.as_str().map(|s| s.to_ascii_lowercase()).as_deref(), Some("true")|Some("1")|Some("on")|Some("yes")|Some("si"))))
}
fn rule_matches(op: &str, thr: Option<f64>, v: &Value) -> bool {
    match op {
        "is_true" => rule_truthy(v),
        "is_false" => !rule_truthy(v),
        _ => match (thr, rule_num(v)) {
            (Some(t), Some(x)) => match op { ">=" => x>=t, "<=" => x<=t, ">" => x>t, "<" => x<t, "==" => (x-t).abs()<1e-9, _ => false },
            _ => false,
        }
    }
}
/// Valuta le regole sensori del tenant sul payload telemetria. Ritorna l'etype della
/// prima regola abilitata che fa match (ordine: sort, poi creazione). System scope.
pub async fn eval_sensor_rules(st: &AppState, tid: Uuid, payload: &Value) -> Option<String> {
    let rows = sqlx::query("SELECT metric,op,threshold,etype FROM app.sensor_rules WHERE tenant_id=$1 AND enabled=true ORDER BY sort,created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.ok()?;
    for r in &rows {
        let metric: String = r.get("metric");
        let op: String = r.get("op");
        let thr: Option<f64> = r.try_get("threshold").ok();
        let et: String = r.get("etype");
        if let Some(v) = payload.get(&metric) {
            if rule_matches(&op, thr, v) { return Some(et); }
        }
    }
    None
}
const RULE_ETYPES: &[&str] = &["sensore_critico","batteria_critica","man_down","perdita_alimentazione","telemetria"];
const RULE_OPS: &[&str] = &[">=","<=",">","<","==","is_true","is_false"];
#[derive(Deserialize)]
pub struct SensorRuleReq { pub metric: String, pub op: String, pub threshold: Option<f64>, pub etype: String, pub label: Option<String>, pub sort: Option<i32> }
pub async fn list_sensor_rules(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,metric,op,threshold,etype,label,enabled,sort FROM app.sensor_rules ORDER BY sort,created_at")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "metric": r.get::<String,_>("metric"), "op": r.get::<String,_>("op"),
        "threshold": r.try_get::<f64,_>("threshold").ok(), "etype": r.get::<String,_>("etype"),
        "label": r.try_get::<String,_>("label").ok(), "enabled": r.get::<bool,_>("enabled"), "sort": r.get::<i32,_>("sort") })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_sensor_rule(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SensorRuleReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.metric.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "metrica richiesta")); }
    if !RULE_OPS.contains(&b.op.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "operatore non valido")); }
    if !RULE_ETYPES.contains(&b.etype.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "etype non ammesso")); }
    let numeric = !matches!(b.op.as_str(), "is_true"|"is_false");
    if numeric && b.threshold.is_none() { return Err(err(StatusCode::BAD_REQUEST, "soglia richiesta per questo operatore")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.sensor_rules(tenant_id,metric,op,threshold,etype,label,sort) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.metric.trim()).bind(&b.op).bind(if numeric { b.threshold } else { None })
        .bind(&b.etype).bind(&b.label).bind(b.sort.unwrap_or(100)).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "sensor_rule_create", json!({"metric":b.metric,"op":b.op,"etype":b.etype})).await;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id")})))
}
pub async fn delete_sensor_rule(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.sensor_rules WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "regola inesistente")); }
    Ok(Json(json!({"ok":true})))
}

pub async fn ingest_thingsboard(
    State(st): State<AppState>,
    h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
    Json(b): Json<Value>,
) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| q.get("token").cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, &token).await
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    if !module_active(&st, tid, "thingsboard").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo thingsboard non abilitato"));
    }
    // Fase A: token device-bound -> device forzato (anti-spoof del nome device dalla querystring).
    let bound_ext = match cred.device_id { Some(did) => bound_device_external(&st, did).await, None => None };
    let device = bound_ext.or_else(|| q.get("device").cloned().filter(|s| !s.is_empty()));

    // Motore regole sensori parametrico (da YouWorkforce). Se nessuna regola fa match,
    // fallback alla policy legacy (flag alarm/critical + soglia temperatura di severity_gate).
    let etype = if let Some(e) = eval_sensor_rules(&st, tid, &b).await { e } else {
        let temp = b.get("temperature").and_then(|v| v.as_f64());
        let alarm_flag = b.get("alarm").and_then(|v| v.as_bool()).unwrap_or(false)
            || b.get("critical").and_then(|v| v.as_bool()).unwrap_or(false);
        let gate = tool_config(&st, tid, "thingsboard", "severity_gate").await;
        let temp_max = gate.get("temperature_max").and_then(|v| v.as_f64()).unwrap_or(90.0);
        if alarm_flag || temp.map(|t| t >= temp_max).unwrap_or(false) { "sensore_critico".into() } else { "telemetria".into() }
    };

    let corr = format!("tb-{}-{}", device.clone().unwrap_or_else(|| "dev".into()), Uuid::new_v4());
    persist_ingest(&st, tid, Normalized {
        source: "thingsboard".into(),
        source_event: corr.clone(),
        idem: corr.clone(),
        etype,
        device_external_id: device,
        site_name: None,
        lat: None, lon: None, accuracy_m: None,
        correlation: corr,
        payload: b.clone(),
        indoor: false, beacon_uid: None, zone: None,
    }).await
}

// ----------------------------- incidents -----------------------------
pub async fn list_incidents(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?;
    let acl = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT i.id,i.etype,i.state,i.priority,i.correlation,i.opened_at,i.outcome,i.assigned_user,i.site_id,
                s.name AS site, u.email AS assignee,
                i.attribution_uncertain, i.attribution_note,
                COALESCE(w.email, du.email) AS worker,
                COALESCE(d.label, d.external_id) AS device
         FROM app.incidents i
           LEFT JOIN app.sites s ON s.id=i.site_id
           LEFT JOIN app.users u ON u.id=i.assigned_user
           LEFT JOIN app.users w ON w.id=i.worker_user_id
           LEFT JOIN app.events e ON e.id=i.event_id
           LEFT JOIN app.devices d ON d.id=e.device_id
           LEFT JOIN app.users du ON du.id=d.assigned_user
         ORDER BY i.state<>'chiuso' DESC, i.priority DESC, i.opened_at DESC")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter()
        .filter(|r| site_allowed(&acl, r.try_get::<Uuid,_>("site_id").ok()))
        .map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "etype": r.get::<String,_>("etype"),
        "state": r.get::<String,_>("state"), "priority": r.get::<i32,_>("priority"),
        "correlation": r.get::<String,_>("correlation"),
        "site": r.try_get::<String,_>("site").ok(),
        "assigned_user": r.try_get::<Uuid,_>("assigned_user").ok(),
        "assignee": r.try_get::<String,_>("assignee").ok(),
        "worker": r.try_get::<String,_>("worker").ok(),
        "device": r.try_get::<String,_>("device").ok(),
        "attribution_uncertain": r.try_get::<bool,_>("attribution_uncertain").unwrap_or(false),
        "attribution_note": r.try_get::<String,_>("attribution_note").ok(),
        "outcome": r.try_get::<String,_>("outcome").ok() })).collect();
    Ok(Json(json!(out)))
}

pub async fn incident_timeline(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    guard_incident_site(&st, &s, iid).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT kind,detail,at FROM app.incident_timeline WHERE incident_id=$1 ORDER BY at")
        .bind(iid).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "kind": r.get::<String,_>("kind"),
        "detail": r.try_get::<Value,_>("detail").unwrap_or(json!(null)) })).collect();
    Ok(Json(json!(out)))
}

pub async fn ack_incident(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    // il reperibile è il destinatario dell'escalation: deve poter prendere in carico.
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"reperibile") {
        return Err(err(StatusCode::FORBIDDEN, "role cannot ack"));
    }
    guard_incident_site(&st, &s, iid).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE id=$1 AND state='aperto'")
        .bind(iid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"not open")); }
    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'human_ack',$3)")
        .bind(tid).bind(iid).bind(json!({"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_ack","incident_id":iid})}).await;
    esc_status_update_notify(&st, tid, iid, Some(s.user_id), &s.email, "ack").await;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct CloseReq { pub outcome: String }
pub async fn close_incident(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>, Json(b): Json<CloseReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"reperibile") {
        return Err(err(StatusCode::FORBIDDEN, "role cannot close"));
    }
    guard_incident_site(&st, &s, iid).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$2,closed_at=now(),version=version+1 WHERE id=$1 AND state<>'chiuso' RETURNING correlation,etype")
        .bind(iid).bind(&b.outcome).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some(row) = row else { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"already closed")); };
    let correlation: String = row.get("correlation"); let etype: String = row.get("etype");
    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
        .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":b.outcome,"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    // P1 close-loop: chiedi a GoAlert di chiudere l'alert con lo stesso dedup (=correlation)
    sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_close',$2)")
        .bind(tid).bind(json!({"incident_id":iid,"summary":format!("close {}",correlation),"dedup":correlation,"etype":etype}))
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_closed","incident_id":iid})}).await;
    esc_status_update_notify(&st, tid, iid, Some(s.user_id), &s.email, "close").await;
    Ok(Json(json!({"ok":true})))
}

// ----------------------------- worker SOS -----------------------------
/// Worker-originated SOS from the PWA (session-authenticated). Distinct from
/// machine ingress; never depends on STT/RAG (contract line 134).
/// Worker Safety Fase B — dispositivo "app" del lavoratore (bind sessione↔device).
/// Usa il device assegnato (devices.assigned_user) se presente; altrimenti auto-provisiona
/// un device source='pwa' external_id `pwa-<uid>` intestato al lavoratore.
pub async fn worker_app_device(st: &AppState, tid: Uuid, uid: Uuid, email: &str) -> Result<(Uuid, String), (StatusCode, String)> {
    if let Some(row) = sqlx::query("SELECT id,external_id FROM app.devices WHERE tenant_id=$1 AND assigned_user=$2 AND COALESCE(enabled,true) ORDER BY created_at LIMIT 1")
        .bind(tid).bind(uid).fetch_optional(&st.owner_pool).await.map_err(ise)? {
        return Ok((row.get("id"), row.get("external_id")));
    }
    let ext = format!("pwa-{}", uid);
    let label = format!("App {}", email);
    let row = sqlx::query(
        "INSERT INTO app.devices(tenant_id,source,external_id,label,assigned_user,enabled) VALUES($1,'pwa',$2,$3,$4,true) \
         ON CONFLICT (tenant_id,source,external_id) DO UPDATE SET assigned_user=EXCLUDED.assigned_user RETURNING id")
        .bind(tid).bind(&ext).bind(&label).bind(uid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    let did: Uuid = row.get("id");
    // storico assegnazione (device PWA): garantisce una riga aperta device->worker (idempotente)
    let _ = sqlx::query("INSERT INTO app.device_assignments(tenant_id,device_id,user_id,assigned_at)
        SELECT $1,$2,$3,now() WHERE NOT EXISTS (SELECT 1 FROM app.device_assignments WHERE tenant_id=$1 AND device_id=$2 AND user_id=$3 AND unassigned_at IS NULL)")
        .bind(tid).bind(did).bind(uid).execute(&st.owner_pool).await;
    Ok((did, ext))
}

/// Posizione continua dall'app lavoratore (watchPosition). Session-auth: il fix è attribuito
/// SEMPRE al device del lavoratore autenticato (bind sessione↔device), mai a un id dal payload.
/// Aggiorna ultima posizione, storico e presenza geofence. Nessun incidente qui.
pub async fn worker_position(State(st): State<AppState>, h: HeaderMap, Json(b): Json<serde_json::Value>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let lat = b.get("lat").and_then(|v| v.as_f64());
    let lon = b.get("lon").and_then(|v| v.as_f64());
    let (Some(lat), Some(lon)) = (lat, lon) else { return Err(err(StatusCode::BAD_REQUEST, "lat/lon mancanti")); };
    let speed = b.get("speed").and_then(|v| v.as_f64());
    let battery = b.get("battery").and_then(|v| v.as_f64());
    let now = st.clock.now();
    let (dev_id, _ext) = worker_app_device(&st, tid, s.user_id, &s.email).await?;
    let src_id = format!("pwa-{}", now.timestamp_millis());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query(
        "INSERT INTO app.device_positions(tenant_id,device_id,source_id,lat,lon,speed,battery,fixed_at) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (tenant_id,device_id,source_id) DO NOTHING")
        .bind(tid).bind(dev_id).bind(&src_id).bind(lat).bind(lon).bind(speed).bind(battery).bind(now)
        .execute(&mut *tx).await.map_err(ise)?;
    sqlx::query(
        "UPDATE app.devices SET last_lat=$2,last_lon=$3,last_speed=$4,last_battery=COALESCE($5,last_battery),last_pos_at=$6,updated_at=now() \
         WHERE id=$1 AND (last_pos_at IS NULL OR last_pos_at <= $6)")
        .bind(dev_id).bind(lat).bind(lon).bind(speed).bind(battery).bind(now)
        .execute(&mut *tx).await.map_err(ise)?;
    // presenza geofence (dentro/fuori) — stessa logica del pull Traccar
    let mut changes: Vec<(Uuid, bool)> = Vec::new();
    let gfs = sqlx::query("SELECT id,geom FROM app.geofences").fetch_all(&mut *tx).await.map_err(ise)?;
    for g in gfs {
        let gid: Uuid = g.get("id");
        let inside = geofence_contains(&g.get::<Value,_>("geom"), lat, lon);
        let prev: Option<bool> = sqlx::query_scalar("SELECT inside FROM app.device_presence WHERE device_id=$1 AND geofence_id=$2")
            .bind(dev_id).bind(gid).fetch_optional(&mut *tx).await.map_err(ise)?;
        if prev != Some(inside) {
            sqlx::query("INSERT INTO app.device_presence(tenant_id,device_id,geofence_id,inside) VALUES($1,$2,$3,$4) \
                ON CONFLICT (device_id,geofence_id) DO UPDATE SET inside=EXCLUDED.inside, changed_at=now()")
                .bind(tid).bind(dev_id).bind(gid).bind(inside).execute(&mut *tx).await.map_err(ise)?;
            sqlx::query("INSERT INTO app.presence_log(tenant_id,device_id,geofence_id,inside,at) VALUES($1,$2,$3,$4,$5)")
                .bind(tid).bind(dev_id).bind(gid).bind(inside).bind(now).execute(&mut *tx).await.map_err(ise)?;
            changes.push((gid, inside));
        }
    }
    tx.commit().await.map_err(ise)?;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"device_position","device_id":dev_id,"lat":lat,"lon":lon})}).await;
    for (gid, inside) in &changes {
        publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"presence","device_id":dev_id,"geofence_id":gid,"inside":inside})}).await;
    }
    Ok(Json(json!({"ok":true,"device_id":dev_id,"presence_changes":changes.len()})))
}

pub async fn worker_sos(State(st): State<AppState>, h: HeaderMap, Json(b): Json<serde_json::Value>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let lat = b.get("lat").and_then(|v| v.as_f64());
    let lon = b.get("lon").and_then(|v| v.as_f64());
    let accuracy = b.get("accuracy").and_then(|v| v.as_f64());
    let correlation = Uuid::new_v4().to_string();
    let now = st.clock.now();
    let (pri, repeat) = sos_escalation(&st, tid).await;
    // Fase B: l'SOS è attribuito al device del lavoratore (bind sessione↔device).
    let dev_id = worker_app_device(&st, tid, s.user_id, &s.email).await.ok().map(|(id,_)| id);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ev = sqlx::query(
        "INSERT INTO app.events(tenant_id,source,source_event,idem_key,etype,origin_ts,received_ts,lat,lon,accuracy_m,device_id,correlation)
         VALUES($1,'pwa',$2,$2,'sos',$3,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(tid).bind(format!("sos-{}-{}", s.user_id, now.timestamp_millis()))
        .bind(now).bind(lat).bind(lon).bind(accuracy).bind(dev_id).bind(&correlation).fetch_one(&mut *tx).await.map_err(ise)?;
    let eid: Uuid = ev.get("id");
    // Fase C gating: un profilo attivo del lavoratore può disabilitare il trigger SOS (fail-open).
    if trigger_suppressed(&st, tid, Some(s.user_id), None, "sos").await {
        tx.commit().await.map_err(ise)?;
        audit(&st, tid, &s.email, "trigger_suppressed_by_profile", json!({"etype":"sos","correlation":correlation})).await;
        return Ok(Json(json!({"event_id":eid,"suppressed_by_profile":true,"etype":"sos","correlation":correlation})));
    }
    let irow = sqlx::query(
        "INSERT INTO app.incidents(tenant_id,event_id,etype,correlation,priority) VALUES($1,$2,'sos',$3,$4) RETURNING id")
        .bind(tid).bind(eid).bind(&correlation).bind(pri).fetch_one(&mut *tx).await.map_err(ise)?;
    let iid: Uuid = irow.get("id");
    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_requested',$3)")
        .bind(tid).bind(iid).bind(json!({"etype":"sos","by":s.email,"priority":pri,"repeat_seconds":repeat})).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_notify',$2)")
        .bind(tid).bind(json!({"incident_id":iid,"summary":format!("SOS [{}] {}",correlation,s.email),"dedup":correlation,
            "integration_key":"","repeat_seconds":repeat,"etype":"sos",
            "details":format!("SOS lavoratore={}; lat={:?}; lon={:?}; correlazione={}",s.email,lat,lon,correlation)}))
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_opened","incident_id":iid,"etype":"sos","correlation":correlation})}).await;
    Ok(Json(json!({"incident_id":iid,"correlation":correlation})))
}

// ----------------------------- check-ins -----------------------------
#[derive(Deserialize)]
pub struct CheckinReq { pub minutes: i64 }
pub async fn create_checkin(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CheckinReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    // Per-tenant check-in policy (loneworker/checkin_timer): default when unset, clamp to max.
    let cfg = tool_config(&st, tid, "loneworker", "checkin_timer").await;
    let def_min = cfg.get("default_minutes").and_then(|v| v.as_i64()).unwrap_or(30);
    let max_min = cfg.get("max_minutes").and_then(|v| v.as_i64()).unwrap_or(120);
    let mins = if b.minutes >= 1 { b.minutes } else { def_min }.min(max_min).max(1);
    let due = st.clock.now() + chrono::Duration::minutes(mins);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.checkins(tenant_id,user_id,due_at) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(s.user_id).bind(due).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id"),"due_at":due})))
}

pub async fn confirm_checkin(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    // SICUREZZA: solo il proprietario puo' confermare il proprio check-in. Altrimenti un
    // altro utente potrebbe sopprimere l'allarme di mancato check-in di un collega isolato.
    let n = sqlx::query("UPDATE app.checkins SET state='confermato' WHERE id=$1 AND user_id=$2 AND state='attivo'")
        .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":n>0})))
}

// ======================= Prompt 8: turni + diario operativo =======================
// Turni server-confirmed; diario con nota dettata (STT lato browser) o manuale.
// Input manuale sempre disponibile; la dettatura NON è un prerequisito.
type Dt = chrono::DateTime<chrono::Utc>;

pub async fn shift_start(State(st): State<AppState>, h: HeaderMap, Json(b): Json<serde_json::Value>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "workforce").await { return Err(err(StatusCode::FORBIDDEN, "modulo workforce non abilitato")); }
    let note = b.get("note").and_then(|v| v.as_str());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // idempotent: an already-open shift is returned instead of erroring (double-tap safe)
    let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.shifts WHERE user_id=$1 AND state='in_corso'")
        .bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let (id, started, reused): (Uuid, Dt, bool) = if let Some(id) = existing {
        let started: Dt = sqlx::query_scalar("SELECT started_at FROM app.shifts WHERE id=$1").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
        (id, started, true)
    } else {
        let row = sqlx::query("INSERT INTO app.shifts(tenant_id,user_id,note) VALUES($1,$2,$3) RETURNING id, started_at")
            .bind(tid).bind(s.user_id).bind(note).fetch_one(&mut *tx).await.map_err(ise)?;
        (row.get("id"), row.get("started_at"), false)
    };
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id,"state":"in_corso","started_at":started,"reused":reused})))
}

// Max shift length (workforce/shift): used to surface overtime. Non-blocking by design
// (a worker is never locked out mid-shift); the UI warns and consuntivi can flag it.
async fn shift_max_hours(st: &AppState, tid: Uuid) -> i64 {
    tool_config(st, tid, "workforce", "shift").await
        .get("max_hours").and_then(|v| v.as_i64()).unwrap_or(12).max(1)
}

pub async fn shift_end(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let max_h = shift_max_hours(&st, tid).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("UPDATE app.shifts SET state='terminato', ended_at=now() WHERE user_id=$1 AND state='in_corso' RETURNING id, started_at, ended_at")
        .bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    match row {
        Some(r) => {
            let started: Dt = r.get("started_at"); let ended: Dt = r.get("ended_at");
            let worked = (ended - started).num_minutes().max(0);
            let over = worked > max_h * 60;
            Ok(Json(json!({"id":r.get::<Uuid,_>("id"),"state":"terminato","started_at":started,"ended_at":ended,
                "worked_minutes":worked,"max_hours":max_h,"over_max":over})))
        }
        None => Err(err(StatusCode::CONFLICT, "nessun turno aperto")),
    }
}

pub async fn shift_current(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let max_h = shift_max_hours(&st, tid).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("SELECT id, started_at FROM app.shifts WHERE user_id=$1 AND state='in_corso'")
        .bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(match row {
        Some(r) => {
            let started: Dt = r.get("started_at");
            let elapsed = (chrono::Utc::now() - started).num_minutes().max(0);
            json!({"active":true,"id":r.get::<Uuid,_>("id"),"started_at":started,
                "elapsed_minutes":elapsed,"max_hours":max_h,"over_max":elapsed > max_h*60})
        }
        None => json!({"active":false,"max_hours":max_h}),
    }))
}

#[derive(Deserialize)]
pub struct DiaryReq { pub body: String, pub work_order_id: Option<Uuid>, pub source: Option<String>, pub idem_key: Option<String> }
pub async fn diary_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DiaryReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "workforce").await { return Err(err(StatusCode::FORBIDDEN, "modulo workforce non abilitato")); }
    let body = b.body.trim();
    if body.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nota vuota")); }
    let source = match b.source.as_deref() { Some("voce") => "voce", _ => "manuale" };
    let idem = b.idem_key.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| Uuid::new_v4().to_string());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.diary_entries(tenant_id,user_id,work_order_id,body,source,idem_key)
         VALUES($1,$2,$3,$4,$5,$6)
         ON CONFLICT (tenant_id,user_id,idem_key) DO NOTHING
         RETURNING id, created_at")
        .bind(tid).bind(s.user_id).bind(b.work_order_id).bind(body).bind(source).bind(&idem)
        .fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    match row {
        Some(r) => Ok(Json(json!({"id":r.get::<Uuid,_>("id"),"created_at":r.get::<Dt,_>("created_at"),"source":source,"deduplicated":false}))),
        None => Ok(Json(json!({"deduplicated":true,"idem_key":idem}))),
    }
}

pub async fn diary_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let all = matches!(s.role.as_str(), "operatore"|"responsabile"|"tenant_admin"|"auditor");
    let sql = if all {
        "SELECT d.id,d.body,d.source,d.created_at,d.work_order_id,u.email AS author
         FROM app.diary_entries d JOIN app.users u ON u.id=d.user_id ORDER BY d.created_at DESC LIMIT 100"
    } else {
        "SELECT d.id,d.body,d.source,d.created_at,d.work_order_id,u.email AS author
         FROM app.diary_entries d JOIN app.users u ON u.id=d.user_id WHERE d.user_id=$1 ORDER BY d.created_at DESC LIMIT 100"
    };
    let q = sqlx::query(sql);
    let q = if all { q } else { q.bind(s.user_id) };
    let rows = q.fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "body": r.get::<String,_>("body"), "source": r.get::<String,_>("source"),
        "created_at": r.get::<Dt,_>("created_at"), "work_order_id": r.try_get::<Uuid,_>("work_order_id").ok(),
        "author": r.get::<String,_>("author") })).collect();
    Ok(Json(json!(out)))
}

// ----------------------------- SSE -----------------------------
pub async fn stream(State(st): State<AppState>, h: HeaderMap)
    -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, (StatusCode, String)> {
    let s = require_tenant(&st, &h).await?;
    let my_tenant = s.tenant_id.unwrap();
    let rx = st.bus.subscribe();
    let stream = async_stream::stream_hack(rx, my_tenant);
    Ok(Sse::new(stream))
}

// ----------------------------- test clock -----------------------------
#[derive(Deserialize)]
pub struct AdvanceReq { pub seconds: i64 }
pub async fn test_advance(State(st): State<AppState>, Json(b): Json<AdvanceReq>) -> R {
    if !st.clock.test_mode { return Err(err(StatusCode::FORBIDDEN,"test clock disabled")); }
    st.clock.advance(b.seconds);
    Ok(Json(json!({"now":st.clock.now()})))
}

// small helpers
fn ise<E: std::fmt::Display>(e: E) -> (StatusCode, String) { (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) }
fn ise_resp() -> axum::response::Response { (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"db"}))).into_response() }

/// Host -> tenant slug. Control host returns None.
pub fn tenant_slug(st: &AppState, h: &HeaderMap) -> Option<String> {
    if let Some(s) = h.get("x-tenant-slug").and_then(|v| v.to_str().ok()) {
        if !s.is_empty() { return Some(s.to_lowercase()); }
    }
    let host = h.get(header::HOST)?.to_str().ok()?.split(':').next()?.to_string();
    if host == st.control_host { return None; }
    let suffix = format!(".{}", st.base_domain);
    host.strip_suffix(&suffix).map(|x| x.to_string())
}

// ======================= Prompt 9: ordini + consuntivi =======================
fn is_planner(s: &Session) -> bool { has_cap(s, "plan") }
fn is_approver(s: &Session) -> bool { has_cap(s, "approve") }

pub async fn module_active(st: &AppState, tid: Uuid, code: &str) -> bool {
    sqlx::query_scalar::<_, String>("SELECT state FROM app.tenant_modules WHERE tenant_id=$1 AND module_code=$2")
        .bind(tid).bind(code).fetch_optional(&st.owner_pool).await.ok().flatten()
        .map(|s| s == "attivo").unwrap_or(false)
}
/// Modulo spento dal superadmin -> la gestione sparisce anche via API.
async fn require_module(st: &AppState, tid: Uuid, code: &str) -> Result<(), (StatusCode, String)> {
    if module_active(st, tid, code).await { Ok(()) }
    else { Err(err(StatusCode::FORBIDDEN, &format!("modulo {code} non abilitato"))) }
}
async fn require_any_module(st: &AppState, tid: Uuid, codes: &[&str]) -> Result<(), (StatusCode, String)> {
    for c in codes { if module_active(st, tid, c).await { return Ok(()); } }
    Err(err(StatusCode::FORBIDDEN, "nessun modulo abilitato per questa funzione"))
}
const DEVICE_MODS: &[&str] = &["loneworker","traccar","iot","thingsboard"];
const CONNECTOR_MODS: &[&str] = &["traccar","thingsboard","goalert","iot"];
/// Moduli che possono PRODURRE incidenti → l'escalation/centrale è condivisa fra tutti.
/// La configurazione e la visibilità escalation (gruppi, routing, on-call, recapiti,
/// override, maintenance, centrale allarmi) non devono sparire se "Sicurezza lavoratore"
/// (loneworker) è spento ma resta attivo un altro modulo che genera allarmi.
const ESCALATION_MODS: &[&str] = &["loneworker","traccar","iot","thingsboard"];

// ================== Per-module tool configuration (schema-driven) ==================
// Effective config for a tool = catalog default_config overlaid with the tenant's
// saved values. System-scope read (owner_pool), always filtered by tenant_id.
// A tool is AVAILABLE unless superadmin set tenant_tool_enabled.enabled=false.
pub async fn tool_config(st: &AppState, tid: Uuid, module: &str, tool: &str) -> Value {
    let row = sqlx::query(
        "SELECT mt.default_config, tc.config
         FROM app.module_tools mt
         LEFT JOIN app.tenant_module_config tc
           ON tc.module_code=mt.module_code AND tc.tool_code=mt.tool_code AND tc.tenant_id=$1
         WHERE mt.module_code=$2 AND mt.tool_code=$3")
        .bind(tid).bind(module).bind(tool).fetch_optional(&st.owner_pool).await.ok().flatten();
    let mut out = serde_json::Map::new();
    if let Some(r) = row {
        if let Ok(Value::Object(d)) = r.try_get::<Value,_>("default_config") { out.extend(d); }
        if let Ok(Value::Object(c)) = r.try_get::<Value,_>("config") { out.extend(c); }
    }
    Value::Object(out)
}

/// UI nav feature flags for the current tenant (any authenticated role).
/// Deriva dalle scelte dell'admin sul tool traccar/alarm_map: quali voci di
/// menu mappa esporre. Default parametrici: Mappa on, Piantine off.
pub async fn ui_features(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    let c = tool_config(&st, tid, "traccar", "alarm_map").await;
    let on = |k: &str, d: bool| c.get(k).and_then(|v| v.as_str()).map(|v| v == "si").unwrap_or(d);
    Ok(Json(json!({
        "map_outdoor": on("view_mappa", true),
        "map_indoor":  on("view_piantine", false),
        // Gestione aree geofence sulla Mappa allarmi e sulle Piantine: NASCOSTA di default
        // (non serve al perimetro sicurezza). Il tenant_admin può attivarla. Sulla Mappa
        // dispositivi (lavori sul campo) resta sempre disponibile, indipendente da questo flag.
        "geofence_alarms": on("geofence_alarms", false),
    })))
}
/// Attiva/disattiva (tenant_admin) la gestione geofence su Mappa allarmi + Piantine.
#[derive(Deserialize)] pub struct GeofenceAlarmsFlag { pub geofence_alarms: bool }
pub async fn ui_features_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GeofenceAlarmsFlag>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    let existing: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='traccar' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let mut obj = match existing { Some(Value::Object(m)) => m, _ => serde_json::Map::new() };
    obj.insert("geofence_alarms".into(), json!(if b.geofence_alarms {"si"} else {"no"}));
    let cfg = Value::Object(obj);
    sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'traccar','alarm_map',$2,NULL)
         ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config, updated_at=now()")
        .bind(tid).bind(&cfg).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "tenant_admin", "geofence_alarms_set", json!({"geofence_alarms":b.geofence_alarms})).await;
    Ok(Json(json!({"ok":true,"geofence_alarms":b.geofence_alarms})))
}

/// Visibilità voci di menu mappa (superadmin): legge/scrive i flag view_mappa/view_piantine
/// del tool traccar/alarm_map. Il superadmin decide se mostrare Mappa, Piantine, entrambe o nessuna.
pub async fn super_map_visibility_get(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let c = tool_config(&st, tid, "traccar", "alarm_map").await;
    let on = |k: &str, d: bool| c.get(k).and_then(|v| v.as_str()).map(|v| v == "si").unwrap_or(d);
    Ok(Json(json!({"map_outdoor": on("view_mappa", true), "map_indoor": on("view_piantine", false)})))
}
#[derive(Deserialize)] pub struct MapVisibility { pub map_outdoor: bool, pub map_indoor: bool }
pub async fn super_map_visibility_set(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<MapVisibility>) -> R {
    require_super(&st, &h).await?;
    // merge preservando gli altri campi del tool alarm_map
    let existing: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='traccar' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let mut obj = match existing { Some(Value::Object(m)) => m, _ => serde_json::Map::new() };
    obj.insert("view_mappa".into(), json!(if b.map_outdoor {"si"} else {"no"}));
    obj.insert("view_piantine".into(), json!(if b.map_indoor {"si"} else {"no"}));
    let cfg = Value::Object(obj);
    sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'traccar','alarm_map',$2,NULL)
         ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config, updated_at=now()")
        .bind(tid).bind(&cfg).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "superadmin", "map_visibility_set", json!({"map_outdoor":b.map_outdoor,"map_indoor":b.map_indoor})).await;
    Ok(Json(json!({"ok":true,"map_outdoor":b.map_outdoor,"map_indoor":b.map_indoor})))
}

/// Trigger di sicurezza (Worker Safety) — vista unica dei trigger critici del perimetro lavoratore
/// e della loro configurazione per-tenant. Aggrega config già esistenti (traccar/alarm_map,
/// loneworker/sos_escalation, loneworker/checkin_timer). Solo tenant_admin, richiede loneworker.
pub async fn safety_triggers_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let amap = tool_config(&st, tid, "traccar", "alarm_map").await;
    let sos_esc = tool_config(&st, tid, "loneworker", "sos_escalation").await;
    let chk = tool_config(&st, tid, "loneworker", "checkin_timer").await;
    let sos_as = amap.get("sos_as").and_then(|v| v.as_str()).unwrap_or("sos").to_string();
    let stopped = amap.get("stopped_as_immobilita").and_then(|v| v.as_str()).map(|x| x=="si").unwrap_or(false);
    // area fuori-geofence: critica solo se mappata a un etype critico via alarm_map.events
    let geo_crit = amap.get("events").and_then(|v| v.as_object())
        .map(|m| m.values().any(|v| v.as_str()==Some("critico"))).unwrap_or(false);
    let triggers = json!([
        {"etype":"sos","label":"SOS","desc":"Pulsante antipanico dall'app lavoratore o dal dispositivo","sources":["App lavoratore (/api/sos)","Traccar: sos"],"critical":true},
        {"etype":"man_down","label":"Uomo a terra","desc":"Rilevamento caduta/incidente","sources":["Traccar: fallDown, accident"],"critical":true},
        {"etype":"immobilita","label":"Immobilità","desc":"Dispositivo fermo oltre soglia","sources":["Traccar: deviceStopped"],"critical":stopped},
        {"etype":"mancato_checkin","label":"Check-in mancato","desc":"Dead-man timer: il lavoratore non conferma entro i minuti previsti","sources":["Timer check-in"],"critical":true},
        {"etype":"movimento_non_accompagnato","label":"Movimento non accompagnato","desc":"Moto senza presenza attesa","sources":["Traccar: unaccompaniedMotion"],"critical":true},
        {"etype":"attraversamento_geofence","label":"Attraversamento area","desc":"Ingresso/uscita da un'area; critico solo se mappato","sources":["Traccar: geofence"],"critical":geo_crit}
    ]);
    Ok(Json(json!({
        "triggers": triggers,
        "config": {
            "sos_as": sos_as,
            "stopped_as_immobilita": stopped,
            "sos_priority": sos_esc.get("priority").and_then(|v| v.as_str()).unwrap_or("alta"),
            "sos_repeat_seconds": sos_esc.get("repeat_seconds").and_then(|v| v.as_i64()).unwrap_or(0),
            "checkin_default_minutes": chk.get("default_minutes").and_then(|v| v.as_i64()).unwrap_or(30),
            "checkin_max_minutes": chk.get("max_minutes").and_then(|v| v.as_i64()).unwrap_or(120)
        }
    })))
}

#[derive(Deserialize)] pub struct SafetyTriggersReq {
    pub sos_as: String, pub stopped_as_immobilita: bool,
    pub sos_priority: String, pub sos_repeat_seconds: i64,
    pub checkin_default_minutes: i64, pub checkin_max_minutes: i64,
}
/// Scrive la config dei trigger di sicurezza mergiando nei tool esistenti (nessuna tabella nuova).
pub async fn safety_triggers_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SafetyTriggersReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    if b.sos_as != "sos" && b.sos_as != "man_down" {
        return Err(err(StatusCode::BAD_REQUEST, "sos_as deve essere 'sos' o 'man_down'"));
    }
    if b.sos_priority != "alta" && b.sos_priority != "media" {
        return Err(err(StatusCode::BAD_REQUEST, "sos_priority deve essere 'alta' o 'media'"));
    }
    // helper merge per-tool (preserva gli altri campi)
    async fn merge_tool(st: &AppState, tid: Uuid, module: &str, tool: &str, patch: Vec<(&str, Value)>) -> Result<(), (StatusCode,String)> {
        let existing: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code=$2 AND tool_code=$3")
            .bind(tid).bind(module).bind(tool).fetch_optional(&st.owner_pool).await.map_err(ise)?;
        let mut obj = match existing { Some(Value::Object(m)) => m, _ => serde_json::Map::new() };
        for (k,v) in patch { obj.insert(k.to_string(), v); }
        let cfg = Value::Object(obj);
        sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,$2,$3,$4,NULL)
             ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config, updated_at=now()")
            .bind(tid).bind(module).bind(tool).bind(&cfg).execute(&st.owner_pool).await.map_err(ise)?;
        Ok(())
    }
    merge_tool(&st, tid, "traccar", "alarm_map", vec![
        ("sos_as", json!(b.sos_as)),
        ("stopped_as_immobilita", json!(if b.stopped_as_immobilita {"si"} else {"no"})),
    ]).await?;
    merge_tool(&st, tid, "loneworker", "sos_escalation", vec![
        ("priority", json!(b.sos_priority)),
        ("repeat_seconds", json!(b.sos_repeat_seconds.max(0))),
    ]).await?;
    merge_tool(&st, tid, "loneworker", "checkin_timer", vec![
        ("default_minutes", json!(b.checkin_default_minutes.clamp(1, 1440))),
        ("max_minutes", json!(b.checkin_max_minutes.clamp(1, 1440))),
    ]).await?;
    audit(&st, tid, &s.email, "safety_triggers_set", json!({"sos_as":b.sos_as,"sos_priority":b.sos_priority})).await;
    Ok(Json(json!({"ok":true})))
}

// ---------------------------------------------------------------------------------------------
// Fase C — Profili di protezione: il legante dei 5 stadi del perimetro Worker Safety.
// Un profilo lega 1..n lavoratori a una policy di escalation (alarm_group), ai trigger abilitati
// e ai parametri di check-in. I device coperti derivano da devices.assigned_user dei membri.
// ---------------------------------------------------------------------------------------------

/// Elenco profili con gruppo escalation, membri (email) e n. device coperti (via assigned_user).
pub async fn protection_profiles_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let rows = sqlx::query(
        "SELECT p.id,p.name,p.descr,p.escalation_group_id,g.name AS group_name,p.triggers,p.checkin_minutes,p.enabled,p.updated_at
           FROM app.protection_profiles p
           LEFT JOIN app.alarm_groups g ON g.id=p.escalation_group_id
          WHERE p.tenant_id=$1 ORDER BY p.name")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut out = Vec::new();
    for r in &rows {
        let pid: Uuid = r.get("id");
        let members = sqlx::query(
            "SELECT u.id,u.email,
                    (SELECT count(*) FROM app.devices d WHERE d.tenant_id=$1 AND d.assigned_user=u.id) AS devices
               FROM app.protection_profile_members m JOIN app.users u ON u.id=m.user_id
              WHERE m.profile_id=$2 ORDER BY u.email")
            .bind(tid).bind(pid).fetch_all(&st.owner_pool).await.map_err(ise)?;
        let mut device_total = 0i64;
        let mem: Vec<Value> = members.iter().map(|m| {
            let dc: i64 = m.get("devices"); device_total += dc;
            json!({"id": m.get::<Uuid,_>("id"), "email": m.get::<String,_>("email"), "devices": dc})
        }).collect();
        let eg: Option<Uuid> = r.get("escalation_group_id");
        out.push(json!({
            "id": pid,
            "name": r.get::<String,_>("name"),
            "descr": r.get::<String,_>("descr"),
            "escalation_group_id": eg,
            "group_name": r.get::<Option<String>,_>("group_name"),
            "triggers": r.get::<Value,_>("triggers"),
            "checkin_minutes": r.get::<Option<i32>,_>("checkin_minutes"),
            "enabled": r.get::<bool,_>("enabled"),
            "members": mem,
            "devices_covered": device_total,
        }));
    }
    // gruppi selezionabili come policy
    let groups = sqlx::query("SELECT id,name FROM app.alarm_groups WHERE tenant_id=$1 ORDER BY name")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let glist: Vec<Value> = groups.iter().map(|g| json!({"id":g.get::<Uuid,_>("id"),"name":g.get::<String,_>("name")})).collect();
    Ok(Json(json!({"profiles": out, "groups": glist})))
}

#[derive(Deserialize)] pub struct ProfileReq {
    pub name: String,
    #[serde(default)] pub descr: String,
    #[serde(default)] pub escalation_group_id: Option<Uuid>,
    #[serde(default)] pub triggers: Option<Value>,
    #[serde(default)] pub checkin_minutes: Option<i32>,
    #[serde(default = "default_true")] pub enabled: bool,
}
fn default_true() -> bool { true }

/// Valida che un gruppo (se indicato) appartenga al tenant.
async fn group_belongs(st: &AppState, tid: Uuid, gid: Option<Uuid>) -> Result<(), (StatusCode,String)> {
    if let Some(g) = gid {
        let ok: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
            .bind(g).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
        if ok.is_none() { return Err(err(StatusCode::BAD_REQUEST, "gruppo escalation inesistente")); }
    }
    Ok(())
}

pub async fn protection_profile_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ProfileReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let name = b.name.trim();
    if name.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome obbligatorio")); }
    group_belongs(&st, tid, b.escalation_group_id).await?;
    let triggers = b.triggers.clone().unwrap_or_else(|| json!({"sos":true,"man_down":true,"immobilita":true,"mancato_checkin":true,"geofence":false}));
    let row = sqlx::query(
        "INSERT INTO app.protection_profiles(tenant_id,name,descr,escalation_group_id,triggers,checkin_minutes,enabled)
         VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(name).bind(b.descr.trim()).bind(b.escalation_group_id).bind(&triggers)
        .bind(b.checkin_minutes).bind(b.enabled)
        .fetch_optional(&st.owner_pool).await
        .map_err(|e| if e.to_string().contains("duplicate") { err(StatusCode::CONFLICT, "nome profilo già esistente") } else { ise(e) })?;
    let id: Uuid = row.ok_or_else(|| err(StatusCode::CONFLICT, "nome profilo già esistente"))?.get("id");
    audit(&st, tid, &s.email, "protection_profile_create", json!({"id":id,"name":name})).await;
    Ok(Json(json!({"id":id})))
}

pub async fn protection_profile_update(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ProfileReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let name = b.name.trim();
    if name.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome obbligatorio")); }
    group_belongs(&st, tid, b.escalation_group_id).await?;
    let triggers = b.triggers.clone().unwrap_or_else(|| json!({"sos":true,"man_down":true,"immobilita":true,"mancato_checkin":true,"geofence":false}));
    let n = sqlx::query(
        "UPDATE app.protection_profiles SET name=$3,descr=$4,escalation_group_id=$5,triggers=$6,checkin_minutes=$7,enabled=$8,updated_at=now()
          WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).bind(name).bind(b.descr.trim()).bind(b.escalation_group_id).bind(&triggers)
        .bind(b.checkin_minutes).bind(b.enabled)
        .execute(&st.owner_pool).await
        .map_err(|e| if e.to_string().contains("duplicate") { err(StatusCode::CONFLICT, "nome profilo già esistente") } else { ise(e) })?
        .rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "profilo inesistente")); }
    audit(&st, tid, &s.email, "protection_profile_update", json!({"id":id,"name":name})).await;
    Ok(Json(json!({"ok":true})))
}

pub async fn protection_profile_delete(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let n = sqlx::query("DELETE FROM app.protection_profiles WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "profilo inesistente")); }
    audit(&st, tid, &s.email, "protection_profile_delete", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)] pub struct ProfileMemberReq { pub user_id: Uuid }
/// Aggiunge un lavoratore al profilo. Un worker è in al massimo un profilo: se già altrove, lo sposta.
pub async fn protection_profile_member_add(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ProfileMemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    // profilo e utente devono appartenere al tenant
    let pok: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.protection_profiles WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    if pok.is_none() { return Err(err(StatusCode::NOT_FOUND, "profilo inesistente")); }
    let uok: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    if uok.is_none() { return Err(err(StatusCode::BAD_REQUEST, "utente inesistente")); }
    // sposta: rimuove da altro profilo (UNIQUE tenant_id,user_id) poi inserisce
    sqlx::query("DELETE FROM app.protection_profile_members WHERE tenant_id=$1 AND user_id=$2")
        .bind(tid).bind(b.user_id).execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.protection_profile_members(tenant_id,profile_id,user_id) VALUES($1,$2,$3)")
        .bind(tid).bind(id).bind(b.user_id).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "protection_profile_member_add", json!({"profile":id,"user":b.user_id})).await;
    Ok(Json(json!({"ok":true})))
}

pub async fn protection_profile_member_del(State(st): State<AppState>, h: HeaderMap, Path((id,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let n = sqlx::query("DELETE FROM app.protection_profile_members WHERE tenant_id=$1 AND profile_id=$2 AND user_id=$3")
        .bind(tid).bind(id).bind(uid).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "membro inesistente")); }
    audit(&st, tid, &s.email, "protection_profile_member_del", json!({"profile":id,"user":uid})).await;
    Ok(Json(json!({"ok":true})))
}

/// Fase C — gating dei trigger per-profilo. Ritorna true se un Profilo di protezione ATTIVO del
/// lavoratore DISABILITA ESPLICITAMENTE il trigger per questo etype → l'incidente NON va aperto.
/// Principio FAIL-OPEN (il single-path critico non si perde per errore): si sopprime SOLO quando
/// esiste un profilo attivo per quel worker e la chiave trigger è esplicitamente `false`.
/// Nessun profilo / etype non mappato / chiave assente o `true` → false (non sopprime).
pub(crate) async fn trigger_suppressed(st: &AppState, tid: Uuid, worker: Option<Uuid>, device_id: Option<Uuid>, etype: &str) -> bool {
    let key = match etype {
        "sos" => "sos",
        "man_down" => "man_down",
        "immobilita" => "immobilita",
        "mancato_checkin" => "mancato_checkin",
        "attraversamento_geofence" => "geofence",
        _ => return false, // etype non riconducibile a un trigger di profilo → mai soppresso
    };
    // lavoratore: esplicito, oppure assegnatario del device dell'evento
    let wid = match worker {
        Some(w) => Some(w),
        None => match device_id {
            Some(d) => sqlx::query_scalar::<_, Option<Uuid>>("SELECT assigned_user FROM app.devices WHERE id=$1 AND tenant_id=$2")
                .bind(d).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten(),
            None => None,
        }
    };
    let Some(wid) = wid else { return false; };
    let trg: Option<Value> = sqlx::query_scalar(
        "SELECT p.triggers FROM app.protection_profile_members m JOIN app.protection_profiles p ON p.id=m.profile_id
          WHERE m.tenant_id=$1 AND m.user_id=$2 AND p.enabled=true")
        .bind(tid).bind(wid).fetch_optional(&st.owner_pool).await.ok().flatten();
    matches!(trg.as_ref().and_then(|t| t.get(key)).and_then(|v| v.as_bool()), Some(false))
}

/// SOS escalation policy (loneworker/sos_escalation): incident priority + repeat cadence.
/// priority "alta"->3, "media"->2. repeat_seconds carried to the GoAlert connector payload.
async fn sos_escalation(st: &AppState, tid: Uuid) -> (i32, i64) {
    let c = tool_config(st, tid, "loneworker", "sos_escalation").await;
    let p = match c.get("priority").and_then(|v| v.as_str()) { Some("media") => 2, _ => 3 };
    let r = c.get("repeat_seconds").and_then(|v| v.as_i64()).unwrap_or(0);
    (p, r)
}

async fn tool_enabled(st: &AppState, tid: Uuid, module: &str, tool: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT enabled FROM app.tenant_tool_enabled WHERE tenant_id=$1 AND module_code=$2 AND tool_code=$3")
        .bind(tid).bind(module).bind(tool).fetch_optional(&st.owner_pool).await.ok().flatten()
        .unwrap_or(true) // absent => available
}

/// Validate an incoming config object against a tool's JSON schema.
/// Rejects unknown keys and out-of-range/enum-violating values. Returns a
/// cleaned object containing only schema-declared keys.
fn validate_config(schema: &Value, input: &Value) -> Result<Value, String> {
    let fields = schema.get("fields").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    let inobj = input.as_object().ok_or_else(|| "config must be an object".to_string())?;
    let known: std::collections::HashSet<String> = fields.iter()
        .filter_map(|f| f.get("key").and_then(|k| k.as_str()).map(|s| s.to_string())).collect();
    for k in inobj.keys() {
        if !known.contains(k) { return Err(format!("unknown field: {k}")); }
    }
    let mut out = serde_json::Map::new();
    for f in &fields {
        let key = match f.get("key").and_then(|k| k.as_str()) { Some(k) => k, None => continue };
        let v = match inobj.get(key) { Some(v) => v, None => continue };
        let ty = f.get("type").and_then(|t| t.as_str()).unwrap_or("string");
        match ty {
            "int" => {
                let n = v.as_i64().ok_or_else(|| format!("{key}: atteso intero"))?;
                if let Some(min) = f.get("min").and_then(|x| x.as_i64()) { if n < min { return Err(format!("{key}: minimo {min}")); } }
                if let Some(max) = f.get("max").and_then(|x| x.as_i64()) { if n > max { return Err(format!("{key}: massimo {max}")); } }
                out.insert(key.into(), json!(n));
            }
            "enum" => {
                let s = v.as_str().ok_or_else(|| format!("{key}: atteso testo"))?;
                let opts: Vec<&str> = f.get("options").and_then(|o| o.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
                if !opts.contains(&s) { return Err(format!("{key}: valore non ammesso")); }
                out.insert(key.into(), json!(s));
            }
            _ => { // string / fallback
                let s = v.as_str().ok_or_else(|| format!("{key}: atteso testo"))?;
                out.insert(key.into(), json!(s));
            }
        }
    }
    Ok(Value::Object(out))
}

// ---- tenant_admin: read tools of a module (schema + effective config) ----
pub async fn list_module_tools(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "tenant_admin"|"auditor") {
        return Err(err(StatusCode::FORBIDDEN, "configurazione moduli riservata all'admin del tenant"));
    }
    let tid = s.tenant_id.unwrap();
    let rows = sqlx::query(
        "SELECT tool_code,name,description,config_schema,default_config,sort
         FROM app.module_tools WHERE module_code=$1 ORDER BY sort,tool_code")
        .bind(&code).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut out = Vec::new();
    for r in rows {
        let tool: String = r.get("tool_code");
        if !tool_enabled(&st, tid, &code, &tool).await { continue; } // superadmin hid it
        out.push(json!({
            "tool_code": tool,
            "name": r.get::<String,_>("name"),
            "description": r.get::<String,_>("description"),
            "schema": r.get::<Value,_>("config_schema"),
            "config": tool_config(&st, tid, &code, &tool).await,
            "editable": s.role == "tenant_admin",
        }));
    }
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct SetToolConfig { pub config: Value }

// ---- tenant_admin: save config for one tool ----
pub async fn set_module_tool(State(st): State<AppState>, h: HeaderMap, Path((code, tool)): Path<(String, String)>, Json(b): Json<SetToolConfig>) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" {
        return Err(err(StatusCode::FORBIDDEN, "solo l'admin del tenant puo configurare"));
    }
    let tid = s.tenant_id.unwrap();
    let schema: Option<Value> = sqlx::query_scalar("SELECT config_schema FROM app.module_tools WHERE module_code=$1 AND tool_code=$2")
        .bind(&code).bind(&tool).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let schema = schema.ok_or_else(|| err(StatusCode::NOT_FOUND, "strumento inesistente"))?;
    if !tool_enabled(&st, tid, &code, &tool).await {
        return Err(err(StatusCode::FORBIDDEN, "strumento non abilitato per questo tenant"));
    }
    let clean = validate_config(&schema, &b.config).map_err(|m| err(StatusCode::BAD_REQUEST, &m))?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query(
        "INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by)
         VALUES($1,$2,$3,$4,$5)
         ON CONFLICT (tenant_id,module_code,tool_code)
         DO UPDATE SET config=EXCLUDED.config, updated_by=EXCLUDED.updated_by, updated_at=now()")
        .bind(tid).bind(&code).bind(&tool).bind(&clean).bind(s.user_id)
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'set_tool_config',$3)")
        .bind(tid).bind(&s.email).bind(json!({"module":code,"tool":tool})).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"config":clean})))
}

// ---- superadmin: list tool availability for a tenant ----
pub async fn super_list_tools(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query(
        "SELECT mt.module_code,mt.tool_code,mt.name,COALESCE(te.enabled,true) AS enabled
         FROM app.module_tools mt
         LEFT JOIN app.tenant_tool_enabled te
           ON te.module_code=mt.module_code AND te.tool_code=mt.tool_code AND te.tenant_id=$1
         ORDER BY mt.module_code,mt.sort,mt.tool_code")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "module_code": r.get::<String,_>("module_code"),
        "tool_code": r.get::<String,_>("tool_code"),
        "name": r.get::<String,_>("name"),
        "enabled": r.get::<bool,_>("enabled"),
    })).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct SetToolEnabled { pub module_code: String, pub tool_code: String, pub enabled: bool }

// ---- superadmin: enable/disable a tool for a tenant ----
pub async fn super_set_tool(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetToolEnabled>) -> R {
    require_super(&st, &h).await?;
    let exists: Option<String> = sqlx::query_scalar("SELECT tool_code FROM app.module_tools WHERE module_code=$1 AND tool_code=$2")
        .bind(&b.module_code).bind(&b.tool_code).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    if exists.is_none() { return Err(err(StatusCode::NOT_FOUND, "strumento inesistente")); }
    sqlx::query(
        "INSERT INTO app.tenant_tool_enabled(tenant_id,module_code,tool_code,enabled)
         VALUES($1,$2,$3,$4)
         ON CONFLICT (tenant_id,module_code,tool_code)
         DO UPDATE SET enabled=EXCLUDED.enabled, updated_at=now()")
        .bind(tid).bind(&b.module_code).bind(&b.tool_code).bind(b.enabled)
        .execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,'superadmin','set_tool_enabled',$2)")
        .bind(tid).bind(json!({"module":b.module_code,"tool":b.tool_code,"enabled":b.enabled})).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct NewOrder { pub title: String, pub site_id: Option<Uuid>, pub instructions: Option<String>, pub assigned_user: Option<Uuid>,
    pub scheduled_start: Option<String>, pub scheduled_end: Option<String>, pub priority: Option<i32>,
    #[serde(default)] pub checklist: Option<Vec<String>> }
pub async fn create_order(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewOrder>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    let ss = b.scheduled_start.as_deref().map(parse_ts).transpose()?;
    let se = b.scheduled_end.as_deref().map(parse_ts).transpose()?;
    if let (Some(a),Some(z))=(ss,se) { if z<=a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); } }
    let pri = b.priority.unwrap_or(2).clamp(1,3);
    let state = if b.assigned_user.is_some() { "assegnato" } else { "bozza" };
    // checklist: array di etichette -> [{label,done:false}]
    let checklist = json!(b.checklist.clone().unwrap_or_default().into_iter().map(|l| json!({"label":l,"done":false})).collect::<Vec<_>>());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.work_orders(tenant_id,site_id,title,instructions,assigned_user,state,created_by,scheduled_start,scheduled_end,priority,checklist)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id")
        .bind(tid).bind(b.site_id).bind(&b.title).bind(&b.instructions).bind(b.assigned_user).bind(state).bind(s.user_id)
        .bind(ss).bind(se).bind(pri).bind(&checklist)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id"),"state":state})))
}

#[derive(Deserialize)]
pub struct EditOrder { pub version: i32, pub title: Option<String>, pub site_id: Option<Uuid>, pub instructions: Option<String>,
    pub assigned_user: Option<Uuid>, pub scheduled_start: Option<String>, pub scheduled_end: Option<String>, pub priority: Option<i32>,
    pub clear_schedule: Option<bool> }
/// Modifica dell'ordine (pianificatore) con concorrenza ottimistica. Non cambia lo stato
/// (eccetto bozza->assegnato se si assegna). Finestra, priorita', sede, istruzioni modificabili.
pub async fn edit_order(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<EditOrder>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let ss = b.scheduled_start.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?;
    let se = b.scheduled_end.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?;
    if let (Some(a),Some(z))=(ss,se) { if z<=a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); } }
    let clear = b.clear_schedule.unwrap_or(false);
    let pri = b.priority.map(|p| p.clamp(1,3));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // COALESCE mantiene i valori esistenti quando il campo non è passato; clear_schedule azzera la finestra.
    let n = sqlx::query(
        "UPDATE app.work_orders SET
           title=COALESCE($3,title),
           instructions=COALESCE($4,instructions),
           site_id=COALESCE($5,site_id),
           assigned_user=COALESCE($6,assigned_user),
           scheduled_start=CASE WHEN $7 THEN NULL ELSE COALESCE($8,scheduled_start) END,
           scheduled_end=CASE WHEN $7 THEN NULL ELSE COALESCE($9,scheduled_end) END,
           priority=COALESCE($10,priority),
           state=CASE WHEN state='bozza' AND $6 IS NOT NULL THEN 'assegnato' ELSE state END,
           version=version+1, updated_at=now()
         WHERE id=$1 AND version=$2")
        .bind(id).bind(b.version).bind(&b.title).bind(&b.instructions).bind(b.site_id).bind(b.assigned_user)
        .bind(clear).bind(ss).bind(se).bind(pri)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::CONFLICT, "versione superata o ordine inesistente")); }
    audit(&st, tid, &s.email, "edit_order", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}

pub async fn list_orders(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    // technician sees only own; planner/admin/auditor see all (RLS already scopes to tenant)
    let cols="o.id,o.title,o.state,o.version,o.outcome,o.instructions,o.site_id,o.assigned_user,o.scheduled_start,o.scheduled_end,o.priority,s.name AS site,u.email AS assignee,\
        o.arrived_at,o.work_started_at,o.work_minutes,o.materials,o.checklist,o.outcome_kind,o.report,o.signed_by,o.signed_at,\
        s.address AS site_address,s.lat AS site_lat,s.lon AS site_lon,s.access_notes AS site_access";
    let rows = if s.role == "lavoratore" {
        sqlx::query(&format!("SELECT {cols}
            FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.users u ON u.id=o.assigned_user
            WHERE o.assigned_user=$1 ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC")).bind(s.user_id).fetch_all(&mut *tx).await
    } else {
        sqlx::query(&format!("SELECT {cols}
            FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.users u ON u.id=o.assigned_user
            ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC")).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "title": r.get::<String,_>("title"), "state": r.get::<String,_>("state"),
        "version": r.get::<i32,_>("version"), "site": r.try_get::<String,_>("site").ok(),
        "site_id": r.try_get::<Uuid,_>("site_id").ok(), "assigned_user": r.try_get::<Uuid,_>("assigned_user").ok(),
        "assignee": r.try_get::<String,_>("assignee").ok(), "outcome": r.try_get::<String,_>("outcome").ok(),
        "scheduled_start": r.try_get::<chrono::DateTime<chrono::Utc>,_>("scheduled_start").ok(),
        "scheduled_end": r.try_get::<chrono::DateTime<chrono::Utc>,_>("scheduled_end").ok(),
        "priority": r.get::<i32,_>("priority"),
        "instructions": r.try_get::<String,_>("instructions").ok(),
        "arrived_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("arrived_at").ok(),
        "work_started_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("work_started_at").ok(),
        "work_minutes": r.try_get::<i32,_>("work_minutes").ok(),
        "materials": r.try_get::<Value,_>("materials").ok(),
        "checklist": r.try_get::<Value,_>("checklist").ok(),
        "outcome_kind": r.try_get::<String,_>("outcome_kind").ok(),
        "report": r.try_get::<String,_>("report").ok(),
        "signed_by": r.try_get::<String,_>("signed_by").ok(),
        "signed_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("signed_at").ok(),
        "site_address": r.try_get::<String,_>("site_address").ok(),
        "site_lat": r.try_get::<f64,_>("site_lat").ok(), "site_lon": r.try_get::<f64,_>("site_lon").ok(),
        // indicazioni d'accesso: solo a chi è autorizzato (assegnatario/lavoratore o pianificatore)
        "site_access": if is_planner(&s) || s.role=="lavoratore" { r.try_get::<String,_>("site_access").ok() } else { None }
    })).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct OrderAction { pub version: i32, pub user_id: Option<Uuid>, pub outcome: Option<String>, pub reason: Option<String> }

/// Valore (controllato dall'utente) per il campo SET della transizione: sempre BINDATO, mai interpolato.
enum SetVal { Txt(String), Id(Uuid) }
async fn order_transition(st: &AppState, s: &Session, iid: Uuid, b: &OrderAction,
    to: &str, set_col: Option<&str>, set_val: Option<SetVal>, from_states: &[&str]) -> R {
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // version/state check dentro l'UPDATE (concorrenza ottimistica). `to`/`from_states` sono
    // costanti interne; il valore utente (outcome/reason/assigned_user) è bindato come $3.
    let states = from_states.iter().map(|x| format!("'{x}'")).collect::<Vec<_>>().join(",");
    let extra = set_col.map(|c| format!("{c}=$3, ")).unwrap_or_default();
    let sql = format!("UPDATE app.work_orders SET {extra}state='{to}', version=version+1, updated_at=now()
         WHERE id=$1 AND version=$2 AND state IN ({states})");
    let mut q = sqlx::query(&sql).bind(iid).bind(b.version);
    if let Some(v) = set_val { q = match v { SetVal::Txt(t) => q.bind(t), SetVal::Id(u) => q.bind(u) }; }
    let n = q.execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n == 0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT, "versione superata o stato non valido")); }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":to})))
}

pub async fn order_assign(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let uid = b.user_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "user_id richiesto"))?;
    order_transition(&st, &s, id, &b, "assegnato",
        Some("assigned_user"), Some(SetVal::Id(uid)), &["bozza","assegnato"]).await
}
pub async fn order_start(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    // only the assigned technician may start
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.work_orders WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
    let _ = tx.commit().await;
    if owner != Some(s.user_id) && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "non assegnato a te")); }
    let r = order_transition(&st, &s, id, &b, "in_corso", None, None, &["assegnato"]).await?;
    // avvio lavoro = timestamp dedicato (distinto dall'arrivo)
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let _ = sqlx::query("UPDATE app.work_orders SET work_started_at=COALESCE(work_started_at,now()) WHERE id=$1").bind(id).execute(&mut *tx).await;
    let _ = tx.commit().await;
    Ok(r)
}
pub async fn order_complete(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    let outcome = b.outcome.clone().unwrap_or_default();
    order_transition(&st, &s, id, &b, "completato", Some("outcome"), Some(SetVal::Txt(outcome)), &["in_corso"]).await
}
pub async fn order_cancel(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let reason = b.reason.clone().unwrap_or_default();
    order_transition(&st, &s, id, &b, "annullato", Some("cancel_reason"), Some(SetVal::Txt(reason)), &["bozza","assegnato","in_corso"]).await
}

// ---- Intervento end-to-end: report, allegati (foto/doc), firma ----
/// Chi può operare sull'esecuzione di un ordine: il pianificatore/admin oppure l'assegnatario.
async fn order_edit_allowed(st: &AppState, s: &Session, oid: Uuid) -> Result<Uuid,(StatusCode,String)> {
    let tid = s.tenant_id.unwrap();
    if !module_active(st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    let au: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.work_orders WHERE id=$1 AND tenant_id=$2")
        .bind(oid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "ordine inesistente"))?;
    if is_planner(&s) || au == Some(s.user_id) { Ok(tid) } else { Err(err(StatusCode::FORBIDDEN, "non autorizzato su questo ordine")) }
}
#[derive(Deserialize)] pub struct OrderReport {
    pub report: Option<String>, pub signed_by: Option<String>,
    #[serde(default)] pub checklist: Option<Value>, #[serde(default)] pub materials: Option<Value>,
    #[serde(default)] pub work_minutes: Option<i32>, #[serde(default)] pub outcome_kind: Option<String> }
/// Salvataggio esecuzione intervento (bozza/rapporto): report, firmatario, checklist, materiali,
/// minuti lavorati, tipo esito. Tutto opzionale e idempotente (COALESCE).
pub async fn order_report_set(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderReport>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_edit_allowed(&st, &s, id).await?;
    let signed_by = b.signed_by.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty());
    let signed_at = signed_by.map(|_| st.clock.now());
    let ok = b.outcome_kind.as_deref().filter(|x| matches!(*x, "completato"|"parziale"|"cliente_assente"|"accesso_impedito"|"ritorno"));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.work_orders SET
            report=COALESCE($2,report), signed_by=COALESCE($3,signed_by), signed_at=COALESCE($4,signed_at),
            checklist=COALESCE($5,checklist), materials=COALESCE($6,materials),
            work_minutes=COALESCE($7,work_minutes), outcome_kind=COALESCE($8,outcome_kind), updated_at=now()
         WHERE id=$1")
        .bind(id).bind(b.report.as_deref()).bind(signed_by).bind(signed_at)
        .bind(b.checklist.as_ref()).bind(b.materials.as_ref()).bind(b.work_minutes).bind(ok)
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_report_set", json!({"order":id,"signed":signed_by.is_some()})).await;
    Ok(Json(json!({"ok":true,"signed_at":signed_at.map(|t| t.to_rfc3339())})))
}
/// Registra l'ARRIVO sul posto (distinto dall'avvio lavoro). Assegnatario o pianificatore.
pub async fn order_arrive(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_edit_allowed(&st, &s, id).await?;
    let now = st.clock.now();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.work_orders SET arrived_at=COALESCE(arrived_at,$2), updated_at=now() WHERE id=$1 AND state IN ('assegnato','in_corso')")
        .bind(id).bind(now).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_arrive", json!({"order":id})).await;
    Ok(Json(json!({"ok":true,"arrived_at":now.to_rfc3339()})))
}
/// Upload allegato: body = bytes del file; metadati in query (kind, filename, caption); ct dall'header.
pub async fn order_attachment_add(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
    body: axum::body::Bytes) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_edit_allowed(&st, &s, id).await?;
    if body.len() < 4 { return Err(err(StatusCode::BAD_REQUEST, "file vuoto")); }
    if body.len() > 10_000_000 { return Err(err(StatusCode::BAD_REQUEST, "file troppo grande (max 10MB)")); }
    let kind = match q.get("kind").map(|s| s.as_str()) { Some("doc") => "doc", Some("signature") => "signature", _ => "photo" };
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
    let filename = q.get("filename").map(|x| x.trim().to_string());
    let caption = q.get("caption").map(|x| x.trim().to_string());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.work_order_attachments(tenant_id,work_order_id,kind,filename,content_type,caption,data,created_by)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(tid).bind(id).bind(kind).bind(&filename).bind(&ct).bind(&caption).bind(body.as_ref()).bind(&s.email)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_attachment_add", json!({"order":id,"kind":kind,"bytes":body.len()})).await;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id"), "kind": kind})))
}
pub async fn order_attachments_list(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_edit_allowed(&st, &s, id).await?;
    let rows = sqlx::query("SELECT id,kind,filename,content_type,caption,created_by,created_at FROM app.work_order_attachments WHERE tenant_id=$1 AND work_order_id=$2 ORDER BY created_at")
        .bind(tid).bind(id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "kind": r.get::<String,_>("kind"),
        "filename": r.try_get::<String,_>("filename").ok(), "content_type": r.get::<String,_>("content_type"),
        "caption": r.try_get::<String,_>("caption").ok(), "created_by": r.try_get::<String,_>("created_by").ok(),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at").to_rfc3339() })).collect();
    Ok(Json(json!(out)))
}
pub async fn order_attachment_get(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>)
    -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let row = sqlx::query("SELECT data,content_type FROM app.work_order_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    let data: Vec<u8> = row.get("data"); let ct: String = row.get("content_type");
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "private, max-age=3600".to_string())], data).into_response())
}
pub async fn order_attachment_del(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    // il pianificatore/admin o chi l'ha caricato
    let owner: Option<String> = sqlx::query_scalar("SELECT created_by FROM app.work_order_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    if !is_planner(&s) && owner.as_deref() != Some(s.email.as_str()) { return Err(err(StatusCode::FORBIDDEN, "non autorizzato")); }
    sqlx::query("DELETE FROM app.work_order_attachments WHERE id=$1 AND tenant_id=$2").bind(aid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_attachment_del", json!({"id":aid})).await;
    Ok(Json(json!({"ok":true})))
}

// ---- consuntivi ----
#[derive(Deserialize)]
pub struct NewTimesheet { pub work_order_id: Option<Uuid>, pub note: Option<String> }
pub async fn create_timesheet(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewTimesheet>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "consuntivi").await { return Err(err(StatusCode::FORBIDDEN, "modulo consuntivi non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.timesheets(tenant_id,work_order_id,user_id,note) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(b.work_order_id).bind(s.user_id).bind(&b.note).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id"),"state":"bozza"})))
}

pub async fn list_timesheets(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "consuntivi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = if is_approver(&s) || s.role=="auditor" {
        sqlx::query("SELECT t.id,t.state,t.created_at,u.email AS owner,o.title AS order_title
            FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
            ORDER BY t.updated_at DESC").fetch_all(&mut *tx).await
    } else {
        sqlx::query("SELECT t.id,t.state,t.created_at,u.email AS owner,o.title AS order_title
            FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
            WHERE t.user_id=$1 ORDER BY t.updated_at DESC").bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "state": r.get::<String,_>("state"),
        "owner": r.get::<String,_>("owner"), "order_title": r.try_get::<String,_>("order_title").ok() })).collect();
    Ok(Json(json!(out)))
}

async fn timesheet_detail(st: &AppState, tid: Uuid, id: Uuid) -> Result<Value, (StatusCode,String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let head = sqlx::query("SELECT t.id,t.state,t.note,t.user_id,u.email AS owner,o.title AS order_title
        FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id WHERE t.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "non trovato"))?;
    let lines = sqlx::query("SELECT id,activity,work_date,minutes,rate_cents,currency FROM app.timesheet_lines WHERE timesheet_id=$1 ORDER BY work_date")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let exps = sqlx::query("SELECT id,description,amount_cents,currency,has_receipt FROM app.expenses WHERE timesheet_id=$1")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // Per-tenant billing policy (consuntivi/rates): round billed minutes up, default currency.
    let rates = tool_config(st, tid, "consuntivi", "rates").await;
    let round = rates.get("round_minutes").and_then(|v| v.as_i64()).unwrap_or(15).max(1);
    let currency = rates.get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
    let mut minutes = 0i64; let mut billed = 0i64; let mut labor = 0i64; let mut missing_rate = false;
    let jl: Vec<Value> = lines.iter().map(|r| {
        let m = r.get::<i32,_>("minutes") as i64; minutes += m;
        let bm = if m > 0 { ((m + round - 1) / round) * round } else { 0 }; billed += bm;
        let rate: Option<i64> = r.try_get("rate_cents").ok();
        match rate { Some(rc) => labor += bm * rc / 60, None => missing_rate = true }
        json!({"id":r.get::<Uuid,_>("id"),"activity":r.get::<String,_>("activity"),
            "work_date":r.get::<chrono::NaiveDate,_>("work_date").to_string(),
            "minutes":m,"billed_minutes":bm,"rate_cents":rate,"currency":r.get::<String,_>("currency")})
    }).collect();
    let mut exp_total = 0i64;
    let je: Vec<Value> = exps.iter().map(|r| {
        let a = r.get::<i64,_>("amount_cents"); exp_total += a;
        json!({"id":r.get::<Uuid,_>("id"),"description":r.get::<String,_>("description"),
            "amount_cents":a,"currency":r.get::<String,_>("currency"),"has_receipt":r.get::<bool,_>("has_receipt")})
    }).collect();
    Ok(json!({
        "id": head.get::<Uuid,_>("id"), "state": head.get::<String,_>("state"),
        "owner": head.get::<String,_>("owner"), "owner_id": head.get::<Uuid,_>("user_id"),
        "order_title": head.try_get::<String,_>("order_title").ok(), "note": head.try_get::<String,_>("note").ok(),
        "lines": jl, "expenses": je, "currency": currency, "round_minutes": round,
        "total": {"minutes":minutes,"billed_minutes":billed,"labor_cents":labor,"expense_cents":exp_total,"total_cents":labor+exp_total,"missing_rate":missing_rate}
    }))
}

pub async fn get_timesheet(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let d = timesheet_detail(&st, s.tenant_id.unwrap(), id).await?;
    // owner or approver/auditor
    let owner_id = d["owner_id"].as_str().and_then(|x| Uuid::parse_str(x).ok());
    if owner_id != Some(s.user_id) && !is_approver(&s) && s.role != "auditor" {
        return Err(err(StatusCode::FORBIDDEN, "non autorizzato"));
    }
    Ok(Json(d))
}

#[derive(Deserialize)]
pub struct NewLine { pub activity: String, pub work_date: String, pub minutes: i32, pub rate_cents: Option<i64>, pub currency: Option<String> }
pub async fn add_line(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<NewLine>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let wd = chrono::NaiveDate::parse_from_str(&b.work_date, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "data non valida"))?;
    let def_cur = tool_config(&st, tid, "consuntivi", "rates").await
        .get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_lines(tenant_id,timesheet_id,activity,work_date,minutes,rate_cents,currency) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(tid).bind(id).bind(&b.activity).bind(wd).bind(b.minutes).bind(b.rate_cents)
        .bind(b.currency.clone().unwrap_or(def_cur)).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct NewExpense { pub description: String, pub amount_cents: i64, pub currency: Option<String>, pub has_receipt: Option<bool> }
pub async fn add_expense(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<NewExpense>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let def_cur = tool_config(&st, tid, "consuntivi", "rates").await
        .get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.expenses(tenant_id,timesheet_id,description,amount_cents,currency,has_receipt) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(tid).bind(id).bind(&b.description).bind(b.amount_cents)
        .bind(b.currency.clone().unwrap_or(def_cur)).bind(b.has_receipt.unwrap_or(false)).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

async fn guard_owner_editable(st: &AppState, tid: Uuid, id: Uuid, uid: Uuid) -> Result<(),(StatusCode,String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("SELECT user_id,state FROM app.timesheets WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let _ = tx.commit().await;
    let row = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "non trovato"))?;
    if row.get::<Uuid,_>("user_id") != uid { return Err(err(StatusCode::FORBIDDEN, "non tuo")); }
    let stt: String = row.get("state");
    if !matches!(stt.as_str(), "bozza"|"da_correggere") { return Err(err(StatusCode::CONFLICT, "non modificabile in questo stato")); }
    Ok(())
}

pub async fn submit_timesheet(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.timesheets SET state='in_revisione',updated_at=now() WHERE id=$1 AND user_id=$2 AND state IN ('bozza','da_correggere')")
        .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"non inviabile")); }
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'submit',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":"in_revisione"})))
}

pub async fn approve_timesheet(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_approver(&s) { return Err(err(StatusCode::FORBIDDEN, "solo responsabile")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // only from in_revisione -> blocks double approval
    let n = sqlx::query("UPDATE app.timesheets SET state='approvato',approved_by=$2,updated_at=now() WHERE id=$1 AND state='in_revisione'")
        .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"non in revisione (doppia approvazione bloccata)")); }
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'approve',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":"approvato"})))
}

#[derive(Deserialize)]
pub struct ReqChanges { pub note: String }
pub async fn request_changes(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ReqChanges>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_approver(&s) { return Err(err(StatusCode::FORBIDDEN, "solo responsabile")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.timesheets SET state='da_correggere',note=$2,updated_at=now() WHERE id=$1 AND state='in_revisione'")
        .bind(id).bind(&b.note).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"non in revisione")); }
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'request_changes',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"note":b.note})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":"da_correggere"})))
}

pub async fn export_timesheet_csv(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Response, (StatusCode,String)> {
    let s = require_tenant(&st, &h).await?;
    let d = timesheet_detail(&st, s.tenant_id.unwrap(), id).await?;
    let owner_id = d["owner_id"].as_str().and_then(|x| Uuid::parse_str(x).ok());
    if owner_id != Some(s.user_id) && !is_approver(&s) && s.role != "auditor" {
        return Err(err(StatusCode::FORBIDDEN, "non autorizzato"));
    }
    let cur = d["currency"].as_str().unwrap_or("EUR").to_string();
    let mut csv = String::from("tipo,descrizione,data,minuti,minuti_fatturati,tariffa_cent_ora,importo_cent,valuta,giustificativo\n");
    for l in d["lines"].as_array().unwrap_or(&vec![]) {
        let m = l["minutes"].as_i64().unwrap_or(0);
        let bm = l["billed_minutes"].as_i64().unwrap_or(m);
        let rate = l["rate_cents"].as_i64();
        let amt = rate.map(|r| bm*r/60);
        csv.push_str(&format!("riga,{},{},{},{},{},{},{},\n",
            csv_esc(l["activity"].as_str().unwrap_or("")), l["work_date"].as_str().unwrap_or(""), m, bm,
            rate.map(|x|x.to_string()).unwrap_or_default(), amt.map(|x|x.to_string()).unwrap_or_default(),
            l["currency"].as_str().unwrap_or(&cur)));
    }
    for e in d["expenses"].as_array().unwrap_or(&vec![]) {
        csv.push_str(&format!("spesa,{},,,,,{},{},{}\n",
            csv_esc(e["description"].as_str().unwrap_or("")), e["amount_cents"].as_i64().unwrap_or(0),
            e["currency"].as_str().unwrap_or(&cur), e["has_receipt"].as_bool().unwrap_or(false)));
    }
    let t = &d["total"];
    csv.push_str(&format!("TOTALE,,,{},{},,{},{},\n", t["minutes"].as_i64().unwrap_or(0), t["billed_minutes"].as_i64().unwrap_or(0), t["total_cents"].as_i64().unwrap_or(0), cur));
    let mut resp = csv.into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, "text/csv; charset=utf-8".parse().unwrap());
    resp.headers_mut().insert(header::CONTENT_DISPOSITION, format!("attachment; filename=consuntivo-{}.csv", id).parse().unwrap());
    Ok(resp)
}
fn csv_esc(s: &str) -> String { if s.contains(',')||s.contains('"') { format!("\"{}\"", s.replace('"',"\"\"")) } else { s.to_string() } }

// ======================= Prompt 7: assistente AI (sola lettura) =======================
// Il modello PROPONE un'operazione strutturata (intent); Rust la valida ed esegue con
// gli stessi servizi/permessi della UI. Tenant/identità dal contesto autenticato, MAI
// dagli argomenti del modello => impossibile una query cross-tenant. Nessun accesso del
// modello a SQL/shell/credenziali. Azioni critiche => rimando alla procedura manuale.
// Provider intercambiabile: qui pianificatore locale deterministico (profilo SIMULATO).

#[derive(Deserialize)]
pub struct AssistantReq { pub q: String }

pub async fn assistant_query(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AssistantReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "assistant").await {
        return Err(err(StatusCode::FORBIDDEN, "assistente non abilitato per questo tenant"));
    }
    let q = b.q.to_lowercase();
    // audit of the operational request (minimal data, not the raw prompt indiscriminately)
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'assistant_query',$3)")
        .bind(tid).bind(&s.email).bind(json!({"len":b.q.len()})).execute(&st.owner_pool).await;

    // --- critical-intent guardrail: never act, point to manual procedure (anche con AI on) ---
    let critical = ["chiudi","annulla","silenzia","spegni","disattiva","ferma","tacita"];
    let on_safety = ["sos","allarme","allarmi","protezione","escalation","reperib","emergenz","incident"];
    if critical.iter().any(|w| q.contains(w)) && on_safety.iter().any(|w| q.contains(w)) {
        return Ok(Json(json!({
            "provider":"SIMULATO","intent":"critical_refused",
            "answer":"Operazione critica sulla catena di sicurezza. L'assistente non la esegue: usa la procedura manuale autorizzata in centrale (presa in carico / chiusura con esito). Il percorso SOS non dipende dall'assistente.",
            "sources":[],"period":null })));
    }

    // Grounding deterministico tenant-scoped: fatti e fonti reali dal DB (mai dal modello).
    let mut plan = assistant_plan(&st, &s, &q).await?;

    // Se youworker.ai e' attivo per questo tenant, riformula in linguaggio naturale usando
    // ESCLUSIVAMENTE i fatti del grounding. Se la chiamata fallisce -> resta il deterministico.
    if ai_feature_on(&st, tid, "assistant").await {
        if let Some(c) = ai_conn(&st, tid).await {
            let sys = "Sei l'assistente operativo di YouWorkforce, piattaforma di sicurezza lavoratori. \
                Rispondi in italiano, breve e preciso. Usa SOLO i fatti forniti nel contesto: non inventare \
                numeri, eventi, nomi o siti. Se il contesto dice zero eventi, dillo chiaramente. Non proporre \
                ne' eseguire azioni sulla catena di sicurezza (SOS/allarmi/escalation): quelle restano manuali.";
            let ctx = format!("Domanda operatore: {}\n\nFatti verificati (grounding): {}",
                b.q, serde_json::to_string(&plan).unwrap_or_default());
            if let Ok(ans) = ai_chat(&st, &c, sys, &ctx).await {
                if let Value::Object(ref mut m) = plan {
                    m.insert("answer".into(), json!(ans));
                    m.insert("provider".into(), json!("youworker.ai"));
                }
            }
        }
    }
    Ok(Json(plan))
}

/// Pianificatore deterministico: instrada l'intento e restituisce fatti+fonti dal DB del
/// tenant (RLS). Nessun accesso del modello a SQL/credenziali; tenant dal contesto.
async fn assistant_plan(st: &AppState, s: &Session, q: &str) -> Result<Value, (StatusCode,String)> {
    let tid = s.tenant_id.unwrap();
    let has = |ws: &[&str]| ws.iter().any(|w| q.contains(w));
    let now = st.clock.now();

    // --- intent routing (local deterministic planner) ---
    if has(&["check-in","check in","checkin","scadut","mancato"]) {
        let site = match_site(st, tid, q).await?;
        let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let rows = sqlx::query(
            "SELECT i.id,i.correlation,i.opened_at,s.name AS site FROM app.incidents i
             LEFT JOIN app.sites s ON s.id=i.site_id
             WHERE i.etype='mancato_checkin' AND i.state<>'chiuso' AND ($1::uuid IS NULL OR i.site_id=$1)
             ORDER BY i.opened_at DESC").bind(site.0).fetch_all(&mut *tx).await.map_err(ise)?;
        tx.commit().await.map_err(ise)?;
        let srcs: Vec<Value> = rows.iter().map(|r| json!({"type":"incident","id":r.get::<Uuid,_>("id"),
            "site":r.try_get::<String,_>("site").ok(),"at":r.get::<chrono::DateTime<chrono::Utc>,_>("opened_at").to_rfc3339()})).collect();
        let ans = if rows.is_empty() {
            format!("Nessun check-in scaduto aperto{}. (zero eventi, non dati mancanti)", site.1.map(|n| format!(" al sito {n}")).unwrap_or_default())
        } else { format!("{} check-in scaduti aperti{}.", rows.len(), site.1.map(|n| format!(" al sito {n}")).unwrap_or_default()) };
        return Ok(json!({"provider":"SIMULATO","intent":"checkins_scaduti","answer":ans,"sources":srcs,"period":"incidenti aperti"}));
    }

    if has(&["sensor","offline","non comunica","comunica"]) {
        let hours: i64 = 1;
        let since = now - chrono::Duration::hours(hours);
        let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let rows = sqlx::query(
            "SELECT e.id,e.device_id,e.origin_ts,d.external_id FROM app.events e
             LEFT JOIN app.devices d ON d.id=e.device_id
             WHERE e.etype='device_offline' AND e.origin_ts>=$1 ORDER BY e.origin_ts DESC")
            .bind(since).fetch_all(&mut *tx).await.map_err(ise)?;
        tx.commit().await.map_err(ise)?;
        let srcs: Vec<Value> = rows.iter().map(|r| json!({"type":"event","id":r.get::<Uuid,_>("id"),
            "device":r.try_get::<String,_>("external_id").ok()})).collect();
        let ans = if rows.is_empty() { format!("Nessun sensore offline nell'ultima ora. (zero eventi)") }
                  else { format!("{} sensori senza comunicazione nell'ultima ora.", rows.len()) };
        return Ok(json!({"provider":"SIMULATO","intent":"sensori_offline","answer":ans,"sources":srcs,"period":format!("ultime {hours}h")}));
    }

    if has(&["sequenza","storia","cronologia","ultimo esito","allarme","incidente","ricostruisci"]) {
        let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let inc = sqlx::query("SELECT id,etype,state,correlation,outcome FROM app.incidents ORDER BY opened_at DESC LIMIT 1")
            .fetch_optional(&mut *tx).await.map_err(ise)?;
        let (ans, srcs) = if let Some(i) = inc {
            let iid: Uuid = i.get("id");
            let tl = sqlx::query("SELECT kind,at FROM app.incident_timeline WHERE incident_id=$1 ORDER BY at").bind(iid)
                .fetch_all(&mut *tx).await.map_err(ise)?;
            let seq: Vec<String> = tl.iter().map(|r| r.get::<String,_>("kind")).collect();
            (format!("Ultimo incidente: {} ({}). Sequenza: {}. Esito verificato: {}.",
                i.get::<String,_>("etype"), i.get::<String,_>("state"), seq.join(" → "),
                i.try_get::<String,_>("outcome").ok().unwrap_or("nessuno".into())),
             vec![json!({"type":"incident","id":iid,"correlation":i.get::<String,_>("correlation")})])
        } else { ("Nessun incidente registrato.".into(), vec![]) };
        tx.commit().await.map_err(ise)?;
        return Ok(json!({"provider":"SIMULATO","intent":"incidente_storia","answer":ans,"sources":srcs,"period":"ultimo incidente"}));
    }

    if has(&["modul","degrad"]) {
        if !matches!(s.role.as_str(), "tenant_admin"|"auditor") {
            return Err(err(StatusCode::FORBIDDEN, "metadati moduli non consentiti a questo ruolo"));
        }
        let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let rows = sqlx::query("SELECT module_code,state FROM app.tenant_modules WHERE state IN ('degradato','in_disattivazione')")
            .fetch_all(&mut *tx).await.map_err(ise)?;
        tx.commit().await.map_err(ise)?;
        let srcs: Vec<Value> = rows.iter().map(|r| json!({"type":"module","code":r.get::<String,_>("module_code"),"state":r.get::<String,_>("state")})).collect();
        let ans = if rows.is_empty() { "Nessun modulo degradato.".to_string() } else { format!("{} moduli in stato degradato/in disattivazione.", rows.len()) };
        return Ok(json!({"provider":"SIMULATO","intent":"moduli_degradati","answer":ans,"sources":srcs,"period":"stato attuale"}));
    }

    if has(&["ordin","intervent","lavoro assegnat"]) {
        if !module_active(st, tid, "interventi").await {
            return Ok(json!({"provider":"SIMULATO","intent":"ordini_aperti","answer":"Modulo interventi non abilitato: dato non disponibile.","sources":[],"period":null}));
        }
        let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let rows = sqlx::query("SELECT id,title,state FROM app.work_orders WHERE state IN ('assegnato','in_corso') ORDER BY updated_at DESC").fetch_all(&mut *tx).await.map_err(ise)?;
        tx.commit().await.map_err(ise)?;
        let srcs: Vec<Value> = rows.iter().map(|r| json!({"type":"order","id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),"state":r.get::<String,_>("state")})).collect();
        let ans = if rows.is_empty() { "Nessun ordine attivo.".into() } else { format!("{} ordini attivi.", rows.len()) };
        return Ok(json!({"provider":"SIMULATO","intent":"ordini_aperti","answer":ans,"sources":srcs,"period":"attivi"}));
    }

    // no confident intent -> ask for clarification (never guess an operation)
    Ok(json!({"provider":"SIMULATO","intent":"clarify",
        "answer":"Non ho abbastanza elementi. Prova: «check-in scaduti al sito Milano», «sensori che non comunicano», «ricostruisci l'ultimo allarme», «ordini attivi», «moduli degradati».",
        "sources":[],"period":null}))
}

/// Resolve an optional site mentioned in the query to a tenant site id (within scope).
async fn match_site(st: &AppState, tid: Uuid, q: &str) -> Result<(Option<Uuid>, Option<String>), (StatusCode,String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,name FROM app.sites").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    for r in rows {
        let name: String = r.get("name");
        if q.contains(&name.to_lowercase()) { return Ok((Some(r.get::<Uuid,_>("id")), Some(name))); }
    }
    Ok((None, None))
}

// ============ Processi AI (youworker.ai) agganciati ai moduli ============
// Ogni endpoint: 1) verifica la feature (modulo assistant + connessione + toggle admin),
// 2) raccoglie fatti REALI dal tenant (RLS), 3) chiede al modello di SINTETIZZARE/PROPORRE
// usando solo quei fatti. Mai azioni sulla catena di sicurezza; proposte, non esecuzioni.

/// Cancello comune: feature attiva => connessione; altrimenti errore parlante.
async fn ai_gate(st: &AppState, tid: Uuid, feature: &str) -> Result<AiConn, (StatusCode,String)> {
    if !ai_feature_on(st, tid, feature).await {
        return Err(err(StatusCode::FORBIDDEN, "funzione AI non attiva: richiede youworker.ai configurato (superadmin) e la feature abilitata dall'admin del tenant"));
    }
    ai_conn(st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione youworker.ai non configurata"))
}
const AI_SAFE: &str = "Non proporre ne' eseguire azioni sulla catena di sicurezza (SOS, allarmi, escalation, reperibilita'): restano manuali in centrale. Usa solo i fatti forniti, non inventare.";

/// Sintesi operativa di un incidente (sola lettura): sequenza + esito + prossime verifiche.
pub async fn incident_ai_summary(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let c = ai_gate(&st, tid, "incident_summary").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let head = sqlx::query("SELECT i.etype,i.state,i.correlation,i.outcome,i.opened_at,i.priority,s.name AS site
        FROM app.incidents i LEFT JOIN app.sites s ON s.id=i.site_id WHERE i.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "incidente non trovato"))?;
    let tl = sqlx::query("SELECT kind,detail,at FROM app.incident_timeline WHERE incident_id=$1 ORDER BY at")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let timeline: Vec<Value> = tl.iter().map(|r| json!({"kind":r.get::<String,_>("kind"),
        "detail":r.try_get::<Value,_>("detail").ok(),"at":r.get::<Dt,_>("at").to_rfc3339()})).collect();
    let facts = json!({"etype":head.get::<String,_>("etype"),"stato":head.get::<String,_>("state"),
        "priorita":head.get::<i32,_>("priority"),"sito":head.try_get::<String,_>("site").ok(),
        "esito":head.try_get::<String,_>("outcome").ok(),
        "aperto_il":head.get::<Dt,_>("opened_at").to_rfc3339(),"sequenza":timeline});
    let sys = format!("Sei l'analista operativo di YouWorkforce. Produci una sintesi chiara e breve (max 6 righe) \
        dell'incidente: cosa e' successo, sequenza, stato/esito, e le prossime verifiche suggerite per la centrale. {AI_SAFE}");
    let out = match ai_chat(&st, &c, &sys, &serde_json::to_string(&facts).unwrap_or_default()).await {
        Ok(a) => a, Err(e) => return Err(err(StatusCode::BAD_GATEWAY, &e)),
    };
    audit(&st, tid, &s.email, "ai_incident_summary", json!({"id":id})).await;
    Ok(Json(json!({"provider":"youworker.ai","summary":out,"facts":facts})))
}

/// Proposta di regole sensori dai dati telemetrici recenti (NON applicata: l'admin la valuta).
pub async fn sensor_rules_suggest(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo l'admin del tenant")); }
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, &["iot","thingsboard"]).await?;
    let c = ai_gate(&st, tid, "sensor_suggest").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let evs = sqlx::query("SELECT etype,payload_ref,origin_ts FROM app.events
        WHERE payload_ref IS NOT NULL ORDER BY origin_ts DESC LIMIT 40")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    let rules = sqlx::query("SELECT metric,op,threshold,etype,enabled FROM app.sensor_rules ORDER BY sort")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let samples: Vec<Value> = evs.iter().map(|r| json!({"etype":r.get::<String,_>("etype"),
        "payload":r.try_get::<Value,_>("payload_ref").ok(),"ts":r.get::<Dt,_>("origin_ts").to_rfc3339()})).collect();
    let existing: Vec<Value> = rules.iter().map(|r| json!({"metric":r.get::<String,_>("metric"),
        "op":r.get::<String,_>("op"),"threshold":r.try_get::<f64,_>("threshold").ok(),
        "etype":r.get::<String,_>("etype"),"enabled":r.get::<bool,_>("enabled")})).collect();
    let facts = json!({"etype_ammessi":RULE_ETYPES,"operatori_ammessi":RULE_OPS,
        "regole_esistenti":existing,"telemetria_recente":samples});
    let sys = format!("Sei un ingegnere della sicurezza IoT di YouWorkforce. Dai dati telemetrici recenti e dalle \
        regole esistenti, proponi 1-5 regole sensori utili (metrica, operatore, soglia -> etype) usando SOLO \
        gli operatori e gli etype ammessi. Spiega in una riga il perche' di ciascuna. Non duplicare regole gia' \
        presenti. Chiarisci che sono PROPOSTE da validare e creare manualmente dall'admin. {AI_SAFE}");
    let out = match ai_chat(&st, &c, &sys, &serde_json::to_string(&facts).unwrap_or_default()).await {
        Ok(a) => a, Err(e) => return Err(err(StatusCode::BAD_GATEWAY, &e)),
    };
    audit(&st, tid, &s.email, "ai_sensor_suggest", json!({"samples":samples.len()})).await;
    Ok(Json(json!({"provider":"youworker.ai","suggestion":out,"observed":samples.len(),"existing_rules":existing.len()})))
}

/// Lettura anomalie check-in (sola lettura): pattern di mancati check-in recenti.
pub async fn checkin_ai_anomaly(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "loneworker").await?;
    let c = ai_gate(&st, tid, "checkin_anomaly").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT i.state,i.outcome,i.opened_at,s.name AS site FROM app.incidents i
        LEFT JOIN app.sites s ON s.id=i.site_id
        WHERE i.etype='mancato_checkin' AND i.opened_at > now()-interval '14 days'
        ORDER BY i.opened_at DESC LIMIT 60").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let events: Vec<Value> = rows.iter().map(|r| json!({"stato":r.get::<String,_>("state"),
        "esito":r.try_get::<String,_>("outcome").ok(),"sito":r.try_get::<String,_>("site").ok(),
        "quando":r.get::<Dt,_>("opened_at").to_rfc3339()})).collect();
    let facts = json!({"finestra":"ultimi 14 giorni","mancati_checkin":events});
    let sys = format!("Sei l'analista di sicurezza di YouWorkforce. Dai mancati check-in recenti, evidenzia pattern \
        o anomalie (siti/orari ricorrenti, concentrazioni) e suggerisci verifiche. Se zero eventi, dillo. {AI_SAFE}");
    let out = match ai_chat(&st, &c, &sys, &serde_json::to_string(&facts).unwrap_or_default()).await {
        Ok(a) => a, Err(e) => return Err(err(StatusCode::BAD_GATEWAY, &e)),
    };
    audit(&st, tid, &s.email, "ai_checkin_anomaly", json!({"count":events.len()})).await;
    Ok(Json(json!({"provider":"youworker.ai","analysis":out,"count":events.len()})))
}

/// Brief operativo per un ordine di intervento (proposta, non esecuzione).
pub async fn work_order_ai_assist(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let c = ai_gate(&st, tid, "interventi_assist").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let head = sqlx::query("SELECT o.title,o.state,o.instructions,o.priority,o.assigned_user,s.name AS site,u.email AS assignee
        FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.users u ON u.id=o.assigned_user WHERE o.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "ordine non trovato"))?;
    // il lavoratore puo' chiedere assistenza solo sui propri ordini
    let assignee_id: Option<Uuid> = head.try_get("assigned_user").ok().flatten();
    if s.role == "lavoratore" && assignee_id != Some(s.user_id) {
        return Err(err(StatusCode::FORBIDDEN, "non assegnato a questo ordine"));
    }
    let diary = sqlx::query("SELECT body,created_at FROM app.diary_entries WHERE work_order_id=$1 ORDER BY created_at DESC LIMIT 20")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let notes: Vec<Value> = diary.iter().map(|r| json!({"nota":r.get::<String,_>("body"),
        "quando":r.get::<Dt,_>("created_at").to_rfc3339()})).collect();
    let facts = json!({"titolo":head.get::<String,_>("title"),"stato":head.get::<String,_>("state"),
        "priorita":head.get::<i32,_>("priority"),"sito":head.try_get::<String,_>("site").ok(),
        "istruzioni":head.try_get::<String,_>("instructions").ok(),"diario":notes});
    let sys = format!("Sei un caposquadra di YouWorkforce. Dall'ordine e dalle note di diario, scrivi un brief \
        operativo breve: obiettivo, passi suggeriti, punti di attenzione sicurezza. E' una PROPOSTA d'aiuto, \
        non sostituisce le istruzioni ufficiali ne' esegue azioni. {AI_SAFE}");
    let out = match ai_chat(&st, &c, &sys, &serde_json::to_string(&facts).unwrap_or_default()).await {
        Ok(a) => a, Err(e) => return Err(err(StatusCode::BAD_GATEWAY, &e)),
    };
    audit(&st, tid, &s.email, "ai_work_order_assist", json!({"id":id})).await;
    Ok(Json(json!({"provider":"youworker.ai","brief":out})))
}

/// Sintesi leggibile di un consuntivo (ore/spese/fatturazione).
pub async fn consuntivo_ai_summary(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let c = ai_gate(&st, tid, "consuntivi_summary").await?;
    let d = timesheet_detail(&st, tid, id).await?;
    let owner_id = d["owner_id"].as_str().and_then(|x| Uuid::parse_str(x).ok());
    if owner_id != Some(s.user_id) && !is_approver(&s) && s.role != "auditor" {
        return Err(err(StatusCode::FORBIDDEN, "non autorizzato"));
    }
    let sys = format!("Sei un amministrativo di YouWorkforce. Riassumi il consuntivo in modo chiaro per un \
        approvatore: totale ore, spese, importo, eventuali tariffe mancanti o anomalie. {AI_SAFE}");
    let out = match ai_chat(&st, &c, &sys, &serde_json::to_string(&d).unwrap_or_default()).await {
        Ok(a) => a, Err(e) => return Err(err(StatusCode::BAD_GATEWAY, &e)),
    };
    audit(&st, tid, &s.email, "ai_consuntivo_summary", json!({"id":id})).await;
    Ok(Json(json!({"provider":"youworker.ai","summary":out})))
}

/// Diario vocale: trascrizione STT server-side via youworker.ai. NON crea la voce di diario
/// automaticamente: restituisce il testo che il lavoratore rivede e invia con /api/diary.
pub async fn diary_transcribe(State(st): State<AppState>, h: HeaderMap, body: axum::body::Bytes) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let c = ai_gate(&st, tid, "voice_diary").await?;
    if body.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "audio mancante")); }
    if body.len() > 20*1024*1024 { return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "audio troppo grande (max 20MB)")); }
    let mime = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("audio/webm").to_string();
    let fname = if mime.contains("mp3")||mime.contains("mpeg") { "audio.mp3" }
        else if mime.contains("wav") { "audio.wav" } else if mime.contains("m4a")||mime.contains("mp4") { "audio.m4a" }
        else if mime.contains("ogg") { "audio.ogg" } else { "audio.webm" };
    let text = match ai_stt(&st, &c, body.to_vec(), fname, &mime).await {
        Ok(t) => t, Err(e) => return Err(err(StatusCode::BAD_GATEWAY, &e)),
    };
    audit(&st, tid, &s.email, "ai_diary_transcribe", json!({"bytes":body.len()})).await;
    Ok(Json(json!({"provider":"youworker.ai","text":text})))
}

// ================= Agente AI: tool-calling che ESEGUE sul DB (non cosmetico) =================
// Il provider (youworker.ai o esterno) non solo risponde: analizza i dati, genera report e
// SCRIVE nelle tabelle tramite un catalogo di "tool" che Rust valida ed esegue con gli stessi
// servizi/permessi/RLS della UI. Invarianti: tenant/identita' dal contesto (mai dal modello);
// scritture a basso rischio eseguite subito (audit); azioni sensibili (crea ordine/regola)
// richiedono conferma dell'operatore; catena di sicurezza SEMPRE vietata all'agente.

#[derive(Clone)]
struct AgCall { id: String, name: String, args: Value }
struct AgReply { text: Option<String>, calls: Vec<AgCall> }
enum AgTurn { User(String), Assistant { text: Option<String>, calls: Vec<AgCall> }, ToolResult { id: String, content: String } }

struct ToolMeta { sensitive: bool, writes: bool }
fn ag_meta(name: &str) -> Option<ToolMeta> {
    match name {
        // lettura/analisi
        "query_incidents"|"query_checkins"|"query_events"|"query_work_orders"|"get_consuntivo"
        |"tb_list_devices"|"tb_device_telemetry"
        |"suggest_device_model"|"device_readiness"
            => Some(ToolMeta{sensitive:false, writes:false}),
        // scrittura a basso rischio -> esecuzione diretta (audit)
        "add_diary_entry"|"add_incident_note"|"add_timesheet_line"|"add_expense"|"save_report"
            => Some(ToolMeta{sensitive:false, writes:true}),
        // scrittura sensibile -> richiede conferma operatore
        "create_work_order"|"create_sensor_rule"|"tb_sync_devices"|"onboard_device"
            => Some(ToolMeta{sensitive:true, writes:true}),
        _ => None,
    }
}

/// Catalogo tool (name, descrizione, JSON-schema parametri). Unica fonte verso i due provider.
fn ag_catalog() -> Vec<(&'static str, &'static str, Value)> {
    let o = |props: Value, req: Value| json!({"type":"object","properties":props,"required":req});
    vec![
      ("query_incidents","Elenca gli incidenti del tenant (opz. filtro stato).",
        o(json!({"state":{"type":"string","enum":["aperto","preso_in_carico","chiuso","tutti"]},"limit":{"type":"integer"}}), json!([]))),
      ("query_checkins","Elenca i check-in attivi e quanti sono scaduti.", o(json!({}), json!([]))),
      ("query_events","Eventi recenti dei dispositivi (default ultime 24h).", o(json!({"hours":{"type":"integer"}}), json!([]))),
      ("query_work_orders","Elenca gli ordini di lavoro (opz. filtro stato).", o(json!({"state":{"type":"string"}}), json!([]))),
      ("get_consuntivo","Dettaglio di un consuntivo (ore/spese/totali) dato il suo id.", o(json!({"timesheet_id":{"type":"string"}}), json!(["timesheet_id"]))),
      ("add_diary_entry","Registra una nota di diario di lavoro.", o(json!({"body":{"type":"string"},"work_order_id":{"type":"string"}}), json!(["body"]))),
      ("add_incident_note","Aggiunge una nota alla timeline di un incidente.", o(json!({"incident_id":{"type":"string"},"text":{"type":"string"}}), json!(["incident_id","text"]))),
      ("add_timesheet_line","Aggiunge una riga a un consuntivo (minuti + attivita').", o(json!({"timesheet_id":{"type":"string"},"activity":{"type":"string"},"work_date":{"type":"string","description":"YYYY-MM-DD"},"minutes":{"type":"integer"},"rate_eur":{"type":"number"}}), json!(["timesheet_id","activity","work_date","minutes"]))),
      ("add_expense","Aggiunge una spesa a un consuntivo.", o(json!({"timesheet_id":{"type":"string"},"description":{"type":"string"},"amount_eur":{"type":"number"}}), json!(["timesheet_id","description","amount_eur"]))),
      ("save_report","Salva un report su DB (consultabile in seguito).", o(json!({"title":{"type":"string"},"kind":{"type":"string"},"body":{"type":"string"}}), json!(["title","body"]))),
      ("create_work_order","Crea una BOZZA di ordine di lavoro (richiede conferma operatore).", o(json!({"title":{"type":"string"},"instructions":{"type":"string"},"priority":{"type":"integer"}}), json!(["title"]))),
      ("create_sensor_rule","Crea una regola sensori (richiede conferma operatore).", o(json!({"metric":{"type":"string"},"op":{"type":"string"},"threshold":{"type":"number"},"etype":{"type":"string"},"label":{"type":"string"}}), json!(["metric","op","etype"]))),
      ("tb_list_devices","Elenca i device ThingsBoard specchiati nell'inventario.", o(json!({}), json!([]))),
      ("tb_device_telemetry","Ultima telemetria live di un device ThingsBoard (per nome).", o(json!({"device":{"type":"string"}}), json!(["device"]))),
      ("tb_sync_devices","Sincronizza i device ThingsBoard e pulla la telemetria (richiede conferma).", o(json!({}), json!([]))),
      ("suggest_device_model","Cerca nel CATALOGO il modello di dispositivo che corrisponde a una descrizione (produttore/modello/scopo) e mostra categoria, capacità e gli allarmi (etype) che genererebbe. Usalo per aiutare l'onboarding.",
        o(json!({"query":{"type":"string","description":"testo libero: produttore, modello, o scopo"},"category":{"type":"string","enum":["personale","veicolo","asset","sensore","smartphone"]}}), json!(["query"]))),
      ("device_readiness","Stato di prontezza di un dispositivo (registrazione/assegnazione/connessione/protezione + segnali dichiarati e verificati). Dai l'external_id/IMEI del device.",
        o(json!({"device_external_id":{"type":"string"}}), json!(["device_external_id"]))),
      ("onboard_device","Genera il Device Onboarding Kit per un modello a catalogo: crea device + credenziale di ingest + routing degli allarmi + (device personali) profilo di protezione. RICHIEDE CONFERMA dell'operatore. Fornisci model_code (da suggest_device_model), external_id/IMEI, e opz. worker_email, group_name, make_profile.",
        o(json!({"model_code":{"type":"string"},"external_id":{"type":"string"},"label":{"type":"string"},"worker_email":{"type":"string"},"group_name":{"type":"string"},"make_profile":{"type":"boolean"}}), json!(["model_code","external_id"]))),
    ]
}

// ---- esecuzione reale di un tool (stessi permessi/RLS della UI, audit) ----
async fn ag_run(st: &AppState, s: &Session, name: &str, a: &Value) -> Result<Value, String> {
    let tid = s.tenant_id.unwrap();
    let gs = |k: &str| a.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
    let gu = |k: &str| a.get(k).and_then(|v| v.as_str()).and_then(|x| Uuid::parse_str(x).ok());
    let gi = |k: &str| a.get(k).and_then(|v| v.as_i64());
    let gf = |k: &str| a.get(k).and_then(|v| v.as_f64());
    macro_rules! tx { () => { db::begin_tenant(&st.rt_pool, tid).await.map_err(|e| e.to_string())? } }
    match name {
        "query_incidents" => {
            let state = gs("state").unwrap_or_else(|| "tutti".into());
            let lim = gi("limit").unwrap_or(20).clamp(1,100);
            let mut t = tx!();
            let base = "SELECT i.id,i.etype,i.state,i.priority,i.outcome,i.opened_at,s.name AS site FROM app.incidents i LEFT JOIN app.sites s ON s.id=i.site_id";
            let rows = if state=="tutti" {
                sqlx::query(&format!("{base} ORDER BY i.opened_at DESC LIMIT {lim}")).fetch_all(&mut *t).await
            } else {
                sqlx::query(&format!("{base} WHERE i.state=$1 ORDER BY i.opened_at DESC LIMIT {lim}")).bind(&state).fetch_all(&mut *t).await
            }.map_err(|e| e.to_string())?;
            t.commit().await.ok();
            Ok(json!(rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"etype":r.get::<String,_>("etype"),
                "stato":r.get::<String,_>("state"),"priorita":r.get::<i32,_>("priority"),
                "sito":r.try_get::<String,_>("site").ok(),"esito":r.try_get::<String,_>("outcome").ok(),
                "aperto":r.get::<Dt,_>("opened_at").to_rfc3339()})).collect::<Vec<_>>()))
        }
        "query_checkins" => {
            let mut t = tx!();
            let rows = sqlx::query("SELECT c.state,c.due_at,u.email,s.name AS site FROM app.checkins c
                JOIN app.users u ON u.id=c.user_id LEFT JOIN app.sites s ON s.id=c.site_id
                WHERE c.state IN ('attivo','scaduto') ORDER BY c.due_at").fetch_all(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.ok();
            let scaduti = rows.iter().filter(|r| r.get::<String,_>("state")=="scaduto").count();
            Ok(json!({"totale":rows.len(),"scaduti":scaduti,"checkin":rows.iter().map(|r| json!({
                "lavoratore":r.get::<String,_>("email"),"stato":r.get::<String,_>("state"),
                "sito":r.try_get::<String,_>("site").ok(),"scadenza":r.get::<Dt,_>("due_at").to_rfc3339()})).collect::<Vec<_>>()}))
        }
        "query_events" => {
            let hours = gi("hours").unwrap_or(24).clamp(1,168);
            let mut t = tx!();
            let rows = sqlx::query("SELECT e.etype,e.origin_ts,d.external_id FROM app.events e
                LEFT JOIN app.devices d ON d.id=e.device_id WHERE e.origin_ts > now()-($1||' hours')::interval
                ORDER BY e.origin_ts DESC LIMIT 100").bind(hours.to_string()).fetch_all(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.ok();
            Ok(json!({"finestra_ore":hours,"eventi":rows.iter().map(|r| json!({"etype":r.get::<String,_>("etype"),
                "device":r.try_get::<String,_>("external_id").ok(),"ts":r.get::<Dt,_>("origin_ts").to_rfc3339()})).collect::<Vec<_>>()}))
        }
        "query_work_orders" => {
            let mut t = tx!();
            let rows = if let Some(stt)=gs("state") {
                sqlx::query("SELECT o.id,o.title,o.state,o.priority FROM app.work_orders o WHERE o.state=$1 ORDER BY o.updated_at DESC LIMIT 50").bind(stt).fetch_all(&mut *t).await
            } else {
                sqlx::query("SELECT o.id,o.title,o.state,o.priority FROM app.work_orders o ORDER BY o.updated_at DESC LIMIT 50").fetch_all(&mut *t).await
            }.map_err(|e| e.to_string())?;
            t.commit().await.ok();
            Ok(json!(rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"titolo":r.get::<String,_>("title"),
                "stato":r.get::<String,_>("state"),"priorita":r.get::<i32,_>("priority")})).collect::<Vec<_>>()))
        }
        "get_consuntivo" => {
            let id = gu("timesheet_id").ok_or("timesheet_id non valido")?;
            timesheet_detail(st, tid, id).await.map_err(|e| e.1)
        }
        "add_diary_entry" => {
            if !module_active(st, tid, "workforce").await { return Err("modulo workforce non abilitato".into()); }
            let body = gs("body").unwrap_or_default(); let body=body.trim();
            if body.is_empty() { return Err("nota vuota".into()); }
            let mut t = tx!();
            let row = sqlx::query("INSERT INTO app.diary_entries(tenant_id,user_id,work_order_id,body,source,idem_key)
                VALUES($1,$2,$3,$4,'ai',$5) RETURNING id").bind(tid).bind(s.user_id).bind(gu("work_order_id")).bind(body)
                .bind(Uuid::new_v4().to_string()).fetch_one(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.map_err(|e| e.to_string())?;
            audit(st, tid, &s.email, "ai_agent_diary", json!({"id":row.get::<Uuid,_>("id")})).await;
            Ok(json!({"ok":true,"diary_id":row.get::<Uuid,_>("id")}))
        }
        "add_incident_note" => {
            if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err("ruolo non abilitato alle note incidente".into()); }
            let iid = gu("incident_id").ok_or("incident_id non valido")?;
            let text = gs("text").unwrap_or_default(); let text=text.trim();
            if text.is_empty() { return Err("nota vuota".into()); }
            let mut t = tx!();
            let n = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail)
                SELECT $1,$2,'nota',$3 WHERE EXISTS(SELECT 1 FROM app.incidents WHERE id=$2)")
                .bind(tid).bind(iid).bind(json!({"body":text,"by":format!("{} (AI)", s.email)})).execute(&mut *t).await.map_err(|e| e.to_string())?.rows_affected();
            t.commit().await.map_err(|e| e.to_string())?;
            if n==0 { return Err("incidente inesistente".into()); }
            audit(st, tid, &s.email, "ai_agent_incident_note", json!({"incident_id":iid})).await;
            Ok(json!({"ok":true}))
        }
        "add_timesheet_line" => {
            let id = gu("timesheet_id").ok_or("timesheet_id non valido")?;
            guard_owner_editable(st, tid, id, s.user_id).await.map_err(|e| e.1)?;
            let wd = chrono::NaiveDate::parse_from_str(&gs("work_date").unwrap_or_default(), "%Y-%m-%d").map_err(|_| "data non valida (YYYY-MM-DD)")?;
            let act = gs("activity").unwrap_or_default(); if act.trim().is_empty() { return Err("attivita' richiesta".into()); }
            let minutes = gi("minutes").unwrap_or(0) as i32; if minutes<=0 { return Err("minuti > 0".into()); }
            let rate_cents = gf("rate_eur").map(|r| (r*100.0).round() as i64);
            let cur = tool_config(st, tid, "consuntivi", "rates").await.get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
            let mut t = tx!();
            sqlx::query("INSERT INTO app.timesheet_lines(tenant_id,timesheet_id,activity,work_date,minutes,rate_cents,currency) VALUES($1,$2,$3,$4,$5,$6,$7)")
                .bind(tid).bind(id).bind(act.trim()).bind(wd).bind(minutes).bind(rate_cents).bind(cur).execute(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.map_err(|e| e.to_string())?;
            audit(st, tid, &s.email, "ai_agent_ts_line", json!({"timesheet_id":id})).await;
            Ok(json!({"ok":true}))
        }
        "add_expense" => {
            let id = gu("timesheet_id").ok_or("timesheet_id non valido")?;
            guard_owner_editable(st, tid, id, s.user_id).await.map_err(|e| e.1)?;
            let desc = gs("description").unwrap_or_default(); if desc.trim().is_empty() { return Err("descrizione richiesta".into()); }
            let amt = gf("amount_eur").ok_or("importo non valido")?; let cents=(amt*100.0).round() as i64;
            let cur = tool_config(st, tid, "consuntivi", "rates").await.get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
            let mut t = tx!();
            sqlx::query("INSERT INTO app.expenses(tenant_id,timesheet_id,description,amount_cents,currency,has_receipt) VALUES($1,$2,$3,$4,$5,false)")
                .bind(tid).bind(id).bind(desc.trim()).bind(cents).bind(cur).execute(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.map_err(|e| e.to_string())?;
            audit(st, tid, &s.email, "ai_agent_expense", json!({"timesheet_id":id})).await;
            Ok(json!({"ok":true}))
        }
        "save_report" => {
            let title = gs("title").unwrap_or_default(); if title.trim().is_empty() { return Err("titolo richiesto".into()); }
            let body = gs("body").unwrap_or_default(); if body.trim().is_empty() { return Err("corpo richiesto".into()); }
            let kind = gs("kind").unwrap_or_else(|| "generico".into());
            let id = ai_report_save(st, s, &kind, title.trim(), body.trim(), json!({"via":"agent"})).await.map_err(|e| e.1)?;
            Ok(json!({"ok":true,"report_id":id}))
        }
        "create_work_order" => {
            if !is_planner(&s) { return Err("solo pianificatore/admin puo' creare ordini".into()); }
            if !module_active(st, tid, "interventi").await { return Err("modulo interventi non abilitato".into()); }
            let title = gs("title").unwrap_or_default(); if title.trim().is_empty() { return Err("titolo richiesto".into()); }
            let pri = gi("priority").unwrap_or(2).clamp(1,3) as i32;
            let mut t = tx!();
            let row = sqlx::query("INSERT INTO app.work_orders(tenant_id,title,instructions,state,created_by,priority)
                VALUES($1,$2,$3,'bozza',$4,$5) RETURNING id").bind(tid).bind(title.trim()).bind(gs("instructions")).bind(s.user_id).bind(pri)
                .fetch_one(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.map_err(|e| e.to_string())?;
            audit(st, tid, &s.email, "ai_agent_create_order", json!({"id":row.get::<Uuid,_>("id")})).await;
            Ok(json!({"ok":true,"order_id":row.get::<Uuid,_>("id"),"state":"bozza"}))
        }
        "create_sensor_rule" => {
            admin_only(s).map_err(|e| e.1)?;
            let metric = gs("metric").unwrap_or_default(); if metric.trim().is_empty() { return Err("metrica richiesta".into()); }
            let op = gs("op").unwrap_or_default(); if !RULE_OPS.contains(&op.as_str()) { return Err("operatore non valido".into()); }
            let etype = gs("etype").unwrap_or_default(); if !RULE_ETYPES.contains(&etype.as_str()) { return Err("etype non ammesso".into()); }
            let numeric = !matches!(op.as_str(), "is_true"|"is_false");
            let thr = gf("threshold"); if numeric && thr.is_none() { return Err("soglia richiesta per questo operatore".into()); }
            let mut t = tx!();
            let row = sqlx::query("INSERT INTO app.sensor_rules(tenant_id,metric,op,threshold,etype,label,sort) VALUES($1,$2,$3,$4,$5,$6,100) RETURNING id")
                .bind(tid).bind(metric.trim()).bind(&op).bind(if numeric { thr } else { None }).bind(&etype).bind(gs("label"))
                .fetch_one(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.map_err(|e| e.to_string())?;
            audit(st, tid, &s.email, "ai_agent_create_rule", json!({"metric":metric,"op":op,"etype":etype})).await;
            Ok(json!({"ok":true,"rule_id":row.get::<Uuid,_>("id")}))
        }
        "tb_list_devices" => {
            if !module_active(st, tid, "thingsboard").await { return Err("modulo thingsboard non abilitato".into()); }
            let mut t = tx!();
            let rows = sqlx::query("SELECT external_id,label,config,updated_at FROM app.devices WHERE source='thingsboard' ORDER BY external_id")
                .fetch_all(&mut *t).await.map_err(|e| e.to_string())?;
            t.commit().await.ok();
            Ok(json!(rows.iter().map(|r| json!({"device":r.get::<String,_>("external_id"),
                "label":r.try_get::<String,_>("label").ok(),"tipo":r.try_get::<Value,_>("config").ok().and_then(|c| c.get("type").and_then(|v| v.as_str()).map(|s| s.to_string())),
                "aggiornato":r.get::<Dt,_>("updated_at").to_rfc3339()})).collect::<Vec<_>>()))
        }
        "tb_device_telemetry" => {
            if !module_active(st, tid, "thingsboard").await { return Err("modulo thingsboard non abilitato".into()); }
            let name = gs("device").unwrap_or_default(); if name.trim().is_empty() { return Err("device richiesto".into()); }
            let tele = tb_device_telemetry(st, tid, name.trim()).await?;
            Ok(json!({"device":name,"telemetria":tele}))
        }
        "tb_sync_devices" => {
            admin_only(s).map_err(|e| e.1)?;
            if !module_active(st, tid, "thingsboard").await { return Err("modulo thingsboard non abilitato".into()); }
            let (d,a) = tb_poll_now(st, tid).await?;
            audit(st, tid, &s.email, "ai_agent_tb_sync", json!({"devices":d,"alarms":a})).await;
            Ok(json!({"ok":true,"device_sincronizzati":d,"incidenti_alzati":a}))
        }
        // ---- Onboarding dispositivi (l'AI attore) ----
        "suggest_device_model" => {
            let q = gs("query").unwrap_or_default(); let q = q.trim();
            if q.is_empty() { return Err("query richiesta".into()); }
            let cat = gs("category");
            let like = format!("%{}%", q.to_lowercase());
            let rows = sqlx::query("SELECT model_code,manufacturer,model_name,protocol,category,capabilities,recommended
                 FROM app.device_models
                 WHERE ($2::text IS NULL OR category=$2)
                   AND (lower(manufacturer) LIKE $1 OR lower(model_name) LIKE $1 OR lower(model_code) LIKE $1 OR lower(category) LIKE $1)
                 ORDER BY recommended DESC NULLS LAST, manufacturer LIMIT 8")
                .bind(&like).bind(cat.as_deref()).fetch_all(&st.owner_pool).await.map_err(|e| e.to_string())?;
            let out: Vec<Value> = rows.iter().map(|r| {
                let caps: Vec<String> = r.get("capabilities");
                let ets: Vec<&str> = caps.iter().filter_map(|c| dok_cap_to_etype(c)).collect();
                json!({"model_code":r.get::<String,_>("model_code"),"manufacturer":r.get::<String,_>("manufacturer"),
                    "model_name":r.get::<String,_>("model_name"),"category":r.try_get::<String,_>("category").unwrap_or_default(),
                    "protocol":r.get::<String,_>("protocol"),"capabilities":caps,"etype_allarmi":ets})
            }).collect();
            Ok(json!({"matches":out,"nota":"Scegli il model_code e usa onboard_device (richiede conferma)."}))
        }
        "device_readiness" => {
            let ext = gs("device_external_id").unwrap_or_default();
            if ext.trim().is_empty() { return Err("device_external_id richiesto".into()); }
            let did: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.devices WHERE tenant_id=$1 AND external_id=$2")
                .bind(tid).bind(ext.trim()).fetch_optional(&st.owner_pool).await.map_err(|e| e.to_string())?;
            let Some(did) = did else { return Err("device non trovato".into()); };
            device_readiness_value(st, tid, did).await.map_err(|e| e.1)
        }
        "onboard_device" => {
            admin_only(s).map_err(|e| e.1)?;
            let model_code = gs("model_code").ok_or("model_code richiesto")?;
            let external_id = gs("external_id").ok_or("external_id richiesto")?;
            // risolvi lavoratore e gruppo per nome/email (comodo per l'AI)
            let user_id = if let Some(em) = gs("worker_email") {
                sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE tenant_id=$1 AND lower(email)=lower($2)")
                    .bind(tid).bind(em.trim()).fetch_optional(&st.owner_pool).await.map_err(|e| e.to_string())?
            } else { None };
            let group_id = if let Some(gn) = gs("group_name") {
                sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE tenant_id=$1 AND lower(name)=lower($2)")
                    .bind(tid).bind(gn.trim()).fetch_optional(&st.owner_pool).await.map_err(|e| e.to_string())?
            } else { None };
            let req = DokReq{ model_code, external_id, label: gs("label"), site_id:None, user_id, group_id,
                make_profile: a.get("make_profile").and_then(|v| v.as_bool()) };
            dok_build(st, tid, &s.email, req).await.map_err(|e| e.1)
        }
        _ => Err("tool sconosciuto".into()),
    }
}

async fn ai_report_save(st: &AppState, s: &Session, kind: &str, title: &str, body: &str, params: Value) -> Result<Uuid,(StatusCode,String)> {
    let tid = s.tenant_id.unwrap();
    let mut t = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.ai_reports(tenant_id,kind,title,body,params,created_by) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(kind).bind(title).bind(body).bind(params).bind(&s.email).fetch_one(&mut *t).await.map_err(ise)?;
    t.commit().await.map_err(ise)?;
    let id: Uuid = row.get("id");
    audit(st, tid, &s.email, "ai_report_save", json!({"id":id,"kind":kind})).await;
    Ok(id)
}

// ---- adattatori provider: serializzazione/parse del tool-calling ----
async fn ai_complete(st: &AppState, c: &AiConn, system: &str, turns: &[AgTurn]) -> Result<AgReply, String> {
    let tools = ag_catalog();
    if c.is_anthropic() {
        let mut msgs: Vec<Value> = Vec::new();
        let mut i = 0;
        while i < turns.len() {
            match &turns[i] {
                AgTurn::User(s) => { msgs.push(json!({"role":"user","content":[{"type":"text","text":s}]})); i+=1; }
                AgTurn::Assistant{text,calls} => {
                    let mut content: Vec<Value> = Vec::new();
                    if let Some(t)=text { if !t.is_empty() { content.push(json!({"type":"text","text":t})); } }
                    for c in calls { content.push(json!({"type":"tool_use","id":c.id,"name":c.name,"input":c.args})); }
                    msgs.push(json!({"role":"assistant","content":content})); i+=1;
                }
                AgTurn::ToolResult{..} => { // raggruppa i tool_result consecutivi in un unico messaggio user
                    let mut content: Vec<Value> = Vec::new();
                    while i < turns.len() { if let AgTurn::ToolResult{id,content:cnt} = &turns[i] {
                        content.push(json!({"type":"tool_result","tool_use_id":id,"content":cnt})); i+=1; } else { break; } }
                    msgs.push(json!({"role":"user","content":content}));
                }
            }
        }
        let tdefs: Vec<Value> = tools.iter().map(|(n,d,s)| json!({"name":n,"description":d,"input_schema":s})).collect();
        let body = json!({"model":c.model,"max_tokens":1024,"system":system,"messages":msgs,"tools":tdefs});
        let resp = st.http.post(ai_url(&c.base_url, &c.chat_path)).header("x-api-key", &c.api_key)
            .header("anthropic-version","2023-06-01").json(&body).send().await.map_err(|e| format!("provider AI irraggiungibile: {e}"))?;
        if !resp.status().is_success() { return Err(format!("provider AI HTTP {}", resp.status())); }
        let v: Value = resp.json().await.map_err(|e| format!("risposta non valida: {e}"))?;
        let mut text=None; let mut calls=Vec::new();
        if let Some(arr)=v.get("content").and_then(|c| c.as_array()) {
            let mut t=String::new();
            for b in arr { match b.get("type").and_then(|x| x.as_str()) {
                Some("text") => { if let Some(x)=b.get("text").and_then(|x| x.as_str()) { t.push_str(x); } }
                Some("tool_use") => { calls.push(AgCall{ id:b.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    name:b.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    args:b.get("input").cloned().unwrap_or(json!({})) }); }
                _=>{} } }
            if !t.trim().is_empty() { text=Some(t.trim().to_string()); }
        }
        return Ok(AgReply{text, calls});
    }
    // OpenAI-compatibile (youworker/openai/deepseek/custom)
    let mut msgs: Vec<Value> = vec![json!({"role":"system","content":system})];
    for t in turns { match t {
        AgTurn::User(s) => msgs.push(json!({"role":"user","content":s})),
        AgTurn::Assistant{text,calls} => {
            let tc: Vec<Value> = calls.iter().map(|c| json!({"id":c.id,"type":"function",
                "function":{"name":c.name,"arguments":serde_json::to_string(&c.args).unwrap_or("{}".into())}})).collect();
            let mut m = json!({"role":"assistant","content":text.clone()});
            if !tc.is_empty() { m["tool_calls"]=json!(tc); }
            msgs.push(m);
        }
        AgTurn::ToolResult{id,content} => msgs.push(json!({"role":"tool","tool_call_id":id,"content":content})),
    }}
    let tdefs: Vec<Value> = tools.iter().map(|(n,d,s)| json!({"type":"function","function":{"name":n,"description":d,"parameters":s}})).collect();
    let body = json!({"model":c.model,"temperature":0.1,"max_tokens":1024,"messages":msgs,"tools":tdefs});
    let resp = st.http.post(ai_url(&c.base_url, &c.chat_path)).bearer_auth(&c.api_key).json(&body).send().await
        .map_err(|e| format!("provider AI irraggiungibile: {e}"))?;
    if !resp.status().is_success() { return Err(format!("provider AI HTTP {}", resp.status())); }
    let v: Value = resp.json().await.map_err(|e| format!("risposta non valida: {e}"))?;
    let m = v.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message")).cloned().unwrap_or(json!({}));
    let text = m.get("content").and_then(|t| t.as_str()).filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string());
    let mut calls = Vec::new();
    if let Some(arr)=m.get("tool_calls").and_then(|t| t.as_array()) {
        for c in arr {
            let f = c.get("function").cloned().unwrap_or(json!({}));
            let args: Value = f.get("arguments").and_then(|a| a.as_str()).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(json!({}));
            calls.push(AgCall{ id:c.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                name:f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(), args });
        }
    }
    Ok(AgReply{text, calls})
}

const AG_SYSTEM: &str = "Sei l'agente operativo di YouWorkforce, piattaforma di sicurezza lavoratori. \
Aiuti l'operatore sul campo e in centrale: ANALIZZA i dati con i tool di lettura, poi AGISCI con i tool \
di scrittura per registrare informazioni (note di diario, note incidente, righe/spese di consuntivo, report). \
Usa SEMPRE i tool per leggere e scrivere: non inventare dati, non dire di aver fatto qualcosa senza il tool. \
Gli id (incidente, consuntivo, ordine) ricavali prima con i tool di lettura. Le azioni 'create_*' sono sensibili \
e verranno confermate dall'operatore prima di scrivere. \
ONBOARDING DISPOSITIVI: puoi aiutare a censire hardware: usa 'suggest_device_model' per trovare il modello a \
catalogo da una descrizione, 'device_readiness' per diagnosticare lo stato/prontezza di un device e dire cosa \
manca, e 'onboard_device' (sensibile, richiede conferma) per generare il kit completo (device+credenziale+routing+\
profilo) differenziando per tipologia d'uso. NON puoi in alcun modo agire sulla catena di sicurezza \
(chiudere/mettere a tacere/escalare allarmi o SOS, modificare reperibilita'): quelle restano manuali. \
Quando hai finito, rispondi in italiano riassumendo cosa hai letto e cosa hai scritto.";

#[derive(Deserialize)]
pub struct AgentReq { pub goal: String }
/// Ciclo agentico: il modello usa i tool; Rust li esegue (lettura + scrittura) con permessi/RLS.
pub async fn ai_agent(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AgentReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let c = ai_gate(&st, tid, "agent").await?;
    if b.goal.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "richiesta vuota")); }
    audit(&st, tid, &s.email, "ai_agent_goal", json!({"len":b.goal.len()})).await;
    let mut turns: Vec<AgTurn> = vec![AgTurn::User(b.goal.clone())];
    let mut actions: Vec<Value> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    let mut answer: Option<String> = None;
    for _step in 0..6u8 {
        let reply = ai_complete(&st, &c, AG_SYSTEM, &turns).await.map_err(|e| err(StatusCode::BAD_GATEWAY, &e))?;
        if reply.calls.is_empty() { answer = reply.text; break; }
        let calls = reply.calls.clone();
        turns.push(AgTurn::Assistant{ text: reply.text.clone(), calls: reply.calls });
        let mut stop = false;
        for call in &calls {
            // difesa: nomi sospetti sulla catena di sicurezza -> rifiuto esplicito
            let lname = call.name.to_lowercase();
            let danger = ["close","ack","escalat","silence","silenzia","delete","drop","module","imperson","connection","secret","goalert"];
            if danger.iter().any(|w| lname.contains(w)) {
                turns.push(AgTurn::ToolResult{ id: call.id.clone(), content: "VIETATO: l'agente non agisce sulla catena di sicurezza ne' su configurazioni; resta manuale.".into() });
                continue;
            }
            match ag_meta(&call.name) {
                None => turns.push(AgTurn::ToolResult{ id: call.id.clone(), content: "tool sconosciuto".into() }),
                Some(m) if m.sensitive => {
                    pending.push(json!({"name":call.name,"args":call.args,"reason":"azione sensibile: richiede conferma dell'operatore"}));
                    turns.push(AgTurn::ToolResult{ id: call.id.clone(), content: "In attesa di conferma dell'operatore; NON eseguito.".into() });
                    stop = true;
                }
                Some(m) => match ag_run(&st, &s, &call.name, &call.args).await {
                    Ok(v) => { actions.push(json!({"name":call.name,"args":call.args,"wrote":m.writes,"result":v}));
                        turns.push(AgTurn::ToolResult{ id: call.id.clone(), content: v.to_string() }); }
                    Err(e) => { actions.push(json!({"name":call.name,"args":call.args,"error":e}));
                        turns.push(AgTurn::ToolResult{ id: call.id.clone(), content: format!("errore: {e}") }); }
                },
            }
        }
        if stop { answer = reply.text; break; }
    }
    Ok(Json(json!({"provider":c.provider,"answer":answer,"actions":actions,"pending":pending})))
}

#[derive(Deserialize)]
pub struct AgentConfirm { pub name: String, pub args: Value }
/// Conferma ed esegue UNA azione sensibile proposta dall'agente (operatore autorizza).
pub async fn ai_agent_confirm(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AgentConfirm>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let _ = ai_gate(&st, tid, "agent").await?;
    match ag_meta(&b.name) {
        Some(m) if m.sensitive => {}
        _ => return Err(err(StatusCode::BAD_REQUEST, "azione non confermabile")),
    }
    let r = ag_run(&st, &s, &b.name, &b.args).await.map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
    Ok(Json(json!({"ok":true,"result":r})))
}

// ---- Report AI: generazione + persistenza + consultazione ----
#[derive(Deserialize)]
pub struct GenReportReq { pub prompt: String, pub kind: Option<String> }
pub async fn ai_report_generate(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GenReportReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let c = ai_gate(&st, tid, "agent").await?;
    // grounding: snapshot operativo del tenant (RLS)
    let mut t = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let inc = sqlx::query("SELECT etype,state,priority,opened_at FROM app.incidents WHERE opened_at > now()-interval '30 days' ORDER BY opened_at DESC LIMIT 100")
        .fetch_all(&mut *t).await.map_err(ise)?;
    let ck = sqlx::query("SELECT state,count(*) AS n FROM app.checkins GROUP BY state").fetch_all(&mut *t).await.map_err(ise)?;
    t.commit().await.ok();
    let facts = json!({
        "incidenti_30g": inc.iter().map(|r| json!({"etype":r.get::<String,_>("etype"),"stato":r.get::<String,_>("state"),
            "priorita":r.get::<i32,_>("priority"),"aperto":r.get::<Dt,_>("opened_at").to_rfc3339()})).collect::<Vec<_>>(),
        "checkin_per_stato": ck.iter().map(|r| json!({"stato":r.get::<String,_>("state"),"n":r.get::<i64,_>("n")})).collect::<Vec<_>>(),
    });
    let sys = "Sei un analista di YouWorkforce. Scrivi un report chiaro e strutturato in italiano (titolo breve, \
        poi corpo con sezioni e numeri) usando SOLO i fatti forniti. Non inventare. Non proporre azioni sulla catena di sicurezza.";
    let user = format!("Richiesta: {}\n\nFatti (grounding): {}", b.prompt, serde_json::to_string(&facts).unwrap_or_default());
    let body = ai_chat(&st, &c, sys, &user).await.map_err(|e| err(StatusCode::BAD_GATEWAY, &e))?;
    let kind = b.kind.unwrap_or_else(|| "generico".into());
    let title = b.prompt.chars().take(80).collect::<String>();
    let id = ai_report_save(&st, &s, &kind, &title, &body, json!({"prompt":b.prompt})).await?;
    Ok(Json(json!({"provider":c.provider,"id":id,"title":title,"kind":kind,"body":body})))
}
pub async fn ai_reports_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut t = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,kind,title,created_by,created_at FROM app.ai_reports ORDER BY created_at DESC LIMIT 100")
        .fetch_all(&mut *t).await.map_err(ise)?;
    t.commit().await.ok();
    Ok(Json(json!(rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"kind":r.get::<String,_>("kind"),
        "title":r.get::<String,_>("title"),"created_by":r.get::<String,_>("created_by"),
        "created_at":r.get::<Dt,_>("created_at").to_rfc3339()})).collect::<Vec<_>>())))
}
pub async fn ai_report_get(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut t = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let r = sqlx::query("SELECT id,kind,title,body,created_by,created_at FROM app.ai_reports WHERE id=$1")
        .bind(id).fetch_optional(&mut *t).await.map_err(ise)?;
    t.commit().await.ok();
    let r = r.ok_or_else(|| err(StatusCode::NOT_FOUND, "report non trovato"))?;
    Ok(Json(json!({"id":r.get::<Uuid,_>("id"),"kind":r.get::<String,_>("kind"),"title":r.get::<String,_>("title"),
        "body":r.get::<String,_>("body"),"created_by":r.get::<String,_>("created_by"),
        "created_at":r.get::<Dt,_>("created_at").to_rfc3339()})))
}

// SSE stream helper module
mod async_stream {
    use super::*;
    use tokio::sync::broadcast::Receiver;
    use tokio_stream::StreamExt;
    pub fn stream_hack(rx: Receiver<TenantEvent>, tenant: Uuid)
        -> impl Stream<Item = Result<SseEvent, Infallible>> {
        tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |msg| {
            match msg {
                Ok(te) if te.tenant_id == tenant =>
                    Some(Ok(SseEvent::default().data(te.payload.to_string()))),
                _ => None,
            }
        })
    }
}

// ============================================================================
// GoAlert parity: impostazioni reperibilita', recapiti+verifica, assenze,
// manutenzione, calendario iCal. Tutto pilotato da YouWorkforce (tenant_admin).
// ============================================================================

/// Sessione GoAlert del tenant o errore parlante (provisioning mancante).
#[allow(dead_code)]
async fn ga_session_or_err(st: &AppState, tid: Uuid) -> Result<(String,String,Option<String>),(StatusCode,String)> {
    goalert_session(st, tid).await
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "GoAlert non provisionato (chiedi al superadmin: Provisiona GoAlert) o credenziali non valide"))
}

// ---- Impostazioni reperibilita' (ladder vocale + status updates) ----
pub async fn oncall_settings_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let c = oncall_cfg(&st, tid).await;
    // gruppo di default (catch-all nativo) + motore corrente, per la UI Escalation
    let dg: Option<Uuid> = sqlx::query_scalar("SELECT default_group_id FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let engine: String = sqlx::query_scalar("SELECT escalation_engine FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or_else(|| "native".into());
    Ok(Json(json!({"voice_fallback_on":c.voice_on,"voice_fallback_min":c.voice_min,"status_updates_on":c.status_on,
        "default_group_id":dg,"engine":engine})))
}
#[derive(Deserialize)]
pub struct OncallSettingsReq { pub voice_fallback_on: Option<bool>, pub voice_fallback_min: Option<i32>, pub status_updates_on: Option<bool>,
    #[serde(default)] pub default_group_id: Option<Uuid>, #[serde(default)] pub clear_default_group: bool }
pub async fn oncall_settings_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OncallSettingsReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let cur = oncall_cfg(&st, tid).await;
    let voice_on = b.voice_fallback_on.unwrap_or(cur.voice_on);
    let voice_min = b.voice_fallback_min.unwrap_or(cur.voice_min).clamp(1, 60);
    let status_on = b.status_updates_on.unwrap_or(cur.status_on);
    // default group: validato nel tenant; clear_default_group lo azzera
    if let Some(g) = b.default_group_id {
        let ok = sqlx::query_scalar::<_, Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
            .bind(g).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "gruppo di default inesistente")); }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.tenant_oncall_cfg(tenant_id,voice_fallback_on,voice_fallback_min,status_updates_on) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id) DO UPDATE SET voice_fallback_on=EXCLUDED.voice_fallback_on,voice_fallback_min=EXCLUDED.voice_fallback_min,status_updates_on=EXCLUDED.status_updates_on,updated_at=now()")
        .bind(tid).bind(voice_on).bind(voice_min).bind(status_on).execute(&mut *tx).await.map_err(ise)?;
    if b.clear_default_group {
        sqlx::query("UPDATE app.tenant_oncall_cfg SET default_group_id=NULL, updated_at=now() WHERE tenant_id=$1").bind(tid).execute(&mut *tx).await.map_err(ise)?;
    } else if let Some(g) = b.default_group_id {
        sqlx::query("UPDATE app.tenant_oncall_cfg SET default_group_id=$2, updated_at=now() WHERE tenant_id=$1").bind(tid).bind(g).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"voice_fallback_on":voice_on,"voice_fallback_min":voice_min,"status_updates_on":status_on})))
}

/// FASE 3 — motore di escalation del tenant (lettura, admin). 'goalert' = shadow; 'native' = cutover.
pub async fn escalation_engine_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let engine = sqlx::query_scalar::<_, String>("SELECT escalation_engine FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.unwrap_or_else(|| "native".into());
    Ok(Json(json!({"engine":engine,
        "note":"native=nativo autoritativo (consegna live+ack, default); goalert=GoAlert motore (nativo in shadow, rollback). GoAlert resta rete di sicurezza."})))
}
/// FASE 4 (prep) — se il tenant è in cutover `native` E ha un percorso di consegna nativo reale,
/// la notifica GoAlert va SOPPRESSA (niente doppio path). Guardia SOS: senza credenziali Twilio
/// live (né override esplicito) NON si sopprime → GoAlert continua a pagare (rete di sicurezza).
pub async fn esc_suppress_goalert(st: &AppState, tid: Uuid) -> bool {
    let engine = sqlx::query_scalar::<_, String>("SELECT escalation_engine FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    // default native: una riga mancante segue il nuovo default del prodotto.
    let native = engine.as_deref().unwrap_or("native") == "native";
    if !native { return false; }
    let has_live = crate::delivery::TwilioCfg::from_env().is_some();
    let force = std::env::var("YW_ESC_SUPPRESS_GOALERT").ok().as_deref() == Some("1");
    has_live || force
}

#[derive(Deserialize)] pub struct EngineReq { pub engine: String }
/// FASE 3 — cutover/rollback del motore (superadmin). native|goalert. Operazione sulla catena di
/// sicurezza: solo superadmin. Rollback = rimettere 'goalert' (immediato, GoAlert è sempre acceso).
pub async fn escalation_engine_set(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<EngineReq>) -> R {
    require_super(&st, &h).await?;
    if b.engine != "goalert" && b.engine != "native" {
        return Err(err(StatusCode::BAD_REQUEST, "engine deve essere 'goalert' o 'native'"));
    }
    sqlx::query("INSERT INTO app.tenant_oncall_cfg(tenant_id,escalation_engine) VALUES($1,$2) ON CONFLICT (tenant_id) DO UPDATE SET escalation_engine=EXCLUDED.escalation_engine,updated_at=now()")
        .bind(tid).bind(&b.engine).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "superadmin", "escalation_engine_set", json!({"engine":b.engine})).await;
    Ok(Json(json!({"ok":true,"engine":b.engine})))
}

// ---- Recapiti GoAlert dei membri (stato verifica) ----
pub async fn oncall_contacts(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // NATIVO: recapiti = telefono/email dell'utente + stato verifica (users.phone_verified_at).
    // Nessuna dipendenza da GoAlert. Membri = utenti presenti in almeno un gruppo di escalation.
    let rows = sqlx::query(
        "SELECT DISTINCT u.id,u.email,u.phone,u.phone_verified_at,u.phone_opt_out_at
           FROM app.users u JOIN app.alarm_group_members m ON m.user_id=u.id
          WHERE u.tenant_id=$1 ORDER BY u.email")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let phone: Option<String> = r.try_get("phone").ok().flatten();
        let verified: Option<chrono::DateTime<chrono::Utc>> = r.try_get("phone_verified_at").ok().flatten();
        let opt_out: Option<chrono::DateTime<chrono::Utc>> = r.try_get("phone_opt_out_at").ok().flatten();
        json!({"user_id":r.get::<Uuid,_>("id"),"email":r.get::<String,_>("email"),
               "phone":phone,"verified":verified.is_some(),"verified_at":verified,"opted_out":opt_out.is_some()})
    }).collect();
    Ok(Json(json!({"members":out,"engine":"native"})))
}
#[derive(Deserialize)] pub struct CmId { pub user_id: Uuid }
/// Invia un codice OTP al telefono dell'utente e lo memorizza (scadenza 10 min). Nativo.
pub async fn oncall_contact_verify(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CmId>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let phone: Option<String> = sqlx::query_scalar("SELECT phone FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let phone = phone.filter(|p| !p.trim().is_empty()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "l'utente non ha un numero di telefono"))?;
    let code: u32 = { use rand::Rng as _; rand::thread_rng().gen_range(100000..1000000) }; // 6 cifre
    let exp = st.clock.now() + chrono::Duration::minutes(10);
    sqlx::query("UPDATE app.users SET phone_verify_code=$1, phone_verify_expires=$2 WHERE id=$3 AND tenant_id=$4")
        .bind(code.to_string()).bind(exp).bind(b.user_id).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    let text = format!("YouWorkforce: codice di verifica {code} (valido 10 minuti).");
    let res = esc_send_sms_native(&st, &phone, &text).await;
    audit(&st, tid, "youworkforce", "oncall_contact_verify", json!({"user":b.user_id,"delivery":res.state})).await;
    Ok(Json(json!({"ok":true,"delivery":res.state,"note":"codice inviato via SMS al recapito"})))
}
#[derive(Deserialize)] pub struct CmVerify { pub user_id: Uuid, pub code: String }
/// Conferma l'OTP e marca il recapito come verificato. Nativo.
pub async fn oncall_contact_code(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CmVerify>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let row = sqlx::query("SELECT phone_verify_code,phone_verify_expires FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    let saved: Option<String> = row.try_get("phone_verify_code").ok().flatten();
    let exp: Option<chrono::DateTime<chrono::Utc>> = row.try_get("phone_verify_expires").ok().flatten();
    let saved = saved.ok_or_else(|| err(StatusCode::BAD_REQUEST, "nessun codice in attesa: richiedi prima l'invio"))?;
    if matches!(exp, Some(e) if st.clock.now() > e) { return Err(err(StatusCode::BAD_REQUEST, "codice scaduto: richiedi un nuovo invio")); }
    if saved.trim() != b.code.trim() { return Err(err(StatusCode::BAD_REQUEST, "codice non valido")); }
    sqlx::query("UPDATE app.users SET phone_verified_at=now(), phone_verify_code=NULL, phone_verify_expires=NULL WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "youworkforce", "oncall_contact_code", json!({"user":b.user_id})).await;
    Ok(Json(json!({"ok":true,"note":"recapito verificato"})))
}
/// Invia un SMS di prova al recapito dell'utente. Nativo.
pub async fn oncall_contact_test(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CmId>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let phone: Option<String> = sqlx::query_scalar("SELECT phone FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let phone = phone.filter(|p| !p.trim().is_empty()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "l'utente non ha un numero di telefono"))?;
    let res = esc_send_sms_native(&st, &phone, "YouWorkforce: messaggio di prova dei reperibili.").await;
    Ok(Json(json!({"ok":true,"delivery":res.state,"note":"messaggio di prova inviato"})))
}

// ---- F3: multi-recapito + regole di notifica per-utente (ladder a 2 livelli, parità GoAlert) ----
pub async fn user_cm_list(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let cms = sqlx::query("SELECT id,kind,value,label,verified_at,disabled_at FROM app.user_contact_methods WHERE tenant_id=$1 AND user_id=$2 ORDER BY created_at")
        .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let rules = sqlx::query("SELECT r.id,r.cm_id,r.delay_min,cm.kind,cm.value FROM app.user_notif_rules r JOIN app.user_contact_methods cm ON cm.id=r.cm_id WHERE r.tenant_id=$1 AND r.user_id=$2 ORDER BY r.delay_min")
        .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cmj: Vec<Value> = cms.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"kind":r.get::<String,_>("kind"),
        "value":r.get::<String,_>("value"),"label":r.get::<String,_>("label"),
        "verified":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("verified_at").ok().flatten().is_some(),
        "disabled":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("disabled_at").ok().flatten().is_some()})).collect();
    let rj: Vec<Value> = rules.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"cm_id":r.get::<Uuid,_>("cm_id"),
        "delay_min":r.get::<i32,_>("delay_min"),"kind":r.get::<String,_>("kind"),"value":r.get::<String,_>("value")})).collect();
    // G3: preferenza status-update dell'utente (null=eredita) + default tenant per l'effettivo
    let upref: Option<bool> = sqlx::query_scalar("SELECT status_updates_on FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let tdef = oncall_cfg(&st, tid).await.status_on;
    Ok(Json(json!({"methods":cmj,"rules":rj,"status_pref":upref,"status_effective":upref.unwrap_or(tdef)})))
}
#[derive(Deserialize)] pub struct CmAddReq { pub kind: String, pub value: String, #[serde(default)] pub label: String }
pub async fn user_cm_add(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<CmAddReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if !matches!(b.kind.as_str(), "sms"|"voice"|"email") { return Err(err(StatusCode::BAD_REQUEST, "kind: sms|voice|email")); }
    let val = b.value.trim();
    if val.is_empty() || val.len() > 200 { return Err(err(StatusCode::BAD_REQUEST, "valore recapito non valido")); }
    let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !ok { return Err(err(StatusCode::NOT_FOUND, "utente inesistente")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.user_contact_methods(tenant_id,user_id,kind,value,label) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(uid).bind(&b.kind).bind(val).bind(b.label.trim()).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn user_cm_del(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.user_contact_methods WHERE id=$1 AND tenant_id=$2").bind(cmid).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "recapito inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct RuleAddReq { pub cm_id: Uuid, pub delay_min: Option<i32> }
pub async fn user_rule_add(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<RuleAddReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let dm = b.delay_min.unwrap_or(0).clamp(0, 1440);
    // il contact method deve appartenere all'utente nel tenant
    let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.user_contact_methods WHERE id=$1 AND user_id=$2 AND tenant_id=$3")
        .bind(b.cm_id).bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !ok { return Err(err(StatusCode::BAD_REQUEST, "recapito non valido per l'utente")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.user_notif_rules(tenant_id,user_id,cm_id,delay_min) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(uid).bind(b.cm_id).bind(dm).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn user_rule_del(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.user_notif_rules WHERE id=$1 AND tenant_id=$2").bind(rid).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "regola inesistente")); }
    Ok(Json(json!({"ok":true})))
}
/// G1 — riabilita (admin) i recapiti di un utente in opt-out: toglie dal DNC telefono primario e
/// contact method; i recapiti vanno poi ri-verificati (OTP). Compliance: azione tracciata.
pub async fn user_reactivate(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // raccogli i valori dell'utente (telefono + CM) e togli ciascuno dal DNC globale
    let mut vals: Vec<String> = sqlx::query_scalar("SELECT value FROM app.user_contact_methods WHERE tenant_id=$1 AND user_id=$2")
        .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    if let Some(ph) = sqlx::query_scalar::<_,Option<String>>("SELECT phone FROM app.users WHERE id=$1 AND tenant_id=$2").bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten() {
        vals.push(ph);
    }
    for v in vals.iter().filter(|v| !v.trim().is_empty()) { esc_optout_clear(&st, v).await; }
    audit(&st, tid, "youworkforce", "contact_reactivate", json!({"user":uid})).await;
    Ok(Json(json!({"ok":true,"note":"recapiti riattivati: da ri-verificare"})))
}

// ---- G2: verifica OTP per singolo contact method (sms/voice/email) ----
async fn cm_fetch(st: &AppState, tid: Uuid, cmid: Uuid) -> Option<(String,String)> {
    sqlx::query("SELECT kind,value FROM app.user_contact_methods WHERE id=$1 AND tenant_id=$2")
        .bind(cmid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()
        .map(|r| (r.get::<String,_>("kind"), r.get::<String,_>("value")))
}
/// Invia un testo (es. OTP) sul canale del recapito, riusando gli adapter: sms/voice via Twilio o
/// sink, email via relay o sink. Nessuna dipendenza da GoAlert.
async fn esc_send_code_native(st: &AppState, kind: &str, value: &str, text: &str) -> crate::delivery::SinkResult {
    let from = std::env::var("YW_ESC_FROM").unwrap_or_else(|_| "+10000000000".into());
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    let relay = std::env::var("YW_ESC_SMTP_RELAY_URL").unwrap_or_default();
    let slack_token = std::env::var("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    let public_base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let live_cfg = crate::delivery::TwilioCfg::from_env();
    let ch = esc_channel_from_kind(kind);
    let body = crate::delivery::request_body(ch, &from, value, text, "verify", 0);
    esc_dispatch(st, ch, value, text, &body, live_cfg.is_some(), live_cfg.as_ref(), &slack_token, &relay, &sink, &public_base).await
}
pub async fn user_cm_verify(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let (kind, value) = cm_fetch(&st, tid, cmid).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "recapito inesistente"))?;
    if esc_is_opted_out(&st, &value).await { return Err(err(StatusCode::BAD_REQUEST, "recapito in opt-out: riabilitalo prima")); }
    let code: u32 = { use rand::Rng as _; rand::thread_rng().gen_range(100000..1000000) };
    let exp = st.clock.now() + chrono::Duration::minutes(10);
    sqlx::query("UPDATE app.user_contact_methods SET verify_code=$1, verify_expires=$2 WHERE id=$3 AND tenant_id=$4")
        .bind(code.to_string()).bind(exp).bind(cmid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    let res = esc_send_code_native(&st, &kind, &value, &format!("YouWorkforce: codice di verifica {code} (10 min).")).await;
    Ok(Json(json!({"ok":true,"delivery":res.state,"note":"codice inviato al recapito"})))
}
#[derive(Deserialize)] pub struct CmCodeReq { pub code: String }
pub async fn user_cm_code(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>, Json(b): Json<CmCodeReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let row = sqlx::query("SELECT verify_code,verify_expires FROM app.user_contact_methods WHERE id=$1 AND tenant_id=$2")
        .bind(cmid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "recapito inesistente"))?;
    let saved: Option<String> = row.try_get("verify_code").ok().flatten();
    let exp: Option<chrono::DateTime<chrono::Utc>> = row.try_get("verify_expires").ok().flatten();
    let saved = saved.ok_or_else(|| err(StatusCode::BAD_REQUEST, "nessun codice in attesa"))?;
    if matches!(exp, Some(e) if st.clock.now() > e) { return Err(err(StatusCode::BAD_REQUEST, "codice scaduto")); }
    if saved.trim() != b.code.trim() { return Err(err(StatusCode::BAD_REQUEST, "codice non valido")); }
    sqlx::query("UPDATE app.user_contact_methods SET verified_at=now(), verify_code=NULL, verify_expires=NULL WHERE id=$1 AND tenant_id=$2")
        .bind(cmid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"note":"recapito verificato"})))
}
pub async fn user_cm_test(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let (kind, value) = cm_fetch(&st, tid, cmid).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "recapito inesistente"))?;
    let res = esc_send_code_native(&st, &kind, &value, "YouWorkforce: messaggio di prova.").await;
    Ok(Json(json!({"ok":true,"delivery":res.state})))
}
// ---- G3: preferenza status-update per-utente (override del default tenant; null = eredita) ----
#[derive(Deserialize)] pub struct StatusPrefReq { pub on: Option<bool> }
pub async fn user_status_pref_set(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<StatusPrefReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.users SET status_updates_on=$2 WHERE id=$1 AND tenant_id=$3")
        .bind(uid).bind(b.on).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "utente inesistente")); }
    Ok(Json(json!({"ok":true,"status_updates_on":b.on})))
}

// ---- Assenze / sostituzioni (override sulla schedule del gruppo) ----
async fn group_schedule_id(st: &AppState, tid: Uuid, gid: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT goalert_schedule_id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten()
}
async fn ga_user_id(st: &AppState, tid: Uuid, uid: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT goalert_user_id FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten()
}
#[derive(Deserialize)] pub struct OverrideQuery { pub group_id: Uuid }
pub async fn oncall_overrides_list(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<OverrideQuery>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // Sorgente NATIVA (app.esc_overrides); nomi utente risolti localmente. GoAlert non richiesto.
    let rows = sqlx::query(
        "SELECT o.id,o.start_at,o.end_at,o.add_user,o.remove_user,
                au.email AS add_email, ru.email AS rem_email
           FROM app.esc_overrides o
           LEFT JOIN app.users au ON au.id=o.add_user
           LEFT JOIN app.users ru ON ru.id=o.remove_user
          WHERE o.tenant_id=$1 AND o.group_id=$2 ORDER BY o.start_at DESC")
        .bind(tid).bind(q.group_id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"),
        "start": r.get::<chrono::DateTime<chrono::Utc>,_>("start_at"),
        "end": r.get::<chrono::DateTime<chrono::Utc>,_>("end_at"),
        "add_user": r.try_get::<Option<Uuid>,_>("add_user").ok().flatten(),
        "add_email": r.try_get::<Option<String>,_>("add_email").ok().flatten(),
        "remove_user": r.try_get::<Option<Uuid>,_>("remove_user").ok().flatten(),
        "remove_email": r.try_get::<Option<String>,_>("rem_email").ok().flatten(),
    })).collect();
    Ok(Json(json!({"overrides":out})))
}
#[derive(Deserialize)]
pub struct OverrideReq { pub group_id: Uuid, pub start: String, pub end: String, pub add_user_id: Option<Uuid>, pub remove_user_id: Option<Uuid> }
pub async fn oncall_override_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.add_user_id.is_none() && b.remove_user_id.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "indica un sostituto (aggiungi) e/o la persona assente (rimuovi)"));
    }
    let start = chrono::DateTime::parse_from_rfc3339(&b.start).map_err(|_| err(StatusCode::BAD_REQUEST, "start: formato RFC3339"))?.with_timezone(&chrono::Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&b.end).map_err(|_| err(StatusCode::BAD_REQUEST, "end: formato RFC3339"))?.with_timezone(&chrono::Utc);
    if end <= start { return Err(err(StatusCode::BAD_REQUEST, "end deve essere dopo start")); }
    // utenti validati nel tenant (se indicati)
    for u in [b.add_user_id, b.remove_user_id].into_iter().flatten() {
        let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2")
            .bind(u).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "utente non appartiene al tenant")); }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.esc_overrides(tenant_id,group_id,start_at,end_at,add_user,remove_user) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.group_id).bind(start).bind(end).bind(b.add_user_id).bind(b.remove_user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let oid: Uuid = row.get("id");
    // GoAlert best-effort (rollback/rete di sicurezza): mirror se provisionato, senza bloccare.
    if let (Some(scid), Some((base,cookie,_))) = (group_schedule_id(&st, tid, b.group_id).await, goalert_session(&st, tid).await) {
        let mut fields = vec![format!("scheduleID:\"{scid}\""), format!("start:\"{}\"", esc_gql(&b.start)), format!("end:\"{}\"", esc_gql(&b.end))];
        if let Some(u) = b.add_user_id { if let Some(g) = ga_user_id(&st, tid, u).await { fields.push(format!("addUserID:\"{g}\"")); } }
        if let Some(u) = b.remove_user_id { if let Some(g) = ga_user_id(&st, tid, u).await { fields.push(format!("removeUserID:\"{g}\"")); } }
        let gq = format!("mutation{{createUserOverride(input:{{{}}}){{id}}}}", fields.join(","));
        let _ = goalert::graphql(&st.http, &base, &cookie, &gq).await;
    }
    audit(&st, tid, "youworkforce", "oncall_override_add", json!({"group":b.group_id,"id":oid})).await;
    Ok(Json(json!({"ok":true,"id":oid})))
}
#[derive(Deserialize)] pub struct OverrideDel { pub id: Uuid }
pub async fn oncall_override_del(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideDel>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_overrides WHERE id=$1 AND tenant_id=$2").bind(b.id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "override inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct OverrideUpdate { pub id: Uuid, pub start: String, pub end: String, pub add_user_id: Option<Uuid>, pub remove_user_id: Option<Uuid> }
/// Modifica un override esistente (edit in-place, parità GoAlert).
pub async fn oncall_override_update(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideUpdate>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.add_user_id.is_none() && b.remove_user_id.is_none() { return Err(err(StatusCode::BAD_REQUEST, "indica sostituto e/o assente")); }
    let start = chrono::DateTime::parse_from_rfc3339(&b.start).map_err(|_| err(StatusCode::BAD_REQUEST, "start: RFC3339"))?.with_timezone(&chrono::Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&b.end).map_err(|_| err(StatusCode::BAD_REQUEST, "end: RFC3339"))?.with_timezone(&chrono::Utc);
    if end <= start { return Err(err(StatusCode::BAD_REQUEST, "end deve essere dopo start")); }
    for u in [b.add_user_id, b.remove_user_id].into_iter().flatten() {
        if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(u).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::BAD_REQUEST, "utente non nel tenant")); }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.esc_overrides SET start_at=$3,end_at=$4,add_user=$5,remove_user=$6 WHERE id=$1 AND tenant_id=$2")
        .bind(b.id).bind(tid).bind(start).bind(end).bind(b.add_user_id).bind(b.remove_user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "override inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- F3: Temporary schedule (sostituzione a tempo, parità GoAlert) ----
pub async fn temp_schedule_list(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<OverrideQuery>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let rows = sqlx::query("SELECT id,start_at,end_at,blocks FROM app.esc_temp_schedules WHERE tenant_id=$1 AND group_id=$2 AND end_at>now() ORDER BY start_at")
        .bind(tid).bind(q.group_id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"start":r.get::<chrono::DateTime<chrono::Utc>,_>("start_at"),
        "end":r.get::<chrono::DateTime<chrono::Utc>,_>("end_at"),"blocks":r.try_get::<Value,_>("blocks").unwrap_or_else(|_| json!([]))})).collect();
    Ok(Json(json!({"schedules":out})))
}
#[derive(Deserialize)] pub struct TempBlock { pub user_id: Uuid, pub hours: f64 }
#[derive(Deserialize)] pub struct TempSchedReq { pub group_id: Uuid, pub start: String, pub end: String, pub blocks: Vec<TempBlock>, #[serde(default)] pub allow_gaps: bool }
pub async fn temp_schedule_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<TempSchedReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let start = chrono::DateTime::parse_from_rfc3339(&b.start).map_err(|_| err(StatusCode::BAD_REQUEST, "start: RFC3339"))?.with_timezone(&chrono::Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&b.end).map_err(|_| err(StatusCode::BAD_REQUEST, "end: RFC3339"))?.with_timezone(&chrono::Utc);
    if end <= start { return Err(err(StatusCode::BAD_REQUEST, "end deve essere dopo start")); }
    if b.blocks.is_empty() || b.blocks.len() > 100 { return Err(err(StatusCode::BAD_REQUEST, "1..100 blocchi")); }
    let mut total_h = 0.0f64;
    for blk in &b.blocks {
        if blk.hours <= 0.0 || blk.hours > 24.0*366.0 { return Err(err(StatusCode::BAD_REQUEST, "ore blocco non valide")); }
        total_h += blk.hours;
        let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(blk.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "utente blocco non nel tenant")); }
    }
    // Allow gaps (parità GoAlert): se i blocchi non coprono tutta la finestra e non è consentito, rifiuta.
    let window_h = (end - start).num_seconds() as f64 / 3600.0;
    if !b.allow_gaps && total_h + 0.001 < window_h {
        return Err(err(StatusCode::BAD_REQUEST, "buco di copertura: i blocchi non coprono tutta la finestra (spunta 'consenti buchi')"));
    }
    let exists = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(b.group_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !exists { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    let blocks = serde_json::to_value(b.blocks.iter().map(|x| json!({"user_id":x.user_id,"hours":x.hours})).collect::<Vec<_>>()).unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.esc_temp_schedules(tenant_id,group_id,start_at,end_at,blocks,allow_gaps) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.group_id).bind(start).bind(end).bind(&blocks).bind(b.allow_gaps).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let id: Uuid = row.get("id");
    audit(&st, tid, "youworkforce", "temp_schedule_add", json!({"group":b.group_id,"id":id,"blocks":b.blocks.len()})).await;
    Ok(Json(json!({"ok":true,"id":id})))
}
pub async fn temp_schedule_del(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideDel>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_temp_schedules WHERE id=$1 AND tenant_id=$2").bind(b.id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "pianificazione inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- Maintenance Mode: sospende escalation dei servizi del tenant ----
#[derive(Deserialize)] pub struct MaintReq { pub hours: Option<i64> }
pub async fn oncall_maintenance(State(st): State<AppState>, h: HeaderMap, Json(b): Json<MaintReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let hours = b.hours.unwrap_or(0).clamp(0, 24);
    let exp = if hours > 0 { st.clock.now() + chrono::Duration::hours(hours) } else { st.clock.now() - chrono::Duration::minutes(1) };
    let exp_s = exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // Sorgente di verità NATIVA: il runner nativo (escalation_delivery_shadow_tenant) sospende
    // le consegne finché now() < maintenance_until. Indipendente da GoAlert.
    let until_db: Option<chrono::DateTime<chrono::Utc>> = if hours > 0 { Some(exp) } else { None };
    sqlx::query("INSERT INTO app.tenant_oncall_cfg(tenant_id,maintenance_until) VALUES($1,$2)
        ON CONFLICT (tenant_id) DO UPDATE SET maintenance_until=EXCLUDED.maintenance_until,updated_at=now()")
        .bind(tid).bind(until_db).execute(&st.owner_pool).await.map_err(ise)?;
    // GoAlert best-effort (rete di sicurezza): se provisionato, allinea i service; altrimenti ignora.
    let mut n = 0;
    if let Some((base, cookie, prov_sid)) = goalert_session(&st, tid).await {
        let mut sids: Vec<String> = sqlx::query_scalar("SELECT goalert_service_id FROM app.alarm_groups WHERE tenant_id=$1 AND goalert_service_id IS NOT NULL")
            .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
        if let Some(p) = prov_sid { if !sids.contains(&p) { sids.push(p); } }
        for sid in &sids {
            let gq = format!("mutation{{updateService(input:{{id:\"{}\",maintenanceExpiresAt:\"{}\"}})}}", esc_gql(sid), exp_s);
            if goalert::graphql(&st.http, &base, &cookie, &gq).await.map(|v| v.get("errors").is_none()).unwrap_or(false) { n += 1; }
        }
    }
    audit(&st, tid, "youworkforce", "oncall_maintenance", json!({"hours":hours,"goalert_services":n,"native":true})).await;
    Ok(Json(json!({"ok":true,"services":n,"until":if hours>0 {Some(exp_s)} else {None},"active":hours>0})))
}
/// Maintenance nativo attivo adesso? (now < maintenance_until). Usato dal runner nativo.
pub async fn maintenance_active(st: &AppState, tid: Uuid) -> bool {
    let until: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT maintenance_until FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    matches!(until, Some(u) if st.clock.now() < u)
}

// ---- Calendario reperibilita' (iCal), nativo YouWorkforce ----
/// Feed personale. `?rotate=1` rigenera il token (revoca il vecchio URL); `?group=<id>` nella .ics
/// filtra ai turni dell'utente in quel gruppo (sottoscrizione per-schedule self-service).
pub async fn oncall_calendar_url(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let uid = s.user_id;
    let rotate = q.get("rotate").map(|v| v=="1"||v=="true").unwrap_or(false);
    let existing: Option<String> = sqlx::query_scalar("SELECT token FROM app.oncall_ical_tokens WHERE tenant_id=$1 AND user_id=$2")
        .bind(tid).bind(uid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let token = match (existing, rotate) {
        (Some(t), false) => t,
        _ => { let t = auth::new_token();
            sqlx::query("INSERT INTO app.oncall_ical_tokens(tenant_id,user_id,token) VALUES($1,$2,$3) ON CONFLICT (tenant_id,user_id) DO UPDATE SET token=EXCLUDED.token")
                .bind(tid).bind(uid).bind(&t).execute(&st.owner_pool).await.map_err(ise)?; t }
    };
    let grp = q.get("group").map(|g| format!("&group={g}")).unwrap_or_default();
    Ok(Json(json!({"url":format!("/api/oncall-calendar.ics?token={token}{grp}"),"token":token})))
}
/// Revoca il feed personale (il vecchio URL smette di funzionare).
pub async fn oncall_calendar_revoke(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    sqlx::query("DELETE FROM app.oncall_ical_tokens WHERE tenant_id=$1 AND user_id=$2").bind(s.tenant_id.unwrap()).bind(s.user_id).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Abbonamento iCal PER-GRUPPO. `?rotate=1` rigenera il token.
pub async fn group_calendar_url(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    let rotate = q.get("rotate").map(|v| v=="1"||v=="true").unwrap_or(false);
    let existing: Option<String> = sqlx::query_scalar("SELECT token FROM app.esc_group_ical_tokens WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let token = match (existing, rotate) {
        (Some(t), false) => t,
        (old, _) => {
            if old.is_some() { sqlx::query("DELETE FROM app.esc_group_ical_tokens WHERE tenant_id=$1 AND group_id=$2").bind(tid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?; }
            let t = auth::new_token();
            sqlx::query("INSERT INTO app.esc_group_ical_tokens(token,tenant_id,group_id) VALUES($1,$2,$3)").bind(&t).bind(tid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?; t
        }
    };
    Ok(Json(json!({"url":format!("/api/group-calendar.ics?token={token}"),"token":token})))
}
/// Revoca il feed del gruppo.
pub async fn group_calendar_revoke(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    sqlx::query("DELETE FROM app.esc_group_ical_tokens WHERE tenant_id=$1 AND group_id=$2").bind(tid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct IcalQuery { pub token: String, #[serde(default)] pub group: Option<Uuid> }
pub async fn group_calendar_ics(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<IcalQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT tenant_id,group_id FROM app.esc_group_ical_tokens WHERE token=$1")
        .bind(&q.token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "token non valido").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let gid: Uuid = row.get("group_id");
    let name: String = sqlx::query_scalar("SELECT name FROM app.alarm_groups WHERE id=$1").bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or_else(|| "Gruppo".into());
    let now = chrono::Utc::now();
    let segs = esc_group_shifts(&st, tid, gid, now, 30).await;
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    let mut ics = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//YouWorkforce//Reperibilita//IT\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\nX-WR-CALNAME:Reperibilita {}\r\n", name.replace('\n'," "));
    for (i,(s0,e0,em)) in segs.iter().filter(|(_,_,e)| !e.is_empty()).enumerate() {
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:grp-{gid}-{i}-{}@youworkforce\r\n", fmt(*s0)));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(now)));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(*s0)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(*e0)));
        ics.push_str(&format!("SUMMARY:On-call: {}\r\n", em.join(", ").replace(',', "\\,")));
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");
    ([(axum::http::header::CONTENT_TYPE, "text/calendar; charset=utf-8")], ics).into_response()
}
pub async fn oncall_calendar_ics(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<IcalQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT tenant_id,user_id FROM app.oncall_ical_tokens WHERE token=$1")
        .bind(&q.token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "token non valido").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let uid: Uuid = row.get("user_id");
    // Con filtro ?group= la sottoscrizione mostra solo i turni calcolati di quel gruppo
    // (i turni manuali di app.oncall non sono associati a un gruppo).
    let shifts = if q.group.is_some() { vec![] } else {
        sqlx::query("SELECT id,starts_at,ends_at,note FROM app.oncall WHERE tenant_id=$1 AND user_id=$2 AND ends_at > now() - interval '7 days' ORDER BY starts_at")
            .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.unwrap_or_default()
    };
    let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//YouWorkforce//Reperibilita//IT\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\nX-WR-CALNAME:Reperibilita YouWorkforce\r\n");
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    for sh in &shifts {
        let id: Uuid = sh.get("id");
        let sdt: chrono::DateTime<chrono::Utc> = sh.get("starts_at");
        let edt: chrono::DateTime<chrono::Utc> = sh.get("ends_at");
        let note: Option<String> = sh.try_get("note").ok().flatten();
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:{id}@youworkforce\r\n"));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(chrono::Utc::now())));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(sdt)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(edt)));
        ics.push_str("SUMMARY:Reperibilita\r\n");
        if let Some(n) = note { if !n.is_empty() { ics.push_str(&format!("DESCRIPTION:{}\r\n", n.replace('\n'," ").replace(',', "\\,"))); } }
        ics.push_str("END:VEVENT\r\n");
    }
    // F3 — turni CALCOLATI dalla rotazione nativa (prossimi 30 giorni), non solo i turni manuali.
    let now = chrono::Utc::now();
    for (gi, (gname, s, e)) in esc_user_rotation_shifts_g(&st, tid, uid, now, 30, q.group).await.iter().enumerate() {
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:rot-{uid}-{gi}-{}@youworkforce\r\n", fmt(*s)));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(now)));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(*s)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(*e)));
        ics.push_str(&format!("SUMMARY:Reperibilita {}\r\n", gname.replace('\n'," ").replace(',', "\\,")));
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");
    ([(axum::http::header::CONTENT_TYPE, "text/calendar; charset=utf-8")], ics).into_response()
}

/// #5 Sync ack/chiusura da GoAlert -> incidente YW. Se il reperibile prende in carico o
/// chiude via SMS/voce/console GoAlert, lo stato dell'incidente YW lo riflette.
/// Girata periodicamente dal worker. Best-effort: un tenant giu' non blocca gli altri.
pub async fn goalert_ack_sync(st: &AppState) {
    let open = sqlx::query("SELECT tenant_id, id, goalert_alert_id FROM app.incidents WHERE state<>'chiuso' AND goalert_alert_id IS NOT NULL ORDER BY tenant_id LIMIT 200")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    if open.is_empty() { return; }
    // raggruppa per tenant per riusare la sessione GoAlert
    let mut by_tenant: std::collections::HashMap<Uuid, Vec<(Uuid,i32)>> = std::collections::HashMap::new();
    for r in &open {
        let tid: Uuid = r.get("tenant_id");
        let iid: Uuid = r.get("id");
        if let Ok(aid) = r.try_get::<i32,_>("goalert_alert_id") {
            by_tenant.entry(tid).or_default().push((iid, aid));
        }
    }
    for (tid, items) in by_tenant {
        let Some((base, cookie, _)) = goalert_session(st, tid).await else { continue; };
        for (iid, aid) in items {
            let v = goalert::graphql(&st.http, &base, &cookie, &format!("{{alert(id:{aid}){{status}}}}")).await;
            let status = v.as_ref().and_then(|v| v.pointer("/data/alert/status").and_then(|x| x.as_str())).unwrap_or("");
            match status {
                "StatusAcknowledged" => {
                    let n = sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE id=$1 AND state='aperto'")
                        .bind(iid).execute(&st.owner_pool).await.map(|r| r.rows_affected()).unwrap_or(0);
                    if n > 0 {
                        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'human_ack',$3)")
                            .bind(tid).bind(iid).bind("preso in carico dal reperibile (GoAlert: SMS/voce/console)").execute(&st.owner_pool).await;
                    }
                }
                "StatusClosed" => {
                    let n = sqlx::query("UPDATE app.incidents SET state='chiuso',closed_at=now(),version=version+1 WHERE id=$1 AND state<>'chiuso'")
                        .bind(iid).execute(&st.owner_pool).await.map(|r| r.rows_affected()).unwrap_or(0);
                    if n > 0 {
                        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'goalert_closed',$3)")
                            .bind(tid).bind(iid).bind("chiuso dal reperibile in GoAlert").execute(&st.owner_pool).await;
                    }
                }
                _ => {}
            }
        }
    }
}

// ============================================================================
// Traccar parity: event policy, report proxy, drivers, maintenance, computed
// attributes, accumulators. Tutto pilotato da YouWorkforce (tenant_admin).
// ============================================================================

/// Connessione Traccar del tenant o errore parlante.
async fn tc_conn_or_err(st: &AppState, tid: Uuid) -> Result<(String,String,String),(StatusCode,String)> {
    traccar_conn(st, tid).await
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione Traccar non configurata (chiedi al superadmin)"))
}
/// Risolve il nostro device uuid -> id numerico Traccar (via uniqueId).
async fn tc_device_num(st: &AppState, base: &str, user: &str, pass: &str, tid: Uuid, dev: Uuid) -> Result<i64,(StatusCode,String)> {
    let ext: Option<String> = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE id=$1 AND tenant_id=$2 AND source='traccar'")
        .bind(dev).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let ext = ext.ok_or_else(|| err(StatusCode::NOT_FOUND, "device Traccar inesistente"))?;
    let dl = st.http.get(traccar_url(base, &format!("/api/devices?uniqueId={ext}")))
        .basic_auth(user, Some(pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar non raggiungibile: {e}")))?;
    if !dl.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, "Traccar: auth/lookup fallito")); }
    let arr: Value = dl.json().await.map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar: json non valido"))?;
    arr.as_array().and_then(|a| a.first()).and_then(|d| d.get("id")).and_then(|v| v.as_i64())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "device non trovato su Traccar"))
}

// ---- Event policy (ignora|registra|critico per tipo evento Traccar) ----
pub async fn traccar_event_policy_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    let amap = tool_config(&st, tid, "traccar", "alarm_map").await;
    let events = amap.get("events").cloned().unwrap_or_else(|| json!({}));
    // ritorna, per ogni tipo noto, handling corrente o default
    let mut out = serde_json::Map::new();
    for &et in TRACCAR_POLICY_EVENTS {
        let (_, def) = traccar_event_default(et);
        let cur = events.get(et).and_then(|v| v.as_str()).unwrap_or(def);
        out.insert(et.to_string(), json!(cur));
    }
    Ok(Json(json!({"events":out})))
}
#[derive(Deserialize)] pub struct EventPolicyReq { pub events: std::collections::HashMap<String,String> }
pub async fn traccar_event_policy_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<EventPolicyReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "traccar").await?;
    // valida chiavi/valori
    for (k, v) in &b.events {
        if !TRACCAR_POLICY_EVENTS.contains(&k.as_str()) { return Err(err(StatusCode::BAD_REQUEST, &format!("tipo evento non valido: {k}"))); }
        if !matches!(v.as_str(), "ignora"|"registra"|"critico") { return Err(err(StatusCode::BAD_REQUEST, &format!("handling non valido per {k}: {v}"))); }
    }
    // merge dentro la config esistente alarm_map.events (preserva sos_as/battery_as ecc.)
    let cur: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='traccar' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let mut cfg = cur.and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut events = cfg.get("events").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    for (k, v) in &b.events { events.insert(k.clone(), json!(v)); }
    cfg.insert("events".into(), Value::Object(events));
    let cfg_v = Value::Object(cfg);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'traccar','alarm_map',$2,$3) ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config,updated_by=EXCLUDED.updated_by,updated_at=now()")
        .bind(tid).bind(&cfg_v).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "traccar_event_policy", json!({"n":b.events.len()})).await;
    Ok(Json(json!({"ok":true,"updated":b.events.len()})))
}

// ---- Report proxy (trips/stops/summary/route), opzionale CSV ----
#[derive(Deserialize)] pub struct TcReportQuery { pub device_id: Uuid, pub from: String, pub to: String, pub csv: Option<bool> }
async fn tc_report(st: &AppState, tid: Uuid, kind: &str, q: &TcReportQuery) -> Result<(bool,String),(StatusCode,String)> {
    if !matches!(kind, "trips"|"stops"|"summary"|"route") { return Err(err(StatusCode::BAD_REQUEST, "report non valido")); }
    let (base, user, pass) = tc_conn_or_err(st, tid).await?;
    let num = tc_device_num(st, &base, &user, &pass, tid, q.device_id).await?;
    let csv = q.csv.unwrap_or(false);
    let accept = if csv { "text/csv" } else { "application/json" };
    // from/to: ISO 8601 (YYYY-MM-DDTHH:MM:SSZ); valido in query senza encoding
    let fq = q.from.replace(' ', "");
    let tq = q.to.replace(' ', "");
    let url = traccar_url(&base, &format!("/api/reports/{kind}?deviceId={num}&from={fq}&to={tq}"));
    let r = st.http.get(&url).header("accept", accept).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar non raggiungibile: {e}")))?;
    if !r.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, &format!("Traccar report {}: HTTP {}", kind, r.status().as_u16()))); }
    let body = r.text().await.map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar: risposta non leggibile"))?;
    Ok((csv, body))
}
pub async fn traccar_report(State(st): State<AppState>, h: HeaderMap, Path(kind): Path<String>, axum::extract::Query(q): axum::extract::Query<TcReportQuery>) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let s = match require_tenant(&st, &h).await { Ok(s) => s, Err(e) => return e.into_response() };
    if !matches!(s.role.as_str(), "tenant_admin"|"operatore"|"auditor") { return err(StatusCode::FORBIDDEN, "ruolo non abilitato").into_response(); }
    let tid = s.tenant_id.unwrap();
    if let Err(e) = require_module(&st, tid, "traccar").await { return e.into_response(); }
    match tc_report(&st, tid, &kind, &q).await {
        Ok((true, body)) => ([(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")], body).into_response(),
        Ok((false, body)) => ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => e.into_response(),
    }
}

// ---- Drivers (proxy Traccar + link device) ----
pub async fn traccar_drivers_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.get(traccar_url(&base, "/api/drivers?all=true")).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!([]));
    Ok(Json(v))
}
#[derive(Deserialize)] pub struct DriverReq { pub name: String, pub unique_id: String, pub device_id: Option<Uuid> }
pub async fn traccar_driver_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DriverReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    if b.name.trim().is_empty() || b.unique_id.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome e identificativo richiesti")); }
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.post(traccar_url(&base, "/api/drivers")).basic_auth(&user, Some(&pass))
        .json(&json!({"name":b.name.trim(),"uniqueId":b.unique_id.trim()})).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    if !r.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, &format!("Traccar: HTTP {}", r.status().as_u16()))); }
    let created: Value = r.json().await.map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar: json non valido"))?;
    let did = created.get("id").and_then(|v| v.as_i64());
    if let (Some(did), Some(dev)) = (did, b.device_id) {
        if let Ok(num) = tc_device_num(&st, &base, &user, &pass, tid, dev).await {
            let _ = st.http.post(traccar_url(&base, "/api/permissions")).basic_auth(&user, Some(&pass))
                .json(&json!({"deviceId":num,"driverId":did})).send().await;
        }
    }
    audit(&st, tid, &s.email, "traccar_driver_add", json!({"name":b.name})).await;
    Ok(Json(created))
}
pub async fn traccar_driver_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let _ = st.http.delete(traccar_url(&base, &format!("/api/drivers/{id}"))).basic_auth(&user, Some(&pass)).send().await;
    Ok(Json(json!({"ok":true})))
}

// ---- Maintenance (proxy Traccar + link device) ----
pub async fn traccar_maint_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.get(traccar_url(&base, "/api/maintenance?all=true")).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!([]));
    Ok(Json(v))
}
#[derive(Deserialize)] pub struct MaintReqT { pub name: String, #[serde(rename="type")] pub kind: String, pub start: f64, pub period: f64, pub device_id: Option<Uuid> }
pub async fn traccar_maint_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<MaintReqT>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    if !matches!(b.kind.as_str(), "totalDistance"|"hours"|"fixTime"|"odometer") { return Err(err(StatusCode::BAD_REQUEST, "tipo: totalDistance|hours|fixTime|odometer")); }
    if b.period <= 0.0 { return Err(err(StatusCode::BAD_REQUEST, "periodo deve essere positivo")); }
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.post(traccar_url(&base, "/api/maintenance")).basic_auth(&user, Some(&pass))
        .json(&json!({"name":b.name.trim(),"type":b.kind,"start":b.start,"period":b.period})).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    if !r.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, &format!("Traccar: HTTP {}", r.status().as_u16()))); }
    let created: Value = r.json().await.map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar: json non valido"))?;
    let mid = created.get("id").and_then(|v| v.as_i64());
    if let (Some(mid), Some(dev)) = (mid, b.device_id) {
        if let Ok(num) = tc_device_num(&st, &base, &user, &pass, tid, dev).await {
            let _ = st.http.post(traccar_url(&base, "/api/permissions")).basic_auth(&user, Some(&pass))
                .json(&json!({"deviceId":num,"maintenanceId":mid})).send().await;
        }
    }
    audit(&st, tid, &s.email, "traccar_maint_add", json!({"name":b.name})).await;
    Ok(Json(created))
}
pub async fn traccar_maint_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let _ = st.http.delete(traccar_url(&base, &format!("/api/maintenance/{id}"))).basic_auth(&user, Some(&pass)).send().await;
    Ok(Json(json!({"ok":true})))
}

// ---- Computed attributes (proxy Traccar + link device) ----
pub async fn traccar_computed_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.get(traccar_url(&base, "/api/attributes/computed?all=true")).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!([]));
    Ok(Json(v))
}
#[derive(Deserialize)] pub struct ComputedReq { pub description: String, pub attribute: String, pub expression: String, #[serde(rename="type")] pub kind: String, pub device_id: Option<Uuid> }
pub async fn traccar_computed_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ComputedReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    if b.attribute.trim().is_empty() || b.expression.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "attributo ed espressione richiesti")); }
    if !matches!(b.kind.as_str(), "number"|"boolean"|"string") { return Err(err(StatusCode::BAD_REQUEST, "tipo: number|boolean|string")); }
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.post(traccar_url(&base, "/api/attributes/computed")).basic_auth(&user, Some(&pass))
        .json(&json!({"description":b.description,"attribute":b.attribute.trim(),"expression":b.expression.trim(),"type":b.kind})).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    if !r.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, &format!("Traccar: HTTP {} (espressione non valida?)", r.status().as_u16()))); }
    let created: Value = r.json().await.map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar: json non valido"))?;
    let aid = created.get("id").and_then(|v| v.as_i64());
    if let (Some(aid), Some(dev)) = (aid, b.device_id) {
        if let Ok(num) = tc_device_num(&st, &base, &user, &pass, tid, dev).await {
            let _ = st.http.post(traccar_url(&base, "/api/permissions")).basic_auth(&user, Some(&pass))
                .json(&json!({"deviceId":num,"attributeId":aid})).send().await;
        }
    }
    audit(&st, tid, &s.email, "traccar_computed_add", json!({"attribute":b.attribute})).await;
    Ok(Json(created))
}
pub async fn traccar_computed_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let _ = st.http.delete(traccar_url(&base, &format!("/api/attributes/computed/{id}"))).basic_auth(&user, Some(&pass)).send().await;
    Ok(Json(json!({"ok":true})))
}

// ---- Accumulators (lettura ultima posizione + reset) ----
pub async fn traccar_accumulators_get(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let num = tc_device_num(&st, &base, &user, &pass, tid, dev).await?;
    // leggi device -> positionId -> posizione -> attributi
    let dl = st.http.get(traccar_url(&base, &format!("/api/devices?id={num}"))).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let devs: Value = dl.json().await.unwrap_or_else(|_| json!([]));
    let pos_id = devs.as_array().and_then(|a| a.first()).and_then(|d| d.get("positionId")).and_then(|v| v.as_i64()).unwrap_or(0);
    let mut total = Value::Null; let mut hours = Value::Null;
    if pos_id > 0 {
        let pl = st.http.get(traccar_url(&base, &format!("/api/positions?id={pos_id}"))).basic_auth(&user, Some(&pass)).send().await;
        if let Ok(pl) = pl { if let Ok(arr) = pl.json::<Value>().await {
            if let Some(a) = arr.as_array().and_then(|a| a.first()).and_then(|p| p.get("attributes")) {
                total = a.get("totalDistance").cloned().unwrap_or(Value::Null);
                hours = a.get("hours").cloned().unwrap_or(Value::Null);
            }
        }}
    }
    Ok(Json(json!({"total_distance":total,"hours":hours})))
}
#[derive(Deserialize)] pub struct AccumSet { pub total_distance: Option<f64>, pub hours: Option<f64> }
pub async fn traccar_accumulators_set(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>, Json(b): Json<AccumSet>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let num = tc_device_num(&st, &base, &user, &pass, tid, dev).await?;
    let payload = json!({"deviceId":num,"totalDistance":b.total_distance.unwrap_or(0.0),"hours":b.hours.unwrap_or(0.0)});
    let r = st.http.put(traccar_url(&base, &format!("/api/devices/{num}/accumulators"))).basic_auth(&user, Some(&pass))
        .json(&payload).send().await.map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    if !r.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, &format!("Traccar: HTTP {}", r.status().as_u16()))); }
    audit(&st, tid, &s.email, "traccar_accumulators", json!({"device":dev})).await;
    Ok(Json(json!({"ok":true})))
}

// ============================================================================
// ThingsBoard parity: operatività lato tenant_admin (inventario, sync, alarm
// policy, attributi, RPC, shared-attr, storico). Connessione resta superadmin.
// ============================================================================

/// Sessione TB del tenant: (base, header Bearer) o errore parlante.
async fn tb_session_or_err(st: &AppState, tid: Uuid) -> Result<(String,String),(StatusCode,String)> {
    let (base, user, pass) = tb_conn(st, tid).await
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione ThingsBoard non configurata (chiedi al superadmin)"))?;
    let token = tb_login(&st.http, &base, &user, &pass).await
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "login ThingsBoard fallito"))?;
    Ok((base, format!("Bearer {token}")))
}
/// Risolve il nostro device uuid -> tb_id (da devices.config).
async fn tb_resolve_tbid(st: &AppState, tid: Uuid, dev: Uuid) -> Result<(String,String),(StatusCode,String)> {
    let row = sqlx::query("SELECT external_id,config FROM app.devices WHERE id=$1 AND tenant_id=$2 AND source='thingsboard'")
        .bind(dev).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "device ThingsBoard inesistente"))?;
    let name: String = row.get("external_id");
    let cfg: Value = row.try_get("config").unwrap_or(json!({}));
    let tbid = cfg.get("tb_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if tbid.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "device non ancora sincronizzato (manca tb_id): esegui Sincronizza")); }
    Ok((tbid, name))
}

// ---- Inventario device TB (tenant admin) ----
pub async fn tb_devices_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "thingsboard").await?;
    let rows = sqlx::query("SELECT id,external_id,label,config,updated_at FROM app.devices WHERE tenant_id=$1 AND source='thingsboard' ORDER BY external_id")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let devs: Vec<Value> = rows.iter().map(|r| {
        let cfg: Value = r.try_get("config").unwrap_or(json!({}));
        json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("external_id"),
            "label":r.try_get::<Option<String>,_>("label").ok().flatten(),
            "tb_id":cfg.get("tb_id").and_then(|v| v.as_str()),
            "tb_type":cfg.get("type").and_then(|v| v.as_str()),
            "updated_at":r.get::<chrono::DateTime<chrono::Utc>,_>("updated_at")})
    }).collect();
    let ps = sqlx::query("SELECT reachable,last_ok_at,device_count,alarm_count,last_error FROM app.thingsboard_poll_state WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let status = ps.map(|p| json!({"reachable":p.get::<bool,_>("reachable"),
        "last_ok_at":p.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("last_ok_at").ok().flatten(),
        "device_count":p.try_get::<Option<i64>,_>("device_count").ok().flatten(),
        "alarm_count":p.try_get::<Option<i64>,_>("alarm_count").ok().flatten(),
        "last_error":p.try_get::<Option<String>,_>("last_error").ok().flatten()})).unwrap_or(json!(null));
    Ok(Json(json!({"devices":devs,"status":status})))
}

// ---- Sync manuale (tenant admin) ----
pub async fn tb_sync_tenant(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "thingsboard").await?;
    match tb_poll_now(&st, tid).await {
        Ok((d,a)) => { audit(&st, tid, &s.email, "thingsboard_sync", json!({"devices":d,"alarms":a})).await;
            Ok(Json(json!({"ok":true,"device_count":d,"alarm_count":a}))) }
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, &e)),
    }
}

// ---- Alarm policy (severità -> ignora|registra|critico) ----
const TB_SEVERITIES: &[&str] = &["CRITICAL","MAJOR","MINOR","WARNING","INDETERMINATE"];
pub async fn tb_alarm_policy_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "thingsboard").await?;
    let pol = tb_alarm_policy(&st, tid).await;
    let mut out = serde_json::Map::new();
    for &sev in TB_SEVERITIES {
        let cur = pol.get(sev).and_then(|v| v.as_str()).unwrap_or_else(|| tb_sev_default(sev));
        out.insert(sev.to_string(), json!(cur));
    }
    Ok(Json(json!({"severity":out})))
}
#[derive(Deserialize)] pub struct TbAlarmPolicyReq { pub severity: std::collections::HashMap<String,String> }
pub async fn tb_alarm_policy_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<TbAlarmPolicyReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "thingsboard").await?;
    for (k,v) in &b.severity {
        if !TB_SEVERITIES.contains(&k.as_str()) { return Err(err(StatusCode::BAD_REQUEST, &format!("severità non valida: {k}"))); }
        if !matches!(v.as_str(), "ignora"|"registra"|"critico") { return Err(err(StatusCode::BAD_REQUEST, &format!("handling non valido per {k}"))); }
    }
    let cur: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='thingsboard' AND tool_code='severity_gate'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let mut cfg = cur.and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut sevm = cfg.get("tb_alarm_severity").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    for (k,v) in &b.severity { sevm.insert(k.clone(), json!(v)); }
    cfg.insert("tb_alarm_severity".into(), Value::Object(sevm));
    let cfg_v = Value::Object(cfg);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'thingsboard','severity_gate',$2,$3) ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config,updated_by=EXCLUDED.updated_by,updated_at=now()")
        .bind(tid).bind(&cfg_v).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "thingsboard_alarm_policy", json!({"n":b.severity.len()})).await;
    Ok(Json(json!({"ok":true,"updated":b.severity.len()})))
}

// ---- Attributi del device (tutte le scope) ----
pub async fn tb_device_attributes(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "thingsboard").await?;
    let (base, hdr) = tb_session_or_err(&st, tid).await?;
    let (tbid, _) = tb_resolve_tbid(&st, tid, dev).await?;
    let r = st.http.get(format!("{base}/api/plugins/telemetry/DEVICE/{tbid}/values/attributes"))
        .header("X-Authorization", &hdr).send().await.map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("ThingsBoard: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!([]));
    Ok(Json(v))
}

// ---- Config remota: scrive shared attributes ----
#[derive(Deserialize)] pub struct SharedAttrReq { pub attributes: Value }
pub async fn tb_shared_attr_set(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>, Json(b): Json<SharedAttrReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "thingsboard").await?;
    if !b.attributes.is_object() || b.attributes.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Err(err(StatusCode::BAD_REQUEST, "attributes: oggetto non vuoto richiesto"));
    }
    let (base, hdr) = tb_session_or_err(&st, tid).await?;
    let (tbid, _) = tb_resolve_tbid(&st, tid, dev).await?;
    let r = st.http.post(format!("{base}/api/plugins/telemetry/DEVICE/{tbid}/attributes/SHARED_SCOPE"))
        .header("X-Authorization", &hdr).json(&b.attributes).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("ThingsBoard: {e}")))?;
    if !r.status().is_success() { return Err(err(StatusCode::BAD_GATEWAY, &format!("ThingsBoard: HTTP {}", r.status().as_u16()))); }
    audit(&st, tid, &s.email, "thingsboard_shared_attr", json!({"device":dev})).await;
    Ok(Json(json!({"ok":true})))
}

// ---- RPC: comando al device (one-way / two-way) ----
#[derive(Deserialize)] pub struct TbRpcReq { pub method: String, pub params: Option<Value>, pub oneway: Option<bool> }
pub async fn tb_device_rpc(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>, Json(b): Json<TbRpcReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "thingsboard").await?;
    if b.method.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "method richiesto")); }
    let (base, hdr) = tb_session_or_err(&st, tid).await?;
    let (tbid, _) = tb_resolve_tbid(&st, tid, dev).await?;
    let way = if b.oneway.unwrap_or(true) { "oneway" } else { "twoway" };
    let body = json!({"method":b.method,"params":b.params.clone().unwrap_or(json!({})),"timeout":5000});
    let r = st.http.post(format!("{base}/api/plugins/rpc/{way}/{tbid}"))
        .header("X-Authorization", &hdr).json(&body).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("ThingsBoard: {e}")))?;
    let code = r.status().as_u16();
    let txt = r.text().await.unwrap_or_default();
    audit(&st, tid, &s.email, "thingsboard_rpc", json!({"device":dev,"method":b.method,"status":code})).await;
    // 409 = device offline (comando non consegnato): lo riportiamo senza fallire la richiesta
    Ok(Json(json!({"status":code,"ok":(200..300).contains(&code),"response":txt})))
}

// ---- Storico telemetria ----
#[derive(Deserialize)] pub struct TbHistoryQuery { pub keys: String, pub from: i64, pub to: i64, pub interval: Option<i64>, pub agg: Option<String> }
pub async fn tb_device_history(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>, axum::extract::Query(q): axum::extract::Query<TbHistoryQuery>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin"|"auditor") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "thingsboard").await?;
    let (base, hdr) = tb_session_or_err(&st, tid).await?;
    let (tbid, _) = tb_resolve_tbid(&st, tid, dev).await?;
    let keys = q.keys.replace(' ', "");
    let mut url = format!("{base}/api/plugins/telemetry/DEVICE/{tbid}/values/timeseries?keys={keys}&startTs={}&endTs={}&limit=5000", q.from, q.to);
    if let (Some(iv), Some(ag)) = (q.interval, q.agg.as_deref()) { url.push_str(&format!("&interval={iv}&agg={ag}")); }
    let r = st.http.get(&url).header("X-Authorization", &hdr).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("ThingsBoard: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!({}));
    Ok(Json(v))
}

// ============================================================================
// #8 Osservabilità: /metrics in formato Prometheus (aggregati, nessun dato per-tenant
// sensibile). Protezione opzionale via YW_METRICS_TOKEN (Bearer). GET -> CSRF-exempt.
// ============================================================================
pub async fn metrics(State(st): State<AppState>, h: HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Ok(tok) = std::env::var("YW_METRICS_TOKEN") {
        if !tok.is_empty() {
            let ok = h.get("authorization").and_then(|v| v.to_str().ok())
                .map(|s| s == format!("Bearer {tok}")).unwrap_or(false);
            if !ok { return (StatusCode::UNAUTHORIZED, "unauthorized").into_response(); }
        }
    }
    let c = |p: &sqlx::PgPool, sql: &'static str| {
        let p = p.clone();
        async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(&p).await.unwrap_or(0) }
    };
    let tenants   = c(&st.owner_pool, "SELECT count(*) FROM app.tenants").await;
    let users     = c(&st.owner_pool, "SELECT count(*) FROM app.users").await;
    let inc_open  = c(&st.owner_pool, "SELECT count(*) FROM app.incidents WHERE state<>'chiuso'").await;
    let inc_total = c(&st.owner_pool, "SELECT count(*) FROM app.incidents").await;
    let inc_crit  = c(&st.owner_pool, "SELECT count(*) FROM app.incidents WHERE state<>'chiuso' AND priority>=3").await;
    let ob_pending= c(&st.owner_pool, "SELECT count(*) FROM app.outbox WHERE state='pending'").await;
    let ob_error  = c(&st.owner_pool, "SELECT count(*) FROM app.outbox WHERE state='error'").await;
    let ob_stuck  = c(&st.owner_pool, "SELECT count(*) FROM app.outbox WHERE state='pending' AND attempts>=5").await;
    let sess      = c(&st.owner_pool, "SELECT count(*) FROM app.sessions WHERE expires_at>now()").await;
    let ck_active = c(&st.owner_pool, "SELECT count(*) FROM app.checkins WHERE state='attivo'").await;
    let db_ok: i64 = if sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&st.owner_pool).await.is_ok() {1} else {0};
    let ver = env!("CARGO_PKG_VERSION");
    let body = format!(
"# HELP yw_build_info Build info.
# TYPE yw_build_info gauge
yw_build_info{{version=\"{ver}\"}} 1
# HELP yw_up Core process health (1=db reachable).
# TYPE yw_up gauge
yw_up {db_ok}
# HELP yw_tenants_total Number of tenants.
# TYPE yw_tenants_total gauge
yw_tenants_total {tenants}
# HELP yw_users_total Number of users.
# TYPE yw_users_total gauge
yw_users_total {users}
# HELP yw_incidents_open Open incidents (state != chiuso).
# TYPE yw_incidents_open gauge
yw_incidents_open {inc_open}
# HELP yw_incidents_critical_open Open incidents with priority >= 3.
# TYPE yw_incidents_critical_open gauge
yw_incidents_critical_open {inc_crit}
# HELP yw_incidents_total Incidents ever created.
# TYPE yw_incidents_total counter
yw_incidents_total {inc_total}
# HELP yw_outbox_pending Outbox messages waiting delivery.
# TYPE yw_outbox_pending gauge
yw_outbox_pending {ob_pending}
# HELP yw_outbox_stuck Pending outbox messages retried >= 5 times.
# TYPE yw_outbox_stuck gauge
yw_outbox_stuck {ob_stuck}
# HELP yw_outbox_error Outbox messages in error state (non-critical dropped).
# TYPE yw_outbox_error gauge
yw_outbox_error {ob_error}
# HELP yw_sessions_active Active (non-expired) sessions.
# TYPE yw_sessions_active gauge
yw_sessions_active {sess}
# HELP yw_checkins_active Active check-ins.
# TYPE yw_checkins_active gauge
yw_checkins_active {ck_active}
");
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body).into_response()
}

// ============================================================================
// #7 GDPR minimo: export dati personali (diritto d'accesso) + anonimizzazione
// (diritto all'oblio, preservando integrità storica) + ritenzione (worker).
// Solo tenant_admin. Audit di ogni operazione.
// ============================================================================

/// Export dei dati personali di un utente del tenant (JSON). Diritto d'accesso.
pub async fn gdpr_export(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let u = sqlx::query("SELECT id,email,role,phone,created_at,anonymized_at FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    let profile = json!({
        "id": u.get::<Uuid,_>("id"), "email": u.get::<String,_>("email"), "role": u.get::<String,_>("role"),
        "phone": u.try_get::<Option<String>,_>("phone").ok().flatten(),
        "created_at": u.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
        "anonymized_at": u.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("anonymized_at").ok().flatten() });
    let jrows = |rows: Vec<sqlx::postgres::PgRow>, f: &dyn Fn(&sqlx::postgres::PgRow)->Value| rows.iter().map(f).collect::<Vec<_>>();
    let checkins = sqlx::query("SELECT id,state,created_at,due_at FROM app.checkins WHERE user_id=$1 AND tenant_id=$2 ORDER BY created_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let diary = sqlx::query("SELECT id,created_at,body FROM app.diary_entries WHERE user_id=$1 AND tenant_id=$2 ORDER BY created_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let oncall = sqlx::query("SELECT id,starts_at,ends_at,note FROM app.oncall WHERE user_id=$1 AND tenant_id=$2 ORDER BY starts_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let incidents = sqlx::query("SELECT id,etype,state,opened_at,closed_at FROM app.incidents WHERE assigned_user=$1 AND tenant_id=$2 ORDER BY opened_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    audit(&st, tid, &s.email, "gdpr_export", json!({"user":uid})).await;
    Ok(Json(json!({
        "profile": profile,
        "checkins": jrows(checkins, &|r| json!({"id":r.get::<Uuid,_>("id"),"state":r.get::<String,_>("state"),
            "created_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("created_at").ok().flatten(),
            "due_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("due_at").ok().flatten()})),
        "diary": jrows(diary, &|r| json!({"id":r.get::<Uuid,_>("id"),
            "created_at":r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
            "body":r.try_get::<Option<String>,_>("body").ok().flatten()})),
        "oncall": jrows(oncall, &|r| json!({"id":r.get::<Uuid,_>("id"),
            "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
            "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at"),
            "note":r.try_get::<Option<String>,_>("note").ok().flatten()})),
        "incidents_assigned": jrows(incidents, &|r| json!({"id":r.get::<Uuid,_>("id"),"etype":r.get::<String,_>("etype"),
            "state":r.get::<String,_>("state"),"opened_at":r.get::<chrono::DateTime<chrono::Utc>,_>("opened_at"),
            "closed_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("closed_at").ok().flatten()})),
        "exported_at": st.clock.now(), "note": "dati personali dell'utente nel tenant"
    })))
}

/// Anonimizza un utente (diritto all'oblio). Non elimina la riga per non rompere
/// l'integrità storica degli incidenti: scrubba la PII e blocca il login.
pub async fn gdpr_anonymize(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if uid == s.user_id { return Err(err(StatusCode::BAD_REQUEST, "non puoi anonimizzare te stesso")); }
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let role = role.ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    if role == "tenant_admin" {
        let admins: i64 = sqlx::query_scalar("SELECT count(*) FROM app.users WHERE tenant_id=$1 AND role='tenant_admin' AND anonymized_at IS NULL")
            .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
        if admins <= 1 { return Err(err(StatusCode::BAD_REQUEST, "non puoi anonimizzare l'ultimo admin")); }
    }
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM app.incidents WHERE assigned_user=$1 AND state<>'chiuso'")
        .bind(uid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    if open > 0 { return Err(err(StatusCode::CONFLICT, "l'utente ha incidenti aperti assegnati: riassegnali o chiudili prima")); }
    let anon_email = format!("anon-{}@anonymized.invalid", &uid.to_string()[..8]);
    let lock = auth::new_token();
    sqlx::query("UPDATE app.users SET email=$2, phone=NULL, goalert_user_id=NULL, pass_hash=$3, anonymized_at=now() WHERE id=$1 AND tenant_id=$4")
        .bind(uid).bind(&anon_email).bind(&lock).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    let _ = sqlx::query("UPDATE app.diary_entries SET body='[rimosso: anonimizzato]' WHERE user_id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).execute(&st.owner_pool).await;
    let _ = sqlx::query("DELETE FROM app.sessions WHERE user_id=$1").bind(uid).execute(&st.owner_pool).await;
    audit(&st, tid, &s.email, "gdpr_anonymize", json!({"user":uid})).await;
    Ok(Json(json!({"ok":true,"anonymized_email":anon_email})))
}

/// #7 Ritenzione: purga dati vecchi. Opt-in via YW_RETENTION_DAYS (0=off, default).
/// Conservativa: elimina incidenti CHIUSI + eventi non referenziati + audit_log oltre N giorni.
pub async fn purge_retention(st: &AppState) {
    let days: i64 = std::env::var("YW_RETENTION_DAYS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    if days <= 0 { return; }
    let cutoff = st.clock.now() - chrono::Duration::days(days);
    let _ = sqlx::query("DELETE FROM app.incident_timeline WHERE incident_id IN (SELECT id FROM app.incidents WHERE state='chiuso' AND closed_at < $1)")
        .bind(cutoff).execute(&st.owner_pool).await;
    let n_inc = sqlx::query("DELETE FROM app.incidents WHERE state='chiuso' AND closed_at < $1")
        .bind(cutoff).execute(&st.owner_pool).await.map(|r| r.rows_affected()).unwrap_or(0);
    let _ = sqlx::query("DELETE FROM app.events WHERE received_ts < $1 AND id NOT IN (SELECT event_id FROM app.incidents WHERE event_id IS NOT NULL)")
        .bind(cutoff).execute(&st.owner_pool).await;
    let _ = sqlx::query("DELETE FROM app.audit_log WHERE at < $1").bind(cutoff).execute(&st.owner_pool).await;
    if n_inc > 0 { tracing::info!("retention: purgati {} incidenti chiusi oltre {} giorni", n_inc, days); }
}

// ============================================================================
// #4 escalation engine nativo — FASE 1 (shadow). Calcola l'on-call col motore nativo
// (crate::escalation) in parallelo a GoAlert, confronta e logga. NESSUNA consegna:
// GoAlert resta il motore in produzione. Obiettivo: 0 divergenze prima del cutover.
// ============================================================================

/// RotationType nativo dal tipo on-call del gruppo. Monthly ora è calendar-aware (nessuna approssimazione).
fn esc_rot_kind(oc_type: &str) -> (crate::escalation::RotationType, i64) {
    use crate::escalation::RotationType as RT;
    match oc_type {
        "hourly" => (RT::Hourly, 1),
        "daily"  => (RT::Daily, 1),
        "monthly"=> (RT::Monthly, 1),
        _        => (RT::Weekly, 1),
    }
}
/// Costruisce la Schedule nativa dalla config del gruppo (stessi input di sync_goalert).
/// Gli override (assenze) sono tirati da GoAlert e innestati a valle da [`esc_goalert_overrides`]
/// (qui `overrides` parte vuoto; serve una query GraphQL async che non appartiene a questa fn pura).
fn esc_build_schedule(g: &sqlx::postgres::PgRow, members: &[Uuid]) -> crate::escalation::Schedule {
    let mode: String = g.try_get("oncall_mode").unwrap_or_else(|_| "tutti".into());
    let coverage: String = g.try_get("coverage_mode").unwrap_or_else(|_| "24_7".into());
    // fuso del gruppo (oncall_tz); i boundary rotation e le fasce di copertura sono in ora locale.
    let tz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
    let rotation = if mode == "turni" {
        let oc_type: String = g.try_get("oncall_type").unwrap_or_else(|_| "weekly".into());
        let oc_shift: i32 = g.try_get("oncall_shift").unwrap_or(1);
        let (kind, mul) = esc_rot_kind(&oc_type);
        let ht: Option<String> = g.try_get("handoff_time").ok().flatten();
        let hd: Option<i32> = g.try_get("handoff_day").ok().flatten();
        // ancora handoff come ora LOCALE nel fuso del gruppo, poi convertita in UTC.
        use chrono::TimeZone as _;
        let naive = chrono::NaiveDateTime::parse_from_str(&rotation_start(ht.as_deref(), hd), "%Y-%m-%dT%H:%M:%SZ")
            .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(2024,1,1).unwrap().and_hms_opt(0,0,0).unwrap());
        let start = match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(d) => d.with_timezone(&chrono::Utc),
            chrono::LocalResult::Ambiguous(a, _) => a.with_timezone(&chrono::Utc),
            chrono::LocalResult::None => { use chrono::TimeZone; chrono::Utc.from_utc_datetime(&naive) },
        };
        Some(crate::escalation::Rotation{ kind, shift_len: (oc_shift as i64).max(1) * mul, start, members: members.to_vec(), tz })
    } else { None };
    let fixed_users = if mode == "turni" { vec![] } else { members.to_vec() };
    let mut rules = vec![];
    if coverage == "orari" {
        let cs: Option<String> = g.try_get("cov_start").ok().flatten();
        let ce: Option<String> = g.try_get("cov_end").ok().flatten();
        let cd: String = g.try_get("cov_days").unwrap_or_else(|_| "1111111".into());
        let mins = |s: &str| parse_hhmm(s).map(|(h,m)| h*60+m);
        if let (Some(a), Some(b)) = (cs.as_deref().and_then(mins), ce.as_deref().and_then(mins)) {
            let mut mask: u8 = 0;
            for (i,c) in cd.chars().take(7).enumerate() { if c=='1' { mask |= 1 << i; } }
            rules.push(crate::escalation::ScheduleRule{ start_min: a, end_min: b, weekday_mask: mask, tz });
        }
    }
    crate::escalation::Schedule{ rotation, fixed_users, rules, overrides: vec![] }
}

/// Email on-call correnti in GoAlert per un servizio (onCallUsers.userName == email nostra).
async fn esc_goalert_oncall_emails(st: &AppState, base: &str, cookie: &str, sid: &str) -> Vec<String> {
    let q = format!("{{service(id:\"{sid}\"){{onCallUsers{{userName}}}}}}");
    goalert::graphql(&st.http, base, cookie, &q).await
        .and_then(|v| v.pointer("/data/service/onCallUsers").and_then(|a| a.as_array()).cloned())
        .map(|a| a.iter().filter_map(|u| u.get("userName").and_then(|x| x.as_str()).map(|s| s.to_lowercase())).collect())
        .unwrap_or_default()
}

/// Uuid sintetico deterministico da un'email (per un sostituto non membro del gruppo: serve
/// solo al confronto per-email dello shadow, non è un utente reale).
fn esc_synth_uuid(email: &str) -> Uuid {
    use std::hash::{Hash, Hasher};
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    email.hash(&mut h1); let a = h1.finish();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    ("yw-esc-synth", email).hash(&mut h2); let b = h2.finish();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_le_bytes());
    bytes[8..].copy_from_slice(&b.to_le_bytes());
    Uuid::from_bytes(bytes)
}

/// Override (assenze) attivi in GoAlert per lo schedule, convertiti nel modello nativo
/// [`crate::escalation::Override`]. Mappa gli utenti GoAlert -> uuid nostro via l'email
/// (`userName`/`name` == email in questo sistema); un sostituto esterno al gruppo ottiene un
/// uuid sintetico e viene aggiunto a `email_of` così il confronto per-email resta corretto.
async fn esc_goalert_overrides(
    st: &AppState, base: &str, cookie: &str, schedule_id: &str,
    email_of: &mut std::collections::HashMap<Uuid, String>,
) -> Vec<crate::escalation::Override> {
    let gq = format!("{{userOverrides(input:{{scheduleID:\"{}\"}}){{nodes{{start end addUser{{name}} removeUser{{name}}}}}}}}", esc_gql(schedule_id));
    let nodes = goalert::graphql(&st.http, base, cookie, &gq).await
        .and_then(|v| v.pointer("/data/userOverrides/nodes").and_then(|a| a.as_array()).cloned())
        .unwrap_or_default();
    let mut uid_of_email: std::collections::HashMap<String, Uuid> =
        email_of.iter().map(|(u, e)| (e.clone(), *u)).collect();
    let mut resolve = |name: Option<&str>| -> Option<Uuid> {
        let e = name?.trim().to_lowercase();
        if e.is_empty() { return None; }
        if let Some(u) = uid_of_email.get(&e) { return Some(*u); }
        let u = esc_synth_uuid(&e);
        uid_of_email.insert(e.clone(), u);
        email_of.insert(u, e);
        Some(u)
    };
    let mut out = vec![];
    for n in &nodes {
        let parse = |k: &str| n.get(k).and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        let (Some(start), Some(end)) = (parse("start"), parse("end")) else { continue };
        let add = resolve(n.pointer("/addUser/name").and_then(|x| x.as_str()));
        let remove = resolve(n.pointer("/removeUser/name").and_then(|x| x.as_str()));
        if add.is_none() && remove.is_none() { continue; }
        out.push(crate::escalation::Override { start, end, add, remove });
    }
    out
}

/// Risolve il membro on-call della rotation con STATO persistente (semantica GoAlert
/// `rotation_state`): carica/semina lo stato, lo avanza fino a `now`, lo persiste, ritorna il membro.
/// Semina alla prima osservazione dal ground-truth GoAlert (indice del membro on-call corrente).
async fn esc_resolve_rot_member(
    st: &AppState, tid: Uuid, gid: Uuid, rot: &crate::escalation::Rotation,
    members: &[Uuid], email_of: &std::collections::HashMap<Uuid,String>, ga: &[String],
    now: chrono::DateTime<chrono::Utc>, allow_native_seed: bool, read_transient: bool,
) -> Option<Uuid> {
    use crate::escalation::RotationState;
    let row = sqlx::query("SELECT position,shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let mut transient = false; // calcolato ma NON persistito (letture: on-call-now, slack, status, bundle)
    let state = match row {
        Some(r) => RotationState{ position: r.get::<i32,_>("position").max(0) as usize, shift_start: r.get("shift_start") },
        None => {
            // SEMINA: preferisci il ground-truth GoAlert (indice del membro on-call ora) per restare
            // allineati finché GoAlert è il motore. Senza GoAlert:
            //  - in CONSEGNA (allow_native_seed=true) semina nativamente alla posizione 0 e PERSISTE;
            //  - in LETTURA (read_transient=true) calcola la posizione 0 ma NON persiste (una GET non
            //    deve mutare lo stato né pre-emptare la semina GoAlert fatta dalla consegna);
            //  - in SHADOW (entrambi false) ritorna None per non generare false divergenze.
            let ga_idx = ga.first().and_then(|e| members.iter().position(|u| email_of.get(u).map(|x| x==e).unwrap_or(false)));
            let (idx, src) = match ga_idx {
                Some(i) => (i, "goalert"),
                None if allow_native_seed => (0usize, "native"),
                None if read_transient => { transient = true; (0usize, "native") },
                None => return None,
            };
            let seeded = RotationState{ position: idx, shift_start: rot.start_time(now) };
            if !transient {
                let _ = sqlx::query("INSERT INTO app.esc_rotation_state(tenant_id,group_id,position,shift_start,seeded_from) VALUES($1,$2,$3,$4,$5) ON CONFLICT (tenant_id,group_id) DO NOTHING")
                    .bind(tid).bind(gid).bind(seeded.position as i32).bind(seeded.shift_start).bind(src).execute(&st.owner_pool).await;
            }
            seeded
        }
    };
    if transient { return state.advanced_to(rot, now, members.len()).active_member(members); }
    let adv = state.advanced_to(rot, now, members.len());
    if adv != state {
        let _ = sqlx::query("UPDATE app.esc_rotation_state SET position=$3,shift_start=$4,updated_at=now() WHERE tenant_id=$1 AND group_id=$2")
            .bind(tid).bind(gid).bind(adv.position as i32).bind(adv.shift_start).execute(&st.owner_pool).await;
    }
    adv.active_member(members)
}

/// Runner shadow per un tenant: per ogni gruppo con servizio GoAlert, confronta on-call
/// nativo vs GoAlert e logga. Best-effort, nessun effetto esterno oltre al log.
pub async fn escalation_shadow_tenant(st: &AppState, tid: Uuid) -> (i64, i64) {
    let Some((base, cookie, _)) = goalert_session(st, tid).await else { return (0,0); };
    let groups = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day,goalert_service_id FROM app.alarm_groups WHERE tenant_id=$1 AND goalert_service_id IS NOT NULL")
        .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    // riferimento temporale = orologio REALE (GoAlert usa il wall-clock), non il clock di test.
    let now = chrono::Utc::now();
    let (mut n, mut matched) = (0i64, 0i64);
    for g in &groups {
        let gid: Uuid = g.get("id");
        let sid: String = match g.try_get("goalert_service_id") { Ok(Some(s)) => s, _ => continue };
        // membri (uuid+email) del gruppo
        let mrows = sqlx::query("SELECT u.id,u.email FROM app.alarm_group_members m JOIN app.users u ON u.id=m.user_id WHERE m.group_id=$1 ORDER BY m.position")
            .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        if mrows.is_empty() { continue; }
        let members: Vec<Uuid> = mrows.iter().map(|r| r.get::<Uuid,_>("id")).collect();
        let mut email_of: std::collections::HashMap<Uuid,String> = mrows.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email").to_lowercase())).collect();
        // on-call GoAlert (ground truth)
        let mut ga = esc_goalert_oncall_emails(st, &base, &cookie, &sid).await;
        ga.sort(); ga.dedup();
        // on-call nativo (override/assenze tirati da GoAlert e modellati nel resolver)
        let mut sched = esc_build_schedule(g, &members);
        if let Some(scid) = group_schedule_id(st, tid, gid).await {
            sched.overrides = esc_goalert_overrides(st, &base, &cookie, &scid, &mut email_of).await;
        }
        let rot_member = match &sched.rotation {
            Some(rot) => esc_resolve_rot_member(st, tid, gid, rot, &members, &email_of, &ga, now, false, false).await,
            None => None,
        };
        let mut native: Vec<String> = sched.on_call_at_rot(now, rot_member).iter().filter_map(|u| email_of.get(u).cloned()).collect();
        native.sort(); native.dedup();
        let is_match = native == ga;
        if is_match { matched += 1; }
        let _ = sqlx::query("INSERT INTO app.esc_shadow_log(tenant_id,group_id,native_oncall,goalert_oncall,match,detail) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(tid).bind(gid).bind(&native).bind(&ga).bind(is_match)
            .bind(json!({"group":g.get::<String,_>("name"),"mode":g.get::<String,_>("oncall_mode"),
                "overrides":sched.overrides.len(),
                "note":"FASE 1 shadow: rotation stateful (seed GoAlert + advance nativo) + override/assenze modellati"}))
            .execute(&st.owner_pool).await;
        n += 1;
    }
    (n, matched)
}

/// Scansione shadow di tutti i tenant con GoAlert provisionato. Girata dal worker a bassa frequenza.
pub async fn escalation_shadow_scan(st: &AppState) {
    let tids: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT tenant_id FROM app.goalert_services")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for tid in tids { let _ = escalation_shadow_tenant(st, tid).await; }
}

/// Riepilogo shadow (tenant_admin/superadmin): match-rate recente + ultime divergenze.
pub async fn escalation_shadow_report(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let agg = sqlx::query("SELECT count(*) AS tot, count(*) FILTER (WHERE match) AS ok FROM app.esc_shadow_log WHERE tenant_id=$1 AND at > now() - interval '7 days'")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    let tot: i64 = agg.get("tot"); let ok: i64 = agg.get("ok");
    let diffs = sqlx::query("SELECT at,group_id,native_oncall,goalert_oncall,detail FROM app.esc_shadow_log WHERE tenant_id=$1 AND match=false ORDER BY at DESC LIMIT 20")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = diffs.iter().map(|r| json!({
        "at": r.get::<chrono::DateTime<chrono::Utc>,_>("at"),
        "group_id": r.try_get::<Uuid,_>("group_id").ok(),
        "native": r.get::<Vec<String>,_>("native_oncall"),
        "goalert": r.get::<Vec<String>,_>("goalert_oncall"),
        "detail": r.try_get::<Value,_>("detail").ok() })).collect();
    let rate = if tot>0 { (ok as f64)/(tot as f64) } else { 1.0 };
    Ok(Json(json!({"window_days":7,"samples":tot,"matches":ok,"match_rate":rate,"recent_diffs":out,
        "note":"Shadow FASE 1: motore nativo vs GoAlert. 0 divergenze = pronti a valutare il cutover. GoAlert resta il motore."})))
}

/// Trigger manuale (superadmin/admin) di un giro shadow sul proprio tenant.
pub async fn escalation_shadow_run(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let (n, m) = escalation_shadow_tenant(&st, tid).await;
    Ok(Json(json!({"ok":true,"groups":n,"matches":m})))
}

// ============================================================================
// #4 escalation nativo — FASE 2: CONSEGNA IN OMBRA (sink di prova, GoAlert resta il motore)
// ============================================================================

/// On-call nativo di un gruppo a `now`, risolto con la stessa catena dello shadow (rotation
/// stateful + override), ma restituendo i recapiti (telefono/email) per costruire la ladder.
/// Richiede una sessione GoAlert (ground-truth per semina rotation + override), coerente con la
/// filosofia shadow: GoAlert è il motore in parallelo finché non si prova la parità.
/// On-call nativo di un gruppo a `now`, con i recapiti per la ladder. SELF-SUFFICIENT:
/// gli override/assenze sono quelli NATIVI (app.esc_overrides); la rotation è seminata
/// nativamente (posizione 0 deterministica) quando non c'è una sessione GoAlert.
/// `ga` è opzionale: se presente (base,cookie) GoAlert viene usato SOLO come ground-truth per la
/// semina iniziale della rotation (compat shadow), ma non è più richiesto per funzionare.
/// `seed=true` SOLO nella consegna (persiste la semina rotation). Le letture (on-call-now, slack,
/// status-update, bundle) passano `seed=false`: calcolano l'on-call senza MAI mutare lo stato.
async fn esc_native_oncall_contacts(
    st: &AppState, tid: Uuid, gid: Uuid, ga: Option<(&str, &str)>, now: chrono::DateTime<chrono::Utc>, seed: bool,
) -> Vec<(Uuid, Option<String>, String)> {
    let Some(g) = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day,goalert_service_id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() else { return vec![]; };
    let gtz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok().and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
    let mrows = sqlx::query("SELECT u.id,u.email,u.phone FROM app.alarm_group_members m JOIN app.users u ON u.id=m.user_id WHERE m.group_id=$1 ORDER BY m.position")
        .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    // NB: il gruppo può non avere membri base ma avere assegnazioni (coperture aggiuntive).
    let members: Vec<Uuid> = mrows.iter().map(|r| r.get::<Uuid,_>("id")).collect();
    let email_of: std::collections::HashMap<Uuid,String> = mrows.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email").to_lowercase())).collect();
    let phone_of: std::collections::HashMap<Uuid,Option<String>> = mrows.iter().map(|r| (r.get::<Uuid,_>("id"), r.try_get::<String,_>("phone").ok())).collect();
    // TEMPORARY SCHEDULE: se attiva, SOSTITUISCE tutto (solo il blocco corrente è on-call).
    if let Some(tu) = esc_temp_schedule_oncall(st, tid, gid, now).await {
        let (email, phone) = match email_of.get(&tu) {
            Some(em) => (Some(em.clone()), phone_of.get(&tu).cloned().flatten()),
            None => { // utente non membro del gruppo: recapiti dal profilo utente
                let row = sqlx::query("SELECT email,phone FROM app.users WHERE id=$1 AND tenant_id=$2")
                    .bind(tu).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
                match row { Some(r) => (r.try_get::<String,_>("email").ok(), r.try_get::<String,_>("phone").ok()), None => (None, None) }
            }
        };
        return match email { Some(em) => vec![(tu, phone, em)], None => vec![] };
    }
    let mut sched = esc_build_schedule(&g, &members);
    // override/assenze NATIVE (indipendenti da GoAlert)
    sched.overrides = esc_native_overrides(st, tid, gid, &email_of).await;
    let rot_member = match &sched.rotation {
        Some(rot) => {
            // ground-truth GoAlert solo per semina (se disponibile); altrimenti seed nativo.
            let mut gae = match (ga, g.try_get::<Option<String>,_>("goalert_service_id").ok().flatten()) {
                (Some((base, cookie)), Some(sid)) => esc_goalert_oncall_emails(st, base, cookie, &sid).await,
                _ => vec![],
            };
            gae.sort(); gae.dedup();
            // consegna (seed): semina+persiste; lettura (!seed): calcolo transiente senza persistere.
            esc_resolve_rot_member(st, tid, gid, rot, &members, &email_of, &gae, now, seed, !seed).await
        }
        None => None,
    };
    let mut uids = sched.on_call_at_rot(now, rot_member);
    // G-A: unione con le assegnazioni multiple (coperture aggiuntive attive adesso).
    for u in esc_assignments_oncall(st, tid, gid, now, gtz).await { if !uids.contains(&u) { uids.push(u); } }
    if uids.is_empty() { return vec![]; }
    // recapiti per TUTTI gli on-call (membri base + membri delle assegnazioni, anche fuori gruppo).
    let crows = sqlx::query("SELECT id,email,phone FROM app.users WHERE id = ANY($1) AND tenant_id=$2")
        .bind(&uids).bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let cmap: std::collections::HashMap<Uuid,(Option<String>,String)> = crows.iter().map(|r|
        (r.get::<Uuid,_>("id"), (r.try_get::<String,_>("phone").ok(), r.get::<String,_>("email")))).collect();
    uids.into_iter().filter_map(|u| {
        let (phone, email) = cmap.get(&u)?.clone();
        Some((u, phone, email))
    }).collect()
}

/// G-B — "Shifts": turni calcolati del gruppo (final schedule) su una finestra. Campiona il resolver
/// (rotation stateful PURA + override + copertura + temporary schedule) a passo fisso e fonde i
/// segmenti con lo stesso on-call. Nessuna mutazione di stato (letture pure).
async fn esc_group_shifts(st: &AppState, tid: Uuid, gid: Uuid, now: chrono::DateTime<chrono::Utc>, days: i64)
    -> Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Vec<String>)> {
    use crate::escalation::RotationState;
    let Some(g) = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day,goalert_service_id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() else { return vec![]; };
    let mrows = sqlx::query("SELECT u.id,u.email FROM app.alarm_group_members m JOIN app.users u ON u.id=m.user_id WHERE m.group_id=$1 ORDER BY m.position")
        .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let members: Vec<Uuid> = mrows.iter().map(|r| r.get::<Uuid,_>("id")).collect();
    let email_of: std::collections::HashMap<Uuid,String> = mrows.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email").to_lowercase())).collect();
    let gtz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok().and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
    // email dei membri delle assegnazioni (possono non essere membri del gruppo)
    let ass_email: std::collections::HashMap<Uuid,String> = sqlx::query(
        "SELECT DISTINCT u.id,u.email FROM app.esc_assignment_members am
           JOIN app.esc_assignments a ON a.id=am.assignment_id
           JOIN app.users u ON u.id=am.user_id
          WHERE a.tenant_id=$1 AND a.group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email").to_lowercase())).collect();
    let mut sched = esc_build_schedule(&g, &members);
    sched.overrides = esc_native_overrides(st, tid, gid, &email_of).await;
    let base_state: Option<RotationState> = match &sched.rotation {
        Some(rot) => Some(
            sqlx::query("SELECT position,shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
                .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten()
                .map(|r| RotationState{ position: r.get::<i32,_>("position").max(0) as usize, shift_start: r.get("shift_start") })
                .unwrap_or(RotationState{ position:0, shift_start: rot.start_time(now) })),
        None => None,
    };
    // PRELOAD una-tantum (niente query nel loop di campionamento): temporary schedules + assegnazioni.
    let temps: Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Vec<(Uuid,f64)>)> =
        sqlx::query("SELECT start_at,end_at,blocks FROM app.esc_temp_schedules WHERE tenant_id=$1 AND group_id=$2")
            .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default()
            .iter().map(|r| {
                let blocks: Value = r.try_get("blocks").unwrap_or_else(|_| json!([]));
                let bl = blocks.as_array().map(|a| a.iter().filter_map(|b| {
                    let u = b.get("user_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok())?;
                    Some((u, b.get("hours").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0)))
                }).collect()).unwrap_or_default();
                (r.get("start_at"), r.get("end_at"), bl)
            }).collect();
    struct AsgM { mask: u8, sm: u32, em: u32, rot: Option<crate::escalation::Rotation>, members: Vec<Uuid> }
    let mut asgs: Vec<AsgM> = vec![];
    for a in sqlx::query("SELECT id,cov_days,cov_start,cov_end,mode,oncall_type,oncall_shift,handoff_time,handoff_day FROM app.esc_assignments WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default().iter() {
        let aid: Uuid = a.get("id");
        let amem: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM app.esc_assignment_members WHERE assignment_id=$1 ORDER BY position")
            .bind(aid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        let cd: String = a.try_get("cov_days").unwrap_or_else(|_| "1111111".into());
        let mut mask: u8 = 0; for (i,c) in cd.chars().take(7).enumerate() { if c=='1' { mask |= 1 << i; } }
        let (sm, em) = match (a.try_get::<Option<String>,_>("cov_start").ok().flatten().as_deref().and_then(parse_hhmm),
                              a.try_get::<Option<String>,_>("cov_end").ok().flatten().as_deref().and_then(parse_hhmm)) {
            (Some((h1,m1)),Some((h2,m2))) => (h1*60+m1, h2*60+m2), _ => (0u32,1440u32) };
        let rot = if a.try_get::<String,_>("mode").unwrap_or_else(|_|"tutti".into())=="turni" && !amem.is_empty() {
            use chrono::TimeZone as _;
            let (kind,mul)=esc_rot_kind(&a.try_get::<String,_>("oncall_type").unwrap_or_else(|_|"weekly".into()));
            let naive = chrono::NaiveDateTime::parse_from_str(&rotation_start(a.try_get::<Option<String>,_>("handoff_time").ok().flatten().as_deref(), a.try_get("handoff_day").ok().flatten()), "%Y-%m-%dT%H:%M:%SZ").unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(2024,1,1).unwrap().and_hms_opt(0,0,0).unwrap());
            let start = match gtz.from_local_datetime(&naive){chrono::LocalResult::Single(d)=>d.with_timezone(&chrono::Utc),chrono::LocalResult::Ambiguous(d,_)=>d.with_timezone(&chrono::Utc),chrono::LocalResult::None=>chrono::Utc.from_utc_datetime(&naive)};
            Some(crate::escalation::Rotation{ kind, shift_len:(a.try_get::<i32,_>("oncall_shift").unwrap_or(1) as i64).max(1)*mul, start, members:amem.clone(), tz:gtz })
        } else { None };
        asgs.push(AsgM{ mask, sm, em, rot, members: amem });
    }
    let temp_at = |t: chrono::DateTime<chrono::Utc>| -> Option<Uuid> {
        let cur = temps.iter().filter(|(s,e,_)| *s<=t && *e>t).max_by_key(|(s,_,_)| *s)?;
        let mut cursor = cur.0;
        for (u,h) in &cur.2 { let bend = cursor + chrono::Duration::milliseconds((h*3_600_000.0) as i64); if t>=cursor && t<bend { return Some(*u); } cursor = bend; }
        None
    };
    let horizon = now + chrono::Duration::days(days.clamp(1, 60));
    let step = chrono::Duration::minutes(30);
    let mut out: Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Vec<String>)> = vec![];
    let mut seg_start = now;
    let mut prev: Option<Vec<String>> = None;
    let mut t = now;
    let mut iters = 0;
    while t <= horizon && iters < 5000 {
        iters += 1;
        let mut emails: Vec<String> = if let Some(tu) = temp_at(t) {
            email_of.get(&tu).cloned().or_else(|| ass_email.get(&tu).cloned()).into_iter().collect()
        } else {
            let rot_member = match (&sched.rotation, base_state) {
                (Some(rot), Some(bs)) if !members.is_empty() => bs.advanced_to(rot, t, members.len()).active_member(&members),
                _ => None,
            };
            let mut uids = sched.on_call_at_rot(t, rot_member);
            // assegnazioni in-memory
            for a in &asgs {
                if !(crate::escalation::ScheduleRule{ start_min:a.sm, end_min:a.em, weekday_mask:a.mask, tz:gtz }).covers(t) { continue; }
                match &a.rot {
                    Some(rot) => { if let Some(u)=rot.active_at(t) { if !uids.contains(&u) { uids.push(u); } } }
                    None => { for u in &a.members { if !uids.contains(u) { uids.push(*u); } } }
                }
            }
            uids.iter().filter_map(|u| email_of.get(u).cloned().or_else(|| ass_email.get(u).cloned())).collect()
        };
        emails.sort(); emails.dedup();
        if prev.as_ref() != Some(&emails) {
            if let Some(p) = prev.take() { out.push((seg_start, t, p)); }
            seg_start = t; prev = Some(emails);
        }
        t += step;
    }
    if let Some(p) = prev.take() { out.push((seg_start, horizon, p)); }
    out
}
pub async fn group_shifts(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let days: i64 = q.get("days").and_then(|d| d.parse().ok()).unwrap_or(30);
    let now = chrono::Utc::now();
    let segs = esc_group_shifts(&st, tid, gid, now, days).await;
    let out: Vec<Value> = segs.iter().filter(|(_,_,e)| !e.is_empty()).map(|(s0,e0,em)| json!({
        "start":s0,"end":e0,"oncall":em})).collect();
    Ok(Json(json!({"shifts":out,"days":days})))
}

/// G-A — on-call aggiuntivo dalle ASSEGNAZIONI del gruppo: ogni assegnazione aggiunge i suoi membri
/// quando la sua finestra (giorni + orario, ora locale `tz`) copre `t`. Ritorna gli uuid in unione.
async fn esc_assignments_oncall(st: &AppState, tid: Uuid, gid: Uuid, t: chrono::DateTime<chrono::Utc>, tz: chrono_tz::Tz) -> Vec<Uuid> {
    let rows = sqlx::query("SELECT id,cov_days,cov_start,cov_end,mode,oncall_type,oncall_shift,handoff_time,handoff_day FROM app.esc_assignments WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut out: Vec<Uuid> = vec![];
    for a in &rows {
        let aid: Uuid = a.get("id");
        let cd: String = a.try_get("cov_days").unwrap_or_else(|_| "1111111".into());
        let cs: Option<String> = a.try_get("cov_start").ok().flatten();
        let ce: Option<String> = a.try_get("cov_end").ok().flatten();
        let mut mask: u8 = 0;
        for (i,c) in cd.chars().take(7).enumerate() { if c=='1' { mask |= 1 << i; } }
        let (sm, em) = match (cs.as_deref().and_then(parse_hhmm), ce.as_deref().and_then(parse_hhmm)) {
            (Some((h1,m1)),Some((h2,m2))) => (h1*60+m1, h2*60+m2),
            _ => (0u32, 1440u32),
        };
        if !(crate::escalation::ScheduleRule{ start_min:sm, end_min:em, weekday_mask:mask, tz }).covers(t) { continue; }
        let mem: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM app.esc_assignment_members WHERE assignment_id=$1 ORDER BY position")
            .bind(aid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        let mode: String = a.try_get("mode").unwrap_or_else(|_| "tutti".into());
        if mode == "turni" && !mem.is_empty() {
            // rotazione dell'assegnazione (nativa): membro corrente via formula pura (deterministica,
            // ancorata all'handoff locale). Niente stato separato: è una feature native-only.
            use chrono::TimeZone as _;
            let oc_type: String = a.try_get("oncall_type").unwrap_or_else(|_| "weekly".into());
            let oc_shift: i32 = a.try_get("oncall_shift").unwrap_or(1);
            let (kind, mul) = esc_rot_kind(&oc_type);
            let ht: Option<String> = a.try_get("handoff_time").ok().flatten();
            let hd: Option<i32> = a.try_get("handoff_day").ok().flatten();
            let naive = chrono::NaiveDateTime::parse_from_str(&rotation_start(ht.as_deref(), hd), "%Y-%m-%dT%H:%M:%SZ")
                .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(2024,1,1).unwrap().and_hms_opt(0,0,0).unwrap());
            let start = match tz.from_local_datetime(&naive) { chrono::LocalResult::Single(d)=>d.with_timezone(&chrono::Utc), chrono::LocalResult::Ambiguous(d,_)=>d.with_timezone(&chrono::Utc), chrono::LocalResult::None=>chrono::Utc.from_utc_datetime(&naive) };
            let rot = crate::escalation::Rotation{ kind, shift_len:(oc_shift as i64).max(1)*mul, start, members: mem.clone(), tz };
            if let Some(u) = rot.active_at(t) { if !out.contains(&u) { out.push(u); } }
        } else {
            for u in mem { if !out.contains(&u) { out.push(u); } }
        }
    }
    out
}

/// Override/assenze native di un gruppo (app.esc_overrides). I membri "remove/add" sono utenti
/// locali risolti per uuid; l'email serve al resolver per la proiezione on-call.
async fn esc_native_overrides(
    st: &AppState, tid: Uuid, gid: Uuid, email_of: &std::collections::HashMap<Uuid,String>,
) -> Vec<crate::escalation::Override> {
    let rows = sqlx::query("SELECT start_at,end_at,add_user,remove_user FROM app.esc_overrides WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let _ = email_of; // i membri sono già locali (uuid); nessuna sintesi necessaria
    rows.iter().filter_map(|r| {
        let start: chrono::DateTime<chrono::Utc> = r.get("start_at");
        let end: chrono::DateTime<chrono::Utc> = r.get("end_at");
        let add: Option<Uuid> = r.try_get("add_user").ok().flatten();
        let remove: Option<Uuid> = r.try_get("remove_user").ok().flatten();
        if add.is_none() && remove.is_none() { return None; }
        Some(crate::escalation::Override { start, end, add, remove })
    }).collect()
}

/// Temporary schedule attivo (parità GoAlert): se un gruppo ha una pianificazione temporanea che
/// copre `now`, essa SOSTITUISCE le assegnazioni: on-call = l'utente del blocco corrente (blocchi
/// sequenziali {user_id, hours} a partire da start_at). None se nessuna temp schedule attiva.
async fn esc_temp_schedule_oncall(
    st: &AppState, tid: Uuid, gid: Uuid, now: chrono::DateTime<chrono::Utc>,
) -> Option<Uuid> {
    let row = sqlx::query("SELECT start_at,blocks FROM app.esc_temp_schedules WHERE tenant_id=$1 AND group_id=$2 AND start_at<=$3 AND end_at>$3 ORDER BY start_at DESC LIMIT 1")
        .bind(tid).bind(gid).bind(now).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let start: chrono::DateTime<chrono::Utc> = row.get("start_at");
    let blocks: Value = row.try_get("blocks").unwrap_or_else(|_| json!([]));
    let arr = blocks.as_array()?;
    let mut cursor = start;
    for b in arr {
        let hours = b.get("hours").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
        let bend = cursor + chrono::Duration::milliseconds((hours * 3_600_000.0) as i64);
        if now >= cursor && now < bend {
            return b.get("user_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok());
        }
        cursor = bend;
    }
    None
}

/// Turni di rotazione CALCOLATI di un utente (parità GoAlert calendar): per ogni gruppo 'turni'
/// di cui è membro, cammina i boundary della rotation su `days` giorni e ritorna i segmenti in cui
/// l'utente è on-call. Usa lo stato persistente (esc_rotation_state) se presente, altrimenti semina
/// nativamente alla posizione 0. Indipendente da GoAlert.
async fn esc_user_rotation_shifts(
    st: &AppState, tid: Uuid, uid: Uuid, now: chrono::DateTime<chrono::Utc>, days: i64,
) -> Vec<(String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    esc_user_rotation_shifts_g(st, tid, uid, now, days, None).await
}
/// Come sopra, con filtro opzionale al singolo gruppo (sottoscrizione per-schedule self-service).
async fn esc_user_rotation_shifts_g(
    st: &AppState, tid: Uuid, uid: Uuid, now: chrono::DateTime<chrono::Utc>, days: i64, group: Option<Uuid>,
) -> Vec<(String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    use crate::escalation::RotationState;
    let mut out = vec![];
    let groups = sqlx::query("SELECT g.id,g.name,g.oncall_mode,g.oncall_type,g.oncall_shift,g.coverage_mode,g.cov_start,g.cov_end,g.cov_days,g.handoff_time,g.handoff_day,g.goalert_service_id
         FROM app.alarm_groups g JOIN app.alarm_group_members m ON m.group_id=g.id
        WHERE g.tenant_id=$1 AND m.user_id=$2 AND g.oncall_mode='turni' AND ($3::uuid IS NULL OR g.id=$3)")
        .bind(tid).bind(uid).bind(group).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let horizon = now + chrono::Duration::days(days);
    for g in &groups {
        let gid: Uuid = g.get("id");
        let name: String = g.get("name");
        let mrows = sqlx::query("SELECT user_id FROM app.alarm_group_members WHERE group_id=$1 ORDER BY position")
            .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        let members: Vec<Uuid> = mrows.iter().map(|r| r.get::<Uuid,_>("user_id")).collect();
        if members.is_empty() { continue; }
        let sched = esc_build_schedule(g, &members);
        let Some(rot) = sched.rotation.as_ref() else { continue };
        let count = members.len();
        // stato base: persistito o seminato nativo
        let base = sqlx::query("SELECT position,shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
            .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten()
            .map(|r| RotationState{ position: r.get::<i32,_>("position").max(0) as usize, shift_start: r.get("shift_start") })
            .unwrap_or_else(|| RotationState{ position: 0, shift_start: rot.start_time(now) });
        let mut stt = base.advanced_to(rot, now, count);
        let mut seg_start = stt.shift_start;
        for _ in 0..400 {
            let seg_end = rot.end_time(seg_start);
            if stt.active_member(&members) == Some(uid) && seg_end > now {
                out.push((name.clone(), seg_start.max(now), seg_end));
            }
            if seg_end > horizon { break; }
            seg_start = seg_end;
            stt = RotationState{ position: (stt.position + 1) % count, shift_start: seg_end };
        }
    }
    out
}

/// Invio SMS nativo one-shot (verifica/test recapiti), riusando gli adapter di delivery:
/// Twilio LIVE se configurato, altrimenti sink/dry (dev). Nessuna dipendenza da GoAlert.
async fn esc_send_sms_native(st: &AppState, to: &str, text: &str) -> crate::delivery::SinkResult {
    use crate::delivery::Channel;
    let from = std::env::var("YW_ESC_FROM").unwrap_or_else(|_| "+10000000000".into());
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    let relay = std::env::var("YW_ESC_SMTP_RELAY_URL").unwrap_or_default();
    let slack_token = std::env::var("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    let public_base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let live_cfg = crate::delivery::TwilioCfg::from_env();
    let live_on = live_cfg.is_some();
    let body = crate::delivery::request_body(Channel::Sms, &from, to, text, "verify", 0);
    esc_dispatch(st, Channel::Sms, to, text, &body, live_on, live_cfg.as_ref(), &slack_token, &relay, &sink, &public_base).await
}

/// EscStatus nativo dallo stato dell'incidente (presa in carico/chiusura = ferma l'escalation).
fn esc_status_from_incident(state: &str) -> crate::escalation::EscStatus {
    use crate::escalation::EscStatus as S;
    match state { "chiuso" => S::Closed, "preso_in_carico" | "intervento" => S::Acked, _ => S::Unacked }
}

/// Timeline canali della ladder a livello tenant (SMS @0, VOICE dopo `voice_fallback_min`).
/// Ritorna (policy per `tick`, canale di ogni step). repeat=0 in FASE 2 (nessuna ripetizione ancora).
fn esc_delivery_policy(voice_on: bool, voice_min: i64, repeat: i64) -> (crate::escalation::EscPolicy, Vec<crate::delivery::Channel>) {
    use crate::escalation::{EscPolicy, EscStep};
    use crate::delivery::Channel;
    let mut steps = vec![]; let mut chans = vec![];
    if voice_on {
        // SMS, poi VOICE: lo step SMS "aspetta" voice_min prima di avanzare al VOICE.
        steps.push(EscStep{ delay_s: voice_min.max(0)*60, targets: vec![] }); chans.push(Channel::Sms);
        steps.push(EscStep{ delay_s: 0, targets: vec![] }); chans.push(Channel::Voice);
    } else {
        steps.push(EscStep{ delay_s: 0, targets: vec![] }); chans.push(Channel::Sms);
    }
    (EscPolicy{ steps, repeat: repeat.max(0) as u32 }, chans)
}

fn esc_channel_from_str(s: &str) -> crate::delivery::Channel {
    use crate::delivery::Channel;
    match s { "voice"=>Channel::Voice, "email"=>Channel::Email, "slack"=>Channel::Slack, _=>Channel::Sms }
}

/// Policy di consegna di un GRUPPO. Parità GoAlert: se il gruppo ha step personalizzati
/// (app.esc_policy_steps) usa quelli (sequenza arbitraria canale+attesa + repeat); altrimenti
/// torna alla ladder derivata dalle impostazioni vocali del tenant (retro-compatibile).
/// Target di uno step di policy: canale + CHI notificare (parità Escalation Policies GoAlert).
#[derive(Clone)]
struct EscStepTarget {
    channel: crate::delivery::Channel,
    kind: String,                 // oncall|group|user|slack
    group: Option<Uuid>,
    user: Option<Uuid>,
    slack: Option<String>,
}
async fn esc_group_policy(
    st: &AppState, tid: Uuid, gid: Uuid, voice_on: bool, voice_min: i64, repeat_tenant: i64,
) -> (crate::escalation::EscPolicy, Vec<EscStepTarget>) {
    use crate::escalation::{EscPolicy, EscStep};
    let rows = sqlx::query("SELECT delay_s,channel,target_kind,target_group_id,target_user_id,target_slack FROM app.esc_policy_steps WHERE tenant_id=$1 AND group_id=$2 ORDER BY idx")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if rows.is_empty() {
        // ladder derivata: tutti gli step verso l'on-call del gruppo (kind 'oncall').
        let (pol, chans) = esc_delivery_policy(voice_on, voice_min, repeat_tenant);
        let tgts = chans.into_iter().map(|c| EscStepTarget{ channel:c, kind:"oncall".into(), group:None, user:None, slack:None }).collect();
        return (pol, tgts);
    }
    let grp_repeat: Option<i32> = sqlx::query_scalar("SELECT esc_repeat FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let repeat = grp_repeat.map(|r| r as i64).unwrap_or(repeat_tenant).max(0) as u32;
    let mut steps = vec![]; let mut tgts = vec![];
    for r in &rows {
        let d: i32 = r.get("delay_s");
        let ch: String = r.get("channel");
        let kind: String = r.try_get("target_kind").unwrap_or_else(|_| "oncall".into());
        steps.push(EscStep{ delay_s: d.max(0) as i64, targets: vec![] });
        tgts.push(EscStepTarget{
            channel: esc_channel_from_str(&ch), kind,
            group: r.try_get("target_group_id").ok().flatten(),
            user: r.try_get("target_user_id").ok().flatten(),
            slack: r.try_get("target_slack").ok().flatten(),
        });
    }
    (EscPolicy{ steps, repeat }, tgts)
}
/// Recipienti di uno step in base al suo target. Per 'slack' ritorna vuoto + il canale Slack.
async fn esc_step_recipients(
    st: &AppState, tid: Uuid, inc_gid: Uuid, spec: &EscStepTarget, ga: Option<(&str,&str)>,
    now: chrono::DateTime<chrono::Utc>,
) -> (Vec<(Uuid, Option<String>, String)>, Option<String>) {
    match spec.kind.as_str() {
        "group" => match spec.group {
            Some(g) => (esc_native_oncall_contacts(st, tid, g, ga, now, false).await, None),
            None => (vec![], None),
        },
        "user" => match spec.user {
            Some(u) => {
                let row = sqlx::query("SELECT email,phone FROM app.users WHERE id=$1 AND tenant_id=$2")
                    .bind(u).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
                match row {
                    Some(r) => { let em: Option<String> = r.try_get("email").ok();
                        let ph: Option<String> = r.try_get("phone").ok();
                        match em { Some(e) => (vec![(u, ph, e)], None), None => (vec![], None) } }
                    None => (vec![], None),
                }
            }
            None => (vec![], None),
        },
        "slack" => (vec![], spec.slack.clone().filter(|s| !s.trim().is_empty())),
        _ => (esc_native_oncall_contacts(st, tid, inc_gid, ga, now, true).await, None), // oncall
    }
}

fn esc_channel_from_kind(k: &str) -> crate::delivery::Channel {
    use crate::delivery::Channel;
    match k { "voice"=>Channel::Voice, "email"=>Channel::Email, _=>Channel::Sms }
}

/// Parametri di trasporto della consegna, raggruppati per non esplodere le firme.
struct EscTransport<'a> {
    from: &'a str, sink: &'a str, relay: &'a str, slack_token: &'a str, public_base: &'a str,
    live_on: bool, live_cfg: Option<&'a crate::delivery::TwilioCfg>,
}

/// Consegna le rung DOVUTE di uno step per gli on-call. Parità GoAlert a 2 livelli:
/// - se l'utente ha REGOLE DI NOTIFICA (user_notif_rules) la ladder è la sua (contact method + ritardo
///   rispetto all'accensione dello step): le regole con ritardo già trascorso vengono consegnate;
/// - altrimenti fallback al canale dello step sul recapito di base (telefono/email).
/// Idempotente su esc_delivery_log; ri-chiamabile a ogni giro (anche in Wait) per far scattare i ritardi.
#[allow(clippy::too_many_arguments)]
async fn esc_deliver_step_rungs(
    st: &AppState, tid: Uuid, iid: Uuid, step_idx: i32, step_fired_at: chrono::DateTime<chrono::Utc>,
    attempt: i32, default_channel: crate::delivery::Channel,
    oncall: &[(Uuid, Option<String>, String)], now: chrono::DateTime<chrono::Utc>, text: &str, tr: &EscTransport<'_>,
) -> i64 {
    use crate::delivery::Channel;
    let mut delivered = 0i64;
    for (uid, phone, email) in oncall {
        let rules = sqlx::query("SELECT cm.kind,cm.value,r.delay_min FROM app.user_notif_rules r JOIN app.user_contact_methods cm ON cm.id=r.cm_id WHERE r.tenant_id=$1 AND r.user_id=$2 AND cm.disabled_at IS NULL ORDER BY r.delay_min")
            .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        // rung = (canale, destinatario, ritardo_min)
        let rungs: Vec<(Channel, String, i64)> = if rules.is_empty() {
            match default_channel {
                Channel::Email => vec![(Channel::Email, email.clone(), 0)],
                _ => phone.clone().map(|p| vec![(default_channel, p, 0)]).unwrap_or_default(),
            }
        } else {
            rules.iter().filter_map(|r| {
                let kind: String = r.get("kind"); let val: String = r.get("value"); let dm: i32 = r.get("delay_min");
                let v = val.trim().to_string(); if v.is_empty() { return None; }
                Some((esc_channel_from_kind(&kind), v, dm as i64))
            }).collect()
        };
        for (chn, to, dm) in rungs {
            if to.trim().is_empty() { continue; }
            if esc_is_opted_out(st, &to).await { continue; } // G1: recapito in opt-out (DNC) -> salta
            if step_fired_at + chrono::Duration::minutes(dm) > now { continue; } // non ancora dovuto
            let claimed = sqlx::query("INSERT INTO app.esc_delivery_log(tenant_id,incident_id,step,channel,to_value,attempt,state) VALUES($1,$2,$3,$4,$5,$6,'queued') ON CONFLICT (tenant_id,incident_id,step,channel,to_value,attempt) DO NOTHING RETURNING id")
                .bind(tid).bind(iid).bind(step_idx).bind(chn.as_str()).bind(&to).bind(attempt).fetch_optional(&st.owner_pool).await.ok().flatten();
            let Some(logrow) = claimed else { continue };
            let log_id: i64 = logrow.get("id");
            let body = crate::delivery::request_body(chn, tr.from, &to, text, &iid.to_string(), step_idx as i64);
            let res = esc_dispatch(st, chn, &to, text, &body, tr.live_on, tr.live_cfg, tr.slack_token, tr.relay, tr.sink, tr.public_base).await;
            esc_persist_delivery(st, tid, iid, log_id, chn.as_str(), &res).await;
            delivered += 1;
        }
    }
    delivered
}

/// Consegna idempotente di UNA rung Slack a un canale (target Slack di uno step, o canale del gruppo).
async fn esc_deliver_slack_rung(st: &AppState, tid: Uuid, iid: Uuid, step: i32, attempt: i32, channel_id: &str, text: &str, tr: &EscTransport<'_>) -> i64 {
    use crate::delivery::Channel;
    let claimed = sqlx::query("INSERT INTO app.esc_delivery_log(tenant_id,incident_id,step,channel,to_value,attempt,state) VALUES($1,$2,$3,'slack',$4,$5,'queued') ON CONFLICT (tenant_id,incident_id,step,channel,to_value,attempt) DO NOTHING RETURNING id")
        .bind(tid).bind(iid).bind(step).bind(channel_id).bind(attempt).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(logrow) = claimed else { return 0; };
    let log_id: i64 = logrow.get("id");
    let body = crate::delivery::request_body(Channel::Slack, tr.from, channel_id, text, &iid.to_string(), step as i64);
    let res = esc_dispatch(st, Channel::Slack, channel_id, text, &body, tr.live_on, tr.live_cfg, tr.slack_token, tr.relay, tr.sink, tr.public_base).await;
    esc_persist_delivery(st, tid, iid, log_id, "slack", &res).await;
    1
}

/// Sceglie l'adapter per un canale e consegna. LIVE (Twilio/Slack/relay) se abilitato, altrimenti
/// sink/dry. Centralizza la selezione per riusarla sia sui recapiti on-call sia sul canale Slack.
#[allow(clippy::too_many_arguments)]
async fn esc_dispatch(
    st: &AppState, channel: crate::delivery::Channel, to: &str, text: &str, body: &serde_json::Value,
    live_on: bool, live_cfg: Option<&crate::delivery::TwilioCfg>, slack_token: &str, relay: &str,
    sink: &str, public_base: &str,
) -> crate::delivery::SinkResult {
    use crate::delivery::Channel;
    match channel {
        Channel::Sms | Channel::Voice if live_on && live_cfg.is_some() => {
            let action = format!("{}/api/ingress/twilio/voice", public_base.trim_end_matches('/'));
            crate::delivery::deliver_twilio_live(&st.http, live_cfg.unwrap(), channel, to, text, &action).await
        }
        Channel::Email if !relay.trim().is_empty() => crate::delivery::deliver_email_relay(&st.http, relay, body).await,
        Channel::Slack if !slack_token.trim().is_empty() => crate::delivery::deliver_slack_live(&st.http, slack_token, to, text).await,
        _ => crate::delivery::deliver_to_sink(&st.http, sink, body).await,
    }
}

/// Persiste l'esito di una consegna: su successo aggiorna la riga; su FALLIMENTO rimuove il claim
/// così il prossimo giro ritenta (at-least-once, SOS mai perso) e logga in timeline.
async fn esc_persist_delivery(st: &AppState, tid: Uuid, iid: Uuid, log_id: i64, channel: &str, res: &crate::delivery::SinkResult) {
    if res.state == "failed" {
        let _ = sqlx::query("DELETE FROM app.esc_delivery_log WHERE id=$1").bind(log_id).execute(&st.owner_pool).await;
        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_delivery_failed',$3)")
            .bind(tid).bind(iid).bind(json!({"channel":channel,"detail":res.detail})).execute(&st.owner_pool).await;
    } else {
        let _ = sqlx::query("UPDATE app.esc_delivery_log SET state=$2,provider_ref=$3,detail=$4 WHERE id=$1")
            .bind(log_id).bind(res.state).bind(res.provider_ref.clone()).bind(res.detail.clone()).execute(&st.owner_pool).await;
    }
}

/// Runner di consegna per un tenant. In SHADOW (`escalation_engine='goalert'`, rollback) consegna al
/// SINK di prova e NON tocca gli incidenti: GoAlert resta il motore. Con il motore NATIVO
/// (`escalation_engine='native'`, default di prodotto) usa gli adapter live (se configurati) ed è autoritativo
/// (l'ack applica la transizione). Idempotenza su `esc_delivery_log`; i fallimenti si ritentano.
pub async fn escalation_delivery_shadow_tenant(st: &AppState, tid: Uuid) -> (i64, i64) {
    use crate::escalation::{self, AlertState, EscAction};
    // MAINTENANCE nativo: finestra attiva -> sospendi le consegne (gli incidenti continuano a
    // essere creati; la ladder riparte a fine finestra). Indipendente da GoAlert.
    if maintenance_active(st, tid).await {
        // segna gli incidenti aperti come "in bundle": a fine finestra verrà inviato UN riepilogo.
        let _ = sqlx::query("UPDATE app.incidents SET bundled_at=now() WHERE tenant_id=$1 AND state<>'chiuso' AND bundled_at IS NULL")
            .bind(tid).execute(&st.owner_pool).await;
        return (0,0);
    }
    // USCITA manutenzione: se ci sono incidenti accumulati, invia UN solo riepilogo (parità GoAlert
    // alert bundling) agli on-call del gruppo di default, poi riprende l'escalation normale.
    let bundled: i64 = sqlx::query_scalar("SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state<>'chiuso' AND bundled_at IS NOT NULL")
        .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    if bundled > 0 {
        let dg: Option<Uuid> = sqlx::query_scalar("SELECT default_group_id FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
            .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
        if let Some(dg) = dg {
            let now0 = chrono::Utc::now();
            let oncall = esc_native_oncall_contacts(st, tid, dg, None, now0, false).await;
            let text = format!("YouWorkforce: {bundled} allarmi durante la manutenzione. Escalation ripresa.");
            for (_u, phone, _email) in &oncall {
                if let Some(p) = phone.clone().filter(|p| !p.trim().is_empty()) {
                    if esc_is_opted_out(st, &p).await { continue; } // G1: salta opt-out
                    let _ = esc_send_sms_native(st, &p, &text).await;
                }
            }
        }
        let _ = sqlx::query("UPDATE app.incidents SET bundled_at=NULL WHERE tenant_id=$1 AND bundled_at IS NOT NULL")
            .bind(tid).execute(&st.owner_pool).await;
    }
    // GoAlert OPZIONALE: se provisionato serve solo come ground-truth per la semina rotation e
    // (storicamente) gli override; il motore nativo funziona anche senza (strada nuova).
    let ga_sess = goalert_session(st, tid).await;
    let ga_opt: Option<(&str,&str)> = ga_sess.as_ref().map(|(b,c,_)| (b.as_str(), c.as_str()));
    let now = chrono::Utc::now();
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    let from = std::env::var("YW_ESC_FROM").unwrap_or_else(|_| "+10000000000".into());
    // adapter LIVE (gate): Twilio reale, Slack, relay email. Default OFF -> sink/dry.
    let live_cfg = crate::delivery::TwilioCfg::from_env();
    let slack_token = std::env::var("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    let relay = std::env::var("YW_ESC_SMTP_RELAY_URL").unwrap_or_default();
    let public_base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    // config vocale + repeat + motore (FASE 3) del tenant
    let vrow = sqlx::query("SELECT voice_fallback_on,voice_fallback_min,esc_repeat,escalation_engine FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let (voice_on, voice_min, repeat, engine) = vrow.map(|r| (r.get::<bool,_>("voice_fallback_on"), r.get::<i32,_>("voice_fallback_min") as i64, r.get::<i32,_>("esc_repeat") as i64, r.get::<String,_>("escalation_engine")))
        .unwrap_or((true,5,0,"native".into()));
    let native = engine == "native"; // nativo autoritativo per questo tenant (default di prodotto)
    // in cutover usa gli adapter live se configurati; in shadow solo col gate globale.
    let live_on = live_cfg.is_some() && (native || crate::delivery::TwilioCfg::live_enabled());
    // La policy di consegna è PER-GRUPPO (F3): ogni gruppo può avere step personalizzati.
    // Costruita dentro il loop in base al gruppo instradato dell'incidente.

    // incidenti aperti instradati a un gruppo nativo.
    // Fase C: se il lavoratore dell'incidente (via assigned_user diretto o device assegnato) ha un
    // Profilo di protezione ATTIVO con un gruppo escalation, quel gruppo HA LA PRECEDENZA sul routing
    // globale per-etype. Fallback al routing globale quando non c'è profilo/gruppo (comportamento legacy).
    // Precedenza del gruppo: profilo di protezione del lavoratore > routing per-etype >
    // gruppo di DEFAULT del tenant (catch-all, mig 0060). Il default garantisce che nessun incidente
    // critico resti senza destinatari sulla strada nuova (nativo), anche senza regola di routing.
    let incs = sqlx::query(
        "SELECT i.id, i.etype, i.state, i.opened_at,
                COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id) AS group_id
           FROM app.incidents i
           LEFT JOIN app.alarm_routing r ON r.tenant_id=i.tenant_id AND r.etype=i.etype
           LEFT JOIN app.events ev ON ev.id=i.event_id
           LEFT JOIN app.devices d ON d.id=ev.device_id
           LEFT JOIN app.protection_profile_members m
                  ON m.tenant_id=i.tenant_id AND m.user_id=COALESCE(i.assigned_user, d.assigned_user)
           LEFT JOIN app.protection_profiles pp ON pp.id=m.profile_id AND pp.enabled=true
           LEFT JOIN app.tenant_oncall_cfg oc ON oc.tenant_id=i.tenant_id
          WHERE i.tenant_id=$1 AND i.state <> 'chiuso'
            AND COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id) IS NOT NULL")
        .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let tr = EscTransport{ from:&from, sink:&sink, relay:&relay, slack_token:&slack_token, public_base:&public_base, live_on, live_cfg: live_cfg.as_ref() };
    let (mut seen, mut delivered) = (0i64, 0i64);
    for inc in &incs {
        let iid: Uuid = inc.get("id");
        let gid: Uuid = inc.get("group_id");
        let etype: String = inc.get("etype");
        let istate: String = inc.get("state");
        seen += 1;

        // policy di consegna del GRUPPO instradato (step personalizzati o ladder derivata)
        let (policy, specs) = esc_group_policy(st, tid, gid, voice_on, voice_min, repeat).await;
        let last_step = policy.steps.len().saturating_sub(1) as i32;

        // stato macchina (seed step=-1 = usize::MAX lato Rust)
        let strow = sqlx::query("SELECT step,step_fired_at,repeat_count FROM app.esc_alert_state WHERE tenant_id=$1 AND incident_id=$2")
            .bind(tid).bind(iid).fetch_optional(&st.owner_pool).await.ok().flatten();
        let (cur_step, fired_at, rc) = match &strow {
            Some(r) => (r.get::<i32,_>("step"), r.get::<chrono::DateTime<chrono::Utc>,_>("step_fired_at"), r.get::<i32,_>("repeat_count")),
            None => (-1, now, 0),
        };
        let rust_step = if cur_step < 0 { usize::MAX } else { cur_step as usize };
        let state = AlertState{ step: rust_step, step_fired_at: fired_at, repeat_count: rc as u32, status: esc_status_from_incident(&istate) };
        let action = escalation::tick(&policy, &state, now);

        match action {
            EscAction::Stop | EscAction::Exhausted => {
                let new_status = if matches!(action, EscAction::Stop) { if istate=="chiuso" {"closed"} else {"ack"} } else { "exhausted" };
                let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,repeat_count,status) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT (tenant_id,incident_id) DO UPDATE SET status=$6,updated_at=now()")
                    .bind(tid).bind(iid).bind(cur_step).bind(fired_at).bind(rc).bind(new_status).execute(&st.owner_pool).await;
            }
            EscAction::Wait => {
                // delay dello step non scaduto: NON si avanza, ma si consegnano le rung DOVUTE dello
                // step corrente (fa scattare i ritardi delle regole di notifica per-utente).
                use crate::delivery::Channel;
                if cur_step >= 0 && !matches!(state.status, crate::escalation::EscStatus::Acked | crate::escalation::EscStatus::Closed) {
                    let spec = specs.get(cur_step as usize).cloned().unwrap_or(EscStepTarget{ channel:Channel::Sms, kind:"oncall".into(), group:None, user:None, slack:None });
                    let (oncall, slack_ch) = esc_step_recipients(st, tid, gid, &spec, ga_opt, now).await;
                    let sstr = iid.to_string(); let short = &sstr[..8];
                    let text = format!("Allarme {etype} (incidente {short})");
                    delivered += esc_deliver_step_rungs(st, tid, iid, cur_step, fired_at, rc, spec.channel, &oncall, now, &text, &tr).await;
                    if let Some(sc) = slack_ch { delivered += esc_deliver_slack_rung(st, tid, iid, cur_step, rc, &sc, &text, &tr).await; }
                }
            }
            EscAction::FireStep(idx, _) => {
                use crate::delivery::Channel;
                // repeat: tick ritorna FireStep(0) ripartendo dall'ultimo step -> nuovo giro.
                let is_repeat_restart = idx == 0 && cur_step == last_step && cur_step >= 0;
                let new_rc = if is_repeat_restart { rc + 1 } else { rc };
                let attempt = new_rc; // il giro di repeat fa parte della chiave di idempotenza
                let spec = specs.get(idx).cloned().unwrap_or(EscStepTarget{ channel:Channel::Sms, kind:"oncall".into(), group:None, user:None, slack:None });
                let (oncall, slack_ch) = esc_step_recipients(st, tid, gid, &spec, ga_opt, now).await;
                let sstr = iid.to_string(); let short = &sstr[..8];
                let text = format!("Allarme {etype} (incidente {short})");
                // rung per-utente (regole di notifica o fallback al canale dello step), ritardi inclusi.
                delivered += esc_deliver_step_rungs(st, tid, iid, idx as i32, now, attempt, spec.channel, &oncall, now, &text, &tr).await;
                // step con target Slack: posta sul canale dello step.
                if let Some(sc) = slack_ch { delivered += esc_deliver_slack_rung(st, tid, iid, idx as i32, attempt, &sc, &text, &tr).await; }
                // Slack per-gruppo (canale del gruppo) sul PRIMO step 'oncall' se configurato.
                if idx == 0 && spec.kind == "oncall" {
                    if let Some(sr) = sqlx::query("SELECT slack_channel_id,slack_notify FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
                        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() {
                        let ch: Option<String> = sr.try_get("slack_channel_id").ok().flatten();
                        let notify: String = sr.try_get("slack_notify").unwrap_or_else(|_| "off".into());
                        if notify != "off" { if let Some(ch) = ch.filter(|s| !s.trim().is_empty()) {
                            delivered += esc_deliver_slack_rung(st, tid, iid, idx as i32, attempt, &ch, &text, &tr).await;
                        } }
                    }
                }
                // avanza/semina lo stato alla rung appena sparata (repeat_count aggiornato)
                let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,repeat_count,status) VALUES($1,$2,$3,$4,$5,'unack') ON CONFLICT (tenant_id,incident_id) DO UPDATE SET step=$3,step_fired_at=$4,repeat_count=$5,updated_at=now()")
                    .bind(tid).bind(iid).bind(idx as i32).bind(now).bind(new_rc).execute(&st.owner_pool).await;
                let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_shadow_delivery',$3)")
                    .bind(tid).bind(iid).bind(json!({"step":idx,"channel":spec.channel.as_str(),"target":spec.kind,"attempt":attempt,"live":live_on,"engine":engine})).execute(&st.owner_pool).await;
            }
        }
    }
    (seen, delivered)
}

/// F3 — Annunci Slack dell'on-call (parità GoAlert "On-Call Notifications"): per ogni gruppo con
/// `slack_notify` != off e un canale Slack, posta chi è reperibile. `onchange` = a ogni handoff
/// (cambia lo shift_start della rotation); `HH:MM` = una volta al giorno dopo quell'ora.
/// Best-effort: senza token Slack live non fa nulla.
pub async fn slack_announce_scan(st: &AppState) {
    let token = std::env::var("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    if token.trim().is_empty() { return; }
    let now = chrono::Utc::now();
    let groups = sqlx::query("SELECT id,tenant_id,name,slack_channel_id,slack_notify,slack_last_announce,oncall_mode,oncall_tz FROM app.alarm_groups WHERE slack_notify<>'off' AND slack_channel_id IS NOT NULL")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for g in &groups {
        let gid: Uuid = g.get("id");
        let tid: Uuid = g.get("tenant_id");
        let name: String = g.get("name");
        let Some(ch) = g.try_get::<Option<String>,_>("slack_channel_id").ok().flatten().filter(|s| !s.trim().is_empty()) else { continue };
        let notify: String = g.try_get("slack_notify").unwrap_or_else(|_| "off".into());
        let last: Option<chrono::DateTime<chrono::Utc>> = g.try_get("slack_last_announce").ok().flatten();
        let mode: String = g.try_get("oncall_mode").unwrap_or_else(|_| "tutti".into());
        let should = if notify == "onchange" {
            if mode != "turni" { false } else {
                // handoff: lo shift_start corrente della rotation è più recente dell'ultimo annuncio
                let ss: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
                    .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten();
                match (ss, last) { (Some(s), Some(l)) => s > l, (Some(_), None) => true, _ => false }
            }
        } else if let Some((hh, mm)) = parse_hhmm(&notify) {
            // orario di annuncio nell'ora LOCALE del gruppo (oncall_tz), DST gestito.
            use chrono::TimeZone as _;
            let tz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok().and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
            let lt = now.with_timezone(&tz);
            let target = lt.date_naive().and_hms_opt(hh, mm, 0)
                .and_then(|d| match tz.from_local_datetime(&d) { chrono::LocalResult::Single(x)=>Some(x), chrono::LocalResult::Ambiguous(x,_)=>Some(x), _=>None })
                .map(|d| d.with_timezone(&chrono::Utc));
            match target { Some(t) => now >= t && last.map(|l| l < t).unwrap_or(true), None => false }
        } else { false };
        if !should { continue; }
        let oncall = esc_native_oncall_contacts(st, tid, gid, None, now, false).await;
        let who = if oncall.is_empty() { "nessuno".to_string() }
            else { oncall.iter().map(|(_,_,em)| em.clone()).collect::<Vec<_>>().join(", ") };
        let text = format!("On-call «{name}»: {who}");
        let _ = crate::delivery::deliver_slack_live(&st.http, &token, &ch, &text).await;
        let _ = sqlx::query("UPDATE app.alarm_groups SET slack_last_announce=$2 WHERE id=$1").bind(gid).bind(now).execute(&st.owner_pool).await;
    }
}

/// Scansione consegna del motore nativo (dal worker). SELF-SUFFICIENT: copre OGNI tenant che ha
/// incidenti aperti (qualunque modulo li produca: safety/IoT/TB/Traccar), più i tenant con GoAlert
/// provisionato (per la semina/compat). Non dipende più dalla sola tabella goalert_services.
pub async fn escalation_delivery_shadow_scan(st: &AppState) {
    let tids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM app.incidents WHERE state <> 'chiuso'
         UNION
         SELECT tenant_id FROM app.goalert_services")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for tid in tids { let _ = escalation_delivery_shadow_tenant(st, tid).await; }
}

/// Riepilogo consegne in ombra (admin): conteggi per stato + ultime consegne.
pub async fn escalation_delivery_report(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let counts = sqlx::query("SELECT state, count(*) AS n FROM app.esc_delivery_log WHERE tenant_id=$1 AND at > now() - interval '7 days' GROUP BY state")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let by_state: Value = counts.iter().map(|r| (r.get::<String,_>("state"), json!(r.get::<i64,_>("n")))).collect::<serde_json::Map<_,_>>().into();
    let recent = sqlx::query("SELECT at,incident_id,step,channel,to_value,state,provider_ref FROM app.esc_delivery_log WHERE tenant_id=$1 ORDER BY at DESC LIMIT 30")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = recent.iter().map(|r| json!({
        "at": r.get::<chrono::DateTime<chrono::Utc>,_>("at"),
        "incident_id": r.get::<Uuid,_>("incident_id"),
        "step": r.get::<i32,_>("step"), "channel": r.get::<String,_>("channel"),
        "to": r.get::<String,_>("to_value"), "state": r.get::<String,_>("state"),
        "provider_ref": r.try_get::<String,_>("provider_ref").ok() })).collect();
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    Ok(Json(json!({"window_days":7,"by_state":by_state,"recent":out,
        "sink_configured": !sink.is_empty(),
        "note":"Consegna FASE 2 in OMBRA: sink di prova, nessun carrier reale. GoAlert resta il motore. 'dry' = nessun sink configurato."})))
}

/// Trigger manuale (admin) di un giro di consegna in ombra sul proprio tenant.
pub async fn escalation_delivery_run(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let (seen, delivered) = escalation_delivery_shadow_tenant(&st, tid).await;
    Ok(Json(json!({"ok":true,"incidents":seen,"deliveries":delivered})))
}

// --------------------------- Ack inbound Twilio (SMS + voce DTMF) ---------------------------

/// Risposta TwiML vuota (ack ricevuto, niente da dire).
fn twiml(body: &str) -> Response {
    ([(header::CONTENT_TYPE, "text/xml")], body.to_string()).into_response()
}

/// Valida la firma Twilio dell'inbound. Se `YW_ESC_TWILIO_AUTH` è impostato la firma è
/// OBBLIGATORIA (403 se assente/errata); in dev senza token si accetta e si logga.
fn esc_verify_inbound(h: &HeaderMap, path: &str, params: &[(String,String)]) -> Result<(), Response> {
    let auth = std::env::var("YW_ESC_TWILIO_AUTH").unwrap_or_default();
    if auth.is_empty() { return Ok(()); } // dev/shadow senza carrier reale
    let base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let sig = h.get("x-twilio-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if crate::delivery::verify_twilio_signature(&auth, &url, params, sig) { Ok(()) }
    else { Err((StatusCode::FORBIDDEN, "firma Twilio non valida").into_response()) }
}

/// Mappa un numero mittente -> incidente aperto più recente a cui era stata consegnata una rung.
async fn esc_incident_by_number(st: &AppState, number: &str) -> Option<(Uuid, Uuid, String)> {
    let row = sqlx::query(
        "SELECT d.tenant_id, d.incident_id, i.state
           FROM app.esc_delivery_log d JOIN app.incidents i ON i.id=d.incident_id
          WHERE d.to_value=$1 AND d.channel IN ('sms','voice') AND i.state <> 'chiuso'
          ORDER BY d.at DESC LIMIT 1")
        .bind(number).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    Some((row.get("tenant_id"), row.get("incident_id"), row.get("state")))
}

/// Registra l'ack: stato macchina + delivery_log(acked) + timeline. In SHADOW NON tocca
/// `incidents.state` (GoAlert resta il motore). Col gate `YW_ESC_ACK_APPLY=1` (cutover) applica
/// la transizione reale (preso_in_carico / chiuso).
async fn esc_apply_ack(st: &AppState, tid: Uuid, iid: Uuid, action: &str, via: &str) {
    let status = if action == "close" { "closed" } else { "ack" };
    let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,status) VALUES($1,$2,0,now(),$3) ON CONFLICT (tenant_id,incident_id) DO UPDATE SET status=$3,updated_at=now()")
        .bind(tid).bind(iid).bind(status).execute(&st.owner_pool).await;
    // applica la transizione reale se il tenant è in CUTOVER (escalation_engine='native', FASE 3)
    // oppure col gate globale YW_ESC_ACK_APPLY=1. In shadow (goalert) NON tocca l'incidente.
    let native = sqlx::query_scalar::<_, String>("SELECT escalation_engine FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().as_deref() == Some("native");
    let apply = native || std::env::var("YW_ESC_ACK_APPLY").ok().as_deref() == Some("1");
    let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_ack_inbound',$3)")
        .bind(tid).bind(iid).bind(json!({"action":action,"via":via,"applied":apply})).execute(&st.owner_pool).await;
    if apply {
        if action == "close" {
            let _ = sqlx::query("UPDATE app.incidents SET state='chiuso', closed_at=now() WHERE id=$1 AND tenant_id=$2 AND state<>'chiuso'").bind(iid).bind(tid).execute(&st.owner_pool).await;
        } else {
            let _ = sqlx::query("UPDATE app.incidents SET state='preso_in_carico' WHERE id=$1 AND tenant_id=$2 AND state='aperto'").bind(iid).bind(tid).execute(&st.owner_pool).await;
        }
        // G3 status-update: avvisa gli altri on-call (attore = chi ha risposto dal carrier, ignoto qui)
        esc_status_update_notify(st, tid, iid, None, &format!("recapito ({via})"), action).await;
    }
}

// ============================================================================
// Parità Services GoAlert — S1 heartbeat monitor, S2 email ingress, S3 bulk, S4 on-call-now (nativi)
// ============================================================================

/// Crea un incidente critico INSTRADATO senza device (heartbeat/email/watchdog): usa la catena
/// persist_ingest, poi fissa il gruppo se richiesto (forced_group_id, override del routing).
async fn raise_routed_incident(st: &AppState, tid: Uuid, etype: &str, source: &str, correlation: &str, payload: Value, group_id: Option<Uuid>) -> Option<Uuid> {
    let res = persist_ingest(st, tid, Normalized{
        source: source.into(), source_event: correlation.into(), idem: correlation.into(),
        etype: etype.into(), device_external_id: None, site_name: None,
        lat: None, lon: None, accuracy_m: None, correlation: correlation.into(),
        payload, indoor: false, beacon_uid: None, zone: None,
    }).await.ok()?;
    let iid = res.0.get("incident_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok());
    if let (Some(iid), Some(g)) = (iid, group_id) {
        let _ = sqlx::query("UPDATE app.incidents SET forced_group_id=$2 WHERE id=$1 AND tenant_id=$3").bind(iid).bind(g).bind(tid).execute(&st.owner_pool).await;
    }
    iid
}

// ---- S1: Heartbeat monitor nativo ----
/// Ping pubblico del monitor (un sistema esterno lo chiama periodicamente). Aggiorna last_ping;
/// se il monitor era 'down', chiude l'incidente (recovery). Nessuna autenticazione di sessione.
pub async fn ingest_heartbeat(State(st): State<AppState>, Path(token): Path<String>) -> Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT id,tenant_id,state,incident_id FROM app.heartbeat_monitors WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "monitor inesistente").into_response(); };
    let id: Uuid = row.get("id"); let tid: Uuid = row.get("tenant_id");
    let state: String = row.get("state");
    let inc: Option<Uuid> = row.try_get("incident_id").ok().flatten();
    let _ = sqlx::query("UPDATE app.heartbeat_monitors SET last_ping_at=now(), state='ok', incident_id=NULL WHERE id=$1").bind(id).execute(&st.owner_pool).await;
    if state == "down" { if let Some(iid) = inc {
        let _ = sqlx::query("UPDATE app.incidents SET state='chiuso', closed_at=now(), outcome='heartbeat ripristinato' WHERE id=$1 AND tenant_id=$2 AND state<>'chiuso'").bind(iid).bind(tid).execute(&st.owner_pool).await;
        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)").bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":"heartbeat ripristinato"})).execute(&st.owner_pool).await;
    }}
    (StatusCode::OK, "ok").into_response()
}
/// Watchdog heartbeat (dal worker): per ogni monitor scaduto e ancora 'ok' apre un incidente
/// 'heartbeat_down' instradato (al suo gruppo o al routing/default). Idempotente sullo stato.
pub async fn heartbeat_watchdog(st: &AppState) {
    let rows = sqlx::query("SELECT id,tenant_id,name,timeout_s,group_id,last_ping_at FROM app.heartbeat_monitors WHERE state='ok'")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    let now = chrono::Utc::now();
    for r in &rows {
        let last: Option<chrono::DateTime<chrono::Utc>> = r.try_get("last_ping_at").ok().flatten();
        let Some(last) = last else { continue }; // mai pingato: in attesa del primo ping
        let timeout: i32 = r.get("timeout_s");
        if (now - last).num_seconds() < timeout as i64 { continue; }
        let id: Uuid = r.get("id"); let tid: Uuid = r.get("tenant_id");
        let name: String = r.get("name"); let g: Option<Uuid> = r.try_get("group_id").ok().flatten();
        let corr = format!("heartbeat-{}-{}", id, now.timestamp_millis());
        if let Some(iid) = raise_routed_incident(st, tid, "heartbeat_down", "heartbeat", &corr,
            json!({"monitor":name,"timeout_s":timeout,"last_ping_at":last}), g).await {
            let _ = sqlx::query("UPDATE app.heartbeat_monitors SET state='down', incident_id=$2 WHERE id=$1").bind(id).bind(iid).execute(&st.owner_pool).await;
        }
    }
}
pub async fn heartbeat_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let rows = sqlx::query("SELECT id,name,token,timeout_s,group_id,last_ping_at,state FROM app.heartbeat_monitors WHERE tenant_id=$1 ORDER BY created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let tok: String = r.get("token");
        json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),
            "timeout_s":r.get::<i32,_>("timeout_s"),"group_id":r.try_get::<Option<Uuid>,_>("group_id").ok().flatten(),
            "last_ping_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("last_ping_at").ok().flatten(),
            "state":r.get::<String,_>("state"),
            "url":format!("{}/api/ingress/heartbeat/{}", base.trim_end_matches('/'), tok)})
    }).collect();
    Ok(Json(json!({"monitors":out})))
}
#[derive(Deserialize)] pub struct HbReq { pub name: String, pub timeout_min: Option<i32>, #[serde(default)] pub group_id: Option<Uuid> }
pub async fn heartbeat_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<HbReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let timeout_s = b.timeout_min.unwrap_or(15).clamp(1, 1440) * 60;
    group_belongs(&st, tid, b.group_id).await?;
    let token = format!("hb-{}", auth::new_token());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.heartbeat_monitors(tenant_id,name,token,timeout_s,group_id) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&token).bind(timeout_s).bind(b.group_id).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "heartbeat_create", json!({"name":b.name,"timeout_s":timeout_s})).await;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn heartbeat_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.heartbeat_monitors WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "monitor inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- S2: Email ingress ----
/// Webhook del provider di posta (Mailgun/SES/…): un'email ricevuta a <token>[+dedup]@dominio diventa
/// un incidente 'email_alarm' instradato. Tenant risolto dal token (local-part), MAI dal corpo.
#[derive(Deserialize)] pub struct EmailIn { pub to: String, #[serde(default)] pub from: String, #[serde(default)] pub subject: String, #[serde(default)] pub body: String }
pub async fn ingest_email(State(st): State<AppState>, Json(b): Json<EmailIn>) -> Response {
    use axum::response::IntoResponse;
    // local-part dell'indirizzo "to": token[+dedup]@dominio
    let local = b.to.split('@').next().unwrap_or("").trim().to_lowercase();
    let (token, dedup) = match local.split_once('+') { Some((t,d)) => (t.to_string(), Some(d.to_string())), None => (local, None) };
    let row = sqlx::query("SELECT tenant_id,group_id FROM app.email_inboxes WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "inbox inesistente").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let g: Option<Uuid> = row.try_get("group_id").ok().flatten();
    let subject = if b.subject.trim().is_empty() { "(senza oggetto)".to_string() } else { b.subject.trim().to_string() };
    // dedup (parità GoAlert email): chiave custom (+key) oppure firma STABILE dell'oggetto, così email
    // ripetute con lo stesso subject confluiscono sullo stesso incidente aperto (niente flood).
    let corr = match dedup {
        Some(d) => format!("email-{}-{}", token, d),
        None => format!("email-{}-{}", token, &hash_token(&subject.to_lowercase())[..16]),
    };
    let payload = json!({"from":b.from,"subject":subject,"body": b.body.chars().take(2000).collect::<String>()});
    let _ = raise_routed_incident(&st, tid, "email_alarm", "email", &corr, payload, g).await;
    (StatusCode::OK, "ok").into_response()
}
pub async fn email_inbox_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let domain = std::env::var("YW_EMAIL_DOMAIN").unwrap_or_else(|_| "alerts.youworkforce.local".into());
    let rows = sqlx::query("SELECT id,token,group_id FROM app.email_inboxes WHERE tenant_id=$1 ORDER BY created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let tok: String = r.get("token");
        json!({"id":r.get::<Uuid,_>("id"),"group_id":r.try_get::<Option<Uuid>,_>("group_id").ok().flatten(),
            "address":format!("{}@{}", tok, domain)})
    }).collect();
    Ok(Json(json!({"inboxes":out})))
}
#[derive(Deserialize)] pub struct InboxReq { #[serde(default)] pub group_id: Option<Uuid> }
pub async fn email_inbox_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<InboxReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    group_belongs(&st, tid, b.group_id).await?;
    let token = format!("in-{}", &auth::new_token()[..12]);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.email_inboxes(tenant_id,token,group_id) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(&token).bind(b.group_id).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn email_inbox_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.email_inboxes WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "inbox inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- S3: bulk acknowledge / close ----
pub async fn incidents_ack_all(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let acl: Option<Vec<Uuid>> = allowed_sites(&st, &s).await.map(|h| h.into_iter().collect());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // solo gli incidenti 'aperto' visibili all'ACL dell'operatore. Gli incidenti senza sede NON sono
    // visibili a un operatore ristretto (coerente con guard_incident_site): quindi esclusi dal bulk.
    let rows = match &acl {
        Some(sites) => sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE state='aperto' AND site_id = ANY($1) RETURNING id")
            .bind(sites).fetch_all(&mut *tx).await.map_err(ise)?,
        None => sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE state='aperto' RETURNING id")
            .fetch_all(&mut *tx).await.map_err(ise)?,
    };
    // trail per-incidente come l'ack singolo (il bulk NON invia lo status-update per non intasare)
    for r in &rows {
        let iid: Uuid = r.get("id");
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'human_ack',$3)")
            .bind(tid).bind(iid).bind(json!({"by":s.email,"bulk":true})).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    let n = rows.len();
    audit(&st, tid, &s.email, "incidents_ack_all", json!({"count":n})).await;
    Ok(Json(json!({"ok":true,"acknowledged":n})))
}
pub async fn incidents_close_all(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CloseReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let acl: Option<Vec<Uuid>> = allowed_sites(&st, &s).await.map(|h| h.into_iter().collect());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // Incidenti senza sede esclusi per operatore ristretto (coerente con guard_incident_site).
    let rows = match &acl {
        Some(sites) => sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$1,closed_at=now(),version=version+1 WHERE state<>'chiuso' AND site_id = ANY($2) RETURNING id,correlation,etype")
            .bind(&b.outcome).bind(sites).fetch_all(&mut *tx).await.map_err(ise)?,
        None => sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$1,closed_at=now(),version=version+1 WHERE state<>'chiuso' RETURNING id,correlation,etype")
            .bind(&b.outcome).fetch_all(&mut *tx).await.map_err(ise)?,
    };
    for r in &rows {
        let iid: Uuid = r.get("id"); let correlation: String = r.get("correlation"); let etype: String = r.get("etype");
        // trail per-incidente come la chiusura singola
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
            .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":b.outcome,"by":s.email,"bulk":true})).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_close',$2)")
            .bind(tid).bind(json!({"incident_id":iid,"summary":format!("close {}",correlation),"dedup":correlation,"etype":etype}))
            .execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    let n = rows.len();
    audit(&st, tid, &s.email, "incidents_close_all", json!({"count":n})).await;
    Ok(Json(json!({"ok":true,"closed":n})))
}

// ---- S4: on-call-now NATIVO (chi è reperibile ora, per gruppo) ----
pub async fn oncall_now_native(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let now = chrono::Utc::now();
    let groups = sqlx::query("SELECT id,name FROM app.alarm_groups WHERE tenant_id=$1 ORDER BY name")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut out = Vec::new();
    let mut flat: Vec<String> = Vec::new();
    for g in &groups {
        let gid: Uuid = g.get("id"); let name: String = g.get("name");
        let oncall = esc_native_oncall_contacts(&st, tid, gid, None, now, false).await;
        let emails: Vec<String> = oncall.iter().map(|(_,_,e)| e.clone()).collect();
        for e in &emails { if !flat.contains(e) { flat.push(e.clone()); } }
        out.push(json!({"group":name,"oncall":emails}));
    }
    Ok(Json(json!({"available":true,"engine":"native","groups":out,"oncall":flat.iter().map(|e| json!({"user":e})).collect::<Vec<_>>()})))
}

/// G1 — Do-Not-Call: registra l'opt-out GLOBALE di un recapito (STOP) e ne disabilita le occorrenze
/// (contact method aggiuntivi + telefono primario) in ogni tenant. Persiste oltre lo stato del modulo.
async fn esc_optout_mark(st: &AppState, value: &str, kind: &str, reason: &str) {
    let v = value.trim();
    if v.is_empty() { return; }
    let _ = sqlx::query("INSERT INTO app.contact_optout(value,kind,reason) VALUES($1,$2,$3) ON CONFLICT (value) DO UPDATE SET at=now(), kind=EXCLUDED.kind, reason=EXCLUDED.reason")
        .bind(v).bind(kind).bind(reason).execute(&st.owner_pool).await;
    // tenant toccati (per audit)
    let tids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM app.user_contact_methods WHERE value=$1
         UNION SELECT tenant_id FROM app.users WHERE phone=$1")
        .bind(v).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let _ = sqlx::query("UPDATE app.user_contact_methods SET disabled_at=now() WHERE value=$1 AND disabled_at IS NULL").bind(v).execute(&st.owner_pool).await;
    let _ = sqlx::query("UPDATE app.users SET phone_opt_out_at=now() WHERE phone=$1 AND phone_opt_out_at IS NULL").bind(v).execute(&st.owner_pool).await;
    for tid in tids { audit(st, tid, "carrier", "contact_optout", json!({"value":v,"kind":kind,"reason":reason})).await; }
}
/// G1 — riattivazione (START): toglie dal DNC e segna i recapiti come DA RI-VERIFICARE.
async fn esc_optout_clear(st: &AppState, value: &str) {
    let v = value.trim(); if v.is_empty() { return; }
    let _ = sqlx::query("DELETE FROM app.contact_optout WHERE value=$1").bind(v).execute(&st.owner_pool).await;
    let _ = sqlx::query("UPDATE app.user_contact_methods SET disabled_at=NULL, verified_at=NULL WHERE value=$1").bind(v).execute(&st.owner_pool).await;
    let _ = sqlx::query("UPDATE app.users SET phone_opt_out_at=NULL WHERE phone=$1").bind(v).execute(&st.owner_pool).await;
}
/// G1 — un recapito è in opt-out (DNC)? Il runner salta questi destinatari.
async fn esc_is_opted_out(st: &AppState, value: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM app.contact_optout WHERE value=$1)")
        .bind(value.trim()).fetch_one(&st.owner_pool).await.unwrap_or(false)
}

/// G3 — Status update NATIVO: quando un incidente cambia stato (ack/chiusura), avvisa gli ALTRI
/// reperibili on-call che hanno lo status-update attivo (override utente -> default tenant),
/// rispettando opt-out. Risolve il gruppo come il runner (profilo > routing > default). Best-effort.
async fn esc_status_update_notify(st: &AppState, tid: Uuid, iid: Uuid, actor: Option<Uuid>, actor_label: &str, action: &str) {
    // default tenant
    let tdef = oncall_cfg(st, tid).await.status_on;
    // gruppo instradato dell'incidente
    let gid: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id)
           FROM app.incidents i
           LEFT JOIN app.alarm_routing r ON r.tenant_id=i.tenant_id AND r.etype=i.etype
           LEFT JOIN app.events ev ON ev.id=i.event_id
           LEFT JOIN app.devices d ON d.id=ev.device_id
           LEFT JOIN app.protection_profile_members m ON m.tenant_id=i.tenant_id AND m.user_id=COALESCE(i.assigned_user,d.assigned_user)
           LEFT JOIN app.protection_profiles pp ON pp.id=m.profile_id AND pp.enabled=true
           LEFT JOIN app.tenant_oncall_cfg oc ON oc.tenant_id=i.tenant_id
          WHERE i.id=$1 AND i.tenant_id=$2")
        .bind(iid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let Some(gid) = gid else { return; };
    let oncall = esc_native_oncall_contacts(st, tid, gid, None, chrono::Utc::now(), false).await;
    let short = iid.to_string(); let short = &short[..8];
    let verb = if action=="close" { "chiuso" } else { "preso in carico" };
    let text = format!("YouWorkforce: incidente {short} {verb} da {actor_label}.");
    for (u, phone, _email) in &oncall {
        if actor == Some(*u) { continue; } // non avvisare chi ha fatto l'azione
        // preferenza per-utente -> default tenant
        let uon: Option<bool> = sqlx::query_scalar("SELECT status_updates_on FROM app.users WHERE id=$1 AND tenant_id=$2")
            .bind(u).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
        if !uon.unwrap_or(tdef) { continue; }
        if let Some(p) = phone.clone().filter(|p| !p.trim().is_empty()) {
            if esc_is_opted_out(st, &p).await { continue; }
            let _ = esc_send_sms_native(st, &p, &text).await;
        }
    }
}

/// Inbound SMS: risposta testuale del reperibile. "OK/ACK/1/PRESO" -> presa in carico;
/// "CHIUDI/CLOSE/RISOLTO" -> chiusura. "STOP" -> opt-out (DNC); "START" -> riattiva.
/// Firma Twilio validata (se token configurato).
pub async fn twilio_ingress_sms(State(st): State<AppState>, h: HeaderMap, axum::extract::Form(params): axum::extract::Form<Vec<(String,String)>>) -> Response {
    if let Err(r) = esc_verify_inbound(&h, "/api/ingress/twilio/sms", &params) { return r; }
    let get = |k: &str| params.iter().find(|(n,_)| n==k).map(|(_,v)| v.clone()).unwrap_or_default();
    let from = get("From"); let body = get("Body").trim().to_uppercase();
    // Opt-out / opt-in (compliance): precedenza su tutto.
    let first = body.split_whitespace().next().unwrap_or("");
    if ["STOP","STOPALL","UNSUBSCRIBE","CANCEL","END","QUIT"].contains(&first) {
        esc_optout_mark(&st, &from, "sms", "carrier_stop").await;
        return twiml("<Response><Message>Disattivato. Rispondi START per riattivare.</Message></Response>");
    }
    if ["START","UNSTOP","YES"].contains(&first) {
        esc_optout_clear(&st, &from).await;
        return twiml("<Response><Message>Riattivato. Verifica il recapito in YouWorkforce.</Message></Response>");
    }
    let action = if ["CHIUDI","CLOSE","RISOLTO"].iter().any(|w| body.contains(w)) { "close" }
                 else if ["OK","ACK","1","PRESO"].iter().any(|w| body.contains(w)) { "ack" }
                 else { "" };
    if !action.is_empty() {
        if let Some((tid, iid, _)) = esc_incident_by_number(&st, &from).await {
            esc_apply_ack(&st, tid, iid, action, "sms").await;
        }
    }
    twiml("<Response></Response>")
}

/// Inbound VOCE: Twilio invia i `Digits` del Gather. "1" -> presa in carico. Se mancano i Digits
/// (hit iniziale) ripropone il Gather. Firma Twilio validata (se token configurato).
pub async fn twilio_ingress_voice(State(st): State<AppState>, h: HeaderMap, axum::extract::Form(params): axum::extract::Form<Vec<(String,String)>>) -> Response {
    if let Err(r) = esc_verify_inbound(&h, "/api/ingress/twilio/voice", &params) { return r; }
    let get = |k: &str| params.iter().find(|(n,_)| n==k).map(|(_,v)| v.clone()).unwrap_or_default();
    let from = get("From"); let digits = get("Digits");
    if digits.is_empty() {
        return twiml("<Response><Gather numDigits=\"1\" timeout=\"20\"><Say language=\"it-IT\">Allarme. Premi 1 per prendere in carico, oppure 9 per non ricevere piu' chiamate.</Say></Gather></Response>");
    }
    if digits == "1" {
        if let Some((tid, iid, _)) = esc_incident_by_number(&st, &from).await {
            esc_apply_ack(&st, tid, iid, "ack", "voice").await;
        }
        return twiml("<Response><Say language=\"it-IT\">Preso in carico. Grazie.</Say></Response>");
    }
    if digits == "9" {
        // opt-out voce (mappatura YW: tasto 9; l'ack resta 1). Disattiva il recapito e il DNC.
        esc_optout_mark(&st, &from, "voice", "carrier_stop").await;
        return twiml("<Response><Say language=\"it-IT\">Recapito disattivato. Riattivalo da YouWorkforce.</Say></Response>");
    }
    twiml("<Response><Say language=\"it-IT\">Tasto non valido.</Say></Response>")
}
