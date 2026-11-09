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
    "batteria_critica"];

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

/// Mappa un evento Traccar (tipo + eventuale alarm + batteria) nell'etype
/// normalizzato, applicando la config per-tenant `traccar/alarm_map`.
/// Usata SIA dal webhook di forwarding SIA dal poller API, così push e pull
/// producono esattamente lo stesso incidente (idempotente a valle).
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
        "traccar_event".into()
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
    Some(Session {
        user_id: r.get("user_id"),
        tenant_id: r.try_get::<Option<Uuid>, _>("tenant_id").ok().flatten(),
        email: r.get("email"),
        role: r.get("role"),
        is_super: r.get("is_super"),
        impersonator: r.try_get::<Option<String>, _>("impersonator").ok().flatten(),
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
                    role: "superadmin".into(), is_super: true, impersonator: None };
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
    let sess = Session { user_id: s.user_id, tenant_id: None, email: imp.clone(), role: "superadmin".into(), is_super: true, impersonator: None };
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
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "code": r.get::<String,_>("code"), "name": r.get::<String,_>("name"), "kind": r.get::<String,_>("kind"),
        "available": r.get::<bool,_>("available"), "state": r.try_get::<String,_>("state").ok().unwrap_or("non_abilitato".into()) })).collect();
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
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct SetTraccarConn { pub base_url: String, pub username: String, pub password: Option<String> }
pub async fn super_set_traccar(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetTraccarConn>) -> R {
    require_super(&st, &h).await?;
    let base = b.base_url.trim_end_matches('/').to_string();
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

// P3: provisioning GoAlert per-tenant (crea escalation policy + servizio + generic key).
#[derive(Deserialize)]
pub struct GoalertProvision { pub user: String, pub pass: String, pub name: Option<String> }
pub async fn goalert_provision(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<GoalertProvision>) -> R {
    require_super(&st, &h).await?;
    let base = st.goalert_base.clone().ok_or_else(|| err(StatusCode::BAD_REQUEST, "GOALERT_BASE non configurato"))?;
    let cookie = goalert::login(&st.http, &base, &b.user, &b.pass).await
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "login GoAlert fallito"))?;
    // suffisso unico: i nomi EP/service devono essere univoci su GoAlert (re-provision sicuro)
    let suf = chrono::Utc::now().timestamp();
    let name = format!("{} {}", b.name.clone().unwrap_or_else(|| format!("YW {}", &tid.to_string()[..8])), suf);
    // EP (repeat 1, nessuno step: lo configura l'admin in GoAlert), servizio, generic key
    let ep = goalert::graphql(&st.http,&base,&cookie,
        &format!("mutation{{createEscalationPolicy(input:{{name:\"{name} EP\",repeat:1}}){{id}}}}")).await
        .and_then(|v| v.pointer("/data/createEscalationPolicy/id").and_then(|x| x.as_str().map(String::from)))
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "createEscalationPolicy fallito"))?;
    let sid = goalert::graphql(&st.http,&base,&cookie,
        &format!("mutation{{createService(input:{{name:\"{name}\",escalationPolicyID:\"{ep}\"}}){{id}}}}")).await
        .and_then(|v| v.pointer("/data/createService/id").and_then(|x| x.as_str().map(String::from)))
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "createService fallito"))?;
    let key = goalert::graphql(&st.http,&base,&cookie,
        &format!("mutation{{createIntegrationKey(input:{{serviceID:\"{sid}\",type:generic,name:\"core-outbox\"}}){{id}}}}")).await
        .and_then(|v| v.pointer("/data/createIntegrationKey/id").and_then(|x| x.as_str().map(String::from)))
        .ok_or_else(|| err(StatusCode::BAD_GATEWAY, "createIntegrationKey fallito"))?;
    // heartbeat monitor (P3): avvisa se la nostra pipeline smette di pingare
    let hb = goalert::graphql(&st.http,&base,&cookie,
        &format!("mutation{{createHeartbeatMonitor(input:{{serviceID:\"{sid}\",name:\"yw-core\",timeoutMinutes:5}}){{href}}}}")).await
        .and_then(|v| v.pointer("/data/createHeartbeatMonitor/href").and_then(|x| x.as_str().map(String::from)));
    let enc_pass = st.secrets.encrypt(&b.pass);   // P3: cifrato a riposo
    sqlx::query("INSERT INTO app.goalert_services(tenant_id,integration_key,service_id,gql_user,gql_pass,heartbeat_url) VALUES($1,$2,$3,$4,$5,$6)
        ON CONFLICT (tenant_id) DO UPDATE SET integration_key=EXCLUDED.integration_key,service_id=EXCLUDED.service_id,
          gql_user=EXCLUDED.gql_user,gql_pass=EXCLUDED.gql_pass,heartbeat_url=EXCLUDED.heartbeat_url,updated_at=now()")
        .bind(tid).bind(&key).bind(&sid).bind(&b.user).bind(&enc_pass).bind(&hb).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "superadmin", "goalert_provision", json!({"service_id":sid})).await;
    Ok(Json(json!({"ok":true,"escalation_policy":ep,"service_id":sid,"integration_key":key,"heartbeat_url":hb})))
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
pub async fn traccar_login(http: &reqwest::Client, base: &str, user: &str, pass: &str) -> Option<String> {
    let resp = http.post(format!("{base}/api/session"))
        .form(&[("email", user), ("password", pass)]).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    let mut c = Vec::new();
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        if let Ok(s)=v.to_str() { if let Some(p)=s.split(';').next() { c.push(p.to_string()); } } }
    if c.is_empty() { None } else { Some(c.join("; ")) }
}
pub async fn traccar_get_config(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let (base,user,pass) = traccar_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "connessione Traccar non configurata"))?;
    let cookie = traccar_login(&st.http, &base, &user, &pass).await.ok_or_else(|| err(StatusCode::BAD_GATEWAY, "login Traccar fallito"))?;
    let srv: Value = st.http.get(format!("{base}/api/server")).header("Cookie", &cookie).send().await
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
    let cookie = traccar_login(&st.http, &base, &user, &pass).await.ok_or_else(|| err(StatusCode::BAD_GATEWAY, "login Traccar fallito"))?;
    let mut srv: Value = st.http.get(format!("{base}/api/server")).header("Cookie", &cookie).send().await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar irraggiungibile"))?.json().await
        .map_err(|_| err(StatusCode::BAD_GATEWAY, "Traccar json"))?;
    let o = srv.as_object_mut().ok_or_else(|| err(StatusCode::BAD_GATEWAY, "server model"))?;
    for e in &b.entries {
        if TR_CFG_BOOL.contains(&e.id.as_str()) { o.insert(e.id.clone(), json!(e.value=="true")); }
        else if TR_CFG_STR.contains(&e.id.as_str()) { o.insert(e.id.clone(), json!(e.value)); }
        else if TR_CFG_NUM.contains(&e.id.as_str()) { o.insert(e.id.clone(), json!(e.value.parse::<f64>().unwrap_or(0.0))); }
    }
    let put = st.http.put(format!("{base}/api/server")).header("Cookie", &cookie).json(&srv).send().await
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
            let sess = Session { user_id: uid, tenant_id: Some(tid), email: b.email.clone(), role: role.clone(), is_super: false, impersonator: None };
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
    Ok(Json(json!({"email":s.email,"role":s.role,"is_super":s.is_super,"tenant_id":s.tenant_id,"impersonator":s.impersonator})))
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
    let rows = sqlx::query("SELECT id,name FROM app.sites ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name")})).collect();
    Ok(Json(json!(out)))
}

pub async fn list_users(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,email,role FROM app.users ORDER BY role,email").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "email": r.get::<String,_>("email"), "role": r.get::<String,_>("role") })).collect();
    Ok(Json(json!(out)))
}

// ============================ Gestione (Fase 1) ============================
// Anagrafiche e operazioni che mancavano: dispositivi, sedi, utenti, gruppi di
// allarme + routing, assegnazione/note incidenti, check-in in centrale.
fn admin_only(s: &Session) -> Result<(), (StatusCode, String)> {
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "riservato all'admin del tenant")); }
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
                d.traccar_status,
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
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "dispositivo gia' presente"))?.get::<Uuid,_>("id");
    audit(&st, tid, &s.email, "create_device", json!({"external_id":b.external_id,"source":b.source})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn update_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<DeviceReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query(
        "UPDATE app.devices SET external_id=$2,source=$3,label=$4,site_id=$5,assigned_user=$6,
            enabled=COALESCE($7,enabled),config=COALESCE($8,config),updated_at=now() WHERE id=$1")
        .bind(id).bind(b.external_id.trim()).bind(&b.source).bind(&b.label).bind(b.site_id).bind(b.assigned_user)
        .bind(b.enabled).bind(b.config.clone()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
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
pub async fn list_ingest_tokens(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, CONNECTOR_MODS).await?;
    let rows = sqlx::query("SELECT token,source,created_at FROM app.ingest_tokens WHERE tenant_id=$1 ORDER BY source,created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "token": r.get::<String,_>("token"), "source": r.get::<String,_>("source"),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at") })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct TokenReq { pub source: String }
pub async fn create_ingest_token(State(st): State<AppState>, h: HeaderMap, Json(b): Json<TokenReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    const SRC: &[&str] = &["traccar","thingsboard","iot"];
    if !SRC.contains(&b.source.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "sorgente non valida")); }
    let token = format!("{}-{}", b.source, &auth::new_token()[..24]);
    sqlx::query("INSERT INTO app.ingest_tokens(token,tenant_id,source) VALUES($1,$2,$3)")
        .bind(&token).bind(tid).bind(&b.source).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_ingest_token", json!({"source":b.source})).await;
    Ok(Json(json!({"token":token})))
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
    let n = sqlx::query("UPDATE app.devices SET assigned_user=$2, assigned_asset=$3, updated_at=now() WHERE id=$1")
        .bind(id).bind(b.user_id).bind(b.asset_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "device inesistente")); }
    audit(&st, tid, &s.email, "assign_device", json!({"device":id,"user":b.user_id,"asset":b.asset_id})).await;
    Ok(Json(json!({"ok":true})))
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
pub struct SiteReq { pub name: String }
pub async fn create_site(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SiteReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.sites(tenant_id,name) VALUES($1,$2) RETURNING id")
        .bind(tid).bind(b.name.trim()).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_site", json!({"name":b.name})).await;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn update_site(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<SiteReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.sites SET name=$2 WHERE id=$1").bind(id).bind(b.name.trim()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
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
pub struct UpdateUser { pub role: Option<String>, pub password: Option<String> }
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
    require_module(&st, tid, "loneworker").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let groups = sqlx::query("SELECT id,name FROM app.alarm_groups ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
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
        json!({"id":gid,"name":g.get::<String,_>("name"),"members":mem})
    }).collect();
    let routes: Vec<Value> = routing.iter().map(|r| json!({"etype":r.get::<String,_>("etype"),"group_id":r.get::<Uuid,_>("group_id")})).collect();
    Ok(Json(json!({"groups":out,"routing":routes})))
}
#[derive(Deserialize)]
pub struct GroupReq { pub name: String }
pub async fn create_group(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.alarm_groups(tenant_id,name) VALUES($1,$2) RETURNING id")
        .bind(tid).bind(b.name.trim()).fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "gruppo gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "gruppo gia' presente"))?.get::<Uuid,_>("id");
    Ok(Json(json!({"id":id})))
}
pub async fn delete_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
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
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.alarm_group_members(tenant_id,group_id,user_id,position) VALUES($1,$2,$3,COALESCE($4,1))
        ON CONFLICT (group_id,user_id) DO UPDATE SET position=EXCLUDED.position")
        .bind(tid).bind(gid).bind(b.user_id).bind(b.position).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn remove_group_member(State(st): State<AppState>, h: HeaderMap, Path((gid,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
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

async fn audit(st: &AppState, tid: Uuid, actor: &str, action: &str, detail: Value) {
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,$3,$4)")
        .bind(tid).bind(actor).bind(action).bind(detail).execute(&st.owner_pool).await;
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
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
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
        "cancel" => { if !is_planner(&s.role) { let _=tx.rollback().await; return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
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
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
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
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
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
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
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
    let trow = sqlx::query("SELECT tenant_id,source FROM app.ingest_tokens WHERE token=$1")
        .bind(token).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid: Uuid = trow.get("tenant_id");
    let source: String = trow.get("source");
    // modular per-tenant: a disabled module refuses new ingress too (history stays readable)
    if !module_active(&st, tid, "loneworker").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo loneworker non abilitato"));
    }

    let idem = b.idem_key.clone().unwrap_or_else(|| b.source_event.clone());
    if idem.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "idem key required")); }
    let correlation = b.correlation.clone().unwrap_or_else(|| Uuid::new_v4().to_string());

    persist_ingest(&st, tid, Normalized {
        source,
        source_event: b.source_event.clone(),
        idem,
        etype: b.etype.clone(),
        device_external_id: b.device_external_id.clone(),
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
pub async fn persist_ingest(st: &AppState, tid: Uuid, n: Normalized) -> R {
    let now = st.clock.now();
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
        .bind(&n.etype).bind(now).bind(now).bind(n.lat).bind(n.lon).bind(n.accuracy_m)
        .bind(n.payload.clone()).bind(&n.correlation).bind(n.indoor).bind(&n.beacon_uid).bind(&n.zone)
        .fetch_optional(&mut *tx).await.map_err(ise)?;

    let Some(ev) = ev else {
        tx.commit().await.map_err(ise)?;
        return Ok(Json(json!({"deduplicated":true,"correlation":n.correlation})));
    };
    let eid: Uuid = ev.get("id");

    let mut incident_id: Option<Uuid> = None;
    if CRITICAL.contains(&n.etype.as_str()) {
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
    let trow = sqlx::query("SELECT tenant_id,source FROM app.ingest_tokens WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid: Uuid = trow.get("tenant_id");
    let source: String = trow.get("source");
    if !module_active(&st, tid, "traccar").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo traccar non abilitato"));
    }

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
        device_external_id: unique_id,
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
    let rows = sqlx::query("SELECT g.id,g.name,g.kind,g.geom,g.site_id,g.floorplan_id,si.name AS site
        FROM app.geofences g LEFT JOIN app.sites si ON si.id=g.site_id ORDER BY g.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"kind":r.get::<String,_>("kind"),
        "geom":r.get::<Value,_>("geom"),"site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok(),
        "floorplan_id":r.try_get::<Uuid,_>("floorplan_id").ok() })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct GeofenceReq { pub name: String, pub kind: String, pub geom: Value, pub site_id: Option<Uuid>, pub floorplan_id: Option<Uuid> }
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
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.geofences(tenant_id,name,kind,geom,site_id,floorplan_id) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.kind).bind(&b.geom).bind(b.site_id).bind(b.floorplan_id)
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
    Ok(Json(json!({"ok":true})))
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
    let tid: Uuid = sqlx::query_scalar("SELECT tenant_id FROM app.ingest_tokens WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    if !module_active(&st, tid, "traccar").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo traccar non abilitato"));
    }
    let pos = b.get("position").or(Some(&b)).unwrap();
    let dev = b.get("device");
    let unique_id = dev.and_then(|d| d.get("uniqueId")).and_then(|v| v.as_str())
        .or_else(|| q.get("id").map(|s| s.as_str()));
    let lat = pos.get("latitude").and_then(|v| v.as_f64());
    let lon = pos.get("longitude").and_then(|v| v.as_f64());
    if lat.is_none() || lon.is_none() { return Err(err(StatusCode::BAD_REQUEST, "posizione senza coordinate")); }
    let Some(ext) = unique_id else { return Err(err(StatusCode::BAD_REQUEST, "device uniqueId mancante")); };
    let out = process_traccar_position(&st, tid, pos, ext).await.map_err(ise)?;
    Ok(Json(out))
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
pub async fn ingest_thingsboard(
    State(st): State<AppState>,
    h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
    Json(b): Json<Value>,
) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| q.get("token").cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let trow = sqlx::query("SELECT tenant_id,source FROM app.ingest_tokens WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid: Uuid = trow.get("tenant_id");
    if !module_active(&st, tid, "thingsboard").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo thingsboard non abilitato"));
    }
    let device = q.get("device").cloned().filter(|s| !s.is_empty());

    let temp = b.get("temperature").and_then(|v| v.as_f64());
    let alarm_flag = b.get("alarm").and_then(|v| v.as_bool()).unwrap_or(false)
        || b.get("critical").and_then(|v| v.as_bool()).unwrap_or(false);
    // Per-tenant severity threshold (thingsboard/severity_gate).
    let gate = tool_config(&st, tid, "thingsboard", "severity_gate").await;
    let temp_max = gate.get("temperature_max").and_then(|v| v.as_f64()).unwrap_or(90.0);
    let critical = alarm_flag || temp.map(|t| t >= temp_max).unwrap_or(false);
    let etype = if critical { "sensore_critico" } else { "telemetria" }.to_string();

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
    require_module(&st, s.tenant_id.unwrap(), "loneworker").await?;
    let acl = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT i.id,i.etype,i.state,i.priority,i.correlation,i.opened_at,i.outcome,i.assigned_user,i.site_id,
                s.name AS site, u.email AS assignee
         FROM app.incidents i LEFT JOIN app.sites s ON s.id=i.site_id LEFT JOIN app.users u ON u.id=i.assigned_user
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
        "outcome": r.try_get::<String,_>("outcome").ok() })).collect();
    Ok(Json(json!(out)))
}

pub async fn incident_timeline(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
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
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") {
        return Err(err(StatusCode::FORBIDDEN, "role cannot ack"));
    }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE id=$1 AND state='aperto'")
        .bind(iid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"not open")); }
    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'human_ack',$3)")
        .bind(tid).bind(iid).bind(json!({"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_ack","incident_id":iid})}).await;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct CloseReq { pub outcome: String }
pub async fn close_incident(State(st): State<AppState>, h: HeaderMap, Path(iid): Path<Uuid>, Json(b): Json<CloseReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !matches!(s.role.as_str(), "operatore"|"tenant_admin") {
        return Err(err(StatusCode::FORBIDDEN, "role cannot close"));
    }
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
    Ok(Json(json!({"ok":true})))
}

// ----------------------------- worker SOS -----------------------------
/// Worker-originated SOS from the PWA (session-authenticated). Distinct from
/// machine ingress; never depends on STT/RAG (contract line 134).
pub async fn worker_sos(State(st): State<AppState>, h: HeaderMap, Json(b): Json<serde_json::Value>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let lat = b.get("lat").and_then(|v| v.as_f64());
    let lon = b.get("lon").and_then(|v| v.as_f64());
    let correlation = Uuid::new_v4().to_string();
    let now = st.clock.now();
    let (pri, repeat) = sos_escalation(&st, tid).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ev = sqlx::query(
        "INSERT INTO app.events(tenant_id,source,source_event,idem_key,etype,origin_ts,received_ts,lat,lon,correlation)
         VALUES($1,'pwa',$2,$2,'sos',$3,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(format!("sos-{}-{}", s.user_id, now.timestamp_millis()))
        .bind(now).bind(lat).bind(lon).bind(&correlation).fetch_one(&mut *tx).await.map_err(ise)?;
    let eid: Uuid = ev.get("id");
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
    let n = sqlx::query("UPDATE app.checkins SET state='confermato' WHERE id=$1 AND state='attivo'")
        .bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
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
fn is_planner(role: &str) -> bool { matches!(role, "pianificatore"|"tenant_admin") }
fn is_approver(role: &str) -> bool { matches!(role, "responsabile"|"tenant_admin") }

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
    })))
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
    pub scheduled_start: Option<String>, pub scheduled_end: Option<String>, pub priority: Option<i32> }
pub async fn create_order(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewOrder>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    let ss = b.scheduled_start.as_deref().map(parse_ts).transpose()?;
    let se = b.scheduled_end.as_deref().map(parse_ts).transpose()?;
    if let (Some(a),Some(z))=(ss,se) { if z<=a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); } }
    let pri = b.priority.unwrap_or(2).clamp(1,3);
    let state = if b.assigned_user.is_some() { "assegnato" } else { "bozza" };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.work_orders(tenant_id,site_id,title,instructions,assigned_user,state,created_by,scheduled_start,scheduled_end,priority)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id")
        .bind(tid).bind(b.site_id).bind(&b.title).bind(&b.instructions).bind(b.assigned_user).bind(state).bind(s.user_id)
        .bind(ss).bind(se).bind(pri)
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
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
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
    let cols="o.id,o.title,o.state,o.version,o.outcome,o.instructions,o.site_id,o.assigned_user,o.scheduled_start,o.scheduled_end,o.priority,s.name AS site,u.email AS assignee";
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
        "instructions": r.try_get::<String,_>("instructions").ok() })).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct OrderAction { pub version: i32, pub user_id: Option<Uuid>, pub outcome: Option<String>, pub reason: Option<String> }

async fn order_transition(st: &AppState, s: &Session, iid: Uuid, b: &OrderAction,
    to: &str, set_sql: &str, from_states: &[&str]) -> R {
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // version check + state check inside the UPDATE (optimistic concurrency)
    let states = from_states.iter().map(|x| format!("'{x}'")).collect::<Vec<_>>().join(",");
    let extra = if set_sql.is_empty() { String::new() } else { format!("{set_sql}, ") };
    let sql = format!("UPDATE app.work_orders SET {extra}state='{to}', version=version+1, updated_at=now()
         WHERE id=$1 AND version=$2 AND state IN ({states})");
    let q = sqlx::query(&sql).bind(iid).bind(b.version);
    let n = q.execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n == 0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT, "versione superata o stato non valido")); }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":to})))
}

pub async fn order_assign(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let uid = b.user_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "user_id richiesto"))?;
    order_transition(&st, &s, id, &b, "assegnato",
        &format!("assigned_user='{uid}'"), &["bozza","assegnato"]).await
}
pub async fn order_start(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    // only the assigned technician may start
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.work_orders WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
    let _ = tx.commit().await;
    if owner != Some(s.user_id) && !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "non assegnato a te")); }
    order_transition(&st, &s, id, &b, "in_corso", "", &["assegnato"]).await
}
pub async fn order_complete(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    let outcome = b.outcome.clone().unwrap_or_default().replace('\'', "''");
    order_transition(&st, &s, id, &b, "completato", &format!("outcome='{outcome}'"), &["in_corso"]).await
}
pub async fn order_cancel(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let reason = b.reason.clone().unwrap_or_default().replace('\'', "''");
    order_transition(&st, &s, id, &b, "annullato", &format!("cancel_reason='{reason}'"), &["bozza","assegnato","in_corso"]).await
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
    let rows = if is_approver(&s.role) || s.role=="auditor" {
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
    if owner_id != Some(s.user_id) && !is_approver(&s.role) && s.role != "auditor" {
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
    if !is_approver(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo responsabile")); }
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
    if !is_approver(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo responsabile")); }
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
    if owner_id != Some(s.user_id) && !is_approver(&s.role) && s.role != "auditor" {
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

    // --- critical-intent guardrail: never act, point to manual procedure ---
    let critical = ["chiudi","annulla","silenzia","spegni","disattiva","ferma","tacita"];
    let on_safety = ["sos","allarme","allarmi","protezione","escalation","reperib","emergenz","incident"];
    if critical.iter().any(|w| q.contains(w)) && on_safety.iter().any(|w| q.contains(w)) {
        return Ok(Json(json!({
            "provider":"SIMULATO","intent":"critical_refused",
            "answer":"Operazione critica sulla catena di sicurezza. L'assistente non la esegue: usa la procedura manuale autorizzata in centrale (presa in carico / chiusura con esito). Il percorso SOS non dipende dall'assistente.",
            "sources":[],"period":null })));
    }

    let has = |ws: &[&str]| ws.iter().any(|w| q.contains(w));
    let now = st.clock.now();

    // --- intent routing (local deterministic planner) ---
    if has(&["check-in","check in","checkin","scadut","mancato"]) {
        let site = match_site(&st, tid, &q).await?;
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
        return Ok(Json(json!({"provider":"SIMULATO","intent":"checkins_scaduti","answer":ans,"sources":srcs,"period":"incidenti aperti"})));
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
        return Ok(Json(json!({"provider":"SIMULATO","intent":"sensori_offline","answer":ans,"sources":srcs,"period":format!("ultime {hours}h")})));
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
        return Ok(Json(json!({"provider":"SIMULATO","intent":"incidente_storia","answer":ans,"sources":srcs,"period":"ultimo incidente"})));
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
        return Ok(Json(json!({"provider":"SIMULATO","intent":"moduli_degradati","answer":ans,"sources":srcs,"period":"stato attuale"})));
    }

    if has(&["ordin","intervent","lavoro assegnat"]) {
        if !module_active(&st, tid, "interventi").await {
            return Ok(Json(json!({"provider":"SIMULATO","intent":"ordini_aperti","answer":"Modulo interventi non abilitato: dato non disponibile.","sources":[],"period":null})));
        }
        let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let rows = sqlx::query("SELECT id,title,state FROM app.work_orders WHERE state IN ('assegnato','in_corso') ORDER BY updated_at DESC").fetch_all(&mut *tx).await.map_err(ise)?;
        tx.commit().await.map_err(ise)?;
        let srcs: Vec<Value> = rows.iter().map(|r| json!({"type":"order","id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),"state":r.get::<String,_>("state")})).collect();
        let ans = if rows.is_empty() { "Nessun ordine attivo.".into() } else { format!("{} ordini attivi.", rows.len()) };
        return Ok(Json(json!({"provider":"SIMULATO","intent":"ordini_aperti","answer":ans,"sources":srcs,"period":"attivi"})));
    }

    // no confident intent -> ask for clarification (never guess an operation)
    Ok(Json(json!({"provider":"SIMULATO","intent":"clarify",
        "answer":"Non ho abbastanza elementi. Prova: «check-in scaduti al sito Milano», «sensori che non comunicano», «ricostruisci l'ultimo allarme», «ordini attivi», «moduli degradati».",
        "sources":[],"period":null})))
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
