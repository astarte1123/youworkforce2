use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, sse::{Sse, Event as SseEvent}}, Json};
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

const CRITICAL: &[&str] = &["sos", "man_down", "immobilita", "mancato_checkin"];

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
    state.sessions.lock().await.get(&tok).cloned()
}

fn set_cookie_header(tok: &str) -> HeaderMap {
    let mut m = HeaderMap::new();
    m.insert(header::SET_COOKIE,
        format!("yw_session={tok}; Path=/; HttpOnly; SameSite=Lax").parse().unwrap());
    m
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

pub async fn superadmin_login(State(st): State<AppState>, Json(b): Json<LoginReq>) -> impl IntoResponse {
    let row = sqlx::query("SELECT id, pass_hash FROM app.superadmins WHERE email=$1")
        .bind(&b.email).fetch_optional(&st.owner_pool).await;
    match row {
        Ok(Some(r)) => {
            let id: Uuid = r.get("id");
            let hash: String = r.get("pass_hash");
            if auth::verify_password(&b.password, &hash) {
                let tok = auth::new_token();
                st.sessions.lock().await.insert(tok.clone(), Session {
                    user_id: id, tenant_id: None, email: b.email.clone(),
                    role: "superadmin".into(), is_super: true });
                (StatusCode::OK, set_cookie_header(&tok), Json(json!({"ok":true,"role":"superadmin"}))).into_response()
            } else { (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response() }
        }
        _ => (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response(),
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
    // user lookup under tenant context
    let mut tx = match db::begin_tenant(&st.rt_pool, tid).await { Ok(t)=>t, Err(_)=>return ise_resp() };
    let urow = sqlx::query("SELECT id,pass_hash,role FROM app.users WHERE email=$1")
        .bind(&b.email).fetch_optional(&mut *tx).await.ok().flatten();
    let _ = tx.commit().await;
    match urow {
        Some(r) if auth::verify_password(&b.password, &r.get::<String,_>("pass_hash")) => {
            let tok = auth::new_token();
            let uid: Uuid = r.get("id"); let role: String = r.get("role");
            st.sessions.lock().await.insert(tok.clone(), Session {
                user_id: uid, tenant_id: Some(tid), email: b.email.clone(), role: role.clone(), is_super: false });
            (StatusCode::OK, set_cookie_header(&tok), Json(json!({"ok":true,"role":role,"tenant":slug}))).into_response()
        }
        _ => (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response(),
    }
}

pub async fn me(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = session_of(&st, &h).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no session"))?;
    Ok(Json(json!({"email":s.email,"role":s.role,"is_super":s.is_super,"tenant_id":s.tenant_id})))
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

    let idem = b.idem_key.clone().unwrap_or_else(|| b.source_event.clone());
    if idem.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "idem key required")); }
    let correlation = b.correlation.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    let now = st.clock.now();

    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;

    // resolve device/site within tenant (optional)
    let device_id: Option<Uuid> = if let Some(ext) = &b.device_external_id {
        sqlx::query_scalar("SELECT id FROM app.devices WHERE source=$1 AND external_id=$2")
            .bind(&source).bind(ext).fetch_optional(&mut *tx).await.map_err(ise)?
    } else { None };
    let site_id: Option<Uuid> = if let Some(sn) = &b.site_name {
        sqlx::query_scalar("SELECT id FROM app.sites WHERE name=$1")
            .bind(sn).fetch_optional(&mut *tx).await.map_err(ise)?
    } else { None };

    // idempotent insert
    let ev = sqlx::query(
        "INSERT INTO app.events(tenant_id,source,source_event,idem_key,site_id,device_id,etype,origin_ts,received_ts,lat,lon,accuracy_m,payload_ref,correlation)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT (tenant_id,source,idem_key) DO NOTHING
         RETURNING id")
        .bind(tid).bind(&source).bind(&b.source_event).bind(&idem).bind(site_id).bind(device_id)
        .bind(&b.etype).bind(now).bind(now).bind(b.lat).bind(b.lon).bind(b.accuracy_m)
        .bind(b.payload.clone().unwrap_or(json!({}))).bind(&correlation)
        .fetch_optional(&mut *tx).await.map_err(ise)?;

    let Some(ev) = ev else {
        tx.commit().await.map_err(ise)?;
        return Ok(Json(json!({"deduplicated":true,"correlation":correlation})));
    };
    let eid: Uuid = ev.get("id");

    let mut incident_id: Option<Uuid> = None;
    if CRITICAL.contains(&b.etype.as_str()) {
        let irow = sqlx::query(
            "INSERT INTO app.incidents(tenant_id,event_id,site_id,etype,correlation,priority)
             VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
            .bind(tid).bind(eid).bind(site_id).bind(&b.etype).bind(&correlation)
            .bind(if b.etype=="sos" {3} else {2})
            .fetch_one(&mut *tx).await.map_err(ise)?;
        let iid: Uuid = irow.get("id");
        incident_id = Some(iid);
        // timeline + outbox notify request in the SAME transaction (contract line 48)
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_requested',$3)")
            .bind(tid).bind(iid).bind(json!({"etype":b.etype})).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_notify',$2)")
            .bind(tid).bind(json!({
                "incident_id": iid, "summary": format!("{} @ {:?}", b.etype, b.site_name),
                "dedup": correlation, "integration_key": ""}))
            .execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;

    if let Some(iid) = incident_id {
        let _ = st.bus.send(TenantEvent { tenant_id: tid, payload: json!({
            "type":"incident_opened","incident_id":iid,"etype":b.etype,"correlation":correlation}) });
    }
    Ok(Json(json!({"event_id":eid,"incident_id":incident_id,"correlation":correlation})))
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
    let _ = st.bus.send(TenantEvent{tenant_id:tid,payload:json!({"type":"incident_ack","incident_id":iid})});
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
    let _ = st.bus.send(TenantEvent{tenant_id:tid,payload:json!({"type":"incident_closed","incident_id":iid})});
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
        .bind(tid).bind(json!({"incident_id":iid,"summary":format!("SOS {}",s.email),"dedup":correlation,"integration_key":""}))
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let _ = st.bus.send(TenantEvent{tenant_id:tid,payload:json!({"type":"incident_opened","incident_id":iid,"etype":"sos","correlation":correlation})});
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
