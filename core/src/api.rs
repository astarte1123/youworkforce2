use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, Response, sse::{Sse, Event as SseEvent}}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::convert::Infallible;
use uuid::Uuid;
use futures::stream::Stream;
use crate::state::{AppState, Session, TenantEvent};
use crate::{auth, db};

type R = Result<Json<Value>, (StatusCode, String)>;
fn err(c: StatusCode, m: &str) -> (StatusCode, String) { (c, m.to_string()) }

const CRITICAL: &[&str] = &["sos", "man_down", "immobilita", "mancato_checkin", "sensore_critico"];

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
        "SELECT user_id, tenant_id, email, role, is_super
         FROM app.sessions WHERE token=$1 AND expires_at > now()")
        .bind(&tok).fetch_optional(&state.owner_pool).await.ok().flatten()?;
    Some(Session {
        user_id: r.get("user_id"),
        tenant_id: r.try_get::<Option<Uuid>, _>("tenant_id").ok().flatten(),
        email: r.get("email"),
        role: r.get("role"),
        is_super: r.get("is_super"),
    })
}

/// Persist a new session (12h TTL). Opportunistic cleanup of expired rows.
async fn persist_session(state: &AppState, tok: &str, s: &Session) {
    let _ = sqlx::query("DELETE FROM app.sessions WHERE expires_at <= now()")
        .execute(&state.owner_pool).await;
    let _ = sqlx::query(
        "INSERT INTO app.sessions(token,user_id,tenant_id,email,role,is_super,expires_at)
         VALUES($1,$2,$3,$4,$5,$6, now() + interval '12 hours')")
        .bind(tok).bind(s.user_id).bind(s.tenant_id).bind(&s.email).bind(&s.role).bind(s.is_super)
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
                    role: "superadmin".into(), is_super: true };
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
pub struct CreateTenant { pub slug: String, pub name: String, pub admin_email: String, pub admin_password: String }

pub async fn create_tenant(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CreateTenant>) -> R {
    require_super(&st, &h).await?;
    let slug = b.slug.to_lowercase();
    const RESERVED: &[&str] = &["tenants","www","admin","api","app"];
    if RESERVED.contains(&slug.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "reserved slug")); }
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    let trow = sqlx::query("INSERT INTO app.tenants(slug,name) VALUES($1,$2) RETURNING id")
        .bind(&slug).bind(&b.name).fetch_one(&mut *tx).await
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
    let rows = sqlx::query("SELECT id,slug,name,created_at FROM app.tenants ORDER BY created_at")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "slug": r.get::<String,_>("slug"),
        "name": r.get::<String,_>("name") })).collect();
    Ok(Json(json!(out)))
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
pub struct SetGoalert { pub integration_key: String }
pub async fn super_set_goalert(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetGoalert>) -> R {
    require_super(&st, &h).await?;
    sqlx::query("INSERT INTO app.goalert_services(tenant_id,integration_key) VALUES($1,$2)
        ON CONFLICT (tenant_id) DO UPDATE SET integration_key=EXCLUDED.integration_key, updated_at=now()")
        .bind(tid).bind(&b.integration_key).execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,'superadmin','set_goalert_key',$2)")
        .bind(tid).bind(json!({"len":b.integration_key.len()})).execute(&st.owner_pool).await.map_err(ise)?;
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
            let sess = Session { user_id: uid, tenant_id: Some(tid), email: b.email.clone(), role: role.clone(), is_super: false };
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
    Ok(Json(json!({"email":s.email,"role":s.role,"is_super":s.is_super,"tenant_id":s.tenant_id})))
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
        "INSERT INTO app.events(tenant_id,source,source_event,idem_key,site_id,device_id,etype,origin_ts,received_ts,lat,lon,accuracy_m,payload_ref,correlation)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT (tenant_id,source,idem_key) DO NOTHING
         RETURNING id")
        .bind(tid).bind(&n.source).bind(&n.source_event).bind(&n.idem).bind(site_id).bind(device_id)
        .bind(&n.etype).bind(now).bind(now).bind(n.lat).bind(n.lon).bind(n.accuracy_m)
        .bind(n.payload.clone()).bind(&n.correlation)
        .fetch_optional(&mut *tx).await.map_err(ise)?;

    let Some(ev) = ev else {
        tx.commit().await.map_err(ise)?;
        return Ok(Json(json!({"deduplicated":true,"correlation":n.correlation})));
    };
    let eid: Uuid = ev.get("id");

    let mut incident_id: Option<Uuid> = None;
    if CRITICAL.contains(&n.etype.as_str()) {
        let irow = sqlx::query(
            "INSERT INTO app.incidents(tenant_id,event_id,site_id,etype,correlation,priority)
             VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
            .bind(tid).bind(eid).bind(site_id).bind(&n.etype).bind(&n.correlation)
            .bind(if n.etype=="sos" {3} else {2})
            .fetch_one(&mut *tx).await.map_err(ise)?;
        let iid: Uuid = irow.get("id");
        incident_id = Some(iid);
        // timeline + outbox notify request in the SAME transaction (contract line 48)
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_requested',$3)")
            .bind(tid).bind(iid).bind(json!({"etype":n.etype,"source":n.source})).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_notify',$2)")
            .bind(tid).bind(json!({
                "incident_id": iid, "summary": format!("{} [{}] @ {} (src:{})", n.etype, n.correlation, n.site_name.clone().unwrap_or("-".into()), n.source),
                "dedup": n.correlation, "integration_key": ""}))
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

    let etype = match (evt_type, alarm) {
        (_, Some("sos")) => "sos",
        (_, Some("fallDown")) => "man_down",
        (_, Some(_)) => "device_alarm",
        ("deviceOffline", _) => "device_offline",
        (t, _) if !t.is_empty() => "traccar_event",
        _ => "traccar_event",
    }.to_string();

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
    }).await
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
    let critical = alarm_flag || temp.map(|t| t >= 90.0).unwrap_or(false);
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
    }).await
}

// ----------------------------- incidents -----------------------------
pub async fn list_incidents(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT i.id,i.etype,i.state,i.priority,i.correlation,i.opened_at,i.outcome,s.name AS site
         FROM app.incidents i LEFT JOIN app.sites s ON s.id=i.site_id
         ORDER BY i.state<>'chiuso' DESC, i.priority DESC, i.opened_at DESC")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "etype": r.get::<String,_>("etype"),
        "state": r.get::<String,_>("state"), "priority": r.get::<i32,_>("priority"),
        "correlation": r.get::<String,_>("correlation"),
        "site": r.try_get::<String,_>("site").ok(),
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
    let n = sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$2,closed_at=now(),version=version+1 WHERE id=$1 AND state<>'chiuso'")
        .bind(iid).bind(&b.outcome).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"already closed")); }
    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
        .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":b.outcome,"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
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
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ev = sqlx::query(
        "INSERT INTO app.events(tenant_id,source,source_event,idem_key,etype,origin_ts,received_ts,lat,lon,correlation)
         VALUES($1,'pwa',$2,$2,'sos',$3,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(format!("sos-{}-{}", s.user_id, now.timestamp_millis()))
        .bind(now).bind(lat).bind(lon).bind(&correlation).fetch_one(&mut *tx).await.map_err(ise)?;
    let eid: Uuid = ev.get("id");
    let irow = sqlx::query(
        "INSERT INTO app.incidents(tenant_id,event_id,etype,correlation,priority) VALUES($1,$2,'sos',$3,3) RETURNING id")
        .bind(tid).bind(eid).bind(&correlation).fetch_one(&mut *tx).await.map_err(ise)?;
    let iid: Uuid = irow.get("id");
    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_requested',$3)")
        .bind(tid).bind(iid).bind(json!({"etype":"sos","by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_notify',$2)")
        .bind(tid).bind(json!({"incident_id":iid,"summary":format!("SOS [{}] {}",correlation,s.email),"dedup":correlation,"integration_key":""}))
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
    let due = st.clock.now() + chrono::Duration::minutes(b.minutes.max(1));
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

pub async fn shift_end(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let row = sqlx::query("UPDATE app.shifts SET state='terminato', ended_at=now() WHERE user_id=$1 AND state='in_corso' RETURNING id, started_at, ended_at")
        .bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    match row {
        Some(r) => Ok(Json(json!({"id":r.get::<Uuid,_>("id"),"state":"terminato","started_at":r.get::<Dt,_>("started_at"),"ended_at":r.get::<Dt,_>("ended_at")}))),
        None => Err(err(StatusCode::CONFLICT, "nessun turno aperto")),
    }
}

pub async fn shift_current(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let row = sqlx::query("SELECT id, started_at FROM app.shifts WHERE user_id=$1 AND state='in_corso'")
        .bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(match row {
        Some(r) => json!({"active":true,"id":r.get::<Uuid,_>("id"),"started_at":r.get::<Dt,_>("started_at")}),
        None => json!({"active":false}),
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

async fn module_active(st: &AppState, tid: Uuid, code: &str) -> bool {
    sqlx::query_scalar::<_, String>("SELECT state FROM app.tenant_modules WHERE tenant_id=$1 AND module_code=$2")
        .bind(tid).bind(code).fetch_optional(&st.owner_pool).await.ok().flatten()
        .map(|s| s == "attivo").unwrap_or(false)
}

#[derive(Deserialize)]
pub struct NewOrder { pub title: String, pub site_id: Option<Uuid>, pub instructions: Option<String>, pub assigned_user: Option<Uuid> }
pub async fn create_order(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewOrder>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !is_planner(&s.role) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    let state = if b.assigned_user.is_some() { "assegnato" } else { "bozza" };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.work_orders(tenant_id,site_id,title,instructions,assigned_user,state,created_by)
         VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.site_id).bind(&b.title).bind(&b.instructions).bind(b.assigned_user).bind(state).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id"),"state":state})))
}

pub async fn list_orders(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    // technician sees only own; planner/admin/auditor see all (RLS already scopes to tenant)
    let rows = if s.role == "lavoratore" {
        sqlx::query("SELECT o.id,o.title,o.state,o.version,o.outcome,o.instructions,s.name AS site,u.email AS assignee
            FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.users u ON u.id=o.assigned_user
            WHERE o.assigned_user=$1 ORDER BY o.updated_at DESC").bind(s.user_id).fetch_all(&mut *tx).await
    } else {
        sqlx::query("SELECT o.id,o.title,o.state,o.version,o.outcome,o.instructions,s.name AS site,u.email AS assignee
            FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.users u ON u.id=o.assigned_user
            ORDER BY o.updated_at DESC").fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "title": r.get::<String,_>("title"), "state": r.get::<String,_>("state"),
        "version": r.get::<i32,_>("version"), "site": r.try_get::<String,_>("site").ok(),
        "assignee": r.try_get::<String,_>("assignee").ok(), "outcome": r.try_get::<String,_>("outcome").ok(),
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
    let mut minutes = 0i64; let mut labor = 0i64; let mut missing_rate = false;
    let jl: Vec<Value> = lines.iter().map(|r| {
        let m = r.get::<i32,_>("minutes") as i64; minutes += m;
        let rate: Option<i64> = r.try_get("rate_cents").ok();
        match rate { Some(rc) => labor += m * rc / 60, None => missing_rate = true }
        json!({"id":r.get::<Uuid,_>("id"),"activity":r.get::<String,_>("activity"),
            "work_date":r.get::<chrono::NaiveDate,_>("work_date").to_string(),
            "minutes":m,"rate_cents":rate,"currency":r.get::<String,_>("currency")})
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
        "lines": jl, "expenses": je,
        "total": {"minutes":minutes,"labor_cents":labor,"expense_cents":exp_total,"total_cents":labor+exp_total,"missing_rate":missing_rate}
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
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_lines(tenant_id,timesheet_id,activity,work_date,minutes,rate_cents,currency) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(tid).bind(id).bind(&b.activity).bind(wd).bind(b.minutes).bind(b.rate_cents)
        .bind(b.currency.clone().unwrap_or("EUR".into())).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct NewExpense { pub description: String, pub amount_cents: i64, pub currency: Option<String>, pub has_receipt: Option<bool> }
pub async fn add_expense(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<NewExpense>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.expenses(tenant_id,timesheet_id,description,amount_cents,currency,has_receipt) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(tid).bind(id).bind(&b.description).bind(b.amount_cents)
        .bind(b.currency.clone().unwrap_or("EUR".into())).bind(b.has_receipt.unwrap_or(false)).execute(&mut *tx).await.map_err(ise)?;
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
    let mut csv = String::from("tipo,descrizione,data,minuti,tariffa_cent_ora,importo_cent,valuta,giustificativo\n");
    for l in d["lines"].as_array().unwrap_or(&vec![]) {
        let m = l["minutes"].as_i64().unwrap_or(0);
        let rate = l["rate_cents"].as_i64();
        let amt = rate.map(|r| m*r/60);
        csv.push_str(&format!("riga,{},{},{},{},{},{},\n",
            csv_esc(l["activity"].as_str().unwrap_or("")), l["work_date"].as_str().unwrap_or(""), m,
            rate.map(|x|x.to_string()).unwrap_or_default(), amt.map(|x|x.to_string()).unwrap_or_default(),
            l["currency"].as_str().unwrap_or("EUR")));
    }
    for e in d["expenses"].as_array().unwrap_or(&vec![]) {
        csv.push_str(&format!("spesa,{},,,,{},{},{}\n",
            csv_esc(e["description"].as_str().unwrap_or("")), e["amount_cents"].as_i64().unwrap_or(0),
            e["currency"].as_str().unwrap_or("EUR"), e["has_receipt"].as_bool().unwrap_or(false)));
    }
    let t = &d["total"];
    csv.push_str(&format!("TOTALE,,,{},,{},EUR,\n", t["minutes"].as_i64().unwrap_or(0), t["total_cents"].as_i64().unwrap_or(0)));
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
