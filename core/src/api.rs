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
// GoAlert consente nei nomi solo lettere/cifre/trattino/underscore/apostrofo/spazio.
fn ga_name(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c=='-' || c=='_' || c=='\'' || c==' ' { c } else { ' ' }).collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}
async fn gql_id(st: &AppState, base: &str, cookie: &str, q: &str, ptr: &str) -> Option<String> {
    goalert::graphql(&st.http, base, cookie, q).await
        .and_then(|v| v.pointer(ptr).and_then(|x| x.as_str().map(String::from)))
}
/// Provisiona in GoAlert i recapiti dell'utente + una regola di notifica immediata.
/// Crea solo i tipi con destinazione abilitata (EMAIL se SMTP, SMS se Twilio). Idempotente.
async fn ensure_contact_methods(st: &AppState, base: &str, cookie: &str, guid: &str, email: &str, phone: Option<&str>, sms_on: bool, email_on: bool) -> i64 {
    let existing = goalert::graphql(&st.http, base, cookie,
        &format!("{{user(id:\"{guid}\"){{contactMethods{{type value}}}}}}")).await;
    let has = |ty: &str, val: &str| existing.as_ref()
        .and_then(|v| v.pointer("/data/user/contactMethods").and_then(|a| a.as_array()))
        .map(|a| a.iter().any(|c| c.get("type").and_then(|x| x.as_str()) == Some(ty)
            && c.get("value").and_then(|x| x.as_str()) == Some(val))).unwrap_or(false);
    let mut n = 0i64;
    if email_on && !email.is_empty() && !has("EMAIL", email) {
        let q = format!("mutation{{createUserContactMethod(input:{{userID:\"{guid}\",name:\"YW email\",type:EMAIL,value:\"{}\",newUserNotificationRule:{{delayMinutes:0}}}}){{id}}}}", esc_gql(email));
        if gql_id(st, base, cookie, &q, "/data/createUserContactMethod/id").await.is_some() { n += 1; }
    }
    if sms_on {
        if let Some(p) = phone {
            if !p.is_empty() && is_e164(p) && !has("SMS", p) {
                let q = format!("mutation{{createUserContactMethod(input:{{userID:\"{guid}\",name:\"YW sms\",type:SMS,value:\"{}\",newUserNotificationRule:{{delayMinutes:0}}}}){{id}}}}", esc_gql(p));
                if gql_id(st, base, cookie, &q, "/data/createUserContactMethod/id").await.is_some() { n += 1; }
            }
        }
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

    let groups = sqlx::query("SELECT id,name,goalert_ep_id,goalert_service_id,goalert_key,oncall_mode,oncall_type,oncall_shift,oncall_tz,goalert_rotation_id FROM app.alarm_groups WHERE tenant_id=$1 ORDER BY name")
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
                // recapiti reali: EMAIL (se SMTP attivo) + SMS (se Twilio attivo e telefono presente)
                let created = ensure_contact_methods(st, &base, &cookie, id, &email, phone.as_deref(), sms_on, email_on).await;
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
        // target dello step: TUTTI i membri (mode 'tutti') oppure il reperibile di TURNO (rotation)
        let mode: String = g.get("oncall_mode");
        let targets = if mode == "turni" {
            let oc_type: String = g.get("oncall_type");
            let oc_shift: i32 = g.get("oncall_shift");
            let oc_tz: String = g.get("oncall_tz");
            let mut rot_id: Option<String> = g.try_get("goalert_rotation_id").ok().flatten();
            let rname = format!("YW {} {} turni {}", &tid.to_string()[..8], ga_name(&gname), suf);
            if rot_id.is_none() {
                rot_id = gql_id(st, &base, &cookie,
                    &format!("mutation{{createRotation(input:{{name:\"{}\",type:{oc_type},start:\"2024-01-01T00:00:00Z\",shiftLength:{oc_shift},timeZone:\"{}\"}}){{id}}}}", esc_gql(&rname), esc_gql(&oc_tz)),
                    "/data/createRotation/id").await;
            }
            if let Some(ref rid) = rot_id {
                // aggiorna membri + parametri della rotation
                let uidlist = uids.iter().map(|u| format!("\"{u}\"")).collect::<Vec<_>>().join(",");
                let _ = goalert::graphql(&st.http, &base, &cookie,
                    &format!("mutation{{updateRotation(input:{{id:\"{rid}\",type:{oc_type},shiftLength:{oc_shift},timeZone:\"{}\",userIDs:[{uidlist}]}})}}", esc_gql(&oc_tz))).await;
                sqlx::query("UPDATE app.alarm_groups SET goalert_rotation_id=$1 WHERE id=$2").bind(rid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?;
                n_rot += 1;
                format!("{{type:rotation,id:\"{rid}\"}}")
            } else {
                // fallback: se la rotation non si crea, notifica tutti
                uids.iter().map(|id| format!("{{type:user,id:\"{id}\"}}")).collect::<Vec<_>>().join(",")
            }
        } else {
            uids.iter().map(|id| format!("{{type:user,id:\"{id}\"}}")).collect::<Vec<_>>().join(",")
        };
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
pub async fn traccar_login(http: &reqwest::Client, base: &str, user: &str, pass: &str) -> Option<String> {
    let resp = http.post(format!("{base}/api/session"))
        .form(&[("email", user), ("password", pass)]).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    let mut c = Vec::new();
    for v in resp.headers().get_all(reqwest::header::SET_COOKIE).iter() {
        if let Ok(s)=v.to_str() { if let Some(p)=s.split(';').next() { c.push(p.to_string()); } } }
    if c.is_empty() { None } else { Some(c.join("; ")) }
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

/// Esegue un pull completo TB per il tenant: specchio device + telemetria -> regole.
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
      Scenario{ code:"smart_farming", name:"Smart Farming", sector:"Agricoltura", desc:"Indicatori di produzione agricola e stato delle strutture.",
        modules:vec!["iot"],
        rules:vec![ r("soil_moisture","<=",Some(20.0),crit), r("temperature","<=",Some(2.0),crit), r("humidity",">=",Some(90.0),tele) ],
        devices:vec![ d("field-1","Sonda campo 1") ] },
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
    let cat = scenario_catalog();
    let scn = cat.iter().find(|s| s.code==b.code).ok_or_else(|| err(StatusCode::BAD_REQUEST, "scenario sconosciuto"))?;
    let with_demo = b.with_demo.unwrap_or(false);
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
    audit(&st, tid, "superadmin", "scenario_apply", json!({"code":scn.code,"with_demo":with_demo,"rules_added":rules_added,"devices_added":devices_added})).await;
    Ok(Json(json!({"ok":true,"code":scn.code,"modules":scn.modules,"rules_added":rules_added,"devices_added":devices_added})))
}
/// Rimuove uno scenario: regole del bundle + device demo (se non referenziati). I moduli
/// restano abilitati (potrebbero servire ad altri scenari/uso del tenant).
pub async fn super_remove_scenario(State(st): State<AppState>, h: HeaderMap, Path((tid,code)): Path<(Uuid,String)>) -> R {
    require_super(&st, &h).await?;
    let rules = sqlx::query("DELETE FROM app.sensor_rules WHERE tenant_id=$1 AND label LIKE $2")
        .bind(tid).bind(format!("scn:{}:%", code)).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    // device demo solo se non referenziati da eventi (FK-safe)
    let devs = sqlx::query("DELETE FROM app.devices WHERE tenant_id=$1 AND source='scenario' AND external_id LIKE $2
        AND id NOT IN (SELECT device_id FROM app.events WHERE device_id IS NOT NULL)")
        .bind(tid).bind(format!("scn-{}-%", code)).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    sqlx::query("DELETE FROM app.tenant_scenarios WHERE tenant_id=$1 AND scenario_code=$2")
        .bind(tid).bind(&code).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "superadmin", "scenario_remove", json!({"code":code,"rules_removed":rules,"devices_removed":devs})).await;
    Ok(Json(json!({"ok":true,"rules_removed":rules,"devices_removed":devs,"note":"moduli lasciati abilitati"})))
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
        "temperature"|"engine_temp" => "°C", "humidity"|"soil_moisture"|"fill_level"|"power_factor" => "%",
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
        "soil_moisture" => "Umidità suolo", "fill_level" => "Riempimento", "level" => "Livello", "fuel" => "Carburante",
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
    let devs = sqlx::query("SELECT id,external_id,label,last_lat,last_lon FROM app.devices WHERE source='scenario' AND external_id LIKE $1 ORDER BY external_id")
        .bind(format!("scn-{}-%", scn.code)).fetch_all(&mut *tx).await.map_err(ise)?;
    let ids: Vec<Uuid> = devs.iter().map(|r| r.get::<Uuid,_>("id")).collect();
    let evs = if ids.is_empty() { Vec::new() } else {
        sqlx::query("SELECT payload_ref,origin_ts FROM app.events WHERE device_id = ANY($1) AND payload_ref IS NOT NULL ORDER BY origin_ts DESC LIMIT 200")
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
    Ok(json!({
        "code":scn.code, "name":scn.name, "sector":scn.sector, "ai_focus":scenario_ai_focus(scn.code),
        "narrative": narrative, "generated": spec.is_some(),
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
    sqlx::query_scalar::<_, bool>("SELECT enabled FROM app.ai_features WHERE tenant_id=$1 AND feature=$2")
        .bind(tid).bind(feature).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or(false)
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
    let mut map = serde_json::Map::new();
    for f in AI_FEATURES { map.insert((*f).to_string(), json!(false)); }
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
    require_module(&st, tid, "loneworker").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let groups = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz FROM app.alarm_groups ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
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
            "oncall_shift":g.get::<i32,_>("oncall_shift"),"oncall_tz":g.get::<String,_>("oncall_tz")})
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
#[derive(Deserialize)]
pub struct GroupOncallReq { pub mode: String, pub oncall_type: Option<String>, pub shift: Option<i32>, pub tz: Option<String> }
// Turni di reperibilita' del gruppo (effetto alla prossima Sincronizza con GoAlert).
pub async fn set_group_oncall(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GroupOncallReq>) -> R {
    let s = require_tenant(&st, &h).await?; admin_only(&s)?;
    let tid = s.tenant_id.unwrap();
    if !matches!(b.mode.as_str(), "tutti"|"turni") { return Err(err(StatusCode::BAD_REQUEST, "mode: 'tutti' o 'turni'")); }
    let ty = b.oncall_type.as_deref().unwrap_or("weekly");
    if !matches!(ty, "hourly"|"daily"|"weekly"|"monthly") { return Err(err(StatusCode::BAD_REQUEST, "tipo turno non valido")); }
    let shift = b.shift.unwrap_or(1).clamp(1, 999);
    let tz = b.tz.as_deref().unwrap_or("Europe/Rome");
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.alarm_groups SET oncall_mode=$2,oncall_type=$3,oncall_shift=$4,oncall_tz=$5 WHERE id=$1")
        .bind(id).bind(&b.mode).bind(ty).bind(shift).bind(tz).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    Ok(Json(json!({"ok":true})))
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
    let trow = sqlx::query("SELECT tenant_id,source FROM app.ingest_tokens WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid: Uuid = trow.get("tenant_id");
    if !module_active(&st, tid, "thingsboard").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo thingsboard non abilitato"));
    }
    let device = q.get("device").cloned().filter(|s| !s.is_empty());

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
    if owner_id != Some(s.user_id) && !is_approver(&s.role) && s.role != "auditor" {
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
            => Some(ToolMeta{sensitive:false, writes:false}),
        // scrittura a basso rischio -> esecuzione diretta (audit)
        "add_diary_entry"|"add_incident_note"|"add_timesheet_line"|"add_expense"|"save_report"
            => Some(ToolMeta{sensitive:false, writes:true}),
        // scrittura sensibile -> richiede conferma operatore
        "create_work_order"|"create_sensor_rule"|"tb_sync_devices"
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
            if !is_planner(&s.role) { return Err("solo pianificatore/admin puo' creare ordini".into()); }
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
e verranno confermate dall'operatore prima di scrivere. NON puoi in alcun modo agire sulla catena di sicurezza \
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
