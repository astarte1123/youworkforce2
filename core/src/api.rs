use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, Response, sse::{Sse, Event as SseEvent}}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::convert::Infallible;
use uuid::Uuid;
use futures::stream::Stream;
use crate::state::{AppState, Session, TenantEvent};
use crate::{auth, db};

pub(crate) type R = Result<Json<Value>, (StatusCode, String)>;
pub(crate) fn err(c: StatusCode, m: &str) -> (StatusCode, String) { (c, m.to_string()) }

const CRITICAL: &[&str] = &["sos", "man_down", "immobilita", "mancato_checkin", "sensore_critico",
    "manomissione_badge", "perdita_alimentazione", "jamming", "perdita_contatto", "traccar_conn_down",
    "batteria_critica", "heartbeat_down", "email_alarm", "geofence_ingresso", "geofence_uscita"];

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
        || path == "/api/invite/accept"
        || path == "/api/events" || path.starts_with("/api/ingress/")
        || path == "/api/_test/advance" || path == "/api/_test/iot-watchdog" || path == "/health";
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
        // wall-clock DI PROPOSITO: blocked_until e' scritto con `now()` SQL (wall); il TEST_CLOCK
        // non deve poter aggirare il throttle di login. NON usare st.clock.now() qui.
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

pub(crate) async fn require_tenant(state: &AppState, h: &HeaderMap) -> Result<Session, (StatusCode, String)> {
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
pub struct CreateTenant { pub slug: String, pub name: String, pub admin_email: String,
    #[serde(default)] pub admin_password: Option<String>,
    #[serde(default)] pub plan: Option<String>,
    #[serde(default)] pub modules: Option<Vec<String>>,
    #[serde(default)] pub scenario: Option<String>,
    #[serde(default)] pub with_demo: Option<bool>,
    #[serde(default)] pub mobile_app_licenses: Option<i32>,
    #[serde(default)] pub iot_device_licenses: Option<i32>,
    pub brand_primary: Option<String>, pub brand_ink: Option<String>, pub brand_soft: Option<String>, pub brand_accent: Option<String> }
/// Pacchetti/piano: preset di moduli da attivare in un colpo alla creazione del tenant.
/// Catalogo definito nel codice (come gli scenari), esposto via API; l'UI lo consuma.
pub struct Plan { pub code: &'static str, pub name: &'static str, pub desc: &'static str, pub modules: &'static [&'static str] }
pub fn plan_catalog() -> Vec<Plan> { vec![
    Plan{ code:"sicurezza", name:"Sicurezza lavoratore", desc:"SOS / uomo a terra / reperibilità (Lone Worker).", modules:&["loneworker"] },
    Plan{ code:"campo", name:"Lavoro sul campo", desc:"Interventi, cantieri, ronde, consuntivi, turni.", modules:&["interventi","cantieri","giri","consuntivi","workforce"] },
    Plan{ code:"iot", name:"IoT", desc:"Sensori, telemetria, soglie→incidenti, cruscotti.", modules:&["iot"] },
    Plan{ code:"full", name:"Completo", desc:"Tutti i moduli + assistente AI.", modules:&["loneworker","interventi","cantieri","giri","consuntivi","workforce","iot","assistant"] },
]}
pub async fn super_plans_list(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    Ok(Json(json!(plan_catalog().iter().map(|p| json!({
        "code":p.code,"name":p.name,"desc":p.desc,"modules":p.modules })).collect::<Vec<_>>())))
}
/// Normalizza+valida uno slug tenant (finisce nel sottodominio): minuscole, a-z0-9-, 2..40, niente trattini ai bordi.
fn clean_slug(raw: &str) -> Result<String, (StatusCode,String)> {
    let s: String = raw.trim().to_lowercase().chars()
        .map(|c| if c.is_ascii_alphanumeric() || c=='-' { c } else if c==' '||c=='_' { '-' } else { '\0' })
        .filter(|c| *c!='\0').collect();
    let s = s.trim_matches('-').to_string();
    let ok = (2..=40).contains(&s.len()) && s.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false);
    if !ok { return Err(err(StatusCode::BAD_REQUEST, "slug non valido: usa 2-40 caratteri a-z, 0-9, trattino")); }
    Ok(s)
}
/// Validazione email minimale (formato), non deliverability.
fn valid_email(e: &str) -> bool {
    let e=e.trim(); let at=e.find('@');
    matches!(at, Some(i) if i>0 && i<e.len()-1) && e.rsplit('@').next().map(|d| d.contains('.')).unwrap_or(false) && !e.contains(' ')
}

pub async fn create_tenant(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CreateTenant>) -> R {
    require_super(&st, &h).await?;
    let slug = clean_slug(&b.slug)?;
    const RESERVED: &[&str] = &["tenants","www","admin","api","app","invite","static","health"];
    if RESERVED.contains(&slug.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "slug riservato")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    if !valid_email(&b.admin_email) { return Err(err(StatusCode::BAD_REQUEST, "email admin non valida")); }
    let (bp, bi, bs, ba) = (clean_color(&b.brand_primary)?, clean_color(&b.brand_ink)?, clean_color(&b.brand_soft)?, clean_color(&b.brand_accent)?);
    // Modalità onboarding: password esplicita (demo/dev) oppure INVITO monouso (produzione, default).
    let direct_pw = b.admin_password.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    let trow = sqlx::query("INSERT INTO app.tenants(slug,name,brand_primary,brand_ink,brand_soft,brand_accent) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(&slug).bind(b.name.trim()).bind(bp).bind(bi).bind(bs).bind(ba).fetch_one(&mut *tx).await
        .map_err(|_| err(StatusCode::CONFLICT, "slug già esistente"))?;
    let tid: Uuid = trow.get("id");
    // L'admin è sempre creato; la password è quella fornita (demo) o una random inutilizzabile finché
    // l'admin non accetta l'invito e imposta la propria (produzione: nessuna password nota al superadmin).
    let pass_hash = auth::hash_password(direct_pw.unwrap_or(&auth::new_token()));
    let uid: Uuid = sqlx::query_scalar("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,$2,$3,'tenant_admin') RETURNING id")
        .bind(tid).bind(b.admin_email.trim()).bind(&pass_hash)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    // licenze iniziali (null = illimitato)
    let lic = |v: Option<i32>| v.filter(|n| *n >= 0);
    sqlx::query("UPDATE app.tenants SET mobile_app_licenses=$2, iot_device_licenses=$3 WHERE id=$1")
        .bind(tid).bind(lic(b.mobile_app_licenses)).bind(lic(b.iot_device_licenses)).execute(&mut *tx).await.map_err(ise)?;
    // seed module rows (all non_abilitato; core identity always on)
    sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state)
        SELECT $1, code, 'non_abilitato' FROM app.modules_catalog")
        .bind(tid).execute(&mut *tx).await.map_err(ise)?;
    // Pacchetto/piano: espande in moduli; unione con eventuali moduli espliciti. Attiva solo i
    // moduli disponibili nel prodotto (available=true). Nessun errore se un codice non esiste.
    let mut mods: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(pc) = b.plan.as_deref() { if let Some(p) = plan_catalog().into_iter().find(|p| p.code==pc) {
        for m in p.modules { mods.insert(m.to_string()); } } }
    if let Some(ms) = &b.modules { for m in ms { mods.insert(m.clone()); } }
    if !mods.is_empty() {
        let codes: Vec<String> = mods.into_iter().collect();
        sqlx::query("UPDATE app.tenant_modules SET state='attivo'
            WHERE tenant_id=$1 AND module_code = ANY($2)
              AND module_code IN (SELECT code FROM app.modules_catalog WHERE available=true)")
            .bind(tid).bind(&codes).execute(&mut *tx).await.map_err(ise)?;
    }
    // Invito monouso (se non è stata data una password diretta): token in chiaro una volta sola.
    let mut invite_token: Option<String> = None;
    if direct_pw.is_none() {
        let tok = auth::new_token();
        let exp = st.clock.now() + chrono::Duration::days(7);
        sqlx::query("INSERT INTO app.user_invites(token_hash,tenant_id,user_id,email,expires_at) VALUES($1,$2,$3,$4,$5)")
            .bind(hash_token(&tok)).bind(tid).bind(uid).bind(b.admin_email.trim()).bind(exp)
            .execute(&mut *tx).await.map_err(ise)?;
        invite_token = Some(tok);
    }
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,'superadmin','create_tenant',$1)")
        .bind(json!({"slug":slug,"onboarding": if invite_token.is_some() {"invite"} else {"password"},
            "plan":b.plan,"scenario":b.scenario})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // Scenario opzionale (attiva i suoi moduli + eventuale demo): riusa il motore scenari, post-commit.
    let mut scenario_applied: Option<String> = None;
    if let Some(sc) = b.scenario.as_deref().filter(|s| !s.is_empty()) {
        match scenario_apply_inner(&st, tid, sc, b.with_demo.unwrap_or(false)).await {
            Ok((code,_,_,_)) => scenario_applied = Some(code),
            Err(e) => tracing::warn!("create_tenant scenario {sc}: {}", e.1),
        }
    }
    Ok(Json(json!({"id":tid,"slug":slug,"invite_token":invite_token,"scenario_applied":scenario_applied})))
}
#[derive(Deserialize)]
pub struct InviteAccept { pub slug: String, pub token: String, pub password: String }
/// Accetta un invito: l'admin imposta la PROPRIA password. Pubblico (no sessione), CSRF-exempt, monouso.
pub async fn accept_invite(State(st): State<AppState>, Json(b): Json<InviteAccept>) -> R {
    if b.password.trim().len() < 8 { return Err(err(StatusCode::BAD_REQUEST, "la password deve avere almeno 8 caratteri")); }
    let tid: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.tenants WHERE slug=$1").bind(b.slug.trim().to_lowercase())
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let tid = tid.ok_or_else(|| err(StatusCode::NOT_FOUND, "tenant inesistente"))?;
    let row = sqlx::query("SELECT user_id,expires_at,used_at FROM app.user_invites WHERE token_hash=$1 AND tenant_id=$2")
        .bind(hash_token(b.token.trim())).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let row = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "invito non valido"))?;
    if row.try_get::<chrono::DateTime<chrono::Utc>,_>("used_at").is_ok() { return Err(err(StatusCode::CONFLICT, "invito già usato")); }
    let exp: chrono::DateTime<chrono::Utc> = row.get("expires_at");
    if exp < st.clock.now() { return Err(err(StatusCode::GONE, "invito scaduto")); }
    let uid: Uuid = row.get("user_id");
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    sqlx::query("UPDATE app.users SET pass_hash=$1 WHERE id=$2").bind(auth::hash_password(b.password.trim())).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.user_invites SET used_at=now() WHERE token_hash=$1").bind(hash_token(b.token.trim())).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"slug":b.slug.trim().to_lowercase()})))
}
/// Verifica un invito (per la pagina di accettazione): ritorna l'email destinataria se valido.
pub async fn check_invite(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let (slug, token) = (q.get("slug").cloned().unwrap_or_default(), q.get("token").cloned().unwrap_or_default());
    let tid: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.tenants WHERE slug=$1").bind(slug.trim().to_lowercase())
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let Some(tid) = tid else { return Ok(Json(json!({"valid":false}))) };
    let row = sqlx::query("SELECT email,expires_at,used_at FROM app.user_invites WHERE token_hash=$1 AND tenant_id=$2")
        .bind(hash_token(token.trim())).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match row {
        Some(r) if r.try_get::<chrono::DateTime<chrono::Utc>,_>("used_at").is_err()
                && r.get::<chrono::DateTime<chrono::Utc>,_>("expires_at") >= st.clock.now()
            => Ok(Json(json!({"valid":true,"email":r.get::<String,_>("email")}))),
        _ => Ok(Json(json!({"valid":false}))),
    }
}

pub async fn list_tenants(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    // seat app mobile = utenti che usano l'app (lavoratore/reperibile); IoT = dispositivi registrati.
    let rows = sqlx::query(
        "SELECT t.id,t.slug,t.name,t.created_at,t.status,
            t.mobile_app_licenses, t.iot_device_licenses,
            t.brand_primary,t.brand_ink,t.brand_soft,t.brand_accent,(t.logo IS NOT NULL) AS has_logo,
            (SELECT count(*) FROM app.users u WHERE u.tenant_id=t.id AND u.role IN ('lavoratore','reperibile')) AS mobile_used,
            (SELECT count(*) FROM app.devices d WHERE d.tenant_id=t.id) AS iot_used,
            (SELECT count(*) FROM app.tenant_modules m WHERE m.tenant_id=t.id AND m.state='attivo') AS mods_active,
            (SELECT count(*) FROM app.users u WHERE u.tenant_id=t.id) AS users_tot,
            (SELECT count(*) FROM app.sites s WHERE s.tenant_id=t.id) AS sites_tot,
            (SELECT count(*) FROM app.incidents i WHERE i.tenant_id=t.id AND i.state<>'chiuso') AS incidents_open,
            EXISTS(SELECT 1 FROM app.user_invites i WHERE i.tenant_id=t.id AND i.used_at IS NULL AND i.expires_at>now()) AS invite_pending
         FROM app.tenants t ORDER BY t.created_at")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "slug": r.get::<String,_>("slug"), "name": r.get::<String,_>("name"),
        "status": r.try_get::<String,_>("status").unwrap_or_else(|_| "attivo".into()),
        "mobile_app_licenses": r.try_get::<Option<i32>,_>("mobile_app_licenses").ok().flatten(),
        "iot_device_licenses": r.try_get::<Option<i32>,_>("iot_device_licenses").ok().flatten(),
        "mobile_used": r.get::<i64,_>("mobile_used"),
        "iot_used": r.get::<i64,_>("iot_used"),
        "mods_active": r.get::<i64,_>("mods_active"),
        "users_tot": r.get::<i64,_>("users_tot"),
        "sites_tot": r.get::<i64,_>("sites_tot"),
        "incidents_open": r.get::<i64,_>("incidents_open"),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at").to_rfc3339(),
        "invite_pending": r.get::<bool,_>("invite_pending"),
        "brand_primary": r.try_get::<Option<String>,_>("brand_primary").ok().flatten(),
        "brand_ink": r.try_get::<Option<String>,_>("brand_ink").ok().flatten(),
        "brand_soft": r.try_get::<Option<String>,_>("brand_soft").ok().flatten(),
        "brand_accent": r.try_get::<Option<String>,_>("brand_accent").ok().flatten(),
        "has_logo": r.get::<bool,_>("has_logo"),
    })).collect();
    Ok(Json(json!(out)))
}
// ===================== Monitoring sistema (superadmin) =====================
fn sys_env_i64(k: &str, d: i64) -> i64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
/// Soglie infrastrutturali (override via env YW_SYS_*): disco/mem/cpu % e staleness worker (s).
pub fn sys_thresholds() -> (f64,f64,f64,i64) {
    (sys_env_i64("YW_SYS_DISK_MAX_PCT",90) as f64, sys_env_i64("YW_SYS_MEM_MAX_PCT",90) as f64,
     sys_env_i64("YW_SYS_CPU_MAX_PCT",95) as f64, sys_env_i64("YW_SYS_WORKER_STALE_SECS",120))
}
/// Soglie + destinatari EFFETTIVI: valore dal pannello (DB) se impostato, altrimenti default env/prodotto.
pub struct SysEff { pub disk: f64, pub mem: f64, pub cpu: f64, pub worker_stale: i64, pub email: Option<String>, pub phone: Option<String> }
pub async fn sys_settings(st: &AppState) -> SysEff {
    let (d,m,c,w) = sys_thresholds();
    let row = sqlx::query("SELECT disk_max_pct,mem_max_pct,cpu_max_pct,worker_stale_secs,alert_email,alert_phone FROM app.system_settings WHERE id=true")
        .fetch_optional(&st.owner_pool).await.ok().flatten();
    let nz = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    match row {
        Some(r) => SysEff{
            disk: r.try_get::<i32,_>("disk_max_pct").ok().map(|x| x as f64).unwrap_or(d),
            mem:  r.try_get::<i32,_>("mem_max_pct").ok().map(|x| x as f64).unwrap_or(m),
            cpu:  r.try_get::<i32,_>("cpu_max_pct").ok().map(|x| x as f64).unwrap_or(c),
            worker_stale: r.try_get::<i32,_>("worker_stale_secs").ok().map(|x| x as i64).unwrap_or(w),
            email: nz(r.try_get::<String,_>("alert_email").ok()), phone: nz(r.try_get::<String,_>("alert_phone").ok()),
        },
        None => SysEff{ disk:d, mem:m, cpu:c, worker_stale:w, email:None, phone:None },
    }
}
fn lvl_pct(pct: f64, max: f64) -> &'static str { if pct>=max {"bad"} else if pct>=max-10.0 {"warn"} else {"ok"} }
/// Snapshot completo dello stato di sistema: hardware (CPU/RAM/disco/host via sysinfo), DB,
/// runtime app, code e worker. Include una lista `checks` (kind/severity/detail) riusata dal watchdog.
pub async fn system_snapshot(st: &AppState) -> Value {
    // --- hardware (bloccante: eseguito su thread dedicato) ---
    let hw: Value = tokio::task::spawn_blocking(|| {
        use sysinfo::{System, Disks};
        let mut sys = System::new();
        sys.refresh_cpu_all();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let (mut dt, mut dfree) = (0u64, 0u64);
        for d in Disks::new_with_refreshed_list().list() { if d.total_space() > dt { dt = d.total_space(); dfree = d.available_space(); } }
        let (mt, mu) = (sys.total_memory(), sys.used_memory());
        let r1 = |x: f64| (x*10.0).round()/10.0;
        json!({
            "host": System::host_name(), "os": System::name(), "os_version": System::os_version(),
            "kernel": System::kernel_version(), "arch": System::cpu_arch(), "cpus": sys.cpus().len(),
            "cpu_pct": r1(sys.global_cpu_usage() as f64),
            "mem_total": mt, "mem_used": mu, "mem_pct": if mt>0 { r1(mu as f64/mt as f64*100.0) } else {0.0},
            "disk_total": dt, "disk_used": dt.saturating_sub(dfree),
            "disk_pct": if dt>0 { r1((dt-dfree) as f64/dt as f64*100.0) } else {0.0},
        })
    }).await.unwrap_or_else(|_| json!({}));
    // --- DB ---
    let db_size: Option<i64> = sqlx::query_scalar("SELECT pg_database_size(current_database())").fetch_one(&st.owner_pool).await.ok();
    let db_ok = db_size.is_some();
    let db_conns: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database()").fetch_one(&st.owner_pool).await.unwrap_or(0);
    let db_max: i64 = sqlx::query_scalar("SELECT setting::bigint FROM pg_settings WHERE name='max_connections'").fetch_one(&st.owner_pool).await.unwrap_or(0);
    // --- code / runtime ---
    let c = |sql: &'static str| async move { sqlx::query_scalar::<_,i64>(sql).fetch_one(&st.owner_pool).await.unwrap_or(0) };
    let tenants = c("SELECT count(*) FROM app.tenants").await;
    let users = c("SELECT count(*) FROM app.users").await;
    let devices = c("SELECT count(*) FROM app.devices").await;
    let incidents_open = c("SELECT count(*) FROM app.incidents WHERE state<>'chiuso'").await;
    let outbox_pending = c("SELECT count(*) FROM app.outbox WHERE state='pending'").await;
    let outbox_error = c("SELECT count(*) FROM app.outbox WHERE state='error'").await;
    let sessions = c("SELECT count(*) FROM app.sessions").await;
    // --- worker heartbeat ---
    let hb = st.worker_heartbeat.load(std::sync::atomic::Ordering::Relaxed);
    let hb_age = if hb>0 { (chrono::Utc::now().timestamp() - hb).max(0) } else { -1 };
    let eff = sys_settings(st).await;
    let (disk_max, mem_max, cpu_max, worker_stale) = (eff.disk, eff.mem, eff.cpu, eff.worker_stale);
    let disk_pct = hw.get("disk_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let mem_pct = hw.get("mem_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let cpu_pct = hw.get("cpu_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
    // --- checks (kind, severity, detail) ---
    let mut checks: Vec<Value> = Vec::new();
    let mut add = |k:&str,sev:&str,detail:String| checks.push(json!({"kind":k,"severity":sev,"detail":detail}));
    add("db", if db_ok {"ok"} else {"bad"}, if db_ok {"database raggiungibile".into()} else {"database IRRAGGIUNGIBILE".into()});
    if db_max>0 { let cp=db_conns as f64/db_max as f64*100.0; add("db_conns", lvl_pct(cp,90.0), format!("connessioni DB {}/{}", db_conns, db_max)); }
    add("disk", lvl_pct(disk_pct,disk_max), format!("disco al {:.0}%", disk_pct));
    add("mem", lvl_pct(mem_pct,mem_max), format!("memoria al {:.0}%", mem_pct));
    add("cpu", lvl_pct(cpu_pct,cpu_max), format!("CPU al {:.0}%", cpu_pct));
    let wsev = if hb_age<0 || hb_age>worker_stale {"bad"} else if hb_age>worker_stale/2 {"warn"} else {"ok"};
    add("worker", wsev, if hb_age<0 {"worker mai visto".into()} else {format!("ultimo giro worker {}s fa", hb_age)});
    if outbox_error>0 { add("outbox", if outbox_error>20 {"bad"} else {"warn"}, format!("{} messaggi outbox in errore", outbox_error)); }
    let overall = if checks.iter().any(|c| c["severity"]=="bad") {"bad"} else if checks.iter().any(|c| c["severity"]=="warn") {"warn"} else {"ok"};
    json!({
        "status": overall,
        "app": {"version": env!("CARGO_PKG_VERSION"), "uptime_secs": st.started.elapsed().as_secs(), "test_clock": st.clock.test_mode},
        "server": hw,
        "db": {"reachable": db_ok, "size_bytes": db_size, "connections": db_conns, "max_connections": db_max},
        "queues": {"outbox_pending": outbox_pending, "outbox_error": outbox_error, "sessions": sessions,
                   "incidents_open": incidents_open, "tenants": tenants, "users": users, "devices": devices},
        "worker": {"heartbeat_age_secs": hb_age},
        "thresholds": {"disk_max_pct": disk_max, "mem_max_pct": mem_max, "cpu_max_pct": cpu_max, "worker_stale_secs": worker_stale},
        "checks": checks,
    })
}
pub async fn super_system(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    Ok(Json(system_snapshot(&st).await))
}
pub async fn super_system_alerts(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query("SELECT id,kind,severity,detail,created_at,notified,resolved_at FROM app.system_alerts ORDER BY created_at DESC LIMIT 100")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "kind": r.get::<String,_>("kind"), "severity": r.get::<String,_>("severity"),
        "detail": r.get::<String,_>("detail"), "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at").to_rfc3339(),
        "notified": r.get::<bool,_>("notified"),
        "resolved_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("resolved_at").ok().map(|d| d.to_rfc3339()),
    })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct SysSettingsReq {
    #[serde(default)] pub disk_max_pct: Option<i32>, #[serde(default)] pub mem_max_pct: Option<i32>,
    #[serde(default)] pub cpu_max_pct: Option<i32>, #[serde(default)] pub worker_stale_secs: Option<i32>,
    #[serde(default)] pub alert_email: Option<String>, #[serde(default)] pub alert_phone: Option<String> }
/// Legge la configurazione monitoring (valori salvati dal pannello + effettivi + default env).
pub async fn super_system_settings_get(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let r = sqlx::query("SELECT disk_max_pct,mem_max_pct,cpu_max_pct,worker_stale_secs,alert_email,alert_phone FROM app.system_settings WHERE id=true")
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let (dd,dm,dc,dw) = sys_thresholds();
    let stored = match &r { Some(r) => json!({
        "disk_max_pct": r.try_get::<i32,_>("disk_max_pct").ok(), "mem_max_pct": r.try_get::<i32,_>("mem_max_pct").ok(),
        "cpu_max_pct": r.try_get::<i32,_>("cpu_max_pct").ok(), "worker_stale_secs": r.try_get::<i32,_>("worker_stale_secs").ok(),
        "alert_email": r.try_get::<String,_>("alert_email").ok(), "alert_phone": r.try_get::<String,_>("alert_phone").ok() }),
        None => json!({}) };
    Ok(Json(json!({"stored": stored, "defaults": {"disk_max_pct":dd as i64,"mem_max_pct":dm as i64,"cpu_max_pct":dc as i64,"worker_stale_secs":dw}})))
}
/// Salva la configurazione monitoring (soglie + destinatari). Vuoto/None = usa il default.
pub async fn super_system_settings_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SysSettingsReq>) -> R {
    let s = require_super(&st, &h).await?;
    let pct = |v: Option<i32>, name: &str| -> Result<Option<i32>,(StatusCode,String)> {
        match v { Some(x) if !(1..=100).contains(&x) => Err(err(StatusCode::BAD_REQUEST, &format!("{name}: usa 1-100"))), x => Ok(x) } };
    let disk=pct(b.disk_max_pct,"disco")?; let mem=pct(b.mem_max_pct,"memoria")?; let cpu=pct(b.cpu_max_pct,"cpu")?;
    let worker = match b.worker_stale_secs { Some(x) if !(10..=86400).contains(&x) => return Err(err(StatusCode::BAD_REQUEST,"worker: 10-86400 secondi")), x => x };
    let nz = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let (email, phone) = (nz(b.alert_email), nz(b.alert_phone));
    let (has_email, has_phone) = (email.is_some(), phone.is_some());
    sqlx::query("UPDATE app.system_settings SET disk_max_pct=$1,mem_max_pct=$2,cpu_max_pct=$3,worker_stale_secs=$4,alert_email=$5,alert_phone=$6,updated_at=now() WHERE id=true")
        .bind(disk).bind(mem).bind(cpu).bind(worker).bind(&email).bind(&phone)
        .execute(&st.owner_pool).await.map_err(ise)?;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'system_settings',$2)")
        .bind(&s.email).bind(json!({"disk":disk,"mem":mem,"cpu":cpu,"worker":worker,"email":has_email,"phone":has_phone})).execute(&st.owner_pool).await;
    Ok(Json(json!({"ok":true})))
}
/// Watchdog infrastrutturale: valuta i check di sistema, apre un alert per ogni problema NUOVO
/// (dedup: un alert aperto per kind entro 60 min), notifica il superadmin (sink/email/SMS) e
/// risolve gli alert quando il check torna ok. Chiamato dal worker.
pub async fn system_watchdog(st: &AppState) {
    let snap = system_snapshot(st).await;
    let checks = snap.get("checks").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for c in &checks {
        let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let sev = c.get("severity").and_then(|v| v.as_str()).unwrap_or("ok");
        let detail = c.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if kind.is_empty() { continue; }
        if sev == "ok" {
            let _ = sqlx::query("UPDATE app.system_alerts SET resolved_at=now() WHERE kind=$1 AND resolved_at IS NULL")
                .bind(kind).execute(&st.owner_pool).await;
            continue;
        }
        // dedup: già un alert aperto per questo kind nell'ultima ora?
        let recent: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM app.system_alerts WHERE kind=$1 AND resolved_at IS NULL AND created_at > now() - interval '60 minutes' LIMIT 1")
            .bind(kind).fetch_optional(&st.owner_pool).await.ok().flatten();
        if recent.is_some() { continue; }
        let id: Option<Uuid> = sqlx::query_scalar("INSERT INTO app.system_alerts(kind,severity,detail) VALUES($1,$2,$3) RETURNING id")
            .bind(kind).bind(sev).bind(&detail).fetch_one(&st.owner_pool).await.ok();
        let sent = system_alert_notify(st, sev, kind, &detail).await;
        if sent { if let Some(i)=id { let _=sqlx::query("UPDATE app.system_alerts SET notified=true WHERE id=$1").bind(i).execute(&st.owner_pool).await; } }
        let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,'system','system_alert',$1)")
            .bind(json!({"kind":kind,"severity":sev,"detail":detail,"notified":sent})).execute(&st.owner_pool).await;
        tracing::warn!("system alert [{sev}] {kind}: {detail} (notified={sent})");
    }
}
/// Notifica un alert di sistema al superadmin su tutti i canali configurati (sink webhook per e2e,
/// email relay, SMS Twilio). Ritorna true se almeno un canale ha consegnato.
async fn system_alert_notify(st: &AppState, sev: &str, kind: &str, detail: &str) -> bool {
    let subj = format!("[YW {}] {} — {}", sev.to_uppercase(), kind, detail);
    let eff = sys_settings(st).await;
    let mut emails: Vec<String> = sqlx::query_scalar("SELECT email FROM app.superadmins").fetch_all(&st.owner_pool).await.unwrap_or_default();
    // destinatari extra dal pannello (o env legacy), separati da virgola
    let extra = eff.email.clone().or_else(|| std::env::var("YW_SYS_ALERT_EMAIL").ok());
    if let Some(x) = extra { for e in x.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) { emails.push(e); } }
    let alert_phone = eff.phone.clone().or_else(|| std::env::var("YW_SYS_ALERT_PHONE").ok()).filter(|s| !s.trim().is_empty());
    let mut any = false;
    // 1) sink webhook (e2e + integrazione generica)
    if let Ok(sink) = std::env::var("YW_ESC_SINK_URL") { if !sink.trim().is_empty() {
        let body = json!({"kind":"system_alert","severity":sev,"check":kind,"text":subj});
        if crate::delivery::deliver_to_sink(&st.http, &sink, &body).await.state == "sent" { any = true; }
    }}
    // 2) email relay ai superadmin + destinatari extra dal pannello
    if let Ok(relay) = std::env::var("YW_ESC_SMTP_RELAY_URL") { if !relay.trim().is_empty() {
        for e in &emails { let body = json!({"to":e,"subject":subj,"text":detail});
            if crate::delivery::deliver_email_relay(&st.http, &relay, &body).await.state == "live" { any = true; } }
    }}
    // 3) SMS Twilio (gated live) al numero configurato dal pannello (o env)
    if crate::delivery::TwilioCfg::live_enabled() { if let (Some(cfg), Some(ph)) = (crate::delivery::TwilioCfg::from_env(), alert_phone.clone()) {
        if crate::delivery::deliver_twilio_live(&st.http, &cfg, crate::delivery::Channel::Sms, &ph, &subj, "system").await.state == "live" { any = true; }
    }}
    any
}
#[derive(Deserialize)] pub struct TenantStatusReq { pub status: String }
/// Sospende/riattiva un tenant (sospeso = login utenti bloccato). Superadmin.
pub async fn set_tenant_status(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<TenantStatusReq>) -> R {
    require_super(&st, &h).await?;
    if !["attivo","sospeso"].contains(&b.status.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "stato non valido")); }
    let n = sqlx::query("UPDATE app.tenants SET status=$2 WHERE id=$1").bind(tid).bind(&b.status)
        .execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "tenant inesistente")); }
    audit(&st, tid, "superadmin", "tenant_status", json!({"status":b.status})).await;
    Ok(Json(json!({"ok":true,"status":b.status})))
}
#[derive(Deserialize)] pub struct DeleteTenantReq { pub slug: String }
/// Elimina DEFINITIVAMENTE un tenant e tutti i suoi dati (FK ON DELETE CASCADE). Richiede
/// la conferma dello slug esatto nel body (anti-errore). Superadmin.
pub async fn delete_tenant(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<DeleteTenantReq>) -> R {
    require_super(&st, &h).await?;
    let slug: Option<String> = sqlx::query_scalar("SELECT slug FROM app.tenants WHERE id=$1").bind(tid)
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let slug = slug.ok_or_else(|| err(StatusCode::NOT_FOUND, "tenant inesistente"))?;
    if b.slug.trim() != slug { return Err(err(StatusCode::BAD_REQUEST, "conferma slug non corrispondente")); }
    // audit PRIMA della delete (le righe audit del tenant sparirebbero con la cascade): riga globale.
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,'superadmin','delete_tenant',$1)")
        .bind(json!({"slug":slug,"tenant_id":tid})).execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.tenants WHERE id=$1").bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"slug":slug})))
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
        // #3 manifest: dipendenze + descrizione + metadata (single source of truth in core::modules)
        "requires": man.map(|m| m.requires).unwrap_or(&[]),
        "provides": man.map(|m| m.provides).unwrap_or(&[]),
        "mkind": man.map(|m| m.kind),
        "capabilities": man.map(|m| m.capabilities).unwrap_or(&[]),
        "widgets": man.map(|m| m.widgets).unwrap_or(&[]),
        "description": man.map(|m| m.description) }) }).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct SetModule { pub module_code: String, pub state: String }

pub async fn set_module(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetModule>) -> R {
    require_super(&st, &h).await?;
    let valid = ["non_abilitato","da_configurare","attivo","degradato","in_disattivazione"];
    if !valid.contains(&b.state.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "bad state")); }
    // Ritiro connettori esterni: un modulo non disponibile nel prodotto (available=false) non può
    // essere attivato (suite self-contained). Disattivarlo resta sempre consentito.
    if b.state != "non_abilitato" {
        let avail: Option<bool> = sqlx::query_scalar("SELECT available FROM app.modules_catalog WHERE code=$1")
            .bind(&b.module_code).fetch_optional(&st.owner_pool).await.map_err(ise)?;
        if avail == Some(false) { return Err(err(StatusCode::BAD_REQUEST, "modulo ritirato dal prodotto (non attivabile)")); }
    }
    // Block disabling loneworker if it is the only SOS path for a tenant with open incidents.
    if b.module_code == "loneworker" && b.state == "non_abilitato" {
        let open = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state<>'chiuso'")
            .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
        if open > 0 { return Err(err(StatusCode::CONFLICT, "open incidents: disattivazione bloccata (apri piano di disattivazione)")); }
    }
    // UPSERT: i moduli aggiunti al catalogo DOPO la creazione del tenant non hanno la riga
    // (il seed la crea solo al create). Un semplice UPDATE non troverebbe nulla e lo stato
    // resterebbe 'non_abilitato'. Con ON CONFLICT l'attivazione funziona per qualsiasi modulo.
    sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES($2,$3,$1)
                 ON CONFLICT (tenant_id,module_code) DO UPDATE SET state=EXCLUDED.state,updated_at=now()")
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
    audit(&st, tid, "superadmin", "set_module", json!({"module":b.module_code,"state":b.state})).await;
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

// ============ Auto-integrazioni: moduli integrati configurati dal prodotto, non dall'uomo ============
// I motori integrati (YWAlert, Traccar) girano sulla nostra infrastruttura: base URL e
// credenziali admin vengono dai default del prodotto (env), così l'attivazione di un modulo li
// configura DA SOLA, senza intervento dell'amministratore. I provider AI restano esterni (chiave cliente).
fn env_opt(k: &str) -> Option<String> { std::env::var(k).ok().filter(|s| !s.trim().is_empty()) }
fn norm_base(b: &str) -> String { let b=b.trim_end_matches('/'); b.strip_suffix("/api").unwrap_or(b).to_string() }

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
/// Stato delle integrazioni di un tenant (semaforo: ok / anomalia / spento). Segnala solo i problemi.
pub async fn super_integrations_status(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    // Traccar
    let tr_on = module_active(&st, tid, "traccar").await;
    let tr_cfg: i64 = sqlx::query_scalar("SELECT count(*) FROM app.traccar_connections WHERE tenant_id=$1").bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    let tr_reach: Option<bool> = sqlx::query_scalar("SELECT reachable FROM app.traccar_poll_state WHERE tenant_id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let tr = if !tr_on { json!({"module":false,"status":"spento"}) }
        else if tr_cfg==0 { json!({"module":true,"status":"anomalia","detail":"motore Traccar non configurato nel prodotto (TRACCAR_BASE)"}) }
        else if tr_reach==Some(false) { json!({"module":true,"status":"anomalia","detail":"server Traccar irraggiungibile"}) }
        else { json!({"module":true,"status":"ok","detail":"connesso"}) };
    Ok(Json(json!({"traccar":tr})))
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
        "traccar" => ("app.traccar_connections", Some("app.traccar_poll_state")),
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
        if let Err(e) = auto_connect_traccar(st, tid).await { tracing::debug!("auto traccar {tid}: {e}"); }
        for m in ["traccar"] { deauto_integration(st, tid, m).await; }
    }
}

// ---- "YouWorkforce pilota YWAlert": sincronizza Gruppi/Routing/Escalation -> YWAlert ----
// Escapa una stringa per inserirla in un literal GraphQL.
fn parse_hhmm(s: &str) -> Option<(u32,u32)> {
    let mut it = s.split(':'); let h: u32 = it.next()?.trim().parse().ok()?; let m: u32 = it.next()?.trim().parse().ok()?;
    if h < 24 && m < 60 { Some((h,m)) } else { None }
}
/// Start della rotation YWAlert: l'istante determina l'ora (e per 'weekly' il giorno) di
/// handoff. 2024-01-01 e' un lunedi'; handoff_day 0=lun..6=dom mappa 2024-01-01..07.
fn rotation_start(handoff_time: Option<&str>, handoff_day: Option<i32>) -> String {
    let day = handoff_day.unwrap_or(0).clamp(0,6);
    let (h,m) = handoff_time.and_then(parse_hhmm).unwrap_or((0,0));
    format!("2024-01-{:02}T{:02}:{:02}:00Z", 1+day, h, m)
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
        modules:vec!["iot"],
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
        modules:vec!["iot"],
        rules:vec![ r("leak","is_true",None,crit), r("flow",">=",Some(100.0),tele), r("fraud","is_true",None,crit) ],
        devices:vec![ d("water-1","Contatore acqua 1") ] },
      Scenario{ code:"smart_retail", name:"Smart Retail (catena freddo)", sector:"Retail", desc:"Frigo/freezer e asset di supermercato: qualita' alimentare e disponibilita'.",
        modules:vec!["iot"],
        rules:vec![ r("temperature",">=",Some(8.0),crit), r("temperature","<=",Some(-25.0),crit), r("door_open","is_true",None,tele) ],
        devices:vec![ d("chiller-1","Frigo 1"), d("freezer-1","Freezer 1") ] },
      Scenario{ code:"smart_farming", name:"Smart Farming", sector:"Agricoltura", desc:"Stoccaggio cereali in silos: livello, temperatura, umidità grano, aerazione. Allarmi su surriscaldamento, grano umido (muffa) e scorta bassa.",
        modules:vec!["iot"],
        rules:vec![ r("temperature",">=",Some(30.0),crit), r("moisture",">=",Some(25.0),crit), r("humidity",">=",Some(80.0),crit), r("crop_level","<=",Some(15.0),tele) ],
        devices:vec![ d("silo-a","Silo A"), d("silo-b","Silo B"), d("silo-c","Silo C") ] },
      Scenario{ code:"site_fleet_tracking", name:"Site Fleet Tracking", sector:"Mezzi", desc:"Escavatori, camion e mezzi in cantiere/miniera: posizione e carburante.",
        modules:vec!["loneworker","iot"],
        rules:vec![ r("fuel","<=",Some(10.0),crit), r("engine_temp",">=",Some(110.0),crit) ],
        devices:vec![ d("vehicle-1","Mezzo 1") ] },
      Scenario{ code:"smart_metering", name:"Smart Metering", sector:"Utility", desc:"Dati smart meter per consumo risorse: perdite, anomalie, frodi.",
        modules:vec!["iot"],
        rules:vec![ r("leak","is_true",None,crit), r("fraud","is_true",None,crit), r("consumption",">=",Some(1000.0),tele) ],
        devices:vec![ d("smartmeter-1","Smart meter 1") ] },
      Scenario{ code:"health_care", name:"Health Care (assisted living)", sector:"Sanita'", desc:"Metriche salute paziente da device connessi, allerta su variazioni critiche.",
        modules:vec!["loneworker","iot"],
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
        modules:vec!["iot"],
        rules:vec![ r("level","<=",Some(10.0),crit), r("level",">=",Some(95.0),crit) ],
        devices:vec![ d("tank-1","Serbatoio 1") ] },
      Scenario{ code:"scada_pool", name:"SCADA Swimming Pool", sector:"SCADA", desc:"Sistemi piscina e processi industriali in tempo reale.",
        modules:vec!["iot"],
        rules:vec![ r("ph","<=",Some(6.8),crit), r("ph",">=",Some(7.8),crit), r("chlorine","<=",Some(0.5),crit), r("temperature",">=",Some(32.0),tele) ],
        devices:vec![ d("pool-1","Centralina piscina 1") ] },
      Scenario{ code:"scada_oil_gas", name:"SCADA Oil & Gas Drilling", sector:"SCADA", desc:"Perforazione: parametri chiave (rig, pompe, BOP) con focus sicurezza.",
        modules:vec!["iot"],
        rules:vec![ r("pressure",">=",Some(350.0),crit), r("gas",">=",Some(20.0),crit), r("vibration",">=",Some(80.0),crit) ],
        devices:vec![ d("rig-1","Rig 1") ] },
      Scenario{ code:"scada_energy", name:"SCADA Energy Management", sector:"SCADA", desc:"Flussi energetici e infrastruttura: efficienza, picchi di domanda, costi.",
        modules:vec!["iot"],
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
/// Aziona l'aerazione di un silo (device dello scenario). Aggiorna lo stato nativo (devices.config)
/// e inietta un campione di telemetria (così il cruscotto riflette il cambio). operatore/tenant_admin.
pub async fn scenario_aeration_set(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>, Json(b): Json<AerationReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !has_cap(&s,"devices.manage") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    // L'azione è nativa (stato + telemetria): nessun server esterno coinvolto.
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
    if !has_cap(&s,"incidents.view") {
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
    // #6 — invii di sicurezza bloccati: notifiche/chiusure YWAlert che non passano
    // (oltre soglia, ancora in coda a ritentare). Visibile all'admin come degrado.
    let outbox_stuck: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM app.outbox WHERE tenant_id=$1 AND kind IN ('notify','close') AND state<>'done' AND attempts>=6")
        .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    let outbox_last_error: Option<String> = sqlx::query_scalar(
        "SELECT last_error FROM app.outbox WHERE tenant_id=$1 AND kind IN ('notify','close') AND state<>'done' AND attempts>=6 ORDER BY attempts DESC LIMIT 1")
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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

/// Validazione E.164: '+' seguito da 8-15 cifre, prima cifra 1-9.
pub(crate) fn is_e164(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 9 || b.len() > 16 || b[0] != b'+' { return false; }
    if !(b'1'..=b'9').contains(&b[1]) { return false; }
    b[1..].iter().all(|c| c.is_ascii_digit())
}

#[derive(Deserialize)]
pub struct GaCfgSet { pub entries: Vec<GaCfgEntry> }
#[derive(Deserialize)]
pub struct GaCfgEntry { pub id: String, pub value: String }
// ----------------------------- tenant plane -----------------------------
pub async fn tenant_login(State(st): State<AppState>, h: HeaderMap, Json(b): Json<LoginReq>) -> impl IntoResponse {
    // tenant resolved from Host (or X-Tenant-Slug for local dev)
    let slug = match tenant_slug(&st, &h) {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, Json(json!({"error":"no tenant host"}))).into_response(),
    };
    let trow = sqlx::query("SELECT id,status FROM app.tenants WHERE slug=$1").bind(&slug)
        .fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(trow) = trow else { return (StatusCode::NOT_FOUND, Json(json!({"error":"tenant not found"}))).into_response() };
    let tid: Uuid = trow.get("id");
    if trow.try_get::<String,_>("status").ok().as_deref() == Some("sospeso") {
        return (StatusCode::FORBIDDEN, Json(json!({"error":"tenant sospeso — contatta l'assistenza"}))).into_response();
    }
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


/// Import "serve a tutti i moduli": copia i membri di un gruppo comune in un GRUPPO DI ALLARME
/// (escalation). Dimostra il riuso senza accoppiare i modelli. Richiede il modulo escalation.
#[derive(Deserialize)] pub struct ImportGroupReq { pub user_group_id: Uuid }
pub async fn alarm_group_import_members(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<ImportGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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

// Identità/utenti (list/CRUD + ACL sedi) estratti in core/src/users.rs (Fase B).

// ============================ Gestione (Fase 1) ============================
// Anagrafiche e operazioni che mancavano: dispositivi, sedi, utenti, gruppi di
// allarme + routing, assegnazione/note incidenti, check-in in centrale.
// ---- Fase 2/4: capability layer (granulare, module-neutral) --------------------
// Catalogo capability. `admin` è SUPER: implica tutte le altre (vedi has_cap).
// Fase 4 (Tappa 1): split per-modulo dei controlli basati su ruolo.
/// Moduli a cui una capability è legata (vuoto = CORE, sempre rilevante).
/// Serve alla UI permessi per mostrare solo i permessi dei moduli attivi + i core.
pub fn cap_modules(cap: &str) -> &'static [&'static str] {
    match cap {
        "incidents.view" | "incidents.handle" => &["loneworker"],
        "escalation.manage"                    => &["loneworker"],
        "devices.manage" | "devices.admin"     => &["loneworker","traccar","iot"],
        "iot.manage"                           => &["iot","traccar"],
        "plan"                                 => &["interventi","cantieri","giri","workforce"],
        "approve" | "timesheets.view_all"      => &["consuntivi"],
        _ => &[], // core: utenti/ruoli/privacy/audit/dashboard/impostazioni/diagnostica
    }
}
pub const CAPABILITIES: &[(&str,&str)] = &[
    ("admin",                "Amministrazione completa (super): implica tutte le capability"),
    ("incidents.view",       "Sicurezza: lettura centrale, mappe, dispositivi, report"),
    ("incidents.handle",     "Sicurezza: gestione allarmi (presa in carico, chiusura, note, test)"),
    ("devices.manage",       "Dispositivi: invio comandi e controllo"),
    ("plan",                 "Pianificazione: ordini/interventi, ronde, turni"),
    ("approve",              "Consuntivi: approvazione/rifiuto"),
    ("timesheets.view_all",  "Consuntivi: lettura di tutti (non solo i propri)"),
    ("config.view",          "Lettura configurazione e diagnostica"),
    // Fase 4 (Tappa 2): aree amministrative, split di `admin`. Nessun ruolo le ha di default
    // (solo `tenant_admin` via `admin` super); assegnabili a ruoli custom per delega mirata.
    ("users.manage",         "Amministrazione: utenti, gruppi di persone, permessi, ruoli custom, sedi/squadre/asset"),
    ("devices.admin",        "Amministrazione dispositivi: censimento, token/onboarding, geofence, provisioning"),
    ("iot.manage",           "IoT e connettori: Traccar/YWAlert, regole sensori, email ingest"),
    ("escalation.manage",    "Escalation e reperibilità: gruppi allarme, turni on-call, override, profili, trigger"),
    ("settings.manage",      "Impostazioni tenant: funzionalità UI e configurazione generale"),
    ("gdpr.manage",          "Privacy GDPR: export e anonimizzazione dati personali"),
    ("audit.view",           "Audit: lettura del registro attività"),
    ("dashboard.manage",     "Dashboard: organizza i widget del tenant (abilita/ordina/dimensiona)"),
];
/// Mappa di DEFAULT ruolo -> capability (riproduce il comportamento storico; `admin`=super).
pub(crate) fn default_role_caps(role: &str) -> Vec<&'static str> {
    match role {
        "tenant_admin"  => vec!["admin"],
        "operatore"     => vec!["incidents.view","incidents.handle","devices.manage"],
        "pianificatore" => vec!["plan"],
        "responsabile"  => vec!["approve","incidents.view","incidents.handle","timesheets.view_all"],
        "auditor"       => vec!["incidents.view","timesheets.view_all","config.view"],
        "reperibile"    => vec!["incidents.handle"],
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
// `admin` è super: implica ogni capability.
pub(crate) fn has_cap(s: &Session, cap: &str) -> bool { s.caps.iter().any(|c| c == cap || c == "admin") }
pub(crate) fn require_cap(s: &Session, cap: &str) -> Result<(), (StatusCode, String)> {
    if !has_cap(s, cap) { return Err(err(StatusCode::FORBIDDEN, &format!("permesso richiesto: {cap}"))); }
    Ok(())
}

/// Conservato per retrocompatibilità/estensioni; i call-site usano ora `require_cap` per-area (Tappa 2).
#[allow(dead_code)]
fn admin_only(s: &Session) -> Result<(), (StatusCode, String)> {
    if !has_cap(s, "admin") { return Err(err(StatusCode::FORBIDDEN, "riservato all'admin del tenant")); }
    Ok(())
}
pub(crate) const USER_ROLES: &[&str] = &["tenant_admin","operatore","pianificatore","responsabile","lavoratore","reperibile","auditor"];

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

// UserSitesReq/get_user_sites/set_user_sites estratti in core/src/users.rs (Fase B).

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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // Elimina in modo completo: telemetria/attributi/comandi/posizioni/presenze/geofence/
    // provisioning sono ON DELETE CASCADE; lo storico eventi resta con device_id azzerato
    // (0109, ON DELETE SET NULL). Niente dispositivi "fantasma" non rimovibili.
    let n = sqlx::query("DELETE FROM app.devices WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => err(StatusCode::CONFLICT, "dispositivo ancora referenziato: rimuovi prima i collegamenti"),
            _ => ise(e),
        })?.rows_affected();
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    let tid = s.tenant_id.unwrap();
    const SRC: &[&str] = &["traccar","iot"];
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    let tid = s.tenant_id.unwrap();
    let n = sqlx::query("UPDATE app.ingest_tokens SET revoked_at=now() WHERE token=$1 AND tenant_id=$2 AND revoked_at IS NULL")
        .bind(&tok).bind(tid).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "token inesistente o già revocato")); }
    audit(&st, tid, &s.email, "revoke_ingest_token", json!({})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_ingest_token(State(st): State<AppState>, h: HeaderMap, Path(tok): Path<String>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, CONNECTOR_MODS).await?;
    let defs = [("iot","IoT nativo",0usize)]; // suite self-contained: solo motore IoT nativo
    let mut out = Vec::new();
    for (code,name,_p) in defs {
        let state: Option<String> = sqlx::query_scalar("SELECT state FROM app.tenant_modules WHERE tenant_id=$1 AND module_code=$2")
            .bind(tid).bind(code).fetch_optional(&st.owner_pool).await.map_err(ise)?;
        let toks: i64 = sqlx::query_scalar("SELECT count(*) FROM app.ingest_tokens WHERE tenant_id=$1 AND source=$2")
            .bind(tid).bind(code).fetch_one(&st.owner_pool).await.map_err(ise)?;
        out.push(json!({"code":code,"name":name,"state":state.unwrap_or("non_abilitato".into()),"tokens":toks}));
    }
    Ok(Json(json!(out)))
}

// ================= Fase A — Catalogo modelli dispositivo =================
pub async fn list_catalog(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
    let term = q.get("q").filter(|s| !s.is_empty()).map(|s| format!("%{}%", s.to_lowercase()));
    let manu = q.get("manufacturer").filter(|s| !s.is_empty()).cloned();
    let cat = q.get("category").filter(|s| !s.is_empty()).cloned();
    // scope = perimetro: 'iot' -> solo sensori; 'safety' -> tracker sicurezza/campo (il dump GPS
    // 'altro', 2174 modelli Traccar, compare solo se si CERCA, per non annegare il catalogo curato).
    let scope = q.get("scope").filter(|s| !s.is_empty()).cloned();
    let rows = sqlx::query(
        "SELECT model_code,manufacturer,model_name,protocol,default_port,category,capabilities,osmand,recommended,notes,blueprint
         FROM app.device_models
         WHERE ($1::text IS NULL OR lower(manufacturer||' '||model_name) LIKE $1)
           AND ($2::text IS NULL OR manufacturer=$2)
           AND ($3::text IS NULL OR category=$3)
           AND (CASE
                  WHEN $4='iot'    THEN category='sensore'
                  WHEN $4='safety' THEN category IN ('personale','smartphone','veicolo','asset') OR ($1 IS NOT NULL AND category='altro')
                  ELSE ($1 IS NOT NULL OR category<>'altro')
                END)
         ORDER BY recommended DESC, manufacturer, model_name
         LIMIT 300")
        .bind(term).bind(manu).bind(cat).bind(scope).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "model_code":r.get::<String,_>("model_code"),"manufacturer":r.get::<String,_>("manufacturer"),
        "model_name":r.get::<String,_>("model_name"),"protocol":r.get::<String,_>("protocol"),
        "default_port":r.try_get::<i32,_>("default_port").ok(),"category":r.get::<String,_>("category"),
        "capabilities":r.get::<Vec<String>,_>("capabilities"),"osmand":r.get::<bool,_>("osmand"),
        "recommended":r.get::<bool,_>("recommended"),"notes":r.try_get::<String,_>("notes").ok(),
        "blueprint":r.try_get::<Option<String>,_>("blueprint").ok().flatten() })).collect();
    Ok(Json(json!(out)))
}
pub async fn catalog_facets(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let _ = require_tenant(&st, &h).await?;
    // facets coerenti col perimetro; il bucket 'altro' (dump GPS Traccar) è sempre escluso dai filtri
    // per non proporre centinaia di produttori-rumore (resta comunque cercabile per testo).
    let scope = q.get("scope").filter(|s| !s.is_empty()).cloned();
    let manu: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT manufacturer FROM app.device_models WHERE category<>'altro'
           AND (CASE WHEN $1='iot' THEN category='sensore'
                     WHEN $1='safety' THEN category IN ('personale','smartphone','veicolo','asset')
                     ELSE true END)
         ORDER BY manufacturer")
        .bind(&scope).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cats: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT category FROM app.device_models WHERE category<>'altro'
           AND (CASE WHEN $1='iot' THEN category='sensore'
                     WHEN $1='safety' THEN category IN ('personale','smartphone','veicolo','asset')
                     ELSE true END)
         ORDER BY category")
        .bind(&scope).fetch_all(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"manufacturers":manu,"categories":cats})))
}

#[derive(Deserialize)]
pub struct AssignReq2 { pub user_id: Option<Uuid>, pub asset_id: Option<Uuid> }
pub async fn assign_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<AssignReq2>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let tport = std::env::var("YW_TELTONIKA_PORT").ok().filter(|p| p!="off" && p!="0").unwrap_or_else(|| "5027".into());
    let host = { let h = base.trim_start_matches("https://").trim_start_matches("http://"); h.split(['/',':']).next().unwrap_or(h).to_string() };
    Ok(json!({
        "device_id": dev_id, "external_id": b.external_id.trim(), "model_code": b.model_code,
        "manufacturer": manufacturer, "model_name": model_name, "protocol": protocol,
        "category": category, "category_label": plan.label, "capabilities": caps,
        "etypes": etypes, "routed_etypes": routed, "routed_group": b.group_id, "profile_id": profile_id,
        "token": secret, "handle": handle,
        // Dati per la guida di collegamento (la console costruisce i passi per protocollo).
        "server_host": host, "teltonika_port": tport,
        "ingress_url": format!("{base}/api/ingress/traccar?token={secret}"),
        "osmand_url": format!("{base}/api/ingress/osmand?token={secret}&id={}&lat={{lat}}&lon={{lon}}&timestamp={{unix}}", b.external_id.trim()),
    }))
}

/// Readiness/idoneità del device per i suoi segnali: dichiarato (dalle capacità del modello) vs
/// verificato (abbiamo mai ricevuto quel segnale). Stati: registrazione · assegnazione · connessione · protezione.
pub async fn device_readiness(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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


// ---------------- Utenti ----------------
// NewUser/create_user, UpdateUser/update_user, delete_user estratti in core/src/users.rs (Fase B).

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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
// Turni + copertura + slack del gruppo (effetto alla prossima Sincronizza con YWAlert).
pub async fn set_group_oncall(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GroupOncallReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
// ---- F3: policy di escalation multi-step del gruppo (parità YWAlert) ----
pub async fn group_policy_get(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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

// ---- F3: label/metadata sui gruppi (parità YWAlert labels) + ricerca ----
#[derive(Deserialize)] pub struct LabelsReq { pub labels: std::collections::HashMap<String,String> }
pub async fn group_labels_set(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<LabelsReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
/// Ricerca gruppi per label (key, key+value) o testo nel nome. Parità YWAlert search.
pub async fn group_search(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<LabelSearchQuery>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    if !has_cap(&s,"incidents.handle") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.playbook.len() > 8000 { return Err(err(StatusCode::BAD_REQUEST, "playbook troppo lungo (max 8000)")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.alarm_groups SET playbook=$2 WHERE id=$1").bind(gid).bind(b.playbook.trim()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- R1: Set Active (parità Rotations YWAlert) — imposta SUBITO il reperibile di turno ----
#[derive(Deserialize)] pub struct SetActiveReq { pub user_id: Uuid }
pub async fn rotation_set_active(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<SetActiveReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_assignments WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "assegnazione inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn assignment_member_add(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.esc_assignment_members WHERE assignment_id=$1 AND user_id=$2 AND tenant_id=$3").bind(aid).bind(uid).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

pub async fn delete_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.alarm_group_members WHERE group_id=$1 AND user_id=$2").bind(gid).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct RouteReq { pub etype: String, pub group_id: Option<Uuid> }
pub async fn set_routing(State(st): State<AppState>, h: HeaderMap, Json(b): Json<RouteReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    if !has_cap(&s,"incidents.handle") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    if !has_cap(&s,"incidents.handle") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    if !has_cap(&s,"incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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


// ============================ Campo Slice 2 — Ronde e punzonature (giri) ============================
const PATROL_KINDS: &[&str] = &["qr","nfc","beacon","gps"];

// ---- Configurazione (pianificatore) ----
pub async fn list_patrol_routes(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // ACL per gruppo: il pianificatore vede tutti i giri; gli altri vedono i giri senza gruppo
    // (aperti, comportamento storico) + quelli assegnati a un gruppo di cui sono membri.
    let base = "SELECT r.id,r.name,r.site_id,r.window_minutes,r.active,r.assigned_group,r.mode,r.grace_min,s.name as site_name,g.name as group_name \
        FROM app.patrol_routes r LEFT JOIN app.sites s ON s.id=r.site_id LEFT JOIN app.user_groups g ON g.id=r.assigned_group";
    let routes = if is_planner(&s) {
        sqlx::query(&format!("{base} ORDER BY r.name")).fetch_all(&mut *tx).await
    } else {
        sqlx::query(&format!("{base} WHERE r.assigned_group IS NULL OR r.assigned_group IN (SELECT group_id FROM app.user_group_members WHERE user_id=$1) ORDER BY r.name")).bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    let cps = sqlx::query("SELECT id,route_id,name,position,kind,code,lat,lon,radius_m,expected_min,max_min,dwell_max_min FROM app.patrol_checkpoints ORDER BY position,name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = routes.iter().map(|r| {
        let rid: Uuid = r.get("id");
        let checkpoints: Vec<Value> = cps.iter().filter(|c| c.get::<Uuid,_>("route_id")==rid).map(|c| json!({
            "id":c.get::<Uuid,_>("id"),"name":c.get::<String,_>("name"),"position":c.get::<i32,_>("position"),
            "kind":c.get::<String,_>("kind"),"code":c.try_get::<String,_>("code").ok(),
            "lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),"radius_m":c.get::<i32,_>("radius_m"),
            "expected_min":c.try_get::<i32,_>("expected_min").ok(),"max_min":c.try_get::<i32,_>("max_min").ok(),"dwell_max_min":c.try_get::<i32,_>("dwell_max_min").ok()})).collect();
        json!({"id":rid,"name":r.get::<String,_>("name"),"site_id":r.try_get::<Uuid,_>("site_id").ok(),
            "site_name":r.try_get::<String,_>("site_name").ok(),"window_minutes":r.try_get::<i32,_>("window_minutes").ok(),
            "active":r.get::<bool,_>("active"),"assigned_group":r.try_get::<Uuid,_>("assigned_group").ok(),
            "mode":r.try_get::<String,_>("mode").ok(),"grace_min":r.try_get::<i32,_>("grace_min").ok(),
            "group_name":r.try_get::<String,_>("group_name").ok(),"checkpoints":checkpoints})
    }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct PatrolRouteReq { pub name: String, #[serde(default)] pub site_id: Option<Uuid>, #[serde(default)] pub window_minutes: Option<i32>, #[serde(default)] pub active: Option<bool>, #[serde(default)] pub assigned_group: Option<Uuid>, #[serde(default)] pub clear_group: Option<bool>, #[serde(default)] pub mode: Option<String>, #[serde(default)] pub grace_min: Option<i32> }
pub async fn create_patrol_route(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PatrolRouteReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.patrol_routes(tenant_id,name,site_id,window_minutes,assigned_group,mode,grace_min) VALUES($1,$2,$3,$4,$5,COALESCE($6,'piedi'),COALESCE($7,0)) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.site_id).bind(b.window_minutes).bind(b.assigned_group).bind(&b.mode).bind(b.grace_min)
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
    let clear_g = b.clear_group.unwrap_or(false);
    let n = sqlx::query("UPDATE app.patrol_routes SET name=$2, site_id=$3, window_minutes=$4, active=COALESCE($5,active), assigned_group=CASE WHEN $6 THEN NULL ELSE COALESCE($7,assigned_group) END, mode=COALESCE($8,mode), grace_min=COALESCE($9,grace_min) WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(b.site_id).bind(b.window_minutes).bind(b.active).bind(clear_g).bind(b.assigned_group).bind(&b.mode).bind(b.grace_min).execute(&mut *tx).await.map_err(ise)?.rows_affected();
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
#[derive(Deserialize)] pub struct CheckpointReq { pub name: String, pub kind: String, #[serde(default)] pub code: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>, #[serde(default)] pub radius_m: Option<i32>, #[serde(default)] pub position: Option<i32>,
    #[serde(default)] pub expected_min: Option<i32>, #[serde(default)] pub max_min: Option<i32>, #[serde(default)] pub dwell_max_min: Option<i32> }
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
    let row = sqlx::query("INSERT INTO app.patrol_checkpoints(tenant_id,route_id,name,position,kind,code,lat,lon,radius_m,expected_min,max_min,dwell_max_min) VALUES($1,$2,$3,$4,$5,$6,$7,$8,COALESCE($9,50),$10,$11,$12) RETURNING id")
        .bind(tid).bind(rid).bind(b.name.trim()).bind(pos).bind(&b.kind).bind(b.code.as_deref()).bind(b.lat).bind(b.lon).bind(b.radius_m).bind(b.expected_min).bind(b.max_min).bind(b.dwell_max_min)
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
    let n = sqlx::query("UPDATE app.patrol_checkpoints SET name=$2, kind=$3, code=$4, lat=$5, lon=$6, radius_m=COALESCE($7,radius_m), position=COALESCE($8,position), expected_min=$9, max_min=$10, dwell_max_min=$11 WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(&b.kind).bind(b.code.as_deref()).bind(b.lat).bind(b.lon).bind(b.radius_m).bind(b.position).bind(b.expected_min).bind(b.max_min).bind(b.dwell_max_min).execute(&mut *tx).await.map_err(ise)?.rows_affected();
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
/// Checkpoint riusabili fra giri: copia la definizione di un checkpoint esistente (stesso tenant)
/// in un altro giro, in coda. Niente schema nuovo: il punto fisico si definisce una volta e si
/// riusa copiandolo dove serve.
#[derive(Deserialize)] pub struct ImportCheckpointReq { pub source_checkpoint_id: Uuid }
pub async fn import_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<ImportCheckpointReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.patrol_routes WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"giro inesistente")); }
    let src = sqlx::query("SELECT name,kind,code,lat,lon,radius_m FROM app.patrol_checkpoints WHERE id=$1")
        .bind(b.source_checkpoint_id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"checkpoint sorgente inesistente"))?;
    let pos: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(position),0)::int FROM app.patrol_checkpoints WHERE route_id=$1").bind(rid).fetch_one(&mut *tx).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.patrol_checkpoints(tenant_id,route_id,name,position,kind,code,lat,lon,radius_m) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(rid).bind(src.get::<String,_>("name")).bind(pos+1).bind(src.get::<String,_>("kind"))
        .bind(src.try_get::<String,_>("code").ok()).bind(src.try_get::<f64,_>("lat").ok()).bind(src.try_get::<f64,_>("lon").ok())
        .bind(src.get::<i32,_>("radius_m")).fetch_one(&mut *tx).await.map_err(ise)?;
    let id = row.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}

// ---- Esecuzione (qualunque utente del tenant col modulo attivo) ----
async fn round_view(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, rid: Uuid) -> Result<Value, (StatusCode,String)> {
    let r = sqlx::query("SELECT ro.id,ro.route_id,ro.user_id,ro.started_at,ro.ended_at,ro.status,rt.name as route_name,rt.window_minutes FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id WHERE ro.id=$1")
        .bind(rid).fetch_optional(&mut **tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"ronda inesistente"))?;
    let route_id: Uuid = r.get("route_id");
    let cps = sqlx::query("SELECT id,name,position,kind,code,lat,lon,radius_m,expected_min,max_min,dwell_max_min FROM app.patrol_checkpoints WHERE route_id=$1 ORDER BY position,name").bind(route_id).fetch_all(&mut **tx).await.map_err(ise)?;
    let punches = sqlx::query("SELECT checkpoint_id,method,punched_at,ok,note FROM app.patrol_punches WHERE round_id=$1").bind(rid).fetch_all(&mut **tx).await.map_err(ise)?;
    let cpv: Vec<Value> = cps.iter().map(|c| {
        let cid: Uuid = c.get("id");
        let p = punches.iter().find(|p| p.get::<Uuid,_>("checkpoint_id")==cid);
        json!({"id":cid,"name":c.get::<String,_>("name"),"position":c.get::<i32,_>("position"),"kind":c.get::<String,_>("kind"),
            "code":c.try_get::<String,_>("code").ok(),"lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),"radius_m":c.get::<i32,_>("radius_m"),
            "expected_min":c.try_get::<i32,_>("expected_min").ok(),"max_min":c.try_get::<i32,_>("max_min").ok(),"dwell_max_min":c.try_get::<i32,_>("dwell_max_min").ok(),
            "punched": p.map(|p| json!({"method":p.get::<String,_>("method"),"at":p.get::<chrono::DateTime<chrono::Utc>,_>("punched_at").to_rfc3339(),"ok":p.get::<bool,_>("ok"),"note":p.try_get::<String,_>("note").ok()}))})
    }).collect();
    let anoms = sqlx::query("SELECT kind,detail,checkpoint_id,created_at FROM app.patrol_anomalies WHERE round_id=$1 ORDER BY created_at").bind(rid).fetch_all(&mut **tx).await.map_err(ise)?;
    let anom_v: Vec<Value> = anoms.iter().map(|a| json!({"kind":a.get::<String,_>("kind"),"detail":a.try_get::<String,_>("detail").ok(),
        "checkpoint_id":a.try_get::<Uuid,_>("checkpoint_id").ok()})).collect();
    let total = cps.len();
    let done = cpv.iter().filter(|c| c["punched"].get("ok").and_then(|v| v.as_bool())==Some(true)).count();
    Ok(json!({"id":rid,"route_id":route_id,"route_name":r.get::<String,_>("route_name"),"user_id":r.get::<Uuid,_>("user_id"),
        "started_at":r.get::<chrono::DateTime<chrono::Utc>,_>("started_at").to_rfc3339(),
        "ended_at":r.try_get::<chrono::DateTime<chrono::Utc>,_>("ended_at").ok().map(|d| d.to_rfc3339()),
        "status":r.get::<String,_>("status"),"window_minutes":r.try_get::<i32,_>("window_minutes").ok(),
        "checkpoints":cpv,"total":total,"done":done,"anomalies":anom_v}))
}
pub async fn start_round(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rr = sqlx::query("SELECT active, assigned_group FROM app.patrol_routes WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"giro inesistente"))?;
    if !rr.get::<bool,_>("active") { return Err(err(StatusCode::BAD_REQUEST,"giro non attivo")); }
    // ACL per gruppo: se il giro è assegnato a un gruppo, solo i membri (o il pianificatore) avviano ronde
    if let Some(g) = rr.try_get::<Option<Uuid>,_>("assigned_group").ok().flatten() {
        if !is_planner(&s) && !user_in_group(&st, s.user_id, g).await {
            return Err(err(StatusCode::FORBIDDEN, "giro riservato a un altro gruppo"));
        }
    }
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
    let round = sqlx::query("SELECT ro.user_id,ro.route_id,ro.started_at,ro.status,rt.window_minutes,rt.grace_min FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id WHERE ro.id=$1")
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
        // wall-clock: started_at e' il DEFAULT now() SQL (wall), quindi l'elapsed si misura sul wall
        // per restare coerente con come e' stato scritto. NON usare st.clock.now() qui.
        let elapsed = (chrono::Utc::now() - started).num_minutes();
        in_window = elapsed <= win as i64;
    }
    let status = if total>0 && done==total && in_window { "completato" } else { "incompleto" };
    sqlx::query("UPDATE app.patrol_rounds SET ended_at=$2, status=$3 WHERE id=$1")
        .bind(id).bind(chrono::Utc::now()).bind(status).execute(&mut *tx).await.map_err(ise)?;
    // ---- V1 vigilanza: rileva anomalie (tappa saltata, tratta in ritardo, giro non completato) ----
    let grace: i32 = round.try_get::<i32,_>("grace_min").unwrap_or(0);
    let cps = sqlx::query("SELECT c.id,c.name,c.position,c.max_min, p.punched_at, p.ok FROM app.patrol_checkpoints c
                 LEFT JOIN app.patrol_punches p ON p.checkpoint_id=c.id AND p.round_id=$1
                 WHERE c.route_id=$2 ORDER BY c.position,c.name").bind(id).bind(route_id).fetch_all(&mut *tx).await.map_err(ise)?;
    // pulizia anomalie precedenti dello stesso round (idempotenza se richiuso)
    sqlx::query("DELETE FROM app.patrol_anomalies WHERE round_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    let mut anomalies: Vec<(Option<Uuid>,&str,String)> = Vec::new();
    let mut prev_time: Option<chrono::DateTime<chrono::Utc>> = None;
    for c in &cps {
        let cid: Uuid = c.get("id");
        let name: String = c.get("name");
        let okp = c.try_get::<bool,_>("ok").ok();
        let at = c.try_get::<chrono::DateTime<chrono::Utc>,_>("punched_at").ok();
        if okp != Some(true) {
            anomalies.push((Some(cid), "saltato", format!("Tappa non punzonata: {name}")));
        } else {
            if let (Some(mx), Some(pt), Some(cur)) = (c.try_get::<i32,_>("max_min").ok(), prev_time, at) {
                let mins = (cur - pt).num_minutes();
                if mins > (mx as i64 + grace as i64) {
                    anomalies.push((Some(cid), "in_ritardo", format!("Tratta verso «{name}»: {mins} min (max {mx}+{grace})")));
                }
            }
            prev_time = at;
        }
    }
    if status=="incompleto" {
        anomalies.push((None, "non_completato", if in_window { format!("Giro incompleto: {done}/{total} tappe") } else { "Giro oltre la finestra di tempo".to_string() }));
    }
    for (cid, kind, detail) in &anomalies {
        sqlx::query("INSERT INTO app.patrol_anomalies(tenant_id,round_id,checkpoint_id,kind,detail) VALUES($1,$2,$3,$4,$5)")
            .bind(tid).bind(id).bind(cid).bind(kind).bind(detail).execute(&mut *tx).await.map_err(ise)?;
    }
    let anom_count = anomalies.len();
    let v = round_view(&mut tx, id).await?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "close_round", json!({"round":id,"status":status,"done":done,"total":total,"anomalies":anom_count})).await;
    Ok(Json(json!({"status":status,"in_window":in_window,"anomalies":anom_count,"round":v})))
}
// ===== Vigilanza V2: ronde pianificate/ricorrenti + cruscotto =====
#[derive(Deserialize)] pub struct ScheduleReq { pub route_id: Uuid, #[serde(default)] pub days_mask: Option<String>, #[serde(default)] pub from_time: Option<String>, #[serde(default)] pub to_time: Option<String>, #[serde(default)] pub every_minutes: Option<i32> }
fn parse_time(s:&Option<String>, def:&str)->chrono::NaiveTime {
    let v=s.as_deref().unwrap_or(def);
    chrono::NaiveTime::parse_from_str(v,"%H:%M").or_else(|_| chrono::NaiveTime::parse_from_str(v,"%H:%M:%S")).unwrap_or_else(|_| chrono::NaiveTime::from_hms_opt(22,0,0).unwrap())
}
pub async fn list_patrol_schedules(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT sc.id,sc.route_id,rt.name AS route_name,sc.days_mask,sc.from_time,sc.to_time,sc.every_minutes,sc.active FROM app.patrol_schedules sc JOIN app.patrol_routes rt ON rt.id=sc.route_id ORDER BY rt.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"route_id":r.get::<Uuid,_>("route_id"),"route_name":r.get::<String,_>("route_name"),
        "days_mask":r.get::<String,_>("days_mask"),
        "from_time":r.get::<chrono::NaiveTime,_>("from_time").format("%H:%M").to_string(),
        "to_time":r.get::<chrono::NaiveTime,_>("to_time").format("%H:%M").to_string(),
        "every_minutes":r.get::<i32,_>("every_minutes"),"active":r.get::<bool,_>("active") })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_patrol_schedule(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ScheduleReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?; if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN,"solo pianificatore/admin")); }
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".to_string());
    if dm.len()!=7 || !dm.chars().all(|c| c=='0'||c=='1') { return Err(err(StatusCode::BAD_REQUEST,"days_mask 7 cifre 0/1")); }
    let ev = b.every_minutes.unwrap_or(60).max(5);
    let (ft,tt)=(parse_time(&b.from_time,"22:00"), parse_time(&b.to_time,"06:00"));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.patrol_schedules(tenant_id,route_id,days_mask,from_time,to_time,every_minutes) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.route_id).bind(&dm).bind(ft).bind(tt).bind(ev).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_patrol_schedule(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?; if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN,"solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.patrol_schedules WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Cruscotto vigilanza: ronde in corso (live), ronde dovute vs svolte oggi, anomalie di giornata.
pub async fn patrol_dashboard(State(st): State<AppState>, h: HeaderMap) -> R {
    use chrono::{TimeZone as _, Datelike, Duration};
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let now = st.clock.now(); let today = now.date_naive(); // orologio controllabile (coerente col resto; TEST_CLOCK)
    let widx = today.weekday().num_days_from_monday() as usize;
    let win_start = chrono::Utc.from_utc_datetime(&today.and_hms_opt(0,0,0).unwrap()) - Duration::hours(12); // include la sera prima (notturni)
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let live = sqlx::query("SELECT ro.id,ro.route_id,rt.name AS route_name,u.email,ro.started_at,
            (SELECT count(*) FROM app.patrol_checkpoints c WHERE c.route_id=ro.route_id)::int AS total,
            (SELECT count(*) FROM app.patrol_punches p WHERE p.round_id=ro.id AND p.ok)::int AS done,
            (SELECT d.last_lat FROM app.devices d WHERE d.assigned_user=ro.user_id AND d.last_lat IS NOT NULL ORDER BY d.last_pos_at DESC NULLS LAST LIMIT 1) AS dev_lat,
            (SELECT d.last_lon FROM app.devices d WHERE d.assigned_user=ro.user_id AND d.last_lon IS NOT NULL ORDER BY d.last_pos_at DESC NULLS LAST LIMIT 1) AS dev_lon
         FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id JOIN app.users u ON u.id=ro.user_id
         WHERE ro.status='in_corso' ORDER BY ro.started_at").fetch_all(&mut *tx).await.map_err(ise)?;
    let rounds = sqlx::query("SELECT route_id,started_at FROM app.patrol_rounds WHERE started_at >= $1").bind(win_start).fetch_all(&mut *tx).await.map_err(ise)?;
    let scheds = sqlx::query("SELECT sc.route_id,rt.name AS route_name,sc.days_mask,sc.from_time,sc.to_time,sc.every_minutes FROM app.patrol_schedules sc JOIN app.patrol_routes rt ON rt.id=sc.route_id WHERE sc.active").fetch_all(&mut *tx).await.map_err(ise)?;
    let anom_today: i64 = sqlx::query_scalar("SELECT count(*) FROM app.patrol_anomalies WHERE created_at >= $1").bind(win_start).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let round_times: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = rounds.iter().map(|r| (r.get("route_id"), r.get("started_at"))).collect();
    let mut slots: Vec<Value> = Vec::new();
    let (mut done_n, mut miss_n, mut up_n) = (0i64,0i64,0i64);
    for sc in &scheds {
        let dm: String = sc.get("days_mask");
        let ft: chrono::NaiveTime = sc.get("from_time"); let tt: chrono::NaiveTime = sc.get("to_time");
        let ev: i32 = sc.get("every_minutes");
        let rid: Uuid = sc.get("route_id"); let rname: String = sc.get("route_name");
        if dm.chars().nth(widx) != Some('1') { continue; }
        let start = chrono::Utc.from_utc_datetime(&today.and_time(ft));
        let mut end = chrono::Utc.from_utc_datetime(&today.and_time(tt));
        if end <= start { end = end + Duration::days(1); } // notturno
        let step = Duration::minutes(ev.max(5) as i64);
        let mut slot = start;
        while slot <= end {
            let covered = round_times.iter().any(|(r,t)| *r==rid && *t >= slot - step/2 && *t < slot + step);
            let status = if covered { done_n+=1; "svolta" } else if slot + step < now { miss_n+=1; "mancante" } else { up_n+=1; "attesa" };
            slots.push(json!({"route_id":rid,"route_name":rname,"at":slot.to_rfc3339(),"status":status}));
            slot = slot + step;
        }
    }
    let live_v: Vec<Value> = live.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"route_name":r.get::<String,_>("route_name"),"email":r.get::<String,_>("email"),
        "started_at":r.get::<chrono::DateTime<chrono::Utc>,_>("started_at").to_rfc3339(),
        "done":r.get::<i32,_>("done"),"total":r.get::<i32,_>("total"),
        "dev_lat":r.try_get::<f64,_>("dev_lat").ok(),"dev_lon":r.try_get::<f64,_>("dev_lon").ok() })).collect();
    Ok(Json(json!({"live":live_v,"slots":slots,"summary":{"svolte":done_n,"mancanti":miss_n,"attese":up_n,"anomalie_oggi":anom_today}})))
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.teams(tenant_id,name) VALUES($1,$2) RETURNING id")
        .bind(tid).bind(b.name.trim()).fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "squadra gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id")})))
}
pub async fn delete_team(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.teams WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "squadra inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn add_team_member(State(st): State<AppState>, h: HeaderMap, Path(tid_p): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.team_members(tenant_id,team_id,user_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(tid_p).bind(b.user_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn remove_team_member(State(st): State<AppState>, h: HeaderMap, Path((team,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.team_members WHERE team_id=$1 AND user_id=$2").bind(team).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Turni pianificati ----------------
#[derive(Deserialize)]
pub struct ShiftPlanReq { pub user_id: Uuid, pub site_id: Option<Uuid>, pub starts_at: String, pub ends_at: String, pub note: Option<String>, #[serde(default)] pub required_qualification: Option<Uuid>, #[serde(default)] pub work_order_id: Option<Uuid>, #[serde(default)] pub commessa_id: Option<Uuid> }

/// Regole di pianificazione del tenant (default di prodotto; preset "standard").
async fn scheduling_rules(st: &AppState, tid: Uuid) -> crate::scheduling::Rules {
    let c = tool_config(st, tid, "workforce", "rules").await;
    let d = crate::scheduling::Rules::default();
    crate::scheduling::Rules {
        min_rest_hours: c.get("min_rest_hours").and_then(|v| v.as_i64()).unwrap_or(d.min_rest_hours),
        max_hours_week: c.get("max_hours_week").and_then(|v| v.as_f64()).unwrap_or(d.max_hours_week),
        max_consec_days: c.get("max_consec_days").and_then(|v| v.as_i64()).unwrap_or(d.max_consec_days),
    }
}
/// Gli altri turni della persona in una finestra ampia attorno al candidato (per i vincoli).
/// `exclude` esclude un turno (usato quando si SPOSTA un turno: non deve confliggere con se stesso).
async fn shift_intervals(st: &AppState, tid: Uuid, uid: Uuid,
    a: chrono::DateTime<chrono::Utc>, z: chrono::DateTime<chrono::Utc>, exclude: Option<Uuid>) -> Vec<crate::scheduling::Interval> {
    let from = a - chrono::Duration::days(9); let to = z + chrono::Duration::days(9);
    sqlx::query("SELECT starts_at,ends_at FROM app.shift_plans WHERE tenant_id=$1 AND user_id=$2 AND ends_at>=$3 AND starts_at<=$4 AND ($5::uuid IS NULL OR id<>$5)")
        .bind(tid).bind(uid).bind(from).bind(to).bind(exclude).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| crate::scheduling::Interval{ start: r.get("starts_at"), end: r.get("ends_at") }).collect()
}
/// Le assenze della persona che toccano la finestra del candidato (vincolo duro).
async fn absence_intervals(st: &AppState, tid: Uuid, uid: Uuid,
    a: chrono::DateTime<chrono::Utc>, z: chrono::DateTime<chrono::Utc>) -> Vec<crate::scheduling::Interval> {
    sqlx::query("SELECT starts_at,ends_at FROM app.worker_absences WHERE tenant_id=$1 AND user_id=$2 AND ends_at>=$3 AND starts_at<=$4")
        .bind(tid).bind(uid).bind(a).bind(z).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| crate::scheduling::Interval{ start: r.get("starts_at"), end: r.get("ends_at") }).collect()
}
/// Se il turno richiede una qualifica, verifica che la persona la possieda valida (non scaduta)
/// all'orario del turno. Nessun requisito o qualifica rimossa -> nessun vincolo.
async fn qual_check(st: &AppState, tid: Uuid, uid: Uuid, required: Option<Uuid>,
    at: chrono::DateTime<chrono::Utc>) -> Option<crate::scheduling::QualReq> {
    let rq = required?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM app.qualifications WHERE id=$1 AND tenant_id=$2")
        .bind(rq).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let name = name?;
    let held: bool = sqlx::query_scalar::<_,Option<i32>>(
        "SELECT 1 FROM app.user_qualifications WHERE tenant_id=$1 AND user_id=$2 AND qualification_id=$3 AND (expires_at IS NULL OR expires_at > $4)")
        .bind(tid).bind(uid).bind(rq).bind(at).fetch_optional(&st.owner_pool).await.ok().flatten().flatten().is_some();
    Some(crate::scheduling::QualReq{ name, satisfied: held })
}
async fn validate_shift_candidate(st: &AppState, tid: Uuid, uid: Uuid,
    a: chrono::DateTime<chrono::Utc>, z: chrono::DateTime<chrono::Utc>, required_qual: Option<Uuid>, exclude: Option<Uuid>) -> Vec<crate::scheduling::Violation> {
    let rules = scheduling_rules(st, tid).await;
    let ex = shift_intervals(st, tid, uid, a, z, exclude).await;
    let ab = absence_intervals(st, tid, uid, a, z).await;
    let q = qual_check(st, tid, uid, required_qual, a).await;
    crate::scheduling::validate(&crate::scheduling::Interval{start:a,end:z}, &ex, &ab, q.as_ref(), &rules)
}

// ---------------- Assenze / indisponibilità (W3a) ----------------
#[derive(Deserialize)]
pub struct AbsenceReq { pub user_id: Uuid, pub starts_at: String, pub ends_at: String, pub kind: Option<String>, pub note: Option<String> }
pub async fn list_absences(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // planner/admin: tutte; altri: solo le proprie.
    let rows = if is_planner(&s) {
        sqlx::query("SELECT a.id,a.user_id,a.starts_at,a.ends_at,a.kind,a.note,u.email FROM app.worker_absences a JOIN app.users u ON u.id=a.user_id ORDER BY a.starts_at DESC")
            .fetch_all(&mut *tx).await
    } else {
        sqlx::query("SELECT a.id,a.user_id,a.starts_at,a.ends_at,a.kind,a.note,u.email FROM app.worker_absences a JOIN app.users u ON u.id=a.user_id WHERE a.user_id=$1 ORDER BY a.starts_at DESC")
            .bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"user_id":r.get::<Uuid,_>("user_id"),"email":r.get::<String,_>("email"),
        "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
        "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at"),
        "kind":r.get::<String,_>("kind"),"note":r.try_get::<String,_>("note").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_absence(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AbsenceReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let kind = b.kind.clone().unwrap_or_else(|| "assenza".into());
    if !["ferie","malattia","permesso","assenza"].contains(&kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo non valido")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.worker_absences(tenant_id,user_id,starts_at,ends_at,kind,note,created_by) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.user_id).bind(a).bind(z).bind(&kind).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_absence", json!({"user":b.user_id,"kind":kind})).await;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn update_absence(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<AbsenceReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let kind = b.kind.clone().unwrap_or_else(|| "assenza".into());
    if !["ferie","malattia","permesso","assenza"].contains(&kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo non valido")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.worker_absences SET user_id=$2,starts_at=$3,ends_at=$4,kind=$5,note=$6 WHERE id=$1")
        .bind(id).bind(b.user_id).bind(a).bind(z).bind(&kind).bind(&b.note).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "assenza inesistente")); }
    audit(&st, tid, &s.email, "update_absence", json!({"user":b.user_id,"kind":kind})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_absence(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.worker_absences WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "assenza inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---------------- Qualifiche e competenze (W3b, modulo `qualifiche`) ----------------
#[derive(Deserialize)] pub struct QualBody { pub name: String, pub note: Option<String> }
pub async fn list_qualifications(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,name,note FROM app.qualifications ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"note":r.try_get::<String,_>("note").ok()})).collect::<Vec<_>>())))
}
pub async fn create_qualification(State(st): State<AppState>, h: HeaderMap, Json(b): Json<QualBody>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.qualifications(tenant_id,name,note) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.note)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "qualifica già esistente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    audit(&st, tid, &s.email, "create_qualification", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_qualification(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.qualifications WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn list_user_qualifications(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT q.id,q.name,uq.expires_at FROM app.user_qualifications uq JOIN app.qualifications q ON q.id=uq.qualification_id WHERE uq.user_id=$1 ORDER BY q.name")
        .bind(uid).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),
        "expires_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("expires_at").ok()})).collect::<Vec<_>>())))
}
#[derive(Deserialize)] pub struct AssignQual { pub qualification_id: Uuid, pub expires_at: Option<String> }
pub async fn user_qualification_add(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<AssignQual>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let exp = match &b.expires_at { Some(e) if !e.trim().is_empty() => Some(parse_ts(e)?), _ => None };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.user_qualifications(tenant_id,user_id,qualification_id,expires_at) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id,user_id,qualification_id) DO UPDATE SET expires_at=EXCLUDED.expires_at")
        .bind(tid).bind(uid).bind(b.qualification_id).bind(exp).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn user_qualification_del(State(st): State<AppState>, h: HeaderMap, Path((uid,qid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_qualifications WHERE user_id=$1 AND qualification_id=$2").bind(uid).bind(qid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Copertura minima (W3c, modulo workforce) ----------------
#[derive(Deserialize)]
pub struct CoverageBody { pub site_id: Option<Uuid>, pub days_mask: Option<String>, pub cov_start: String, pub cov_end: String,
    pub min_people: Option<i32>, pub required_qualification: Option<Uuid>, pub note: Option<String> }
pub async fn list_coverage(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT c.id,c.site_id,c.days_mask,c.cov_start,c.cov_end,c.min_people,c.required_qualification,c.note,
            s.name AS site, q.name AS qual
        FROM app.coverage_requirements c LEFT JOIN app.sites s ON s.id=c.site_id LEFT JOIN app.qualifications q ON q.id=c.required_qualification
        ORDER BY c.created_at").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok(),
        "days_mask":r.get::<String,_>("days_mask"),"cov_start":r.get::<String,_>("cov_start"),"cov_end":r.get::<String,_>("cov_end"),
        "min_people":r.get::<i32,_>("min_people"),"required_qualification":r.try_get::<Uuid,_>("required_qualification").ok(),
        "qual":r.try_get::<String,_>("qual").ok(),"note":r.try_get::<String,_>("note").ok()})).collect::<Vec<_>>())))
}
pub async fn create_coverage(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CoverageBody>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (sh, eh) = (parse_hhmm(&b.cov_start), parse_hhmm(&b.cov_end));
    if sh.is_none() || eh.is_none() { return Err(err(StatusCode::BAD_REQUEST, "orari HH:MM richiesti")); }
    let (sh, eh) = (sh.unwrap(), eh.unwrap());
    if (eh.0*60+eh.1) <= (sh.0*60+sh.1) { return Err(err(StatusCode::BAD_REQUEST, "la fine deve essere dopo l'inizio (stessa giornata)")); }
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".into());
    if dm.len()!=7 || !dm.chars().all(|c| c=='0'||c=='1') { return Err(err(StatusCode::BAD_REQUEST, "giorni: 7 cifre 0/1 (lun..dom)")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.coverage_requirements(tenant_id,site_id,days_mask,cov_start,cov_end,min_people,required_qualification,note) VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(tid).bind(b.site_id).bind(&dm).bind(&b.cov_start).bind(&b.cov_end).bind(b.min_people.unwrap_or(1).max(1)).bind(b.required_qualification).bind(&b.note)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn update_coverage(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<CoverageBody>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (sh, eh) = (parse_hhmm(&b.cov_start), parse_hhmm(&b.cov_end));
    if sh.is_none() || eh.is_none() { return Err(err(StatusCode::BAD_REQUEST, "orari HH:MM richiesti")); }
    let (sh, eh) = (sh.unwrap(), eh.unwrap());
    if (eh.0*60+eh.1) <= (sh.0*60+sh.1) { return Err(err(StatusCode::BAD_REQUEST, "la fine deve essere dopo l'inizio (stessa giornata)")); }
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".into());
    if dm.len()!=7 || !dm.chars().all(|c| c=='0'||c=='1') { return Err(err(StatusCode::BAD_REQUEST, "giorni: 7 cifre 0/1 (lun..dom)")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.coverage_requirements SET site_id=$2,days_mask=$3,cov_start=$4,cov_end=$5,min_people=$6,required_qualification=$7,note=$8 WHERE id=$1")
        .bind(id).bind(b.site_id).bind(&dm).bind(&b.cov_start).bind(&b.cov_end).bind(b.min_people.unwrap_or(1).max(1)).bind(b.required_qualification).bind(&b.note)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND,"inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_coverage(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.coverage_requirements WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND,"inesistente")); }
    Ok(Json(json!({"ok":true})))
}
/// Controllo copertura: per ogni requisito e giorno applicabile nell'intervallo, conta le persone che
/// coprono l'INTERA fascia (con sede/qualifica giusta) e segnala i giorni sotto il minimo (i "buchi").
pub async fn coverage_check(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    use chrono::{Datelike, TimeZone};
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let now = st.clock.now();
    let pd = |k:&str, def: chrono::DateTime<chrono::Utc>| q.get(k).and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok().map(|t| t.with_timezone(&chrono::Utc))).unwrap_or(def);
    let from = pd("from", now);
    let mut to = pd("to", now + chrono::Duration::days(14));
    if (to - from).num_days() > 62 { to = from + chrono::Duration::days(62); } // cap
    // filtro facoltativo a un singolo requisito (?requirement=<id>)
    let only: Option<Uuid> = q.get("requirement").and_then(|v| Uuid::parse_str(v).ok());
    let reqs = sqlx::query("SELECT c.id,c.site_id,c.days_mask,c.cov_start,c.cov_end,c.min_people,c.required_qualification,s.name AS site
        FROM app.coverage_requirements c LEFT JOIN app.sites s ON s.id=c.site_id WHERE c.tenant_id=$1 AND ($2::uuid IS NULL OR c.id=$2)")
        .bind(tid).bind(only).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut gaps: Vec<Value> = Vec::new();
    for r in &reqs {
        let rid: Uuid = r.get("id");
        let site_id: Option<Uuid> = r.try_get("site_id").ok();
        let dm: String = r.get("days_mask");
        let (sh,sm) = match parse_hhmm(&r.get::<String,_>("cov_start")) { Some(x)=>x, None=>continue };
        let (eh,em) = match parse_hhmm(&r.get::<String,_>("cov_end")) { Some(x)=>x, None=>continue };
        let min_people: i32 = r.get("min_people");
        let qual: Option<Uuid> = r.try_get("required_qualification").ok();
        let site_name: Option<String> = r.try_get("site").ok();
        let mut d = from.date_naive();
        let last = to.date_naive();
        while d <= last {
            let widx = d.weekday().num_days_from_monday() as usize; // 0=lun
            if dm.chars().nth(widx) == Some('1') {
                let ws = chrono::Utc.from_utc_datetime(&d.and_hms_opt(sh,sm,0).unwrap());
                let we = chrono::Utc.from_utc_datetime(&d.and_hms_opt(eh,em,0).unwrap());
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(DISTINCT sp.user_id) FROM app.shift_plans sp
                     WHERE sp.tenant_id=$1 AND sp.starts_at<=$2 AND sp.ends_at>=$3
                       AND ($4::uuid IS NULL OR sp.site_id=$4)
                       AND ($5::uuid IS NULL OR EXISTS(SELECT 1 FROM app.user_qualifications uq
                              WHERE uq.tenant_id=$1 AND uq.user_id=sp.user_id AND uq.qualification_id=$5
                                AND (uq.expires_at IS NULL OR uq.expires_at>$2)))")
                    .bind(tid).bind(ws).bind(we).bind(site_id).bind(qual)
                    .fetch_one(&st.owner_pool).await.unwrap_or(0);
                if (count as i32) < min_people {
                    gaps.push(json!({"requirement_id":rid,"date":d.to_string(),"site":site_name,
                        "window":format!("{:02}:{:02}–{:02}:{:02}",sh,sm,eh,em),
                        "have":count,"need":min_people}));
                }
            }
            d = d.succ_opt().unwrap();
        }
    }
    Ok(Json(json!({"gaps":gaps,"requirements":reqs.len(),"from":from.to_rfc3339(),"to":to.to_rfc3339()})))
}
/// Equità (vincolo MORBIDO, informativo): distribuzione del carico per persona nell'intervallo —
/// ore, numero turni, weekend, notti. Aiuta a bilanciare; non blocca nulla.
pub async fn fairness_report(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let now = st.clock.now();
    let pd = |k:&str, def: chrono::DateTime<chrono::Utc>| q.get(k).and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok().map(|t| t.with_timezone(&chrono::Utc))).unwrap_or(def);
    let from = pd("from", now - chrono::Duration::days(30));
    let to   = pd("to", now + chrono::Duration::days(1));
    let rows = sqlx::query(
        "SELECT u.email,
            count(*) AS n,
            COALESCE(sum(EXTRACT(EPOCH FROM (sp.ends_at-sp.starts_at))/3600.0),0)::float8 AS hours,
            count(*) FILTER (WHERE EXTRACT(DOW FROM sp.starts_at) IN (0,6)) AS weekend,
            count(*) FILTER (WHERE EXTRACT(HOUR FROM sp.starts_at) >= 22 OR EXTRACT(HOUR FROM sp.starts_at) < 6) AS nights
         FROM app.shift_plans sp JOIN app.users u ON u.id=sp.user_id
         WHERE sp.tenant_id=$1 AND sp.starts_at>=$2 AND sp.starts_at<$3
         GROUP BY u.email ORDER BY hours DESC")
        .bind(tid).bind(from).bind(to).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let people: Vec<Value> = rows.iter().map(|r| json!({
        "email":r.get::<String,_>("email"),"shifts":r.get::<i64,_>("n"),
        "hours":(r.get::<f64,_>("hours")*10.0).round()/10.0,
        "weekend":r.get::<i64,_>("weekend"),"nights":r.get::<i64,_>("nights")})).collect();
    // scarto ore max-min (indice di squilibrio)
    let hrs: Vec<f64> = rows.iter().map(|r| r.get::<f64,_>("hours")).collect();
    let spread = match (hrs.iter().cloned().fold(f64::MIN, f64::max), hrs.iter().cloned().fold(f64::MAX, f64::min)) {
        (mx, mn) if !hrs.is_empty() => ((mx-mn)*10.0).round()/10.0, _ => 0.0 };
    Ok(Json(json!({"people":people,"spread_hours":spread,"from":from.to_rfc3339(),"to":to.to_rfc3339()})))
}

// ---------------- W4: AI turni — proposta sostituti (cambio turno) ----------------
// Deterministico e GROUNDED: usa il MOTORE regole per scartare chi ha conflitti/assenze o manca la
// competenza, e ordina per equità (meno ore). Se youworker.ai è attivo, aggiunge una frase di
// consiglio in linguaggio naturale (fallback: testo deterministico). L'AI propone, l'uomo assegna.
pub async fn shift_substitutes(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let sh = sqlx::query("SELECT user_id,site_id,starts_at,ends_at,required_qualification FROM app.shift_plans WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "turno inesistente"))?;
    let cur: Uuid = sh.get("user_id");
    let a: chrono::DateTime<chrono::Utc> = sh.get("starts_at");
    let z: chrono::DateTime<chrono::Utc> = sh.get("ends_at");
    let req_qual: Option<Uuid> = sh.try_get("required_qualification").ok();
    // bacino candidati: ruoli assegnabili del tenant
    let pool = sqlx::query("SELECT id,email FROM app.users WHERE tenant_id=$1 AND role IN ('lavoratore','operatore','reperibile','responsabile','pianificatore') AND id<>$2")
        .bind(tid).bind(cur).fetch_all(&st.owner_pool).await.unwrap_or_default();
    // carico (ore ultimi 30g) per equità
    let from30 = a - chrono::Duration::days(30);
    let load = sqlx::query("SELECT user_id, COALESCE(sum(EXTRACT(EPOCH FROM (ends_at-starts_at))/3600.0),0)::float8 AS hrs FROM app.shift_plans WHERE tenant_id=$1 AND starts_at>=$2 AND starts_at<$3 GROUP BY user_id")
        .bind(tid).bind(from30).bind(a).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let hmap: std::collections::HashMap<Uuid,f64> = load.iter().map(|r| (r.get::<Uuid,_>("user_id"), r.get::<f64,_>("hrs"))).collect();
    let mut cands: Vec<(Uuid,String,f64,Vec<crate::scheduling::Violation>)> = Vec::new();
    for u in &pool {
        let uid: Uuid = u.get("id");
        let vs = validate_shift_candidate(&st, tid, uid, a, z, req_qual, None).await;
        if crate::scheduling::has_hard(&vs) { continue; } // scarta chi ha conflitti duri
        cands.push((uid, u.get::<String,_>("email"), *hmap.get(&uid).unwrap_or(&0.0), vs));
    }
    // ordina: prima chi non ha nemmeno avvisi, poi meno ore (equità)
    cands.sort_by(|x,y| (x.3.len(), (x.2*10.0) as i64).cmp(&(y.3.len(), (y.2*10.0) as i64)));
    let candidates: Vec<Value> = cands.iter().take(10).map(|(uid,em,hrs,vs)| json!({
        "user_id":uid,"email":em,"hours_30d":(hrs*10.0).round()/10.0,
        "warnings": vs.iter().map(|v| v.message.clone()).collect::<Vec<_>>()})).collect();
    // sintesi: deterministica, poi (se AI attiva) riformulata
    let det = match cands.first() {
        Some((_,em,hrs,vs)) => format!("Consiglio: {} ({} ore negli ultimi 30g{}). {} candidati idonei.",
            em, (hrs*10.0).round()/10.0, if vs.is_empty() {", nessun avviso"} else {", con avvisi"}, cands.len()),
        None => "Nessun sostituto idoneo (tutti in conflitto, assenti o senza la competenza richiesta).".into(),
    };
    let mut summary = det.clone();
    if module_active(&st, tid, "assistant").await && ai_feature_on(&st, tid, "assistant").await {
        if let Some(c) = ai_conn(&st, tid).await {
            let sys = "Sei l'assistente di pianificazione turni di YouWorkforce. In UNA frase, consiglia il \
                sostituto migliore usando SOLO i fatti forniti (equità=meno ore, nessun avviso meglio). Italiano, conciso.";
            let ctx = format!("Candidati idonei (già filtrati dal motore regole): {}", serde_json::to_string(&candidates).unwrap_or_default());
            if let Ok(ans) = ai_chat(&st, &c, sys, &ctx).await { if !ans.trim().is_empty() { summary = ans; } }
        }
    }
    Ok(Json(json!({"shift_id":id,"candidates":candidates,"summary":summary,
        "provider": if summary==det {"deterministico"} else {"youworker.ai"}})))
}
fn violations_json(vs: &[crate::scheduling::Violation]) -> Value {
    json!(vs.iter().map(|v| json!({"code":v.code,"hard":v.hard,"message":v.message})).collect::<Vec<_>>())
}

/// Preflight: valida un turno candidato senza crearlo (riusabile dalla UI). Non modifica nulla.
pub async fn shift_plan_validate(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ShiftPlanReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    let vs = validate_shift_candidate(&st, tid, b.user_id, a, z, b.required_qualification, None).await;
    Ok(Json(json!({"ok": !crate::scheduling::has_hard(&vs), "violations": violations_json(&vs)})))
}

pub async fn create_shift_plan(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ShiftPlanReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    // Motore regole: i vincoli DURI (es. sovrapposizione) bloccano; gli avvisi (riposo, ore
    // settimana) non bloccano ma tornano nella risposta. Con `force=1` si ignorano anche gli hard
    // (override consapevole del pianificatore), tranne la finestra non valida.
    let force = h.get("x-force").map(|v| v=="1"||v=="true").unwrap_or(false);
    let vs = validate_shift_candidate(&st, tid, b.user_id, a, z, b.required_qualification, None).await;
    if crate::scheduling::has_hard(&vs) && !force {
        let msg = vs.iter().filter(|v| v.hard).map(|v| v.message.clone()).collect::<Vec<_>>().join(" ");
        return Err(err(StatusCode::CONFLICT, &format!("Conflitto: {msg} (usa Forza per procedere)")));
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.shift_plans(tenant_id,user_id,site_id,starts_at,ends_at,note,created_by,required_qualification,work_order_id,commessa_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id")
        .bind(tid).bind(b.user_id).bind(b.site_id).bind(a).bind(z).bind(&b.note).bind(s.user_id).bind(b.required_qualification).bind(b.work_order_id).bind(b.commessa_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let iid: Uuid = row.get("id");
    audit(&st, tid, &s.email, "create_shift_plan", json!({"user":b.user_id,"warnings":vs.len()})).await;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"shift_planned","id":iid})}).await;
    Ok(Json(json!({"id":iid, "warnings": violations_json(&vs)})))
}

#[derive(Deserialize)]
pub struct ShiftMove { pub starts_at: String, pub ends_at: String, #[serde(default)] pub user_id: Option<Uuid> }
/// Sposta/ridimensiona un turno (drag&drop dal calendario) o lo riassegna. Validato dal motore
/// (overlap/assenza/competenza = duri, salvo header x-force; riposo/ore = avvisi). Escludo il turno
/// stesso dai conflitti così trascinarlo di poco non confligge con se stesso.
pub async fn shift_plan_move(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ShiftMove>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let cur = sqlx::query("SELECT user_id,required_qualification,state FROM app.shift_plans WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "turno inesistente"))?;
    let new_uid = b.user_id.unwrap_or_else(|| cur.get::<Uuid,_>("user_id"));
    let req_qual: Option<Uuid> = cur.try_get("required_qualification").ok();
    let force = h.get("x-force").map(|v| v=="1"||v=="true").unwrap_or(false);
    let vs = validate_shift_candidate(&st, tid, new_uid, a, z, req_qual, Some(id)).await;
    if crate::scheduling::has_hard(&vs) && !force {
        let msg = vs.iter().filter(|v| v.hard).map(|v| v.message.clone()).collect::<Vec<_>>().join(" ");
        return Err(err(StatusCode::CONFLICT, &format!("Conflitto: {msg} (usa Forza per procedere)")));
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.shift_plans SET starts_at=$2, ends_at=$3, user_id=$4 WHERE id=$1")
        .bind(id).bind(a).bind(z).bind(new_uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "shift_plan_move", json!({"id":id,"user":new_uid})).await;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"shift_planned","id":id})}).await;
    Ok(Json(json!({"ok":true, "warnings": violations_json(&vs)})))
}

#[derive(Deserialize)]
pub struct SuggestReq { pub from: String, pub to: String, #[serde(default)] pub days_mask: Option<String>,
    pub cov_start: String, pub cov_end: String, #[serde(default)] pub min_people: Option<i32>,
    #[serde(default)] pub candidates: Option<Vec<Uuid>> }
/// W4/W5 — SUGGERIMENTO AI di combinazioni turni: per ogni giorno applicabile propone chi assegnare
/// per coprire la fascia, scegliendo candidati VALIDI (motore: no conflitti/assenze/competenza) e
/// bilanciando il carico (equità: aggiorna le ore man mano). NON crea nulla: il pianificatore rivede
/// e applica. Se youworker.ai è attivo, aggiunge una frase di sintesi.
pub async fn shift_suggest(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SuggestReq>) -> R {
    use chrono::{Datelike, TimeZone};
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let d0 = chrono::NaiveDate::parse_from_str(&b.from, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "from: data YYYY-MM-DD"))?;
    let mut d1 = chrono::NaiveDate::parse_from_str(&b.to, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "to: data YYYY-MM-DD"))?;
    if (d1 - d0).num_days() > 62 { d1 = d0 + chrono::Duration::days(62); }
    let (sh, eh) = (parse_hhmm(&b.cov_start).ok_or_else(|| err(StatusCode::BAD_REQUEST, "cov_start HH:MM"))?,
                    parse_hhmm(&b.cov_end).ok_or_else(|| err(StatusCode::BAD_REQUEST, "cov_end HH:MM"))?);
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".into());
    let need = b.min_people.unwrap_or(1).max(1);
    // pool candidati
    let pool: Vec<(Uuid,String)> = match &b.candidates {
        Some(ids) if !ids.is_empty() => sqlx::query("SELECT id,email FROM app.users WHERE tenant_id=$1 AND id=ANY($2)")
            .bind(tid).bind(ids).fetch_all(&st.owner_pool).await.unwrap_or_default(),
        _ => sqlx::query("SELECT id,email FROM app.users WHERE tenant_id=$1 AND role IN ('lavoratore','operatore','reperibile','responsabile','pianificatore')")
            .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default(),
    }.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email"))).collect();
    // carico iniziale (ultimi 30g prima di from) per equità, poi aggiornato man mano
    let from_dt = chrono::Utc.from_utc_datetime(&d0.and_hms_opt(0,0,0).unwrap());
    let mut hours: std::collections::HashMap<Uuid,f64> = sqlx::query(
        "SELECT user_id, COALESCE(sum(EXTRACT(EPOCH FROM (ends_at-starts_at))/3600.0),0)::float8 AS hrs FROM app.shift_plans WHERE tenant_id=$1 AND starts_at>=$2 AND starts_at<$3 GROUP BY user_id")
        .bind(tid).bind(from_dt - chrono::Duration::days(30)).bind(from_dt).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| (r.get::<Uuid,_>("user_id"), r.get::<f64,_>("hrs"))).collect();
    let shift_hours = ((eh.0*60+eh.1) as f64 - (sh.0*60+sh.1) as f64)/60.0;
    let mut proposals: Vec<Value> = Vec::new();
    let mut d = d0;
    while d <= d1 {
        let widx = d.weekday().num_days_from_monday() as usize;
        if dm.chars().nth(widx) == Some('1') {
            let ws = chrono::Utc.from_utc_datetime(&d.and_hms_opt(sh.0,sh.1,0).unwrap());
            let we = chrono::Utc.from_utc_datetime(&d.and_hms_opt(eh.0,eh.1,0).unwrap());
            // candidati validi per questo giorno, ordinati per meno ore (equità)
            let mut ok_c: Vec<(Uuid,String,f64)> = Vec::new();
            for (uid,em) in &pool {
                let vs = validate_shift_candidate(&st, tid, *uid, ws, we, None, None).await;
                if crate::scheduling::has_hard(&vs) { continue; }
                ok_c.push((*uid, em.clone(), *hours.get(uid).unwrap_or(&0.0)));
            }
            ok_c.sort_by(|a,b| ((a.2*10.0) as i64).cmp(&((b.2*10.0) as i64)));
            for (uid,em,_) in ok_c.into_iter().take(need as usize) {
                *hours.entry(uid).or_insert(0.0) += shift_hours; // bilancia i giorni successivi
                proposals.push(json!({"date":d.to_string(),"user_id":uid,"email":em,
                    "starts_at":ws.to_rfc3339(),"ends_at":we.to_rfc3339()}));
            }
        }
        d = d.succ_opt().unwrap();
    }
    let det = format!("{} assegnazioni proposte su {}–{} ({} persone/giorno), bilanciate per equità.",
        proposals.len(), b.from, b.to, need);
    let mut summary = det.clone();
    if module_active(&st, tid, "assistant").await && ai_feature_on(&st, tid, "assistant").await {
        if let Some(c) = ai_conn(&st, tid).await {
            let sys = "Assistente pianificazione turni YouWorkforce. In UNA frase riassumi la proposta usando SOLO i fatti. Italiano, conciso.";
            let ctx = format!("Proposte: {}", serde_json::to_string(&proposals).unwrap_or_default());
            if let Ok(ans) = ai_chat(&st, &c, sys, &ctx).await { if !ans.trim().is_empty() { summary = ans; } }
        }
    }
    Ok(Json(json!({"proposals":proposals,"summary":summary,"provider": if summary==det {"deterministico"} else {"youworker.ai"}})))
}

#[derive(Deserialize)] pub struct ColorReq { pub color: Option<String> }
/// Imposta il colore calendario di un utente (il pianificatore per chiunque, ognuno per sé).
pub async fn user_set_color(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<ColorReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if uid != s.user_id && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin o te stesso")); }
    let c = match b.color.as_deref().map(|x| x.trim()) {
        None | Some("") => None,
        Some(x) => {
            let ok = x.starts_with('#') && x[1..].chars().all(|c| c.is_ascii_hexdigit()) && (x.len()==4 || x.len()==7);
            if !ok { return Err(err(StatusCode::BAD_REQUEST, "colore non valido (#rgb o #rrggbb)")); }
            Some(x.to_string())
        }
    };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.users SET cal_color=$2 WHERE id=$1").bind(uid).bind(&c).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"color":c})))
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
        "SELECT p.id,p.user_id,p.site_id,p.starts_at,p.ends_at,p.note,p.state,u.email,si.name AS site,
                p.work_order_id, o.title AS order_title, p.commessa_id, cm.code AS commessa_code, cm.name AS commessa_name
         FROM app.shift_plans p JOIN app.users u ON u.id=p.user_id LEFT JOIN app.sites si ON si.id=p.site_id
         LEFT JOIN app.work_orders o ON o.id=p.work_order_id
         LEFT JOIN app.commesse cm ON cm.id=p.commessa_id
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
        "note":r.try_get::<String,_>("note").ok(),"state":r.get::<String,_>("state"),
        "work_order_id":r.try_get::<Uuid,_>("work_order_id").ok(),"order_title":r.try_get::<String,_>("order_title").ok(),
        "commessa_id":r.try_get::<Uuid,_>("commessa_id").ok(),"commessa_code":r.try_get::<String,_>("commessa_code").ok(),"commessa_name":r.try_get::<String,_>("commessa_name").ok() })).collect();
    Ok(Json(json!(out)))
}
// ===== Modulo Cantieri: commessa (progetto) -> lavorazioni (WBS) -> SAL; riuso turni =====
fn parse_date_opt(s: &Option<String>) -> Option<chrono::NaiveDate> {
    s.as_ref().and_then(|x| chrono::NaiveDate::parse_from_str(x.trim(), "%Y-%m-%d").ok())
}
#[derive(Deserialize)]
pub struct CommessaReq {
    pub code: String, pub name: String,
    #[serde(default)] pub site_id: Option<Uuid>, #[serde(default)] pub client_name: Option<String>,
    #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub cig: Option<String>, #[serde(default)] pub cup: Option<String>,
    #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String>,
    #[serde(default)] pub budget_amount: Option<f64>, #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub address: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>,
}
#[derive(Deserialize)]
pub struct CommessaPatch {
    #[serde(default)] pub name: Option<String>, #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub client_name: Option<String>, #[serde(default)] pub start_date: Option<String>,
    #[serde(default)] pub end_date: Option<String>, #[serde(default)] pub budget_amount: Option<f64>,
    #[serde(default)] pub note: Option<String>, #[serde(default)] pub labor_cost_hour: Option<f64>,
    #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub cig: Option<String>, #[serde(default)] pub cup: Option<String>,
    #[serde(default)] pub address: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>,
}
pub async fn list_commesse(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT c.id,c.code,c.name,c.status,COALESCE(cl.name,c.client_name) AS client_name,c.start_date,c.end_date,c.budget_amount::float8 AS budget,
                c.address AS address,c.lat AS lat,c.lon AS lon,
                si.name AS site,si.id AS site_id,si.lat AS site_lat,si.lon AS site_lon,
                COALESCE((SELECT CASE WHEN sum(weight)>0 THEN round(sum(weight*progress_pct)/sum(weight)) ELSE 0 END
                          FROM app.commessa_lavorazioni l WHERE l.commessa_id=c.id),0)::int AS progress,
                (SELECT count(*) FROM app.commessa_lavorazioni l WHERE l.commessa_id=c.id)::int AS lav_count,
                (SELECT count(*) FROM app.shift_plans p WHERE p.commessa_id=c.id AND p.state<>'annullato')::int AS shift_count
         FROM app.commesse c LEFT JOIN app.sites si ON si.id=c.site_id LEFT JOIN app.clients cl ON cl.id=c.client_id ORDER BY c.created_at DESC")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"code":r.get::<String,_>("code"),"name":r.get::<String,_>("name"),
        "status":r.get::<String,_>("status"),"client_name":r.try_get::<String,_>("client_name").ok(),
        "site":r.try_get::<String,_>("site").ok(),"site_id":r.try_get::<Uuid,_>("site_id").ok(),
        "site_lat":r.try_get::<f64,_>("site_lat").ok(),"site_lon":r.try_get::<f64,_>("site_lon").ok(),
        "address":r.try_get::<String,_>("address").ok(),
        "lat":r.try_get::<f64,_>("lat").ok(),"lon":r.try_get::<f64,_>("lon").ok(),
        "start_date":r.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":r.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "budget":r.try_get::<f64,_>("budget").ok(),
        "progress":r.get::<i32,_>("progress"),"lav_count":r.get::<i32,_>("lav_count"),"shift_count":r.get::<i32,_>("shift_count")
    })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_commessa(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CommessaReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.code.trim().is_empty() || b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "codice e nome richiesti")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let dup: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.commesse WHERE code=$1").bind(b.code.trim()).fetch_optional(&mut *tx).await.map_err(ise)?;
    if dup.is_some() { return Err(err(StatusCode::CONFLICT, "codice commessa già esistente")); }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.commesse(tenant_id,code,name,site_id,client_name,start_date,end_date,budget_amount,status,note,created_by,client_id,cig,cup,address,lat,lon)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8::numeric,COALESCE($9,'pianificato'),$10,$11,$12,$13,$14,$15,$16,$17) RETURNING id")
        .bind(tid).bind(b.code.trim()).bind(b.name.trim()).bind(b.site_id).bind(&b.client_name)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(b.budget_amount)
        .bind(&b.status).bind(&b.note).bind(s.user_id).bind(b.client_id).bind(&b.cig).bind(&b.cup)
        .bind(&b.address).bind(b.lat).bind(b.lon)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_create", json!({"code":b.code,"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn get_commessa(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let c = sqlx::query(
        "SELECT c.id,c.code,c.name,c.status,COALESCE(cl.name,c.client_name) AS client_name,c.client_id,c.cig,c.cup,
                c.start_date,c.end_date,c.budget_amount::float8 AS budget,
                c.labor_cost_hour::float8 AS labor_rate,
                c.note,c.site_id,si.name AS site,c.address AS address,c.lat AS lat,c.lon AS lon,si.lat AS site_lat,si.lon AS site_lon,
                COALESCE((SELECT CASE WHEN sum(weight)>0 THEN round(sum(weight*progress_pct)/sum(weight)) ELSE 0 END
                          FROM app.commessa_lavorazioni l WHERE l.commessa_id=c.id),0)::int AS progress
         FROM app.commesse c LEFT JOIN app.sites si ON si.id=c.site_id LEFT JOIN app.clients cl ON cl.id=c.client_id WHERE c.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let c = match c { Some(r)=>r, None=>{ return Err(err(StatusCode::NOT_FOUND, "commessa non trovata")); } };
    let lav = sqlx::query("SELECT id,name,position,weight::float8 AS weight,progress_pct,status,note,start_date,end_date,depends_on,
        (end_date IS NOT NULL AND end_date < current_date AND progress_pct<100) AS late FROM app.commessa_lavorazioni WHERE commessa_id=$1 ORDER BY position,name").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let deprows = sqlx::query("SELECT d.lav_id,d.depends_on_id FROM app.commessa_lav_deps d JOIN app.commessa_lavorazioni l ON l.id=d.lav_id WHERE l.commessa_id=$1").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let sal = sqlx::query("SELECT s.id,s.sal_date,s.pct,s.amount::float8 AS amount,s.note,s.created_at,u.email AS author FROM app.commessa_sal s LEFT JOIN app.users u ON u.id=s.created_by WHERE s.commessa_id=$1 ORDER BY s.sal_date DESC,s.created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let shifts = sqlx::query("SELECT p.id,p.starts_at,p.ends_at,p.state,u.email FROM app.shift_plans p JOIN app.users u ON u.id=p.user_id WHERE p.commessa_id=$1 AND p.state<>'annullato' ORDER BY p.starts_at").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let teams = sqlx::query("SELECT t.id,t.name,(SELECT count(*) FROM app.team_members m WHERE m.team_id=t.id)::int AS members FROM app.commessa_teams ct JOIN app.teams t ON t.id=ct.team_id WHERE ct.commessa_id=$1 ORDER BY t.name").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let pres = sqlx::query("SELECT p.id,p.clock_in,p.clock_out,u.email FROM app.commessa_presenze p JOIN app.users u ON u.id=p.user_id WHERE p.commessa_id=$1 ORDER BY p.clock_in DESC LIMIT 50").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let my_open: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.commessa_presenze WHERE commessa_id=$1 AND user_id=$2 AND clock_out IS NULL").bind(id).bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    // C3 — costi: voci (materiali/noli/...) + ore di presenza chiuse valorizzate al costo orario
    let costi = sqlx::query("SELECT k.id,k.description,k.category,k.amount::float8 AS amount,k.cost_date,u.email AS author FROM app.commessa_costi k LEFT JOIN app.users u ON u.id=k.created_by WHERE k.commessa_id=$1 ORDER BY k.cost_date DESC,k.created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let labor_hours: f64 = sqlx::query_scalar("SELECT COALESCE(sum(EXTRACT(EPOCH FROM (clock_out-clock_in))/3600.0),0)::float8 FROM app.commessa_presenze WHERE commessa_id=$1 AND clock_out IS NOT NULL").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    let materials_cost: f64 = sqlx::query_scalar("SELECT COALESCE(sum(amount),0)::float8 FROM app.commessa_costi WHERE commessa_id=$1").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    let diario = sqlx::query("SELECT g.id,g.diary_date,g.weather,g.present_count,g.body,u.email AS author FROM app.commessa_diario g LEFT JOIN app.users u ON u.id=g.created_by WHERE g.commessa_id=$1 ORDER BY g.diary_date DESC,g.created_at DESC LIMIT 100").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let fatture = sqlx::query("SELECT f.id,f.number,f.amount::float8 AS amount,f.issue_date,f.status,f.note,f.sal_id FROM app.commessa_fatture f WHERE f.commessa_id=$1 ORDER BY f.issue_date DESC,f.created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let atts = sqlx::query("SELECT id,entity_kind,kind,filename,content_type,caption,created_by,created_at FROM app.commessa_attachments WHERE commessa_id=$1 ORDER BY created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let att_out: Vec<Value> = atts.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"entity_kind":r.get::<String,_>("entity_kind"),"kind":r.get::<String,_>("kind"),
        "filename":r.try_get::<String,_>("filename").ok(),"content_type":r.get::<String,_>("content_type"),
        "caption":r.try_get::<String,_>("caption").ok(),"created_by":r.try_get::<String,_>("created_by").ok() })).collect();
    let diario_out: Vec<Value> = diario.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"diary_date":r.get::<chrono::NaiveDate,_>("diary_date").to_string(),
        "weather":r.try_get::<String,_>("weather").ok(),"present_count":r.try_get::<i32,_>("present_count").ok(),
        "body":r.get::<String,_>("body"),"author":r.try_get::<String,_>("author").ok() })).collect();
    let fatture_out: Vec<Value> = fatture.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"number":r.get::<String,_>("number"),"amount":r.get::<f64,_>("amount"),
        "issue_date":r.get::<chrono::NaiveDate,_>("issue_date").to_string(),"status":r.get::<String,_>("status"),
        "note":r.try_get::<String,_>("note").ok(),"sal_id":r.try_get::<Uuid,_>("sal_id").ok() })).collect();
    let labor_rate: f64 = c.try_get::<f64,_>("labor_rate").ok().unwrap_or(0.0);
    let labor_cost = (labor_hours*labor_rate*100.0).round()/100.0;
    let total_cost = ((labor_cost+materials_cost)*100.0).round()/100.0;
    let budget_v = c.try_get::<f64,_>("budget").ok();
    let variance = budget_v.map(|b| ((b-total_cost)*100.0).round()/100.0);
    let costi_out: Vec<Value> = costi.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"description":r.get::<String,_>("description"),"category":r.get::<String,_>("category"),
        "amount":r.get::<f64,_>("amount"),"cost_date":r.get::<chrono::NaiveDate,_>("cost_date").to_string(),
        "author":r.try_get::<String,_>("author").ok() })).collect();
    let team_out: Vec<Value> = teams.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"members":r.get::<i32,_>("members")})).collect();
    let pres_out: Vec<Value> = pres.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"email":r.get::<String,_>("email"),
        "clock_in":r.get::<chrono::DateTime<chrono::Utc>,_>("clock_in"),
        "clock_out":r.try_get::<chrono::DateTime<chrono::Utc>,_>("clock_out").ok() })).collect();
    let mut deps_map: std::collections::HashMap<Uuid,Vec<Uuid>> = std::collections::HashMap::new();
    for r in &deprows { deps_map.entry(r.get::<Uuid,_>("lav_id")).or_default().push(r.get::<Uuid,_>("depends_on_id")); }
    let lav_out: Vec<Value> = lav.iter().map(|r| { let lid=r.get::<Uuid,_>("id"); json!({
        "id":lid,"name":r.get::<String,_>("name"),"position":r.get::<i32,_>("position"),
        "weight":r.get::<f64,_>("weight"),"progress_pct":r.get::<i32,_>("progress_pct"),
        "status":r.get::<String,_>("status"),"note":r.try_get::<String,_>("note").ok(),
        "start_date":r.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":r.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "depends_on":r.try_get::<Uuid,_>("depends_on").ok(),"deps":deps_map.get(&lid).cloned().unwrap_or_default(),
        "late":r.try_get::<bool,_>("late").ok().unwrap_or(false) }) }).collect();
    let sal_out: Vec<Value> = sal.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"sal_date":r.get::<chrono::NaiveDate,_>("sal_date").to_string(),
        "pct":r.get::<i32,_>("pct"),"amount":r.try_get::<f64,_>("amount").ok(),
        "note":r.try_get::<String,_>("note").ok(),"author":r.try_get::<String,_>("author").ok() })).collect();
    let sh_out: Vec<Value> = shifts.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"email":r.get::<String,_>("email"),"state":r.get::<String,_>("state"),
        "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
        "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at") })).collect();
    Ok(Json(json!({
        "id":c.get::<Uuid,_>("id"),"code":c.get::<String,_>("code"),"name":c.get::<String,_>("name"),
        "status":c.get::<String,_>("status"),"client_name":c.try_get::<String,_>("client_name").ok(),
        "site":c.try_get::<String,_>("site").ok(),"site_id":c.try_get::<Uuid,_>("site_id").ok(),
        "address":c.try_get::<String,_>("address").ok(),
        "lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),
        "site_lat":c.try_get::<f64,_>("site_lat").ok(),"site_lon":c.try_get::<f64,_>("site_lon").ok(),
        "start_date":c.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":c.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "budget":c.try_get::<f64,_>("budget").ok(),"note":c.try_get::<String,_>("note").ok(),
        "client_id":c.try_get::<Uuid,_>("client_id").ok(),"cig":c.try_get::<String,_>("cig").ok(),"cup":c.try_get::<String,_>("cup").ok(),
        "progress":c.get::<i32,_>("progress"),
        "lavorazioni":lav_out,"sal":sal_out,"shifts":sh_out,
        "teams":team_out,"presenze":pres_out,"my_open":my_open.is_some(),
        "labor_rate":labor_rate,"costi":costi_out,
        "cost":{"labor_hours":(labor_hours*100.0).round()/100.0,"labor_cost":labor_cost,"materials_cost":materials_cost,
                "total_cost":total_cost,"budget":budget_v,"variance":variance},
        "diario":diario_out,"fatture":fatture_out,"attachments":att_out
    })))
}
pub async fn update_commessa(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<CommessaPatch>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query(
        "UPDATE app.commesse SET name=COALESCE($2,name),status=COALESCE($3,status),client_name=COALESCE($4,client_name),
         start_date=COALESCE($5,start_date),end_date=COALESCE($6,end_date),budget_amount=COALESCE($7::numeric,budget_amount),
         note=COALESCE($8,note),labor_cost_hour=COALESCE($9::numeric,labor_cost_hour),
         client_id=COALESCE($10,client_id),cig=COALESCE($11,cig),cup=COALESCE($12,cup),
         address=COALESCE($13,address),lat=COALESCE($14,lat),lon=COALESCE($15,lon) WHERE id=$1")
        .bind(id).bind(&b.name).bind(&b.status).bind(&b.client_name)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(b.budget_amount).bind(&b.note).bind(b.labor_cost_hour)
        .bind(b.client_id).bind(&b.cig).bind(&b.cup)
        .bind(&b.address).bind(b.lat).bind(b.lon)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "commessa non trovata")); }
    audit(&st, tid, &s.email, "commessa_update", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_commessa(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commesse WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_delete", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct LavorazioneReq { pub name: String, #[serde(default)] pub weight: Option<f64>, #[serde(default)] pub position: Option<i32>, #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String> }
pub async fn create_lavorazione(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<LavorazioneReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let (sd,ed)=(parse_date_opt(&b.start_date),parse_date_opt(&b.end_date));
    if let (Some(a),Some(z))=(sd,ed) { if z<a { return Err(err(StatusCode::BAD_REQUEST, "fine prima dell'inizio")); } }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let pos = match b.position { Some(p)=>p, None=> sqlx::query_scalar::<_,Option<i32>>("SELECT max(position) FROM app.commessa_lavorazioni WHERE commessa_id=$1").bind(cid).fetch_one(&mut *tx).await.map_err(ise)?.unwrap_or(0)+1 };
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.commessa_lavorazioni(tenant_id,commessa_id,name,position,weight,note,start_date,end_date) VALUES($1,$2,$3,$4,COALESCE($5::numeric,1),$6,$7,$8) RETURNING id")
        .bind(tid).bind(cid).bind(b.name.trim()).bind(pos).bind(b.weight).bind(&b.note).bind(sd).bind(ed)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)]
pub struct LavorazionePatch { #[serde(default)] pub name: Option<String>, #[serde(default)] pub weight: Option<f64>, #[serde(default)] pub progress_pct: Option<i32>, #[serde(default)] pub status: Option<String>, #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String>, #[serde(default)] pub depends_on: Option<Uuid>, #[serde(default)] pub clear_dep: Option<bool>,
    #[serde(default)] pub deps: Option<Vec<Uuid>> }
pub async fn update_lavorazione(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<LavorazionePatch>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if let Some(p)=b.progress_pct { if !(0..=100).contains(&p) { return Err(err(StatusCode::BAD_REQUEST, "avanzamento 0..100")); } }
    if b.depends_on==Some(id) { return Err(err(StatusCode::BAD_REQUEST, "una lavorazione non può dipendere da sé stessa")); }
    let (sd,ed)=(parse_date_opt(&b.start_date),parse_date_opt(&b.end_date));
    let clear_dep=b.clear_dep.unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // se status='fatto' porta a 100; date e dipendenza con COALESCE (clear_dep azzera la dipendenza)
    let n = sqlx::query(
        "UPDATE app.commessa_lavorazioni SET name=COALESCE($2,name),weight=COALESCE($3::numeric,weight),
         progress_pct=CASE WHEN $5='fatto' THEN 100 ELSE COALESCE($4,progress_pct) END,
         status=COALESCE($5,status),note=COALESCE($6,note),
         start_date=COALESCE($7,start_date),end_date=COALESCE($8,end_date),
         depends_on=CASE WHEN $9 THEN NULL ELSE COALESCE($10,depends_on) END
         WHERE id=$1")
        .bind(id).bind(&b.name).bind(b.weight).bind(b.progress_pct).bind(&b.status).bind(&b.note)
        .bind(sd).bind(ed).bind(clear_dep).bind(b.depends_on)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "lavorazione non trovata")); }
    // dipendenze MULTIPLE: se passate, sostituiscono il set (no self, no 2-cicli)
    if let Some(deps)=&b.deps {
        let cm: Uuid = sqlx::query_scalar("SELECT commessa_id FROM app.commessa_lavorazioni WHERE id=$1").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
        sqlx::query("DELETE FROM app.commessa_lav_deps WHERE lav_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
        for dep in deps {
            if *dep==id { continue; }
            // la dipendenza deve appartenere alla stessa commessa
            let ok: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.commessa_lavorazioni WHERE id=$1 AND commessa_id=$2").bind(dep).bind(cm).fetch_optional(&mut *tx).await.map_err(ise)?;
            if ok.is_none() { continue; }
            // evita ciclo diretto: dep non deve già dipendere da id
            let cyc: Option<(Uuid,Uuid)> = sqlx::query_as("SELECT lav_id,depends_on_id FROM app.commessa_lav_deps WHERE lav_id=$1 AND depends_on_id=$2").bind(dep).bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
            if cyc.is_some() { return Err(err(StatusCode::BAD_REQUEST, "dipendenza ciclica non ammessa")); }
            sqlx::query("INSERT INTO app.commessa_lav_deps(tenant_id,lav_id,depends_on_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(tid).bind(id).bind(dep).execute(&mut *tx).await.map_err(ise)?;
        }
        // mantieni la colonna singola allineata (prima dipendenza) per retrocompatibilità
        let first = deps.iter().find(|d| **d!=id).copied();
        sqlx::query("UPDATE app.commessa_lavorazioni SET depends_on=$2 WHERE id=$1").bind(id).bind(first).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_lavorazione(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_lavorazioni WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct SalReq { pub pct: i32, #[serde(default)] pub amount: Option<f64>, #[serde(default)] pub note: Option<String>, #[serde(default)] pub sal_date: Option<String> }
pub async fn create_sal(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<SalReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if !(0..=100).contains(&b.pct) { return Err(err(StatusCode::BAD_REQUEST, "pct 0..100")); }
    let d = parse_date_opt(&b.sal_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.commessa_sal(tenant_id,commessa_id,sal_date,pct,amount,note,created_by) VALUES($1,$2,$3,$4,$5::numeric,$6,$7) RETURNING id")
        .bind(tid).bind(cid).bind(d).bind(b.pct).bind(b.amount).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_sal", json!({"commessa":cid,"pct":b.pct})).await;
    Ok(Json(json!({"id":id})))
}
// ----- Cantieri C1: squadre assegnate + presenze (timbrature) -----
#[derive(Deserialize)] pub struct AssignTeamReq { pub team_id: Uuid }
pub async fn commessa_assign_team(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<AssignTeamReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.commessa_teams(tenant_id,commessa_id,team_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(cid).bind(b.team_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn commessa_remove_team(State(st): State<AppState>, h: HeaderMap, Path((cid,team)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_teams WHERE commessa_id=$1 AND team_id=$2").bind(cid).bind(team).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct ClockReq { #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64> }
pub async fn commessa_clock_in(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<ClockReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // niente doppia timbratura aperta sulla stessa commessa
    let open: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.commessa_presenze WHERE commessa_id=$1 AND user_id=$2 AND clock_out IS NULL")
        .bind(cid).bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    if open.is_some() { return Err(err(StatusCode::CONFLICT, "hai già una presenza aperta su questa commessa")); }
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.commessa_presenze(tenant_id,commessa_id,user_id,lat,lon,source) VALUES($1,$2,$3,$4,$5,'manuale') RETURNING id")
        .bind(tid).bind(cid).bind(s.user_id).bind(b.lat).bind(b.lon).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn commessa_clock_out(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.commessa_presenze SET clock_out=now() WHERE commessa_id=$1 AND user_id=$2 AND clock_out IS NULL")
        .bind(cid).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::BAD_REQUEST, "nessuna presenza aperta")); }
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri C3: costi di commessa -----
#[derive(Deserialize)]
pub struct CostoReq { pub description: String, #[serde(default)] pub category: Option<String>, pub amount: f64, #[serde(default)] pub cost_date: Option<String> }
pub async fn add_commessa_costo(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<CostoReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.description.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "descrizione richiesta")); }
    let d = parse_date_opt(&b.cost_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.commessa_costi(tenant_id,commessa_id,description,category,amount,cost_date,created_by) VALUES($1,$2,$3,COALESCE($4,'materiale'),$5::numeric,$6,$7) RETURNING id")
        .bind(tid).bind(cid).bind(b.description.trim()).bind(&b.category).bind(b.amount).bind(d).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_commessa_costo(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_costi WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri C4: giornale di cantiere -----
#[derive(Deserialize)]
pub struct DiarioReq { pub body: String, #[serde(default)] pub diary_date: Option<String>, #[serde(default)] pub weather: Option<String>, #[serde(default)] pub present_count: Option<i32> }
pub async fn add_commessa_diario(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<DiarioReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    if b.body.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "testo richiesto")); }
    let d = parse_date_opt(&b.diary_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.commessa_diario(tenant_id,commessa_id,diary_date,weather,present_count,body,created_by) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(cid).bind(d).bind(&b.weather).bind(b.present_count).bind(b.body.trim()).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_commessa_diario(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_diario WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri C5: fatturazione da SAL -----
#[derive(Deserialize)]
pub struct FatturaReq { pub number: String, pub amount: f64, #[serde(default)] pub sal_id: Option<Uuid>, #[serde(default)] pub issue_date: Option<String>, #[serde(default)] pub note: Option<String> }
pub async fn add_commessa_fattura(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<FatturaReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.number.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "numero richiesto")); }
    let d = parse_date_opt(&b.issue_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.commessa_fatture(tenant_id,commessa_id,sal_id,number,amount,issue_date,note,created_by) VALUES($1,$2,$3,$4,$5::numeric,$6,$7,$8) RETURNING id")
        .bind(tid).bind(cid).bind(b.sal_id).bind(b.number.trim()).bind(b.amount).bind(d).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_fattura", json!({"commessa":cid,"number":b.number,"amount":b.amount})).await;
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)] pub struct FatturaPatch { pub status: String }
pub async fn update_commessa_fattura(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<FatturaPatch>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if !["emessa","pagata","annullata"].contains(&b.status.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "stato non valido")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.commessa_fatture SET status=$2 WHERE id=$1").bind(id).bind(&b.status).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "fattura non trovata")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_commessa_fattura(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_fatture WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri R3: allegati di commessa (bytea, self-contained) -----
pub async fn commessa_attachment_add(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>, body: axum::body::Bytes) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    if body.len() < 4 { return Err(err(StatusCode::BAD_REQUEST, "file vuoto")); }
    if body.len() > 10_000_000 { return Err(err(StatusCode::BAD_REQUEST, "file troppo grande (max 10MB)")); }
    let kind = match q.get("kind").map(|s| s.as_str()) { Some("doc") => "doc", _ => "photo" };
    let ek = q.get("entity_kind").map(|x| x.trim()).filter(|x| ["diario","fattura","sal","commessa"].contains(x)).unwrap_or("commessa");
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
    let filename = q.get("filename").map(|x| x.trim().to_string());
    let caption = q.get("caption").map(|x| x.trim().to_string());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.commessa_attachments(tenant_id,commessa_id,entity_kind,kind,filename,content_type,caption,data,created_by) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(cid).bind(ek).bind(kind).bind(&filename).bind(&ct).bind(&caption).bind(body.as_ref()).bind(&s.email)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id")})))
}
pub async fn commessa_attachment_get(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    let row = sqlx::query("SELECT data,content_type FROM app.commessa_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    let data: Vec<u8> = row.get("data"); let ct: String = row.get("content_type");
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "private, max-age=3600".to_string())], data).into_response())
}
pub async fn commessa_attachment_del(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let owner: Option<String> = sqlx::query_scalar("SELECT created_by FROM app.commessa_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    if !is_planner(&s) && owner.as_deref() != Some(s.email.as_str()) { return Err(err(StatusCode::FORBIDDEN, "non autorizzato")); }
    sqlx::query("DELETE FROM app.commessa_attachments WHERE id=$1 AND tenant_id=$2").bind(aid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct ShiftPlanAction { pub action: String } // confirm|cancel
pub async fn shift_plan_action(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ShiftPlanAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = match b.action.as_str() {
        // il lavoratore conferma il PROPRIO turno; il pianificatore può confermare per conto di chiunque
        "confirm" => if is_planner(&s) {
            sqlx::query("UPDATE app.shift_plans SET state='confermato' WHERE id=$1 AND state='pianificato'")
                .bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected()
        } else {
            sqlx::query("UPDATE app.shift_plans SET state='confermato' WHERE id=$1 AND user_id=$2 AND state='pianificato'")
                .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected()
        },
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
pub async fn update_oncall(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OncallReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.oncall SET user_id=$2,starts_at=$3,ends_at=$4,note=$5 WHERE id=$1")
        .bind(id).bind(b.user_id).bind(a).bind(z).bind(&b.note).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "reperibilita' inesistente")); }
    Ok(Json(json!({"ok":true})))
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
        // DEDUP esplicito (parità YWAlert): se esiste già un incidente APERTO con la stessa chiave
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
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'notify',$2)")
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
    if !module_active(&st, tid, "iot").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo IoT non abilitato"));
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
    if !has_cap(&s,"devices.manage") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"devices.manage") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT g.id,g.name,g.kind,g.geom,g.site_id,g.floorplan_id,g.color,g.alert_on,si.name AS site
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
        "floorplan_id":r.try_get::<Uuid,_>("floorplan_id").ok(),"alert_on":r.try_get::<String,_>("alert_on").ok(),"device_ids":devs,"etypes":evs }) }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct GeofenceAlertReq { pub alert_on: Option<String> }
/// Imposta la direzione di allarme della geofence: null (nessuno), 'enter' (zona vietata), 'exit' (area sicura).
pub async fn set_geofence_alert(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GeofenceAlertReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    let tid = s.tenant_id.unwrap();
    let v = b.alert_on.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty());
    if let Some(x)=v { if !["enter","exit"].contains(&x) { return Err(err(StatusCode::BAD_REQUEST,"alert_on: enter|exit|null")); } }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.geofences SET alert_on=$2 WHERE id=$1").bind(id).bind(v).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND,"geofence inesistente")); }
    audit(&st, tid, &s.email, "set_geofence_alert", json!({"id":id,"alert_on":v})).await;
    Ok(Json(json!({"ok":true})))
}
fn clean_hex_color(c: Option<&str>) -> String {
    let c = c.unwrap_or("").trim();
    let ok = c.starts_with('#') && (c.len()==7 || c.len()==4) && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if ok { c.to_string() } else { "#16a34a".to_string() }
}
#[derive(Deserialize)]
pub struct GeofenceReq { pub name: String, pub kind: String, pub geom: Value, pub site_id: Option<Uuid>, pub floorplan_id: Option<Uuid>, pub color: Option<String>,
    #[serde(default)] pub alert_on: Option<String> }
pub async fn create_geofence(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GeofenceReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let alert_on = b.alert_on.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty());
    if let Some(x)=alert_on { if !["enter","exit"].contains(&x) { return Err(err(StatusCode::BAD_REQUEST, "alert_on: enter|exit|null")); } }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.geofences(tenant_id,name,kind,geom,site_id,floorplan_id,color,alert_on) VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.kind).bind(&b.geom).bind(b.site_id).bind(b.floorplan_id).bind(&color).bind(alert_on)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "nome gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id");
    audit(&st, tid, &s.email, "create_geofence", json!({"name":b.name,"kind":b.kind})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_geofence(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
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
    if !has_cap(&s,"incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
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
    if !has_cap(&s,"incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
        "geofence_ingresso" => "Ingresso in zona vietata (geofence)",
        "geofence_uscita" => "Uscita da area sicura (geofence)",
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
           AND i.state='aperto'   -- mappa: SOLO allarmi non gestiti; gestiti/chiusi restano nello storico/report
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
    if !has_cap(&s,"incidents.view") {
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
    if !has_cap(&s,"incidents.view") {
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
    if !module_active(&st, tid, "iot").await {
        return Err(err(StatusCode::FORBIDDEN, "modulo IoT non abilitato"));
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

// ===== IoT nativo F1: telemetria time-series (native, nessun ThingsBoard) =====
/// Ingest telemetria nativa (token device-bound): body {ts?, values:{key:val,...}} oppure {key:val,...}.
pub async fn ingest_telemetry(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>, Json(b): Json<Value>) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| q.get("token").cloned()).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, &token).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    let mut mod_ok=false; for m in DEVICE_MODS { if module_active(&st, tid, m).await { mod_ok=true; break; } }
    if !mod_ok { return Err(err(StatusCode::FORBIDDEN, "nessun modulo dispositivi attivo")); }
    // device: dal token (bound) o da external id in query
    let dev_id: Uuid = match cred.device_id {
        Some(d) => d,
        None => { let ext = q.get("id").or_else(|| q.get("deviceid")).ok_or_else(|| err(StatusCode::BAD_REQUEST, "device id mancante"))?;
            sqlx::query_scalar("SELECT id FROM app.devices WHERE tenant_id=$1 AND external_id=$2").bind(tid).bind(ext)
                .fetch_optional(&st.owner_pool).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "device non registrato"))? }
    };
    let ts = b.get("ts").and_then(|v| v.as_str()).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_else(|| st.clock.now());
    let values = b.get("values").and_then(|v| v.as_object()).cloned()
        .unwrap_or_else(|| b.as_object().cloned().unwrap_or_default().into_iter().filter(|(k,_)| k!="ts").collect());
    if values.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nessuna telemetria (values vuoto)")); }
    let attrs = b.get("attributes").and_then(|v| v.as_object()).cloned();
    let (n, alert, incident) = persist_telemetry(&st, tid, dev_id, ts, &values, attrs.as_ref()).await?;
    Ok(Json(json!({"ok":true,"device_id":dev_id,"stored":n,"alert":alert,"incident":incident})))
}

/// Cuore telemetria riusabile: normalizza (mappa device -> metrica canonica + unità + trasformazione),
/// persiste la serie, aggiorna l'ultimo stato, valuta le soglie (su chiavi canoniche + grezze).
/// Usato dall'ingest HTTP nativo e dai webhook di protocollo (LoRaWAN/TTN, ...).
/// Ritorna (record salvati, alert IoT, incidente sicurezza). L'alert IoT (vista separata) è SEMPRE
/// alzato se una soglia scatta; l'incidente di sicurezza SOLO in coesistenza col modulo Lone Worker.
pub async fn persist_telemetry(st: &AppState, tid: Uuid, dev_id: Uuid,
    ts: chrono::DateTime<chrono::Utc>, values: &serde_json::Map<String,Value>,
    attrs: Option<&serde_json::Map<String,Value>>) -> Result<(usize, Value, Value),(StatusCode,String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // NORMALIZZAZIONE: mappa del device (chiave grezza -> metrica canonica + trasformazione lineare)
    // e unità delle metriche canoniche. Senza mappa il valore resta grezzo (retro-compatibile).
    let map_rows = sqlx::query("SELECT raw_key,metric,scale,offset_v FROM app.device_metric_map WHERE device_id=$1")
        .bind(dev_id).fetch_all(&mut *tx).await.map_err(ise)?;
    let mut kmap: std::collections::HashMap<String,(String,f64,f64)> = std::collections::HashMap::new();
    for r in &map_rows { kmap.insert(r.get::<String,_>("raw_key"), (r.get::<String,_>("metric"), r.get::<f64,_>("scale"), r.get::<f64,_>("offset_v"))); }
    let mut units: std::collections::HashMap<String,String> = std::collections::HashMap::new();
    if !kmap.is_empty() {
        for r in &sqlx::query("SELECT metric,unit FROM app.iot_metrics").fetch_all(&mut *tx).await.map_err(ise)? {
            units.insert(r.get::<String,_>("metric"), r.get::<String,_>("unit"));
        }
    }
    let mut n=0; let mut last = serde_json::Map::new();
    // canon: payload per le regole/soglie con le CHIAVI CANONICHE (oltre a quelle grezze) -> una regola
    // su 'temperature' scatta a prescindere dalla chiave grezza del device ('t'/'temp'/...).
    let mut canon = values.clone();
    for (k,v) in values {
        let (mut num, s) = match v { Value::Number(x)=>(x.as_f64(), None), Value::Bool(x)=>(None, Some(x.to_string())), Value::String(x)=>(x.parse::<f64>().ok().map(Some).unwrap_or(None), Some(x.clone())), _=>(None, Some(v.to_string())) };
        let (metric, unit): (Option<String>, Option<String>) = match kmap.get(k) {
            Some((m, sc, off)) => {
                if let Some(x) = num { num = Some(sc*x + off); }
                let cv = match num { Some(x) => serde_json::Number::from_f64(x).map(Value::Number).unwrap_or_else(|| v.clone()), None => v.clone() };
                canon.insert(m.clone(), cv);
                (Some(m.clone()), units.get(m).cloned())
            }
            None => (None, None),
        };
        sqlx::query("INSERT INTO app.device_telemetry(tenant_id,device_id,key,ts,num,str,metric,unit) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(tid).bind(dev_id).bind(k).bind(ts).bind(num).bind(&s).bind(&metric).bind(&unit).execute(&mut *tx).await.map_err(ise)?;
        last.insert(k.clone(), v.clone()); n+=1;
    }
    if let Some(attrs) = attrs {
        for (k,v) in attrs {
            sqlx::query("INSERT INTO app.device_attributes(tenant_id,device_id,scope,key,value) VALUES($1,$2,'client',$3,$4)
                         ON CONFLICT (device_id,scope,key) DO UPDATE SET value=EXCLUDED.value,updated_at=now()")
                .bind(tid).bind(dev_id).bind(k).bind(v).execute(&mut *tx).await.map_err(ise)?;
        }
    }
    // last_telemetry_at sull'orologio controllabile (= ts): il watchdog offline lo confronta con
    // lo stesso orologio, quindi è coerente e pilotabile in test (TEST_CLOCK).
    sqlx::query("UPDATE app.devices SET last_attributes=COALESCE(last_attributes,'{}'::jsonb)||$2::jsonb, last_telemetry_at=$3, updated_at=now() WHERE id=$1")
        .bind(dev_id).bind(Value::Object(last)).bind(ts).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    publish_event(st, TenantEvent{tenant_id:tid,payload:json!({"type":"telemetry","device_id":dev_id})}).await;
    // RIPRISTINO: il device ha parlato -> se c'era un alert offline aperto, chiudilo e segnala il rientro.
    iot_recover_online(st, tid, dev_id).await;
    let mut incident = json!(null);
    let mut alert = json!(null);
    let payload = Value::Object(canon.clone());
    if let Some(etype) = eval_sensor_rules(st, tid, &payload).await {
        // 1) alert IoT in VISTA SEPARATA (sempre): è il mestiere IoT, non la sicurezza del lavoratore.
        alert = raise_iot_soglia_alert(st, tid, dev_id, &etype, &canon).await;
        // 2) COESISTENZA: se il tenant ha anche Lone Worker, la soglia alza ANCHE l'incidente di
        //    sicurezza (centrale + escalation), come prima. Un tenant puramente IoT non lo fa.
        if module_active(st, tid, "loneworker").await {
            let ext: String = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE id=$1").bind(dev_id)
                .fetch_optional(&st.owner_pool).await.map_err(ise)?.unwrap_or_default();
            let ev = format!("iot-telem-{}", Uuid::new_v4());
            let r = persist_ingest(st, tid, Normalized{
                source: "iot".into(), source_event: ev.clone(), idem: ev.clone(), etype,
                device_external_id: Some(ext), site_name: None, lat: None, lon: None, accuracy_m: None,
                correlation: ev, payload, indoor: false, beacon_uid: None, zone: None }).await?;
            incident = r.0;
        }
    }
    Ok((n, alert, incident))
}

/// Webhook LoRaWAN/TTN (The Things Network v3) e compatibili (ChirpStack): uplink JSON ->
/// payload decodificato -> pipeline telemetria nativa (stessa normalizzazione dell'HTTP).
/// Token device-bound o `?id=`/`end_device_ids.device_id`. CSRF-exempt (macchina).
pub async fn ingest_lorawan(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>, Json(b): Json<Value>) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        .or_else(|| q.get("token").cloned()).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, &token).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    let mut mod_ok=false; for m in DEVICE_MODS { if module_active(&st, tid, m).await { mod_ok=true; break; } }
    if !mod_ok { return Err(err(StatusCode::FORBIDDEN, "nessun modulo dispositivi attivo")); }
    // payload decodificato: TTN v3 = uplink_message.decoded_payload; ChirpStack = object/objectJSON.
    let decoded = b.pointer("/uplink_message/decoded_payload")
        .or_else(|| b.get("object")).or_else(|| b.get("objectJSON")).or_else(|| b.get("decoded_payload"))
        .and_then(|v| v.as_object()).cloned()
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "payload decodificato assente (serve un decoder a monte)"))?;
    if decoded.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "payload decodificato vuoto")); }
    // device: token bound, oppure ?id=, oppure end_device_ids.device_id (TTN)
    let dev_id: Uuid = match cred.device_id {
        Some(d) => d,
        None => {
            let ext = q.get("id").cloned()
                .or_else(|| b.pointer("/end_device_ids/device_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "device id mancante"))?;
            sqlx::query_scalar("SELECT id FROM app.devices WHERE tenant_id=$1 AND external_id=$2").bind(tid).bind(&ext)
                .fetch_optional(&st.owner_pool).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "device non registrato"))?
        }
    };
    let ts = b.pointer("/uplink_message/received_at").and_then(|v| v.as_str())
        .or_else(|| b.get("time").and_then(|v| v.as_str()))
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| st.clock.now());
    let (n, alert, incident) = persist_telemetry(&st, tid, dev_id, ts, &decoded, None).await?;
    Ok(Json(json!({"ok":true,"device_id":dev_id,"stored":n,"alert":alert,"incident":incident})))
}
pub async fn iot_telemetry_latest(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"incidents.view") && !has_cap(&s,"config.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT DISTINCT ON (key) key,ts,num,str,metric,unit FROM app.device_telemetry WHERE device_id=$1 ORDER BY key,ts DESC").bind(dev).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"key":r.get::<String,_>("key"),
        "ts":r.get::<chrono::DateTime<chrono::Utc>,_>("ts").to_rfc3339(),
        "num":r.try_get::<f64,_>("num").ok(),"str":r.try_get::<String,_>("str").ok(),
        "metric":r.try_get::<String,_>("metric").ok(),"unit":r.try_get::<String,_>("unit").ok() })).collect();
    Ok(Json(json!(out)))
}

// ---- IoT Fase 1: metriche canoniche + mappa di normalizzazione per-device ----
/// Catalogo delle metriche canoniche (etichetta IT + unità). Serve ai picker della UI.
pub async fn iot_metrics_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    let rows = sqlx::query("SELECT metric,label,unit,kind,category,sort FROM app.iot_metrics ORDER BY sort,metric")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"metric":r.get::<String,_>("metric"),"label":r.get::<String,_>("label"),
        "unit":r.get::<String,_>("unit"),"kind":r.get::<String,_>("kind"),"category":r.get::<String,_>("category")})).collect();
    Ok(Json(json!(out)))
}
/// Mappa di normalizzazione del device: chiave grezza -> metrica canonica (+ trasformazione lineare).
pub async fn device_map_get(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"config.view") && !has_cap(&s,"iot.manage") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT m.raw_key,m.metric,m.scale,m.offset_v,im.label,im.unit
        FROM app.device_metric_map m JOIN app.iot_metrics im ON im.metric=m.metric
        WHERE m.device_id=$1 ORDER BY m.raw_key").bind(dev).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"raw_key":r.get::<String,_>("raw_key"),"metric":r.get::<String,_>("metric"),
        "scale":r.get::<f64,_>("scale"),"offset":r.get::<f64,_>("offset_v"),
        "label":r.get::<String,_>("label"),"unit":r.get::<String,_>("unit")})).collect();
    Ok(Json(json!({"map":out})))
}
#[derive(Deserialize)]
pub struct MapRow { pub raw_key: String, pub metric: String, #[serde(default)] pub scale: Option<f64>, #[serde(default)] pub offset: Option<f64> }
#[derive(Deserialize)]
pub struct MapReq { pub rows: Vec<MapRow> }
/// Sostituisce la mappa del device (set completo). Normalizza la telemetria futura.
pub async fn device_map_set(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>, Json(b): Json<MapReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?; require_cap(&s, "iot.manage")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // il device deve esistere nel tenant (RLS); valida le metriche canoniche contro il catalogo
    let dev_ok: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.devices WHERE id=$1)").bind(dev).fetch_one(&mut *tx).await.map_err(ise)?;
    if !dev_ok { return Err(err(StatusCode::NOT_FOUND, "device inesistente")); }
    sqlx::query("DELETE FROM app.device_metric_map WHERE device_id=$1").bind(dev).execute(&mut *tx).await.map_err(ise)?;
    for row in &b.rows {
        if row.raw_key.trim().is_empty() || row.metric.trim().is_empty() { continue; }
        let known: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.iot_metrics WHERE metric=$1)").bind(&row.metric).fetch_one(&mut *tx).await.map_err(ise)?;
        if !known { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, &format!("metrica sconosciuta: {}", row.metric))); }
        sqlx::query("INSERT INTO app.device_metric_map(tenant_id,device_id,raw_key,metric,scale,offset_v) VALUES($1,$2,$3,$4,$5,$6)
                     ON CONFLICT (device_id,raw_key) DO UPDATE SET metric=EXCLUDED.metric,scale=EXCLUDED.scale,offset_v=EXCLUDED.offset_v")
            .bind(tid).bind(dev).bind(row.raw_key.trim()).bind(row.metric.trim()).bind(row.scale.unwrap_or(1.0)).bind(row.offset.unwrap_or(0.0))
            .execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "device_map_set", json!({"device":dev,"n":b.rows.len()})).await;
    Ok(Json(json!({"ok":true,"n":b.rows.len()})))
}

// ---- IoT Fase 2: Blueprint (profilo-lavoro) + attivazione zero-touch ----
/// Catalogo dei blueprint: "che lavoro deve fare il device". L'admin ne sceglie uno e l'attivazione
/// configura tutto da sola (device + token + normalizzazione + soglie + istruzioni di collegamento).
pub async fn iot_blueprints_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    let rows = sqlx::query("SELECT code,name,job,category,protocol,key_map,default_rules,icon FROM app.iot_blueprints ORDER BY sort,code")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    // sensori reali del catalogo agganciati ai blueprint: così il picker "per un lavoro" propone l'hardware.
    let models = sqlx::query("SELECT model_code,manufacturer,model_name,protocol,recommended,blueprint FROM app.device_models WHERE blueprint IS NOT NULL ORDER BY recommended DESC, manufacturer, model_name")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut out: Vec<Value> = Vec::new();
    for r in &rows {
        let code: String = r.get("code");
        let devs: Vec<Value> = models.iter().filter(|m| m.get::<Option<String>,_>("blueprint").as_deref()==Some(code.as_str()))
            .map(|m| json!({"model_code":m.get::<String,_>("model_code"),"manufacturer":m.get::<String,_>("manufacturer"),
                "model_name":m.get::<String,_>("model_name"),"protocol":m.get::<String,_>("protocol"),"recommended":m.get::<bool,_>("recommended")})).collect();
        out.push(json!({
            "code":code,"name":r.get::<String,_>("name"),"job":r.get::<String,_>("job"),
            "category":r.get::<String,_>("category"),"protocol":r.get::<String,_>("protocol"),"icon":r.get::<String,_>("icon"),
            "metrics": r.get::<Value,_>("key_map").as_array().map(|a| a.iter().filter_map(|x| x.get("metric").and_then(|m| m.as_str())).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>()).unwrap_or_default(),
            "rules": r.get::<Value,_>("default_rules"), "models": devs }));
    }
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct ActivateReq { #[serde(default)] pub blueprint: Option<String>, #[serde(default)] pub model_code: Option<String>, pub external_id: String, #[serde(default)] pub label: Option<String>, #[serde(default)] pub site_id: Option<Uuid> }
/// Attivazione zero-touch: da un blueprint crea il device + token device-bound + mappa di
/// normalizzazione + soglie di default, e restituisce le istruzioni di collegamento per il protocollo.
/// "L'admin dice quale device per quale lavoro; tutto il resto si sistema."
pub async fn iot_activate(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ActivateReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "iot").await { return Err(err(StatusCode::FORBIDDEN, "modulo IoT non attivo")); }
    let ext = b.external_id.trim();
    if ext.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "identificativo dispositivo richiesto")); }
    // blueprint: esplicito, oppure derivato dal modello di sensore scelto ("quale device per quale lavoro").
    let model_code = b.model_code.clone().filter(|s| !s.trim().is_empty());
    let bp_code = match b.blueprint.clone().filter(|s| !s.trim().is_empty()) {
        Some(c) => c,
        None => match &model_code {
            Some(mc) => sqlx::query_scalar::<_,Option<String>>("SELECT blueprint FROM app.device_models WHERE model_code=$1")
                .bind(mc).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten()
                .ok_or_else(|| err(StatusCode::BAD_REQUEST, "il modello scelto non ha un profilo-lavoro associato: indica un blueprint"))?,
            None => return Err(err(StatusCode::BAD_REQUEST, "serve un blueprint o un modello di sensore")),
        }
    };
    let bp = sqlx::query("SELECT name,protocol,key_map,default_rules,offline_after_s FROM app.iot_blueprints WHERE code=$1")
        .bind(&bp_code).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "blueprint sconosciuto"))?;
    let bp_name: String = bp.get("name");
    let protocol: String = bp.get("protocol");
    let key_map: Value = bp.get("key_map");
    let default_rules: Value = bp.get("default_rules");
    let offline_after_s: Option<i32> = bp.try_get("offline_after_s").ok().flatten();

    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // device (source 'iot'); external_id unico per tenant
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.devices WHERE tenant_id=$1 AND external_id=$2)")
        .bind(tid).bind(ext).fetch_one(&mut *tx).await.map_err(ise)?;
    if exists { return Err(err(StatusCode::CONFLICT, "esiste già un dispositivo con questo identificativo")); }
    let label = b.label.clone().unwrap_or_else(|| bp_name.clone());
    let cfg = json!({"blueprint": bp_code, "protocol": protocol, "model_code": model_code});
    let dev_id: Uuid = sqlx::query_scalar("INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,config,offline_after_s,model_code) VALUES($1,$2,'iot',$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(ext).bind(&label).bind(b.site_id).bind(&cfg).bind(offline_after_s).bind(&model_code).fetch_one(&mut *tx).await.map_err(ise)?;
    // mappa di normalizzazione dal blueprint
    if let Some(arr) = key_map.as_array() {
        for m in arr {
            let (Some(rk), Some(mt)) = (m.get("raw_key").and_then(|x| x.as_str()), m.get("metric").and_then(|x| x.as_str())) else { continue };
            let sc = m.get("scale").and_then(|x| x.as_f64()).unwrap_or(1.0);
            let off = m.get("offset").and_then(|x| x.as_f64()).unwrap_or(0.0);
            sqlx::query("INSERT INTO app.device_metric_map(tenant_id,device_id,raw_key,metric,scale,offset_v) VALUES($1,$2,$3,$4,$5,$6)
                         ON CONFLICT (device_id,raw_key) DO UPDATE SET metric=EXCLUDED.metric,scale=EXCLUDED.scale,offset_v=EXCLUDED.offset_v")
                .bind(tid).bind(dev_id).bind(rk).bind(mt).bind(sc).bind(off).execute(&mut *tx).await.map_err(ise)?;
        }
    }
    // soglie di default (idempotenti: non duplica una regola uguale già presente nel tenant)
    let mut rules_added = 0;
    if let Some(arr) = default_rules.as_array() {
        for r in arr {
            let metric = r.get("metric").and_then(|x| x.as_str()).unwrap_or("");
            let op = r.get("op").and_then(|x| x.as_str()).unwrap_or("");
            let etype = r.get("etype").and_then(|x| x.as_str()).unwrap_or("");
            if metric.is_empty() || !RULE_OPS.contains(&op) || !RULE_ETYPES.contains(&etype) { continue; }
            let threshold = r.get("threshold").and_then(|x| x.as_f64());
            let label_r = r.get("label").and_then(|x| x.as_str());
            let dup: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.sensor_rules WHERE metric=$1 AND op=$2 AND etype=$3 AND threshold IS NOT DISTINCT FROM $4)")
                .bind(metric).bind(op).bind(etype).bind(threshold).fetch_one(&mut *tx).await.map_err(ise)?;
            if dup { continue; }
            sqlx::query("INSERT INTO app.sensor_rules(tenant_id,metric,op,threshold,etype,label,sort) VALUES($1,$2,$3,$4,$5,$6,100)")
                .bind(tid).bind(metric).bind(op).bind(threshold).bind(etype).bind(label_r).execute(&mut *tx).await.map_err(ise)?;
            rules_added += 1;
        }
    }
    tx.commit().await.map_err(ise)?;
    // token device-bound (segreto mostrato UNA volta)
    let secret = format!("iot-{}", &auth::new_token()[..24]);
    let hash = hash_token(&secret);
    let handle = format!("iot-{}", &secret[4..12]);
    sqlx::query("INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id,label) VALUES($1,$2,$3,'iot',$4,$5)")
        .bind(&handle).bind(&hash).bind(tid).bind(dev_id).bind(&label).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "iot_activate", json!({"blueprint":bp_code,"model_code":model_code,"device":dev_id,"external_id":ext})).await;
    // istruzioni di collegamento per protocollo (il segreto va nell'URL/credenziale)
    let connect = match protocol.as_str() {
        "lorawan" => json!({"protocol":"lorawan",
            "webhook_url": format!("/api/ingress/lorawan?token={secret}"),
            "note":"Imposta questo URL come webhook di uplink su TTN/ChirpStack (serve un decoder a monte che produca il payload decodificato)."}),
        "mqtt" => json!({"protocol":"mqtt",
            "host": "il server YouWorkforce", "port": 1883,
            "topic": format!("yw/{tid}/{ext}/telemetry"),
            "command_topic": format!("yw/{tid}/{ext}/command"),
            "username": secret,
            "body_example": {"values":{"co2":650,"temperature":22.4}},
            "note":"Broker MQTT nativo YW (porta 1883). Connetti con il token come username; pubblica JSON {values:{...}} sul topic telemetria. Sottoscrivi il topic comandi per ricevere i comandi in coda."}),
        _ => json!({"protocol":"http",
            "url": format!("/api/ingress/telemetry?token={secret}"),
            "method":"POST",
            "body_example": {"values":{"temperature":4.2}},
            "note":"Invia una POST JSON a questo URL ad ogni misura."}),
    };
    Ok(Json(json!({"ok":true,"device_id":dev_id,"external_id":ext,"blueprint":bp_code,"model_code":model_code,
        "token":secret,"rules_added":rules_added,"connect":connect,
        "note":"Copia il token adesso: non sarà più mostrato."})))
}
pub async fn iot_telemetry_series(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"incidents.view") && !has_cap(&s,"config.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let key = q.get("key").cloned();
    let from = q.get("from").and_then(|x| chrono::DateTime::parse_from_rfc3339(x).ok()).map(|d| d.with_timezone(&chrono::Utc));
    let to = q.get("to").and_then(|x| chrono::DateTime::parse_from_rfc3339(x).ok()).map(|d| d.with_timezone(&chrono::Utc));
    let lim: i64 = q.get("limit").and_then(|x| x.parse().ok()).unwrap_or(500).min(5000);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // filtro per `key`: accetta sia la chiave grezza sia la metrica canonica (uniformità viste)
    let rows = sqlx::query("SELECT key,ts,num,str,metric,unit FROM app.device_telemetry WHERE device_id=$1 \
            AND ($2::text IS NULL OR key=$2 OR metric=$2) AND ($3::timestamptz IS NULL OR ts>=$3) AND ($4::timestamptz IS NULL OR ts<=$4) \
            ORDER BY ts DESC LIMIT $5")
        .bind(dev).bind(&key).bind(from).bind(to).bind(lim).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"key":r.get::<String,_>("key"),
        "ts":r.get::<chrono::DateTime<chrono::Utc>,_>("ts").to_rfc3339(),
        "num":r.try_get::<f64,_>("num").ok(),"str":r.try_get::<String,_>("str").ok(),
        "metric":r.try_get::<String,_>("metric").ok(),"unit":r.try_get::<String,_>("unit").ok() })).collect();
    Ok(Json(json!(out)))
}
// ===== IoT nativo F2: attributi (client/shared/server) + comandi/RPC =====
pub async fn iot_attributes_get(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"incidents.view") && !has_cap(&s,"config.view") && !has_cap(&s,"iot.manage") { return Err(err(StatusCode::FORBIDDEN,"ruolo non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT scope,key,value,updated_at FROM app.device_attributes WHERE device_id=$1 ORDER BY scope,key").bind(dev).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut client=serde_json::Map::new(); let mut shared=serde_json::Map::new(); let mut server=serde_json::Map::new();
    for r in &rows { let sc:String=r.get("scope"); let k:String=r.get("key"); let v:Value=r.get("value");
        match sc.as_str() { "shared"=>{shared.insert(k,v);}, "server"=>{server.insert(k,v);}, _=>{client.insert(k,v);} } }
    Ok(Json(json!({"client":client,"shared":shared,"server":server})))
}
#[derive(Deserialize)] pub struct AttrSetReq { pub scope: String, pub values: serde_json::Map<String,Value> }
pub async fn iot_attributes_set(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>, Json(b): Json<AttrSetReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "iot").await?; require_cap(&s,"iot.manage")?;
    if !["client","shared","server"].contains(&b.scope.as_str()) { return Err(err(StatusCode::BAD_REQUEST,"scope client|shared|server")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    for (k,v) in &b.values {
        sqlx::query("INSERT INTO app.device_attributes(tenant_id,device_id,scope,key,value) VALUES($1,$2,$3,$4,$5)
                     ON CONFLICT (device_id,scope,key) DO UPDATE SET value=EXCLUDED.value,updated_at=now()")
            .bind(tid).bind(dev).bind(&b.scope).bind(k).bind(v).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "iot_attributes_set", json!({"device":dev,"scope":b.scope})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct RpcReq { pub method: String, #[serde(default)] pub params: Option<Value> }
pub async fn iot_command_post(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>, Json(b): Json<RpcReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "iot").await?; require_cap(&s,"devices.manage")?;
    if b.method.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST,"metodo richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let drow = sqlx::query("SELECT source,external_id FROM app.devices WHERE id=$1").bind(dev).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"device inesistente"))?;
    let (src, ext): (String, String) = (drow.get("source"), drow.try_get::<String,_>("external_id").unwrap_or_default());
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.iot_commands(tenant_id,device_id,method,params,created_by) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(dev).bind(b.method.trim()).bind(&b.params).bind(s.user_id).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // consegna immediata se il device è su edge Teltonika; altrimenti resta in coda (il device fa pull)
    let mut status="pending"; let mut result: Option<String> = None;
    if src=="teltonika" && !ext.is_empty() {
        match crate::teltonika::send_command(&ext, b.method.trim()).await {
            Ok(r)=>{ status="done"; result=Some(r); }, Err(e)=>{ status="error"; result=Some(e); }
        }
        let mut tx2 = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        sqlx::query("UPDATE app.iot_commands SET status=$2,result=$3,sent_at=now() WHERE id=$1").bind(id).bind(status).bind(&result).execute(&mut *tx2).await.map_err(ise)?;
        tx2.commit().await.map_err(ise)?;
    }
    Ok(Json(json!({"id":id,"status":status,"result":result})))
}
pub async fn iot_commands_list(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,method,params,status,result,created_at,sent_at FROM app.iot_commands WHERE device_id=$1 ORDER BY created_at DESC LIMIT 50").bind(dev).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"method":r.get::<String,_>("method"),
        "params":r.try_get::<Value,_>("params").ok(),"status":r.get::<String,_>("status"),"result":r.try_get::<String,_>("result").ok(),
        "created_at":r.get::<chrono::DateTime<chrono::Utc>,_>("created_at").to_rfc3339() })).collect();
    Ok(Json(json!(out)))
}
/// Pull del device (via token): config shared da applicare + comandi pending (li marca 'sent').
pub async fn iot_device_pull(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let token = h.get("x-ingest-token").and_then(|v| v.to_str().ok()).map(|s| s.to_string()).or_else(|| q.get("token").cloned())
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing ingest token"))?;
    let cred = resolve_ingest_token(&st, &token).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad ingest token"))?;
    let tid = cred.tenant_id;
    let dev_id: Uuid = match cred.device_id { Some(d)=>d, None=>{ let ext=q.get("id").ok_or_else(|| err(StatusCode::BAD_REQUEST,"device id mancante"))?;
        sqlx::query_scalar("SELECT id FROM app.devices WHERE tenant_id=$1 AND external_id=$2").bind(tid).bind(ext).fetch_optional(&st.owner_pool).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"device"))? } };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let sh = sqlx::query("SELECT key,value FROM app.device_attributes WHERE device_id=$1 AND scope='shared'").bind(dev_id).fetch_all(&mut *tx).await.map_err(ise)?;
    let cmds = sqlx::query("SELECT id,method,params FROM app.iot_commands WHERE device_id=$1 AND status='pending' ORDER BY created_at").bind(dev_id).fetch_all(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.iot_commands SET status='sent',sent_at=now() WHERE device_id=$1 AND status='pending'").bind(dev_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut shared=serde_json::Map::new(); for r in &sh { shared.insert(r.get::<String,_>("key"), r.get::<Value,_>("value")); }
    let cmd_v: Vec<Value> = cmds.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"method":r.get::<String,_>("method"),"params":r.try_get::<Value,_>("params").ok()})).collect();
    Ok(Json(json!({"shared":shared,"commands":cmd_v})))
}
// ---- MQTT nativo (broker YW porta 1883): helper d'ingest/comandi riusabili ----
// Il broker vive in mqtt.rs; qui stanno le funzioni che toccano DEVICE_MODS/persist_telemetry
// (private a questo modulo) e che impongono l'ISOLAMENTO TENANT a partire dal topic.
/// Risolve token+device da una CONNECT MQTT. Ritorna (tenant, source, device-bound?).
/// Rifiuta se il token è invalido o nessun modulo dispositivi è attivo.
pub async fn mqtt_auth(st: &AppState, token: &str) -> Result<(Uuid, Option<Uuid>), String> {
    let cred = resolve_ingest_token(st, token).await.ok_or("credenziale di ingest non valida")?;
    let tid = cred.tenant_id;
    let mut mod_ok=false; for m in DEVICE_MODS { if module_active(st, tid, m).await { mod_ok=true; break; } }
    if !mod_ok { return Err("nessun modulo dispositivi attivo per questo tenant".into()); }
    Ok((tid, cred.device_id))
}
/// Dal topic `yw/{tid}/{ext}/...` ricava l'external_id, imponendo che il tenant del topic
/// combaci con quello autenticato (ISOLAMENTO). Ritorna None se il topic non è nel formato YW.
fn mqtt_topic_ext(topic: &str, tid: Uuid) -> Result<Option<String>, String> {
    let p: Vec<&str> = topic.split('/').collect();
    if p.len() >= 3 && p[0] == "yw" {
        if p[1] != tid.to_string() { return Err("il tenant del topic non combacia con la credenziale".into()); }
        return Ok(Some(p[2].to_string()));
    }
    Ok(None)
}
/// Risolve il device dal token (bound) o dall'external_id del topic, con difesa in profondità:
/// se il token è vincolato a un device E il topic porta un external_id, DEVONO combaciare.
async fn mqtt_device(st: &AppState, tid: Uuid, bound: Option<Uuid>, ext_topic: Option<&str>) -> Result<Uuid, String> {
    match bound {
        Some(d) => {
            if let Some(ext) = ext_topic {
                if let Some(be) = bound_device_external(st, d).await {
                    if be != ext { return Err("l'external_id del topic non combacia con il device del token".into()); }
                }
            }
            Ok(d)
        }
        None => {
            let ext = ext_topic.ok_or("device non identificabile: token non vincolato e topic senza external_id")?;
            sqlx::query_scalar("SELECT id FROM app.devices WHERE tenant_id=$1 AND external_id=$2").bind(tid).bind(ext)
                .fetch_optional(&st.owner_pool).await.map_err(|e| e.to_string())?.ok_or_else(|| "device non registrato".into())
        }
    }
}
/// Ingest di una PUBLISH MQTT: topic `yw/{tid}/{ext}/telemetry`, payload JSON `{values:{...}}`.
/// Stessa pipeline di normalizzazione+soglie dell'HTTP. Ritorna i record salvati.
pub async fn mqtt_ingest(st: &AppState, tid: Uuid, bound: Option<Uuid>, topic: &str, payload: &[u8]) -> Result<usize, String> {
    let ext = mqtt_topic_ext(topic, tid)?;
    let dev_id = mqtt_device(st, tid, bound, ext.as_deref()).await?;
    let b: Value = serde_json::from_slice(payload).map_err(|_| "payload non è JSON valido".to_string())?;
    let ts = b.get("ts").and_then(|v| v.as_str()).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_else(|| st.clock.now());
    let values = b.get("values").and_then(|v| v.as_object()).cloned()
        .unwrap_or_else(|| b.as_object().cloned().unwrap_or_default().into_iter().filter(|(k,_)| k!="ts").collect());
    if values.is_empty() { return Err("nessuna telemetria (values vuoto)".into()); }
    let attrs = b.get("attributes").and_then(|v| v.as_object()).cloned();
    let (n, _alert, _incident) = persist_telemetry(st, tid, dev_id, ts, &values, attrs.as_ref()).await.map_err(|(_,m)| m)?;
    Ok(n)
}
/// Flush dei comandi pending per il device sottoscritto (`yw/{tid}/{ext}/command`): li ritorna
/// come JSON e li marca 'sent' (stessa semantica del pull HTTP). Il broker li pubblica al client.
pub async fn mqtt_flush_commands(st: &AppState, tid: Uuid, bound: Option<Uuid>, topic: &str) -> Result<Vec<Value>, String> {
    let ext = mqtt_topic_ext(topic, tid)?;
    let dev_id = mqtt_device(st, tid, bound, ext.as_deref()).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(|e| e.to_string())?;
    let cmds = sqlx::query("SELECT id,method,params FROM app.iot_commands WHERE device_id=$1 AND status='pending' ORDER BY created_at")
        .bind(dev_id).fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
    if !cmds.is_empty() {
        sqlx::query("UPDATE app.iot_commands SET status='sent',sent_at=now() WHERE device_id=$1 AND status='pending'")
            .bind(dev_id).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(cmds.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"method":r.get::<String,_>("method"),"params":r.try_get::<Value,_>("params").ok()})).collect())
}

// ================= IoT Fase 3 — alert IoT in VISTA SEPARATA =================
// Gli alert di processo IoT (soglia / offline / ripristino) vivono in app.iot_alerts, con un ciclo
// di vita PROPRIO e distinto dagli incidenti di sicurezza (SOS/man_down + escalation). Notifica
// nativa = SSE 'iot_alert' + record persistente consultabile dalla vista "Alert IoT".
fn iot_severity_for(etype: &str) -> &'static str {
    if etype.ends_with("_critico") || matches!(etype, "sensore_critico"|"perdita_alimentazione") { "critical" }
    else if etype.contains("batteria") { "warning" }
    else if etype == "telemetria" { "info" }
    else { "warning" }
}
/// Alza (o aggiorna se già aperto) un alert IoT. owner_pool + tenant_id ESPLICITO (bypassa RLS:
/// usato sia dal path utente sia dal watchdog di sistema -> isolamento garantito dal filtro esplicito).
/// Dedup su (device,kind,metric) non-chiuso: aggiorna valore+occorrenze senza nuova notifica
/// (anti-spam). Ritorna Some(alert) se NUOVO (e notifica via SSE), None se duplicato.
#[allow(clippy::too_many_arguments)]
pub async fn raise_iot_alert(st: &AppState, tid: Uuid, device_id: Uuid, kind: &str, severity: &str,
    metric: Option<&str>, value: Option<f64>, threshold: Option<f64>, rule_etype: Option<&str>, message: &str) -> Option<Value> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM app.iot_alerts WHERE tenant_id=$1 AND device_id=$2 AND kind=$3 AND state<>'chiuso' AND metric IS NOT DISTINCT FROM $4 ORDER BY opened_at DESC LIMIT 1")
        .bind(tid).bind(device_id).bind(kind).bind(metric).fetch_optional(&st.owner_pool).await.ok().flatten();
    if let Some(id) = existing {
        let _ = sqlx::query("UPDATE app.iot_alerts SET occurrences=occurrences+1, last_value=COALESCE($2,last_value), updated_at=now() WHERE id=$1")
            .bind(id).bind(value).execute(&st.owner_pool).await;
        return None;
    }
    let row = sqlx::query(
        "INSERT INTO app.iot_alerts(tenant_id,device_id,kind,severity,metric,last_value,threshold,rule_etype,message)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(device_id).bind(kind).bind(severity).bind(metric).bind(value).bind(threshold).bind(rule_etype).bind(message)
        .fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let id: Uuid = row.get("id");
    let alert = json!({"id":id,"device_id":device_id,"kind":kind,"severity":severity,"metric":metric,"value":value,"threshold":threshold,"message":message});
    publish_event(st, TenantEvent{tenant_id:tid, payload:json!({"type":"iot_alert","alert":alert})}).await;
    Some(alert)
}
/// Soglia telemetria -> alert IoT. Ritrova la regola che ha fatto scattare (metric/threshold/label)
/// e costruisce un alert leggibile. Ritorna l'alert (o {deduplicated} / null) per la risposta d'ingest.
async fn raise_iot_soglia_alert(st: &AppState, tid: Uuid, device_id: Uuid, etype: &str, canon: &serde_json::Map<String,Value>) -> Value {
    let rules = sqlx::query("SELECT metric,op,threshold,label FROM app.sensor_rules WHERE tenant_id=$1 AND etype=$2 AND enabled=true ORDER BY sort,created_at")
        .bind(tid).bind(etype).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut metric=String::new(); let mut threshold:Option<f64>=None; let mut value:Option<f64>=None; let mut label:Option<String>=None;
    for r in &rules {
        let m: String = r.get("metric"); let op: String = r.get("op"); let thr: Option<f64> = r.try_get("threshold").ok();
        if let Some(v) = canon.get(&m) { if rule_matches(&op, thr, v) { value=v.as_f64(); metric=m; threshold=thr; label=r.try_get("label").ok(); break; } }
    }
    let severity = iot_severity_for(etype);
    let msg = label.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let ml = if metric.is_empty() { etype.to_string() } else { metric.clone() };
        match (value, threshold) { (Some(v),Some(t)) => format!("{ml} fuori soglia ({v} vs {t})"), _ => format!("{ml} fuori soglia") }
    });
    let m_opt = (!metric.is_empty()).then_some(metric.as_str());
    match raise_iot_alert(st, tid, device_id, "soglia", severity, m_opt, value, threshold, Some(etype), &msg).await {
        Some(a) => a, None => json!({"deduplicated":true}),
    }
}
/// Ripristino: il device ha ripreso a parlare -> chiude gli alert offline aperti e segnala il rientro.
pub async fn iot_recover_online(st: &AppState, tid: Uuid, device_id: Uuid) {
    let closed = sqlx::query_scalar::<_,Uuid>(
        "UPDATE app.iot_alerts SET state='chiuso', closed_at=now(), closed_by='watchdog', updated_at=now()
         WHERE tenant_id=$1 AND device_id=$2 AND kind='offline' AND state<>'chiuso' RETURNING id")
        .bind(tid).bind(device_id).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if !closed.is_empty() {
        let name = sqlx::query_scalar::<_,Option<String>>("SELECT COALESCE(label,external_id) FROM app.devices WHERE id=$1")
            .bind(device_id).fetch_optional(&st.owner_pool).await.ok().flatten().flatten().unwrap_or_else(|| "device".into());
        let _ = raise_iot_alert(st, tid, device_id, "ripristino", "info", None, None, None, None, &format!("{name} è tornato online")).await;
    }
}
/// Watchdog offline: per ogni device IoT con soglia di silenzio superata alza un alert 'offline'.
/// Orologio controllabile (coerente con last_telemetry_at, scritto dallo stesso orologio) -> testabile.
pub async fn iot_offline_watchdog(st: &AppState) -> anyhow::Result<()> {
    let now = st.clock.now();
    let rows = sqlx::query(
        "SELECT d.id,d.tenant_id,d.offline_after_s,d.last_telemetry_at,COALESCE(d.label,d.external_id) AS name
         FROM app.devices d JOIN app.tenant_modules m ON m.tenant_id=d.tenant_id AND m.module_code='iot' AND m.state='attivo'
         WHERE d.offline_after_s IS NOT NULL AND d.offline_after_s>0 AND d.last_telemetry_at IS NOT NULL")
        .fetch_all(&st.owner_pool).await?;
    for r in rows {
        let after: i32 = r.get("offline_after_s");
        let last: chrono::DateTime<chrono::Utc> = r.get("last_telemetry_at");
        let secs = (now - last).num_seconds();
        if secs <= after as i64 { continue; }
        let tid: Uuid = r.get("tenant_id"); let dev: Uuid = r.get("id");
        let name: Option<String> = r.try_get("name").ok();
        let msg = format!("{} silenzioso da {}s (soglia {}s)", name.unwrap_or_else(|| "device".into()), secs, after);
        let _ = raise_iot_alert(st, tid, dev, "offline", "warning", None, None, Some(after as f64), Some("offline"), &msg).await;
    }
    Ok(())
}
/// Hook di test (solo TEST_CLOCK): forza una passata del watchdog offline per e2e deterministici.
pub async fn test_iot_watchdog(State(st): State<AppState>) -> R {
    if !st.clock.test_mode { return Err(err(StatusCode::FORBIDDEN,"test clock disabled")); }
    iot_offline_watchdog(&st).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---- API vista "Alert IoT" (separata dalla Centrale sicurezza) ----
#[derive(Deserialize)] pub struct IotAlertQuery { pub state: Option<String>, pub device: Option<Uuid>, pub severity: Option<String> }
pub async fn iot_alerts_list(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<IotAlertQuery>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, CONNECTOR_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT a.id,a.device_id,COALESCE(d.label,d.external_id) AS device_name,a.kind,a.severity,a.state,
                a.metric,a.last_value,a.threshold,a.rule_etype,a.message,a.occurrences,a.opened_at,a.updated_at,
                a.ack_at,a.ack_by,a.closed_at,a.closed_by
         FROM app.iot_alerts a LEFT JOIN app.devices d ON d.id=a.device_id
         WHERE ($1::text IS NULL OR a.state=$1) AND ($2::uuid IS NULL OR a.device_id=$2) AND ($3::text IS NULL OR a.severity=$3)
         ORDER BY (a.state<>'chiuso') DESC, a.opened_at DESC LIMIT 500")
        .bind(&q.state).bind(q.device).bind(&q.severity).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "device_id": r.get::<Uuid,_>("device_id"),
        "device_name": r.try_get::<Option<String>,_>("device_name").ok().flatten(),
        "kind": r.get::<String,_>("kind"), "severity": r.get::<String,_>("severity"), "state": r.get::<String,_>("state"),
        "metric": r.try_get::<Option<String>,_>("metric").ok().flatten(),
        "last_value": r.try_get::<Option<f64>,_>("last_value").ok().flatten(),
        "threshold": r.try_get::<Option<f64>,_>("threshold").ok().flatten(),
        "rule_etype": r.try_get::<Option<String>,_>("rule_etype").ok().flatten(),
        "message": r.get::<String,_>("message"), "occurrences": r.get::<i32,_>("occurrences"),
        "opened_at": r.get::<chrono::DateTime<chrono::Utc>,_>("opened_at"),
        "updated_at": r.get::<chrono::DateTime<chrono::Utc>,_>("updated_at"),
        "ack_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("ack_at").ok().flatten(),
        "ack_by": r.try_get::<Option<String>,_>("ack_by").ok().flatten(),
        "closed_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("closed_at").ok().flatten(),
        "closed_by": r.try_get::<Option<String>,_>("closed_by").ok().flatten(),
    })).collect();
    Ok(Json(json!(out)))
}
pub async fn iot_alerts_summary(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, CONNECTOR_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = sqlx::query(
        "SELECT count(*) FILTER (WHERE state<>'chiuso') AS open,
                count(*) FILTER (WHERE state<>'chiuso' AND severity='critical') AS critical,
                count(*) FILTER (WHERE state<>'chiuso' AND severity='warning') AS warning,
                count(*) FILTER (WHERE state<>'chiuso' AND severity='info') AS info,
                count(*) FILTER (WHERE state='aperto') AS unacked
         FROM app.iot_alerts").fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({
        "open": r.get::<i64,_>("open"), "critical": r.get::<i64,_>("critical"),
        "warning": r.get::<i64,_>("warning"), "info": r.get::<i64,_>("info"), "unacked": r.get::<i64,_>("unacked") })))
}
pub async fn iot_alert_ack(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.iot_alerts SET state='preso_in_carico', ack_at=now(), ack_by=$2, updated_at=now() WHERE id=$1 AND state='aperto'")
        .bind(id).bind(&s.email).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "alert inesistente o non più aperto")); }
    audit(&st, tid, &s.email, "iot_alert_ack", json!({"id":id})).await;
    publish_event(&st, TenantEvent{tenant_id:tid, payload:json!({"type":"iot_alert_update","id":id,"state":"preso_in_carico"})}).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn iot_alert_close(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.iot_alerts SET state='chiuso', closed_at=now(), closed_by=$2, updated_at=now() WHERE id=$1 AND state<>'chiuso'")
        .bind(id).bind(&s.email).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "alert inesistente o già chiuso")); }
    audit(&st, tid, &s.email, "iot_alert_close", json!({"id":id})).await;
    publish_event(&st, TenantEvent{tenant_id:tid, payload:json!({"type":"iot_alert_update","id":id,"state":"chiuso"})}).await;
    Ok(Json(json!({"ok":true})))
}

// ---- API "Monitoraggio IoT" (Fase 4): stato flotta + ultimi valori + trend ----
// Overview aggregata dei device IoT: online/offline (last_telemetry_at vs watchdog), ultimi valori
// canonici (metrica+unità), alert aperti, e una mini-serie (spark) della metrica primaria.
pub async fn iot_monitor(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, CONNECTOR_MODS).await?;
    let now = st.clock.now();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let devs = sqlx::query(
        "SELECT d.id, COALESCE(d.label,d.external_id) AS name, d.external_id, d.last_telemetry_at, d.offline_after_s,
                s.name AS site_name
         FROM app.devices d LEFT JOIN app.sites s ON s.id=d.site_id
         WHERE d.source='iot' ORDER BY name")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    let mut out: Vec<Value> = Vec::new();
    let (mut n_online, mut n_offline, mut n_nowd, mut n_alerts) = (0i64,0i64,0i64,0i64);
    for d in &devs {
        let dev: Uuid = d.get("id");
        let last: Option<chrono::DateTime<chrono::Utc>> = d.try_get("last_telemetry_at").ok().flatten();
        let oas: Option<i32> = d.try_get("offline_after_s").ok().flatten();
        let secs = last.map(|t| (now - t).num_seconds());
        let stale_after = oas.unwrap_or(3600) as i64;
        let online = secs.map(|sx| sx <= stale_after).unwrap_or(false);
        if online { n_online+=1; } else { n_offline+=1; }
        if oas.is_none() { n_nowd+=1; }
        // ultimi valori canonici (una riga per metrica) + etichetta/unità/sort dal catalogo
        let latest = sqlx::query(
            "SELECT DISTINCT ON (t.metric) t.metric, t.num, t.unit, t.ts, m.label, m.sort
             FROM app.device_telemetry t JOIN app.iot_metrics m ON m.metric=t.metric
             WHERE t.device_id=$1 AND t.metric IS NOT NULL ORDER BY t.metric, t.ts DESC")
            .bind(dev).fetch_all(&mut *tx).await.map_err(ise)?;
        let mut metrics: Vec<(i32,Value)> = latest.iter().map(|r| {
            let sort: i32 = r.try_get("sort").unwrap_or(100);
            (sort, json!({
                "metric": r.get::<String,_>("metric"), "label": r.try_get::<Option<String>,_>("label").ok().flatten(),
                "unit": r.try_get::<Option<String>,_>("unit").ok().flatten(),
                "value": r.try_get::<Option<f64>,_>("num").ok().flatten(),
                "ts": r.get::<chrono::DateTime<chrono::Utc>,_>("ts") }))
        }).collect();
        metrics.sort_by_key(|(k,_)| *k);
        let primary = metrics.first().map(|(_,v)| v.get("metric").and_then(|x| x.as_str()).unwrap_or("").to_string());
        // spark (trend) della metrica primaria: ultimi 24 punti numerici, in ordine cronologico
        let mut spark: Vec<f64> = Vec::new();
        if let Some(pm) = &primary {
            let sp = sqlx::query_scalar::<_,Option<f64>>(
                "SELECT num FROM app.device_telemetry WHERE device_id=$1 AND metric=$2 AND num IS NOT NULL ORDER BY ts DESC LIMIT 24")
                .bind(dev).bind(pm).fetch_all(&mut *tx).await.map_err(ise)?;
            spark = sp.into_iter().flatten().collect(); spark.reverse();
        }
        let alerts: i64 = sqlx::query_scalar("SELECT count(*) FROM app.iot_alerts WHERE device_id=$1 AND state<>'chiuso'")
            .bind(dev).fetch_one(&mut *tx).await.map_err(ise)?;
        n_alerts += alerts;
        out.push(json!({
            "id": dev, "name": d.try_get::<Option<String>,_>("name").ok().flatten(),
            "external_id": d.get::<String,_>("external_id"),
            "site": d.try_get::<Option<String>,_>("site_name").ok().flatten(),
            "online": online, "has_watchdog": oas.is_some(), "offline_after_s": oas,
            "last_telemetry_at": last, "seconds_since": secs, "open_alerts": alerts,
            "metrics": metrics.into_iter().map(|(_,v)| v).collect::<Vec<_>>(), "primary": primary, "spark": spark }));
    }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({
        "summary": {"total": devs.len() as i64, "online": n_online, "offline": n_offline, "no_watchdog": n_nowd, "open_alerts": n_alerts},
        "devices": out })))
}

// ---- Teltonika: comando GPRS (codec12) verso un device connesso ----
#[derive(Deserialize)] pub struct GprsCmdReq { pub command: String }
pub async fn device_gprs_command(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GprsCmdReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    let tid = s.tenant_id.unwrap();
    if b.command.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "comando vuoto")); }
    let imei: Option<String> = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let imei = imei.ok_or_else(|| err(StatusCode::NOT_FOUND, "device inesistente"))?;
    match crate::teltonika::send_command(&imei, b.command.trim()).await {
        Ok(resp) => { audit(&st, tid, &s.email, "gprs_command", json!({"device":id,"command":b.command})).await;
                      Ok(Json(json!({"ok":true,"response":resp}))) }
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, &e)),
    }
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

/// Vigilanza: auto-punzonatura da posizione device. Se il device (dev_id) è assegnato a una
/// guardia con una ronda in corso, e la posizione cade nel raggio di un checkpoint GPS non ancora
/// punzonato, registra il punch automatico. Riusa il device-tracking nativo (OsmAnd/Teltonika/PWA).
pub async fn patrol_auto_punch(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, dev_id: Uuid, lat: f64, lon: f64) -> Result<u32, sqlx::Error> {
    let uid: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.devices WHERE id=$1").bind(dev_id).fetch_optional(&mut **tx).await?.flatten();
    let Some(uid) = uid else { return Ok(0); };
    let rnd = sqlx::query("SELECT id, route_id FROM app.patrol_rounds WHERE user_id=$1 AND status='in_corso' ORDER BY started_at DESC LIMIT 1")
        .bind(uid).fetch_optional(&mut **tx).await?;
    let Some(rnd) = rnd else { return Ok(0); };
    let (round_id, route_id): (Uuid, Uuid) = (rnd.get("id"), rnd.get("route_id"));
    let cps = sqlx::query("SELECT id,lat,lon,radius_m FROM app.patrol_checkpoints WHERE route_id=$1 AND kind='gps' AND lat IS NOT NULL AND lon IS NOT NULL \
            AND NOT EXISTS(SELECT 1 FROM app.patrol_punches p WHERE p.round_id=$2 AND p.checkpoint_id=patrol_checkpoints.id AND p.ok)")
        .bind(route_id).bind(round_id).fetch_all(&mut **tx).await?;
    let mut n = 0u32;
    for c in &cps {
        let (clat, clon, rad): (f64,f64,i32) = (c.get("lat"), c.get("lon"), c.get("radius_m"));
        if haversine_m(clat, clon, lat, lon) <= rad as f64 {
            let cid: Uuid = c.get("id");
            sqlx::query("INSERT INTO app.patrol_punches(tenant_id,round_id,checkpoint_id,user_id,method,lat,lon,ok,note) \
                VALUES($1,$2,$3,$4,'gps',$5,$6,true,'auto da dispositivo') \
                ON CONFLICT (round_id,checkpoint_id) DO UPDATE SET ok=true,method='gps',lat=EXCLUDED.lat,lon=EXCLUDED.lon,punched_at=now(),note='auto da dispositivo'")
                .bind(tid).bind(round_id).bind(cid).bind(uid).bind(lat).bind(lon).execute(&mut **tx).await?;
            n += 1;
        }
    }
    Ok(n)
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
    // Vigilanza: auto-punch se giri attivo e device legato a guardia in ronda
    if module_active(st, tid, "giri").await { let _ = patrol_auto_punch(&mut tx, tid, dev_id, lat, lon).await; }
    tx.commit().await?;
    // Fase B: al primo fix il device diventa "attivo"
    let _ = sqlx::query("UPDATE app.device_provisioning SET step='attivo',updated_at=now() WHERE device_id=$1 AND step<>'attivo'")
        .bind(dev_id).execute(&st.owner_pool).await;
    publish_event(st, TenantEvent{tenant_id:tid,payload:json!({"type":"device_position","device_id":dev_id,"lat":lat,"lon":lon})}).await;
    for (gid, inside) in &presence_changes {
        publish_event(st, TenantEvent{tenant_id:tid,payload:json!({"type":"presence","device_id":dev_id,"geofence_id":gid,"inside":inside})}).await;
        // allarme su attraversamento, se la geofence ha alert_on nella direzione del cambio
        geofence_crossing_alarm(st, tid, *gid, dev_id, *inside, Some(lat), Some(lon)).await;
    }
    Ok(json!({"ok":true,"device_id":dev_id,"deduplicated":dedup.is_none(),"presence_changes":presence_changes.len()}))
}
/// Se la geofence ha `alert_on` = direzione del cambio (enter=ingresso, exit=uscita), apre un
/// allarme (incidente + escalation) con gli etype configurati in geofence_events, oppure con
/// l'etype di default (geofence_ingresso/uscita). Idempotente per attraversamento.
pub async fn geofence_crossing_alarm(st: &AppState, tid: Uuid, gid: Uuid, dev_id: Uuid, inside: bool, lat: Option<f64>, lon: Option<f64>) {
    let g = match sqlx::query("SELECT name,alert_on FROM app.geofences WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() { Some(r)=>r, None=>return };
    let alert_on: Option<String> = g.try_get("alert_on").ok();
    let dir = if inside { "enter" } else { "exit" };
    if alert_on.as_deref() != Some(dir) { return; }
    let name: String = g.try_get("name").unwrap_or_default();
    let ext: String = match sqlx::query_scalar::<_,String>("SELECT external_id FROM app.devices WHERE id=$1").bind(dev_id)
        .fetch_optional(&st.owner_pool).await.ok().flatten() { Some(e)=>e, None=>return };
    let mut etypes: Vec<String> = sqlx::query_scalar("SELECT etype FROM app.geofence_events WHERE geofence_id=$1")
        .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if etypes.is_empty() { etypes.push(if inside { "geofence_ingresso".into() } else { "geofence_uscita".into() }); }
    for et in etypes {
        let corr = format!("gf-{}-{}-{}", gid, dir, Uuid::new_v4());
        let _ = persist_ingest(st, tid, Normalized{
            source: "geofence".into(), source_event: corr.clone(), idem: corr.clone(), etype: et,
            device_external_id: Some(ext.clone()), site_name: None,
            lat, lon, accuracy_m: None, correlation: corr,
            payload: json!({"geofence": name, "direction": dir}),
            indoor: false, beacon_uid: None, zone: Some(name.clone()),
        }).await;
    }
}

/// Storico posizioni di un device (trail/mappa). operatore/admin/auditor.
pub async fn list_device_positions(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
    if !has_cap(&s,"incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
pub async fn update_sensor_rule(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<SensorRuleReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    if b.metric.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "metrica richiesta")); }
    if !RULE_OPS.contains(&b.op.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "operatore non valido")); }
    if !RULE_ETYPES.contains(&b.etype.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "etype non ammesso")); }
    let numeric = !matches!(b.op.as_str(), "is_true"|"is_false");
    if numeric && b.threshold.is_none() { return Err(err(StatusCode::BAD_REQUEST, "soglia richiesta per questo operatore")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.sensor_rules SET metric=$2,op=$3,threshold=$4,etype=$5,label=$6,sort=COALESCE($7,sort) WHERE id=$1")
        .bind(id).bind(b.metric.trim()).bind(&b.op).bind(if numeric { b.threshold } else { None })
        .bind(&b.etype).bind(&b.label).bind(b.sort).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "regola inesistente")); }
    audit(&st, tid, &s.email, "sensor_rule_update", json!({"metric":b.metric,"op":b.op,"etype":b.etype})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_sensor_rule(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.sensor_rules WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "regola inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ----------------------------- incidents -----------------------------
pub async fn list_incidents(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?;
    let acl = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT i.id,i.etype,i.state,i.priority,i.correlation,i.opened_at,i.outcome,i.assigned_user,i.site_id,
                s.name AS site, s.address AS site_address, u.email AS assignee,
                i.attribution_uncertain, i.attribution_note,
                COALESCE(w.email, du.email) AS worker,
                COALESCE(d.label, d.external_id) AS device,
                e.indoor, e.beacon_uid, e.zone AS ev_zone, e.accuracy_m,
                COALESCE(e.lat, b.lat) AS lat, COALESCE(e.lon, b.lon) AS lon,
                b.zone_name, b.floor, b.floorplan_id, b.x AS fp_x, b.y AS fp_y, fp.name AS floorplan_name
         FROM app.incidents i
           LEFT JOIN app.sites s ON s.id=i.site_id
           LEFT JOIN app.users u ON u.id=i.assigned_user
           LEFT JOIN app.users w ON w.id=i.worker_user_id
           LEFT JOIN app.events e ON e.id=i.event_id
           LEFT JOIN app.devices d ON d.id=e.device_id
           LEFT JOIN app.users du ON du.id=d.assigned_user
           LEFT JOIN app.beacons b ON b.beacon_uid=e.beacon_uid AND b.tenant_id=i.tenant_id
           LEFT JOIN app.floorplans fp ON fp.id=b.floorplan_id
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
        "site_address": r.try_get::<String,_>("site_address").ok(),
        "assigned_user": r.try_get::<Uuid,_>("assigned_user").ok(),
        "assignee": r.try_get::<String,_>("assignee").ok(),
        "worker": r.try_get::<String,_>("worker").ok(),
        "device": r.try_get::<String,_>("device").ok(),
        // posizione: indoor (piantina/zona/beacon + x,y sul piano) e/o outdoor (lat/lon)
        "indoor": r.try_get::<bool,_>("indoor").unwrap_or(false),
        "zone": r.try_get::<String,_>("zone_name").ok().or_else(|| r.try_get::<String,_>("ev_zone").ok()),
        "beacon_uid": r.try_get::<String,_>("beacon_uid").ok(),
        "floor": r.try_get::<String,_>("floor").ok(),
        "floorplan_id": r.try_get::<Uuid,_>("floorplan_id").ok(),
        "floorplan_name": r.try_get::<String,_>("floorplan_name").ok(),
        "fp_x": r.try_get::<f64,_>("fp_x").ok(), "fp_y": r.try_get::<f64,_>("fp_y").ok(),
        "lat": r.try_get::<f64,_>("lat").ok(), "lon": r.try_get::<f64,_>("lon").ok(),
        "accuracy_m": r.try_get::<f64,_>("accuracy_m").ok(),
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
    if !has_cap(&s,"incidents.handle") {
        return Err(err(StatusCode::FORBIDDEN, "role cannot ack"));
    }
    guard_incident_site(&st, &s, iid).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE id=$1 AND tenant_id=$2 AND state='aperto'")
        .bind(iid).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
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
    if !has_cap(&s,"incidents.handle") {
        return Err(err(StatusCode::FORBIDDEN, "role cannot close"));
    }
    guard_incident_site(&st, &s, iid).await?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$2,closed_at=now(),version=version+1 WHERE id=$1 AND tenant_id=$3 AND state<>'chiuso' RETURNING correlation,etype")
        .bind(iid).bind(&b.outcome).bind(tid).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some(row) = row else { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"already closed")); };
    let correlation: String = row.get("correlation"); let etype: String = row.get("etype");
    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
        .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":b.outcome,"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    // P1 close-loop: chiedi a YWAlert di chiudere l'alert con lo stesso dedup (=correlation)
    sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'close',$2)")
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
    // Vigilanza: auto-punch anche dalla posizione inviata dalla PWA della guardia
    if module_active(&st, tid, "giri").await { let _ = patrol_auto_punch(&mut tx, tid, dev_id, lat, lon).await; }
    tx.commit().await.map_err(ise)?;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"device_position","device_id":dev_id,"lat":lat,"lon":lon})}).await;
    for (gid, inside) in &changes {
        publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"presence","device_id":dev_id,"geofence_id":gid,"inside":inside})}).await;
        geofence_crossing_alarm(&st, tid, *gid, dev_id, *inside, Some(lat), Some(lon)).await;
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
    sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'notify',$2)")
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
    // P1: il check-in è una primitiva di Sicurezza lavoratore. Senza loneworker attivo la Centrale
    // non esiste → un check-in creato non sarebbe mai sorvegliato. Gate esplicito (no black-hole).
    require_module(&st, tid, "loneworker").await?;
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
    require_module(&st, s.tenant_id.unwrap(), "loneworker").await?;
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
            // wall-clock: started_at e' il DEFAULT now() SQL (wall). Coerenza con lo storage.
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
    // P4: il diario di lavoro appartiene al perimetro Campo. Disponibile con workforce (turni) O
    // interventi (ordini) attivi, così non dipende da un singolo modulo Campo.
    require_any_module(&st, tid, &["workforce","interventi"]).await?;
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
    require_any_module(&st, s.tenant_id.unwrap(), &["workforce","interventi"]).await?;
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
pub(crate) fn ise<E: std::fmt::Display>(e: E) -> (StatusCode, String) { (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()) }
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
pub(crate) fn is_planner(s: &Session) -> bool { has_cap(s, "plan") }
pub(crate) fn is_approver(s: &Session) -> bool { has_cap(s, "approve") }

pub async fn module_active(st: &AppState, tid: Uuid, code: &str) -> bool {
    sqlx::query_scalar::<_, String>("SELECT state FROM app.tenant_modules WHERE tenant_id=$1 AND module_code=$2")
        .bind(tid).bind(code).fetch_optional(&st.owner_pool).await.ok().flatten()
        .map(|s| s == "attivo").unwrap_or(false)
}

/// Stato della configurazione del tenant: checklist "cosa manca per essere operativi", calcolata
/// dallo stato REALE del DB e condizionata ai moduli attivi. Unica fonte per il wizard di primo
/// avvio e per la checklist persistente. Solo tenant_admin.
pub async fn setup_status(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if s.role != "tenant_admin" { return Err(err(StatusCode::FORBIDDEN, "solo l'amministratore del tenant")); }
    let tid = s.tenant_id.unwrap();
    let pool = st.owner_pool.clone();
    let cnt = |sql: &'static str| {
        let pool = pool.clone();
        async move { sqlx::query_scalar::<_, i64>(sql).bind(tid).fetch_one(&pool).await.unwrap_or(0) }
    };
    // moduli attivi (per decidere quali item mostrare)
    let lw = module_active(&st, tid, "loneworker").await;
    let iot = module_active(&st, tid, "iot").await;
    let traccar = module_active(&st, tid, "traccar").await;
    let cantieri = module_active(&st, tid, "cantieri").await;
    let interventi = module_active(&st, tid, "interventi").await;
    let workforce = module_active(&st, tid, "workforce").await;
    let safety = lw || iot || traccar;        // moduli che generano allarmi/escalation
    let devices_mod = lw || iot || traccar;   // moduli con dispositivi
    let field = cantieri || interventi;       // lavoro sul campo con clienti

    let n_sites = cnt("SELECT count(*) FROM app.sites WHERE tenant_id=$1").await;
    let n_users = cnt("SELECT count(*) FROM app.users WHERE tenant_id=$1 AND anonymized_at IS NULL").await;
    let n_groups = cnt("SELECT count(*) FROM app.alarm_groups WHERE tenant_id=$1").await;
    let has_default_group: i64 = sqlx::query_scalar("SELECT count(*) FROM app.tenant_oncall_cfg WHERE tenant_id=$1 AND default_group_id IS NOT NULL")
        .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    let n_verified = cnt("SELECT count(*) FROM app.users WHERE tenant_id=$1 AND phone_verified_at IS NOT NULL").await;
    let n_routing = cnt("SELECT count(*) FROM app.alarm_routing WHERE tenant_id=$1").await;
    let n_rules = cnt("SELECT count(*) FROM app.sensor_rules WHERE tenant_id=$1").await;
    let n_devices = cnt("SELECT count(*) FROM app.devices WHERE tenant_id=$1").await;
    let n_clients = cnt("SELECT count(*) FROM app.clients WHERE tenant_id=$1").await;

    let mut items: Vec<Value> = vec![];
    let mut push = |key:&str, show:bool, done:bool, title:&str, why:&str, detail:String, link:&str, cta:&str| {
        if show { items.push(json!({"key":key,"done":done,"title":title,"why":why,"detail":detail,"link":link,"cta":cta})); }
    };
    push("sites", true, n_sites>0, "Crea le sedi",
        "Le sedi sono i luoghi di lavoro a cui colleghi dispositivi, turni e allarmi.",
        format!("{n_sites} sedi"), "anagrafiche", "Aggiungi una sede");
    push("users", true, n_users>1, "Invita utenti e assegna i ruoli",
        "Le persone del team accedono secondo il ruolo (operatore, pianificatore, lavoratore…).",
        format!("{n_users} utenti"), "utenti-ruoli", "Invita utenti");
    push("groups", safety, n_groups>0 && has_default_group>0, "Configura la reperibilità",
        "Senza un gruppo on-call predefinito, gli allarmi non raggiungono nessuno.",
        if n_groups==0 {"nessun gruppo".into()} else if has_default_group==0 {"gruppo senza default on-call".into()} else {format!("{n_groups} gruppi")},
        "gruppi", "Crea un gruppo");
    push("contacts", safety, n_verified>0, "Verifica i recapiti",
        "SMS e chiamate di escalation partono solo verso telefoni verificati.",
        format!("{n_verified} recapiti verificati"), "turni", "Verifica recapiti");
    push("routing", lw, n_routing>0, "Instrada gli allarmi",
        "Definisci a quale gruppo va ogni tipo di allarme (SOS, uomo a terra…).",
        format!("{n_routing} regole di instradamento"), "trigger", "Aggiungi instradamento");
    push("rules", iot, n_rules>0, "Imposta le regole dei sensori",
        "Le soglie decidono quando un dato IoT diventa un incidente.",
        format!("{n_rules} regole sensore"), "trigger", "Aggiungi regola");
    push("devices", devices_mod, n_devices>0, "Collega i dispositivi",
        "Badge, tracker GPS o sensori IoT: la procedura guidata ti accompagna.",
        format!("{n_devices} dispositivi"), "dispositivi", "Aggiungi dispositivo");
    push("clients", field, n_clients>0, "Aggiungi i clienti",
        "I committenti di commesse e interventi: servono per collegarli al lavoro.",
        format!("{n_clients} clienti"), "clienti", "Aggiungi cliente");
    let _ = workforce; // (workforce usa gli stessi item: utenti/sedi/turni)

    let total = items.len() as i64;
    let done = items.iter().filter(|i| i["done"].as_bool().unwrap_or(false)).count() as i64;
    Ok(Json(json!({"ready": done==total, "done":done, "total":total, "items":items})))
}

/// "Cosa fare adesso": azioni operative in sospeso, tagliate sul RUOLO dell'utente, calcolate dai
/// dati reali. Serve a far atterrare ognuno sul proprio lavoro senza cercarlo. Ogni ruolo tenant.
pub async fn home_tasks(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let role = s.role.as_str();
    let uid = s.user_id;
    let pool = st.owner_pool.clone();
    let c1 = |sql: &'static str| { let p = pool.clone(); async move {
        sqlx::query_scalar::<_, i64>(sql).bind(tid).fetch_one(&p).await.unwrap_or(0) } };
    let mut items: Vec<Value> = vec![];

    // Allarmi da prendere in carico — ruoli di centrale/sicurezza
    if matches!(role, "operatore"|"responsabile"|"tenant_admin"|"reperibile") {
        let n = c1("SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state='aperto'").await;
        if n > 0 {
            let link = if role == "reperibile" { "miei-allarmi" } else { "centrale" };
            items.push(json!({"title":"Allarmi da prendere in carico","detail":format!("{n} aperti"),"link":link,"tone":"bad"}));
        }
    }
    // Pianificazione — interventi attivi + turni da confermare
    if matches!(role, "pianificatore"|"tenant_admin") {
        let o = c1("SELECT count(*) FROM app.work_orders WHERE tenant_id=$1 AND state IN ('bozza','assegnato','in_corso')").await;
        if o > 0 { items.push(json!({"title":"Interventi attivi","detail":format!("{o} da gestire"),"link":"ordini","tone":"info"})); }
        let tt = c1("SELECT count(*) FROM app.shift_plans WHERE tenant_id=$1 AND state='pianificato'").await;
        if tt > 0 { items.push(json!({"title":"Turni da confermare","detail":format!("{tt} pianificati"),"link":"turni","tone":"info"})); }
    }
    // Lavoratore — i propri interventi assegnati
    if role == "lavoratore" {
        let mine = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM app.work_orders WHERE tenant_id=$1 AND assigned_user=$2 AND state IN ('assegnato','in_corso')")
            .bind(tid).bind(uid).fetch_one(&pool).await.unwrap_or(0);
        if mine > 0 { items.push(json!({"title":"I tuoi interventi","detail":format!("{mine} assegnati"),"link":"ordini","tone":"info"})); }
    }
    Ok(Json(json!({"items":items})))
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
const DEVICE_MODS: &[&str] = &["loneworker","iot"]; // suite self-contained: niente connettori esterni
const CONNECTOR_MODS: &[&str] = &["iot"]; // solo IoT nativo (connettori esterni ritirati)
/// Moduli che possono PRODURRE incidenti → l'escalation/centrale è condivisa fra tutti.
/// La configurazione e la visibilità escalation (gruppi, routing, on-call, recapiti,
/// override, maintenance, centrale allarmi) non devono sparire se "Sicurezza lavoratore"
/// (loneworker) è spento ma resta attivo un altro modulo che genera allarmi.
const ESCALATION_MODS: &[&str] = &["loneworker","iot"]; // moduli nativi che producono allarmi

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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "settings.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
/// priority "alta"->3, "media"->2. repeat_seconds carried to the YWAlert connector payload.
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
    if !has_cap(&s,"config.view") {
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
    #[serde(default)] pub assigned_group: Option<Uuid>, #[serde(default)] pub client_id: Option<Uuid>,
    pub scheduled_start: Option<String>, pub scheduled_end: Option<String>, pub priority: Option<i32>,
    #[serde(default)] pub checklist: Option<Vec<String>> }

/// Tappa 3: l'utente è membro del gruppo di persone condiviso? (owner_pool: check additivo di ACL).
async fn user_in_group(st: &AppState, user_id: Uuid, group_id: Uuid) -> bool {
    sqlx::query_scalar::<_,i64>("SELECT count(*) FROM app.user_group_members WHERE user_id=$1 AND group_id=$2")
        .bind(user_id).bind(group_id).fetch_one(&st.owner_pool).await.unwrap_or(0) > 0
}
pub async fn create_order(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewOrder>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    let ss = b.scheduled_start.as_deref().map(parse_ts).transpose()?;
    let se = b.scheduled_end.as_deref().map(parse_ts).transpose()?;
    if let (Some(a),Some(z))=(ss,se) { if z<=a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); } }
    let pri = b.priority.unwrap_or(2).clamp(1,3);
    // assegnato se c'è un assegnatario (utente o gruppo)
    let state = if b.assigned_user.is_some() || b.assigned_group.is_some() { "assegnato" } else { "bozza" };
    // checklist: array di etichette -> [{label,done:false}]
    let checklist = json!(b.checklist.clone().unwrap_or_default().into_iter().map(|l| json!({"label":l,"done":false})).collect::<Vec<_>>());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.work_orders(tenant_id,site_id,title,instructions,assigned_user,assigned_group,state,created_by,scheduled_start,scheduled_end,priority,checklist,client_id)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING id")
        .bind(tid).bind(b.site_id).bind(&b.title).bind(&b.instructions).bind(b.assigned_user).bind(b.assigned_group).bind(state).bind(s.user_id)
        .bind(ss).bind(se).bind(pri).bind(&checklist).bind(b.client_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id"),"state":state})))
}

#[derive(Deserialize)]
pub struct EditOrder { pub version: i32, pub title: Option<String>, pub site_id: Option<Uuid>, pub instructions: Option<String>,
    pub assigned_user: Option<Uuid>, #[serde(default)] pub assigned_group: Option<Uuid>, #[serde(default)] pub clear_group: Option<bool>,
    #[serde(default)] pub client_id: Option<Uuid>,
    pub scheduled_start: Option<String>, pub scheduled_end: Option<String>, pub priority: Option<i32>,
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
    let clear_g = b.clear_group.unwrap_or(false);
    let pri = b.priority.map(|p| p.clamp(1,3));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // COALESCE mantiene i valori esistenti quando il campo non è passato; clear_schedule/clear_group azzerano.
    let n = sqlx::query(
        "UPDATE app.work_orders SET
           title=COALESCE($3,title),
           instructions=COALESCE($4,instructions),
           site_id=COALESCE($5,site_id),
           assigned_user=COALESCE($6,assigned_user),
           assigned_group=CASE WHEN $11 THEN NULL ELSE COALESCE($12,assigned_group) END,
           scheduled_start=CASE WHEN $7 THEN NULL ELSE COALESCE($8,scheduled_start) END,
           scheduled_end=CASE WHEN $7 THEN NULL ELSE COALESCE($9,scheduled_end) END,
           priority=COALESCE($10,priority),
           client_id=COALESCE($13,client_id),
           state=CASE WHEN state='bozza' AND ($6 IS NOT NULL OR $12 IS NOT NULL) THEN 'assegnato' ELSE state END,
           version=version+1, updated_at=now()
         WHERE id=$1 AND version=$2")
        .bind(id).bind(b.version).bind(&b.title).bind(&b.instructions).bind(b.site_id).bind(b.assigned_user)
        .bind(clear).bind(ss).bind(se).bind(pri).bind(clear_g).bind(b.assigned_group).bind(b.client_id)
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
    let cols="o.id,o.title,o.state,o.version,o.outcome,o.instructions,o.site_id,o.assigned_user,o.assigned_group,o.scheduled_start,o.scheduled_end,o.priority,s.name AS site,u.email AS assignee,g.name AS group_name,\
        o.arrived_at,o.work_started_at,o.work_minutes,o.materials,o.checklist,o.outcome_kind,o.report,o.signed_by,o.signed_at,\
        s.address AS site_address,s.lat AS site_lat,s.lon AS site_lon,s.access_notes AS site_access,o.client_id,cl.name AS client_name";
    let joins="FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.users u ON u.id=o.assigned_user LEFT JOIN app.user_groups g ON g.id=o.assigned_group LEFT JOIN app.clients cl ON cl.id=o.client_id";
    let rows = if s.role == "lavoratore" {
        // tecnico: i propri ordini + quelli assegnati a un gruppo di persone di cui è membro
        sqlx::query(&format!("SELECT {cols} {joins}
            WHERE o.assigned_user=$1 OR o.assigned_group IN (SELECT group_id FROM app.user_group_members WHERE user_id=$1)
            ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC")).bind(s.user_id).fetch_all(&mut *tx).await
    } else {
        sqlx::query(&format!("SELECT {cols} {joins}
            ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC")).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "title": r.get::<String,_>("title"), "state": r.get::<String,_>("state"),
        "version": r.get::<i32,_>("version"), "site": r.try_get::<String,_>("site").ok(),
        "site_id": r.try_get::<Uuid,_>("site_id").ok(), "assigned_user": r.try_get::<Uuid,_>("assigned_user").ok(),
        "assigned_group": r.try_get::<Uuid,_>("assigned_group").ok(), "group_name": r.try_get::<String,_>("group_name").ok(),
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
        "site_access": if is_planner(&s) || s.role=="lavoratore" { r.try_get::<String,_>("site_access").ok() } else { None },
        "client_id": r.try_get::<Uuid,_>("client_id").ok(), "client_name": r.try_get::<String,_>("client_name").ok()
    })).collect();
    Ok(Json(json!(out)))
}
// ===== Assistenza A1: anagrafica Cliente + referenti =====
#[derive(Deserialize)]
pub struct ClientReq { pub name: String, #[serde(default)] pub vat: Option<String>, #[serde(default)] pub address: Option<String>,
    #[serde(default)] pub phone: Option<String>, #[serde(default)] pub email: Option<String>, #[serde(default)] pub notes: Option<String>,
    #[serde(default)] pub pec: Option<String>, #[serde(default)] pub sdi: Option<String>, #[serde(default)] pub payment_terms: Option<String> }
pub async fn list_clients(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT c.id,c.name,c.vat,c.address,c.phone,c.email,c.notes,c.active,
                (SELECT count(*) FROM app.work_orders o WHERE o.client_id=c.id)::int AS orders,
                (SELECT count(*) FROM app.client_contacts k WHERE k.client_id=c.id)::int AS contacts
         FROM app.clients c ORDER BY c.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"vat":r.try_get::<String,_>("vat").ok(),
        "address":r.try_get::<String,_>("address").ok(),"phone":r.try_get::<String,_>("phone").ok(),
        "email":r.try_get::<String,_>("email").ok(),"notes":r.try_get::<String,_>("notes").ok(),
        "active":r.get::<bool,_>("active"),"orders":r.get::<i32,_>("orders"),"contacts":r.get::<i32,_>("contacts") })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_client(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ClientReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.clients(tenant_id,name,vat,address,phone,email,notes,pec,sdi,payment_terms,created_by) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.vat).bind(&b.address).bind(&b.phone).bind(&b.email).bind(&b.notes).bind(&b.pec).bind(&b.sdi).bind(&b.payment_terms).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "client_create", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn get_client(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let c = sqlx::query("SELECT id,name,vat,address,phone,email,notes,active,pec,sdi,payment_terms FROM app.clients WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let c = match c { Some(r)=>r, None=>{ return Err(err(StatusCode::NOT_FOUND, "cliente non trovato")); } };
    let ks = sqlx::query("SELECT id,name,role,phone,email FROM app.client_contacts WHERE client_id=$1 ORDER BY name").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let orders = sqlx::query("SELECT o.id,o.title,o.state,o.scheduled_start FROM app.work_orders o WHERE o.client_id=$1 ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC LIMIT 50").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let kout: Vec<Value> = ks.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),
        "role":r.try_get::<String,_>("role").ok(),"phone":r.try_get::<String,_>("phone").ok(),"email":r.try_get::<String,_>("email").ok()})).collect();
    let oout: Vec<Value> = orders.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),
        "state":r.get::<String,_>("state"),"scheduled_start":r.try_get::<chrono::DateTime<chrono::Utc>,_>("scheduled_start").ok()})).collect();
    Ok(Json(json!({"id":c.get::<Uuid,_>("id"),"name":c.get::<String,_>("name"),"vat":c.try_get::<String,_>("vat").ok(),
        "address":c.try_get::<String,_>("address").ok(),"phone":c.try_get::<String,_>("phone").ok(),"email":c.try_get::<String,_>("email").ok(),
        "notes":c.try_get::<String,_>("notes").ok(),"active":c.get::<bool,_>("active"),
        "pec":c.try_get::<String,_>("pec").ok(),"sdi":c.try_get::<String,_>("sdi").ok(),"payment_terms":c.try_get::<String,_>("payment_terms").ok(),
        "contacts":kout,"orders":oout})))
}
pub async fn update_client(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ClientReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.clients SET name=COALESCE($2,name),vat=$3,address=$4,phone=$5,email=$6,notes=$7,pec=$8,sdi=$9,payment_terms=$10 WHERE id=$1")
        .bind(id).bind(if b.name.trim().is_empty(){None}else{Some(b.name.trim())}).bind(&b.vat).bind(&b.address).bind(&b.phone).bind(&b.email).bind(&b.notes).bind(&b.pec).bind(&b.sdi).bind(&b.payment_terms)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "cliente non trovato")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_client(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.clients WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "client_delete", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct ContactReq { pub name: String, #[serde(default)] pub role: Option<String>, #[serde(default)] pub phone: Option<String>, #[serde(default)] pub email: Option<String> }
pub async fn create_contact(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<ContactReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.client_contacts(tenant_id,client_id,name,role,phone,email) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(cid).bind(b.name.trim()).bind(&b.role).bind(&b.phone).bind(&b.email).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_contact(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.client_contacts WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ===== Interventi A2: listino prezzi + valorizzazione della visita =====
#[derive(Deserialize)]
pub struct PriceItemReq { pub name: String, #[serde(default)] pub code: Option<String>, #[serde(default)] pub category: Option<String>,
    #[serde(default)] pub unit: Option<String>, #[serde(default)] pub unit_price: Option<f64>, #[serde(default)] pub active: Option<bool> }
pub async fn list_price_items(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,code,name,category,unit,unit_price::float8 AS unit_price,active FROM app.price_list_items ORDER BY active DESC,category,name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"code":r.try_get::<String,_>("code").ok(),"name":r.get::<String,_>("name"),
        "category":r.get::<String,_>("category"),"unit":r.get::<String,_>("unit"),
        "unit_price":r.get::<f64,_>("unit_price"),"active":r.get::<bool,_>("active") })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_price_item(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PriceItemReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.price_list_items(tenant_id,code,name,category,unit,unit_price) VALUES($1,$2,$3,COALESCE($4,'altro'),COALESCE($5,'pezzo'),COALESCE($6::numeric,0)) RETURNING id")
        .bind(tid).bind(&b.code).bind(b.name.trim()).bind(&b.category).bind(&b.unit).bind(b.unit_price)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_price_item(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PriceItemReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.price_list_items SET name=COALESCE($2,name),code=$3,category=COALESCE($4,category),unit=COALESCE($5,unit),unit_price=COALESCE($6::numeric,unit_price),active=COALESCE($7,active) WHERE id=$1")
        .bind(id).bind(if b.name.trim().is_empty(){None}else{Some(b.name.trim())}).bind(&b.code).bind(&b.category).bind(&b.unit).bind(b.unit_price).bind(b.active)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "voce non trovata")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_price_item(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.price_list_items WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Chi può valorizzare una visita: pianificatore o l'assegnatario dell'ordine.
async fn order_line_allowed(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, s: &Session, wo: Uuid) -> Result<bool, (StatusCode,String)> {
    if is_planner(s) { return Ok(true); }
    let asg: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.work_orders WHERE id=$1").bind(wo).fetch_optional(&mut **tx).await.map_err(ise)?.flatten();
    Ok(asg == Some(s.user_id))
}
pub async fn list_order_lines(State(st): State<AppState>, h: HeaderMap, Path(wo): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,item_id,description,category,unit,qty::float8 AS qty,unit_price::float8 AS unit_price,(qty*unit_price)::float8 AS total FROM app.work_order_lines WHERE work_order_id=$1 ORDER BY created_at").bind(wo).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut tot=0.0;
    let out: Vec<Value> = rows.iter().map(|r| { let t=r.get::<f64,_>("total"); tot+=t; json!({
        "id":r.get::<Uuid,_>("id"),"item_id":r.try_get::<Uuid,_>("item_id").ok(),"description":r.get::<String,_>("description"),
        "category":r.get::<String,_>("category"),"unit":r.get::<String,_>("unit"),
        "qty":r.get::<f64,_>("qty"),"unit_price":r.get::<f64,_>("unit_price"),"total":t }) }).collect();
    Ok(Json(json!({"lines":out,"total":(tot*100.0).round()/100.0})))
}
#[derive(Deserialize)]
pub struct OrderLineReq { pub description: String, #[serde(default)] pub item_id: Option<Uuid>, #[serde(default)] pub category: Option<String>,
    #[serde(default)] pub unit: Option<String>, #[serde(default)] pub qty: Option<f64>, #[serde(default)] pub unit_price: Option<f64> }
pub async fn add_order_line(State(st): State<AppState>, h: HeaderMap, Path(wo): Path<Uuid>, Json(b): Json<OrderLineReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    if b.description.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "descrizione richiesta")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if !order_line_allowed(&mut tx, &s, wo).await? { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore o assegnatario")); }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.work_order_lines(tenant_id,work_order_id,item_id,description,category,unit,qty,unit_price,created_by)
         VALUES($1,$2,$3,$4,COALESCE($5,'altro'),COALESCE($6,'pezzo'),COALESCE($7::numeric,1),COALESCE($8::numeric,0),$9) RETURNING id")
        .bind(tid).bind(wo).bind(b.item_id).bind(b.description.trim()).bind(&b.category).bind(&b.unit).bind(b.qty).bind(b.unit_price).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_order_line(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // consenti a pianificatore o assegnatario dell'ordine a cui appartiene la riga
    let wo: Option<Uuid> = sqlx::query_scalar("SELECT work_order_id FROM app.work_order_lines WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let wo = match wo { Some(x)=>x, None=>{ return Err(err(StatusCode::NOT_FOUND,"riga non trovata")); } };
    if !order_line_allowed(&mut tx, &s, wo).await? { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore o assegnatario")); }
    sqlx::query("DELETE FROM app.work_order_lines WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ===== Interventi A3: dispatch — suggerimento tecnico per un intervento =====
// Grounded sui fatti del tenant: esclude gli assenti nel giorno dell'intervento, ordina per
// carico (meno interventi quel giorno, poi meno aperti). youworker.ai opzionale per il riassunto.
pub async fn suggest_order_tech(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    use chrono::TimeZone as _;
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ord = sqlx::query("SELECT scheduled_start,title FROM app.work_orders WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let ord = match ord { Some(r)=>r, None=>{ return Err(err(StatusCode::NOT_FOUND,"intervento non trovato")); } };
    let sched: Option<chrono::DateTime<chrono::Utc>> = ord.try_get("scheduled_start").ok();
    let day = sched.map(|d| d.date_naive()).unwrap_or_else(|| st.clock.now().date_naive());
    let day_start = chrono::Utc.from_utc_datetime(&day.and_hms_opt(0,0,0).unwrap());
    let day_end = day_start + chrono::Duration::days(1);
    let workers = sqlx::query("SELECT id,email FROM app.users WHERE role IN ('lavoratore','operatore') ORDER BY email").fetch_all(&mut *tx).await.map_err(ise)?;
    let loads = sqlx::query("SELECT assigned_user, count(*) FILTER (WHERE state IN ('assegnato','in_corso'))::int AS open_load, count(*) FILTER (WHERE scheduled_start::date=$1)::int AS day_load FROM app.work_orders WHERE assigned_user IS NOT NULL GROUP BY assigned_user")
        .bind(day).fetch_all(&mut *tx).await.map_err(ise)?;
    let absent = sqlx::query("SELECT DISTINCT user_id FROM app.worker_absences WHERE starts_at < $2 AND ends_at > $1")
        .bind(day_start).bind(day_end).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    use std::collections::{HashMap,HashSet};
    let mut open_m: HashMap<Uuid,i32>=HashMap::new(); let mut day_m: HashMap<Uuid,i32>=HashMap::new();
    for r in &loads { let u:Uuid=r.get("assigned_user"); open_m.insert(u, r.get::<i32,_>("open_load")); day_m.insert(u, r.get::<i32,_>("day_load")); }
    let absent_set: HashSet<Uuid> = absent.iter().map(|r| r.get::<Uuid,_>("user_id")).collect();
    let mut cand: Vec<(Uuid,String,i32,i32,bool)> = workers.iter().map(|r| {
        let u:Uuid=r.get("id"); (u, r.get::<String,_>("email"), *day_m.get(&u).unwrap_or(&0), *open_m.get(&u).unwrap_or(&0), absent_set.contains(&u))
    }).collect();
    cand.sort_by(|a,b| a.4.cmp(&b.4).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)).then(a.1.cmp(&b.1)));
    let out: Vec<Value> = cand.iter().map(|(u,em,dl,ol,ab)| json!({
        "user_id":u,"email":em,"day_load":dl,"open_load":ol,"absent":ab,
        "reason": if *ab {format!("assente il {day}")} else {format!("{dl} interventi il {day} · {ol} aperti")} })).collect();
    let det = {
        let avail: Vec<&(Uuid,String,i32,i32,bool)> = cand.iter().filter(|c| !c.4).collect();
        match avail.first() {
            Some(top) => format!("Consiglio: {} (carico più basso: {} il {}, {} aperti). {} tecnici disponibili.", top.1, top.2, day, top.3, avail.len()),
            None => "Nessun tecnico disponibile quel giorno (tutti assenti).".to_string(),
        }
    };
    let mut summary = det.clone();
    if module_active(&st, tid, "assistant").await && ai_feature_on(&st, tid, "assistant").await {
        if let Some(c) = ai_conn(&st, tid).await {
            let sys = "Assistente dispatch YouWorkforce. In UNA frase consiglia il tecnico migliore usando SOLO i fatti (carico, assenze). Italiano, conciso.";
            let ctx = format!("Intervento: {}. Candidati: {}", ord.get::<String,_>("title"), serde_json::to_string(&out).unwrap_or_default());
            if let Ok(ans) = ai_chat(&st, &c, sys, &ctx).await { if !ans.trim().is_empty() { summary = ans; } }
        }
    }
    Ok(Json(json!({"day":day.to_string(),"candidates":out,"summary":summary,"provider": if summary==det {"deterministico"} else {"youworker.ai"}})))
}
// ===== Interventi A4: contratti/SLA + piani di manutenzione ricorrente =====
#[derive(Deserialize)]
pub struct ContractReq { pub name: String, #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub sla_hours: Option<i32>,
    #[serde(default)] pub hours_bundle: Option<f64>, #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String>,
    #[serde(default)] pub active: Option<bool>, #[serde(default)] pub note: Option<String> }
pub async fn list_contracts(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT c.id,c.name,c.sla_hours,c.hours_bundle::float8 AS hours_bundle,c.start_date,c.end_date,c.active,c.note,c.client_id,cl.name AS client_name FROM app.service_contracts c LEFT JOIN app.clients cl ON cl.id=c.client_id ORDER BY c.active DESC,c.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"sla_hours":r.try_get::<i32,_>("sla_hours").ok(),
        "hours_bundle":r.try_get::<f64,_>("hours_bundle").ok(),
        "start_date":r.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":r.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "active":r.get::<bool,_>("active"),"note":r.try_get::<String,_>("note").ok(),
        "client_id":r.try_get::<Uuid,_>("client_id").ok(),"client_name":r.try_get::<String,_>("client_name").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_contract(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ContractReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.service_contracts(tenant_id,client_id,name,sla_hours,hours_bundle,start_date,end_date,note,created_by) VALUES($1,$2,$3,$4,$5::numeric,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(b.client_id).bind(b.name.trim()).bind(b.sla_hours).bind(b.hours_bundle)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_contract(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ContractReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.service_contracts SET name=COALESCE($2,name),client_id=$3,sla_hours=$4,hours_bundle=$5::numeric,start_date=$6,end_date=$7,active=COALESCE($8,active),note=$9 WHERE id=$1")
        .bind(id).bind(if b.name.trim().is_empty(){None}else{Some(b.name.trim())}).bind(b.client_id).bind(b.sla_hours).bind(b.hours_bundle)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(b.active).bind(&b.note)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "contratto non trovato")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_contract(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.service_contracts WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct PlanReq { pub title: String, #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub site_id: Option<Uuid>,
    #[serde(default)] pub contract_id: Option<Uuid>, #[serde(default)] pub every_days: Option<i32>, #[serde(default)] pub next_date: Option<String>,
    #[serde(default)] pub active: Option<bool> }
pub async fn list_plans(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT p.id,p.title,p.every_days,p.next_date,p.active,p.client_id,cl.name AS client_name,p.site_id,si.name AS site FROM app.maintenance_plans p LEFT JOIN app.clients cl ON cl.id=p.client_id LEFT JOIN app.sites si ON si.id=p.site_id ORDER BY p.active DESC,p.next_date").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),"every_days":r.get::<i32,_>("every_days"),
        "next_date":r.get::<chrono::NaiveDate,_>("next_date").to_string(),"active":r.get::<bool,_>("active"),
        "client_id":r.try_get::<Uuid,_>("client_id").ok(),"client_name":r.try_get::<String,_>("client_name").ok(),
        "site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_plan(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PlanReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.title.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "titolo richiesto")); }
    let ev = b.every_days.unwrap_or(30).max(1);
    let nd = parse_date_opt(&b.next_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.maintenance_plans(tenant_id,client_id,site_id,contract_id,title,every_days,next_date) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.client_id).bind(b.site_id).bind(b.contract_id).bind(b.title.trim()).bind(ev).bind(nd)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_plan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PlanReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.title.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "titolo richiesto")); }
    let ev = b.every_days.unwrap_or(30).max(1);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.maintenance_plans SET title=$2,client_id=$3,site_id=$4,contract_id=$5,every_days=$6,next_date=COALESCE($7,next_date),active=COALESCE($8,active) WHERE id=$1")
        .bind(id).bind(b.title.trim()).bind(b.client_id).bind(b.site_id).bind(b.contract_id).bind(ev).bind(parse_date_opt(&b.next_date)).bind(b.active)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "piano non trovato")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_plan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.maintenance_plans WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Genera l'appuntamento dalla ricorrenza e avanza la prossima data.
pub async fn generate_plan_order(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    use chrono::TimeZone as _;
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let p = sqlx::query("SELECT title,client_id,site_id,contract_id,every_days,next_date FROM app.maintenance_plans WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let p = match p { Some(r)=>r, None=>{ return Err(err(StatusCode::NOT_FOUND,"piano non trovato")); } };
    let nd: chrono::NaiveDate = p.get("next_date");
    let start = chrono::Utc.from_utc_datetime(&nd.and_hms_opt(9,0,0).unwrap());
    let oid: Uuid = sqlx::query_scalar(
        "INSERT INTO app.work_orders(tenant_id,site_id,title,state,created_by,scheduled_start,priority,client_id,contract_id)
         VALUES($1,$2,$3,'bozza',$4,$5,2,$6,$7) RETURNING id")
        .bind(tid).bind(p.try_get::<Uuid,_>("site_id").ok()).bind(format!("{} (manutenzione)", p.get::<String,_>("title"))).bind(s.user_id)
        .bind(start).bind(p.try_get::<Uuid,_>("client_id").ok()).bind(p.try_get::<Uuid,_>("contract_id").ok())
        .fetch_one(&mut *tx).await.map_err(ise)?;
    let ev: i32 = p.get("every_days");
    let next = nd + chrono::Duration::days(ev as i64);
    sqlx::query("UPDATE app.maintenance_plans SET next_date=$2,last_order=$3 WHERE id=$1").bind(id).bind(next).bind(oid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":oid,"next_date":next.to_string()})))
}
// ===== Interventi A5: richieste/ticket =====
#[derive(Deserialize)]
pub struct RequestReq { pub title: String, #[serde(default)] pub description: Option<String>, #[serde(default)] pub client_id: Option<Uuid>,
    #[serde(default)] pub priority: Option<i32>, #[serde(default)] pub requester: Option<String>, #[serde(default)] pub status: Option<String> }
pub async fn list_requests(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT r.id,r.title,r.description,r.priority,r.status,r.requester,r.created_at,r.client_id,cl.name AS client_name,r.order_id FROM app.service_requests r LEFT JOIN app.clients cl ON cl.id=r.client_id ORDER BY (r.status IN ('convertita','chiusa')), r.created_at DESC").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),"description":r.try_get::<String,_>("description").ok(),
        "priority":r.get::<i32,_>("priority"),"status":r.get::<String,_>("status"),"requester":r.try_get::<String,_>("requester").ok(),
        "created_at":r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
        "client_id":r.try_get::<Uuid,_>("client_id").ok(),"client_name":r.try_get::<String,_>("client_name").ok(),
        "order_id":r.try_get::<Uuid,_>("order_id").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_request(State(st): State<AppState>, h: HeaderMap, Json(b): Json<RequestReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    if b.title.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "titolo richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.service_requests(tenant_id,client_id,title,description,priority,requester,created_by) VALUES($1,$2,$3,$4,COALESCE($5,2),$6,$7) RETURNING id")
        .bind(tid).bind(b.client_id).bind(b.title.trim()).bind(&b.description).bind(b.priority).bind(&b.requester).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_request(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<RequestReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.service_requests SET title=COALESCE($2,title),description=$3,priority=COALESCE($4,priority),status=COALESCE($5,status),requester=$6,client_id=$7 WHERE id=$1")
        .bind(id).bind(if b.title.trim().is_empty(){None}else{Some(b.title.trim())}).bind(&b.description).bind(b.priority).bind(&b.status).bind(&b.requester).bind(b.client_id)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "richiesta non trovata")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_request(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.service_requests WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Converte la richiesta in un intervento (bozza) e la segna "convertita".
pub async fn convert_request(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = sqlx::query("SELECT title,description,client_id,priority,status FROM app.service_requests WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let r = match r { Some(x)=>x, None=>{ return Err(err(StatusCode::NOT_FOUND,"richiesta non trovata")); } };
    if r.get::<String,_>("status")=="convertita" { return Err(err(StatusCode::CONFLICT,"richiesta già convertita")); }
    let oid: Uuid = sqlx::query_scalar(
        "INSERT INTO app.work_orders(tenant_id,title,instructions,state,created_by,priority,client_id) VALUES($1,$2,$3,'bozza',$4,$5,$6) RETURNING id")
        .bind(tid).bind(r.get::<String,_>("title")).bind(r.try_get::<String,_>("description").ok()).bind(s.user_id).bind(r.get::<i32,_>("priority")).bind(r.try_get::<Uuid,_>("client_id").ok())
        .fetch_one(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.service_requests SET status='convertita',order_id=$2 WHERE id=$1").bind(id).bind(oid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":oid})))
}

#[derive(Deserialize)]
pub struct OrderAction { pub version: i32, pub user_id: Option<Uuid>, #[serde(default)] pub group_id: Option<Uuid>, pub outcome: Option<String>, pub reason: Option<String> }

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

/// Tappa 3: assegna l'ordine a un GRUPPO di persone condiviso. I membri del gruppo lo vedono
/// nei propri ordini e possono eseguirlo (ACL per gruppo in order_edit_allowed).
pub async fn order_assign_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let gid = b.group_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "group_id richiesto"))?;
    let tid = s.tenant_id.unwrap();
    // il gruppo deve esistere nel tenant (RLS su user_groups)
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.user_groups WHERE id=$1 AND tenant_id=$2").bind(gid).bind(tid)
        .fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente"));
    }
    order_transition(&st, &s, id, &b, "assegnato",
        Some("assigned_group"), Some(SetVal::Id(gid)), &["bozza","assegnato"]).await
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
    let row = sqlx::query("SELECT assigned_user, assigned_group FROM app.work_orders WHERE id=$1 AND tenant_id=$2")
        .bind(oid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "ordine inesistente"))?;
    let au: Option<Uuid> = row.try_get("assigned_user").ok().flatten();
    let ag: Option<Uuid> = row.try_get("assigned_group").ok().flatten();
    if is_planner(&s) || au == Some(s.user_id) { return Ok(tid); }
    // ACL per gruppo: membro del gruppo di persone assegnato
    if let Some(g) = ag { if user_in_group(st, s.user_id, g).await { return Ok(tid); } }
    Err(err(StatusCode::FORBIDDEN, "non autorizzato su questo ordine"))
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
    let id: Uuid = row.get("id");
    // Storico completo: anche la creazione lascia traccia (Slice 1).
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'create',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"work_order_id":b.work_order_id})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id,"state":"bozza","version":1})))
}

/// Riassunto leggibile dei materiali dell'intervento, per la nota del consuntivo generato.
fn materials_note(m: &Value) -> Option<String> {
    let arr = m.as_array()?;
    let items: Vec<String> = arr.iter().filter_map(|x| {
        let d = x.get("descr").and_then(|v| v.as_str())?;
        let q = x.get("qta").and_then(|v| v.as_f64()).map(|q| format!(" {q}")).unwrap_or_default();
        let u = x.get("unita").and_then(|v| v.as_str()).map(|u| format!(" {u}")).unwrap_or_default();
        Some(format!("{d}{q}{u}"))
    }).collect();
    if items.is_empty() { None } else { Some(format!("Materiali intervento: {}", items.join("; "))) }
}

/// Ponte intervento -> consuntivo (Slice 2): genera (o riusa) una BOZZA di consuntivo per
/// l'assegnatario dell'ordine, prefillata con una riga "labor" dai minuti lavorati dell'intervento
/// (tariffa da valorizzare finché non c'è il listino). Idempotente: ri-eseguire non duplica la riga
/// (vincolo univoco per ordine+utente+labor). I materiali finiscono nella nota. Un solo inserimento:
/// il tecnico non ricompila ore/materiali a mano.
pub async fn generate_timesheet_from_order(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    if !module_active(&st, tid, "consuntivi").await { return Err(err(StatusCode::FORBIDDEN, "modulo consuntivi non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let o = sqlx::query("SELECT title,assigned_user,work_minutes,work_started_at,arrived_at,materials FROM app.work_orders WHERE id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "ordine non trovato"))?;
    let owner: Uuid = o.try_get::<Uuid,_>("assigned_user").map_err(|_| err(StatusCode::BAD_REQUEST, "ordine non assegnato a una persona"))?;
    // Autorizzazione: l'assegnatario stesso o un pianificatore.
    if owner != s.user_id && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo l'assegnatario o il pianificatore")); }
    let wm: i32 = o.try_get::<i32,_>("work_minutes").ok().filter(|m| *m > 0)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "nessuna esecuzione da consuntivare (minuti lavorati mancanti)"))?;
    let title: String = o.get("title");
    let started: Option<chrono::DateTime<chrono::Utc>> = o.try_get("work_started_at").ok();
    let arrived: Option<chrono::DateTime<chrono::Utc>> = o.try_get("arrived_at").ok();
    let wd = started.or(arrived).map(|t| t.date_naive()).unwrap_or_else(|| st.clock.now().date_naive());
    let materials: Value = o.try_get("materials").unwrap_or(json!([]));
    let def_cur = tool_config(&st, tid, "consuntivi", "rates").await
        .get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
    // Riusa una bozza aperta per (ordine, assegnatario) oppure creane una.
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM app.timesheets WHERE work_order_id=$1 AND user_id=$2 AND state IN ('bozza','da_correggere') ORDER BY created_at DESC LIMIT 1")
        .bind(id).bind(owner).fetch_optional(&mut *tx).await.map_err(ise)?;
    let (ts_id, created) = match existing {
        Some(t) => (t, false),
        None => {
            let note = materials_note(&materials);
            let r = sqlx::query("INSERT INTO app.timesheets(tenant_id,work_order_id,user_id,note) VALUES($1,$2,$3,$4) RETURNING id")
                .bind(tid).bind(id).bind(owner).bind(note).fetch_one(&mut *tx).await.map_err(ise)?;
            let tsid: Uuid = r.get("id");
            sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'create',$3)")
                .bind(tid).bind(tsid).bind(json!({"by":s.email,"from_order":id})).execute(&mut *tx).await.map_err(ise)?;
            (tsid, true)
        }
    };
    // Riga labor idempotente dall'ordine (tariffa NULL = da valorizzare; listino = Slice 3).
    let ins = sqlx::query(
        "INSERT INTO app.timesheet_lines(tenant_id,timesheet_id,activity,work_date,minutes,rate_cents,currency,source_order_id,source_user_id,source_kind)
         VALUES($1,$2,$3,$4,$5,NULL,$6,$7,$8,'labor')
         ON CONFLICT (tenant_id,source_order_id,source_user_id,source_kind) WHERE source_order_id IS NOT NULL DO NOTHING
         RETURNING id")
        .bind(tid).bind(ts_id).bind(&title).bind(wd).bind(wm).bind(&def_cur).bind(id).bind(owner)
        .fetch_optional(&mut *tx).await.map_err(ise)?;
    let line_added = ins.is_some();
    if line_added {
        sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1").bind(ts_id).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'add_line',$3)")
            .bind(tid).bind(ts_id).bind(json!({"by":s.email,"source":"order","order_id":id,"minutes":wm})).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "timesheet_from_order", json!({"order_id":id,"timesheet_id":ts_id,"created":created,"line_added":line_added})).await;
    Ok(Json(json!({"timesheet_id":ts_id,"created":created,"line_added":line_added})))
}

pub async fn list_timesheets(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "consuntivi").await?;
    // ACL di sede (coerente con incidenti): i ruoli ristretti (operatore/lavoratore/reperibile,
    // anche con view_all via ruolo custom) vedono SOLO i propri consuntivi + quelli di ordini
    // nelle loro sedi. I ruoli non ristretti (responsabile/auditor/admin) con view_all vedono tutto.
    let allowed = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = if has_cap(&s,"timesheets.view_all") {
        match &allowed {
            None => sqlx::query("SELECT t.id,t.state,t.version,t.created_at,u.email AS owner,o.title AS order_title
                FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
                ORDER BY t.updated_at DESC").fetch_all(&mut *tx).await,
            Some(set) => {
                let sites: Vec<Uuid> = set.iter().copied().collect();
                sqlx::query("SELECT t.id,t.state,t.version,t.created_at,u.email AS owner,o.title AS order_title
                    FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
                    WHERE t.user_id=$1 OR o.site_id = ANY($2) ORDER BY t.updated_at DESC")
                    .bind(s.user_id).bind(&sites).fetch_all(&mut *tx).await
            }
        }
    } else {
        sqlx::query("SELECT t.id,t.state,t.version,t.created_at,u.email AS owner,o.title AS order_title
            FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
            WHERE t.user_id=$1 ORDER BY t.updated_at DESC").bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "state": r.get::<String,_>("state"), "version": r.get::<i32,_>("version"),
        "owner": r.get::<String,_>("owner"), "order_title": r.try_get::<String,_>("order_title").ok() })).collect();
    Ok(Json(json!(out)))
}

async fn timesheet_detail(st: &AppState, tid: Uuid, id: Uuid) -> Result<Value, (StatusCode,String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let head = sqlx::query("SELECT t.id,t.state,t.note,t.user_id,t.version,o.site_id,u.email AS owner,o.title AS order_title
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
        // arrotondamento al centesimo (half-up) per riga: evita il troncamento della divisione intera
        match rate { Some(rc) => labor += (bm * rc + 30) / 60, None => missing_rate = true }
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
        "id": head.get::<Uuid,_>("id"), "state": head.get::<String,_>("state"), "version": head.get::<i32,_>("version"),
        "owner": head.get::<String,_>("owner"), "owner_id": head.get::<Uuid,_>("user_id"),
        "site_id": head.try_get::<Uuid,_>("site_id").ok(),
        "order_title": head.try_get::<String,_>("order_title").ok(), "note": head.try_get::<String,_>("note").ok(),
        "lines": jl, "expenses": je, "currency": currency, "round_minutes": round,
        "total": {"minutes":minutes,"billed_minutes":billed,"labor_cents":labor,"expense_cents":exp_total,"total_cents":labor+exp_total,"missing_rate":missing_rate}
    }))
}

/// ACL di lettura di un consuntivo: proprietario sempre; altrimenti serve `timesheets.view_all`
/// e, per i ruoli ristretti per sede, il consuntivo deve appartenere a una sede consentita.
async fn guard_timesheet_access(st: &AppState, s: &Session, d: &Value) -> Result<(),(StatusCode,String)> {
    let owner_id = d["owner_id"].as_str().and_then(|x| Uuid::parse_str(x).ok());
    if owner_id == Some(s.user_id) { return Ok(()); }
    if !has_cap(s,"timesheets.view_all") { return Err(err(StatusCode::FORBIDDEN, "non autorizzato")); }
    if let Some(set) = allowed_sites(st, s).await {
        let site = d["site_id"].as_str().and_then(|x| Uuid::parse_str(x).ok());
        match site { Some(si) if set.contains(&si) => {}, _ => return Err(err(StatusCode::FORBIDDEN, "consuntivo fuori dalle tue sedi")) }
    }
    Ok(())
}

pub async fn get_timesheet(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let d = timesheet_detail(&st, s.tenant_id.unwrap(), id).await?;
    guard_timesheet_access(&st, &s, &d).await?;
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
    sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'add_line',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"activity":b.activity,"minutes":b.minutes})).execute(&mut *tx).await.map_err(ise)?;
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
    sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'add_expense',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"description":b.description,"amount_cents":b.amount_cents})).execute(&mut *tx).await.map_err(ise)?;
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
    let n = sqlx::query("UPDATE app.timesheets SET state='in_revisione',version=version+1,updated_at=now() WHERE id=$1 AND user_id=$2 AND state IN ('bozza','da_correggere')")
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
    // Segregation of duties: di default NON si approva il proprio consuntivo. Un tenant può
    // consentirlo esplicitamente (consuntivi.approval.allow_self_approve=true) per realtà piccole.
    let allow_self = tool_config(&st, tid, "consuntivi", "approval").await
        .get("allow_self_approve").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("SELECT user_id,state FROM app.timesheets WHERE id=$1").bind(id)
        .fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"non trovato"))?;
    if row.get::<Uuid,_>("user_id") == s.user_id && !allow_self {
        let _=tx.rollback().await; return Err(err(StatusCode::FORBIDDEN,"non puoi approvare il tuo consuntivo (separazione dei compiti)"));
    }
    // only from in_revisione -> blocks double approval
    let n = sqlx::query("UPDATE app.timesheets SET state='approvato',approved_by=$2,version=version+1,updated_at=now() WHERE id=$1 AND state='in_revisione'")
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
    let allow_self = tool_config(&st, tid, "consuntivi", "approval").await
        .get("allow_self_approve").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let owner = sqlx::query_scalar::<_,Uuid>("SELECT user_id FROM app.timesheets WHERE id=$1").bind(id)
        .fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"non trovato"))?;
    if owner == s.user_id && !allow_self {
        let _=tx.rollback().await; return Err(err(StatusCode::FORBIDDEN,"non puoi revisionare il tuo consuntivo (separazione dei compiti)"));
    }
    let n = sqlx::query("UPDATE app.timesheets SET state='da_correggere',note=$2,version=version+1,updated_at=now() WHERE id=$1 AND state='in_revisione'")
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
    guard_timesheet_access(&st, &s, &d).await?;
    let cur = d["currency"].as_str().unwrap_or("EUR").to_string();
    let mut csv = String::from("tipo,descrizione,data,minuti,minuti_fatturati,tariffa_cent_ora,importo_cent,valuta,giustificativo\n");
    for l in d["lines"].as_array().unwrap_or(&vec![]) {
        let m = l["minutes"].as_i64().unwrap_or(0);
        let bm = l["billed_minutes"].as_i64().unwrap_or(m);
        let rate = l["rate_cents"].as_i64();
        let amt = rate.map(|r| (bm*r+30)/60); // half-up, coerente con il calcolo del dettaglio
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
        if !has_cap(&s,"config.view") {
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
    require_any_module(&st, tid, &["iot"]).await?;
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
    if owner_id != Some(s.user_id) && !has_cap(&s,"timesheets.view_all") {
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
            if !module_active(st, tid, "interventi").await { return Err("modulo interventi non abilitato".into()); }
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
            if !module_active(st, tid, "consuntivi").await { return Err("modulo consuntivi non abilitato".into()); }
            let id = gu("timesheet_id").ok_or("timesheet_id non valido")?;
            timesheet_detail(st, tid, id).await.map_err(|e| e.1)
        }
        "add_diary_entry" => {
            if !module_active(st, tid, "workforce").await && !module_active(st, tid, "interventi").await {
                return Err("diario non disponibile: nessun modulo Campo (workforce/interventi) attivo".into()); }
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
            if !has_cap(&s,"incidents.handle") { return Err("ruolo non abilitato alle note incidente".into()); }
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
            require_cap(s, "iot.manage").map_err(|e| e.1)?;
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
            require_cap(s, "devices.admin").map_err(|e| e.1)?;
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
            let danger = ["close","ack","escalat","silence","silenzia","delete","drop","module","imperson","connection","secret","ywalert"];
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
// YWAlert parity: impostazioni reperibilita', recapiti+verifica, assenze,
// manutenzione, calendario iCal. Tutto pilotato da YouWorkforce (tenant_admin).
// ============================================================================


// ---- Impostazioni reperibilita' (ladder vocale + status updates) ----
pub async fn oncall_settings_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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

// ---- Recapiti YWAlert dei membri (stato verifica) ----
pub async fn oncall_contacts(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // NATIVO: recapiti = telefono/email dell'utente + stato verifica (users.phone_verified_at).
    // Nessuna dipendenza da YWAlert. Membri = utenti presenti in almeno un gruppo di escalation.
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let phone: Option<String> = sqlx::query_scalar("SELECT phone FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let phone = phone.filter(|p| !p.trim().is_empty()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "l'utente non ha un numero di telefono"))?;
    let res = esc_send_sms_native(&st, &phone, "YouWorkforce: messaggio di prova dei reperibili.").await;
    Ok(Json(json!({"ok":true,"delivery":res.state,"note":"messaggio di prova inviato"})))
}

// ---- F3: multi-recapito + regole di notifica per-utente (ladder a 2 livelli, parità YWAlert) ----
pub async fn user_cm_list(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
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
/// sink, email via relay o sink. Nessuna dipendenza da YWAlert.
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let (kind, value) = cm_fetch(&st, tid, cmid).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "recapito inesistente"))?;
    let res = esc_send_code_native(&st, &kind, &value, "YouWorkforce: messaggio di prova.").await;
    Ok(Json(json!({"ok":true,"delivery":res.state})))
}
// ---- G3: preferenza status-update per-utente (override del default tenant; null = eredita) ----
#[derive(Deserialize)] pub struct StatusPrefReq { pub on: Option<bool> }
pub async fn user_status_pref_set(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<StatusPrefReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.users SET status_updates_on=$2 WHERE id=$1 AND tenant_id=$3")
        .bind(uid).bind(b.on).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "utente inesistente")); }
    Ok(Json(json!({"ok":true,"status_updates_on":b.on})))
}

// ---- Assenze / sostituzioni (override nativi sul gruppo, tabella app.esc_overrides) ----
#[derive(Deserialize)] pub struct OverrideQuery { pub group_id: Uuid }
pub async fn oncall_overrides_list(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<OverrideQuery>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // Sorgente NATIVA (app.esc_overrides); nomi utente risolti localmente. YWAlert non richiesto.
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    audit(&st, tid, "youworkforce", "oncall_override_add", json!({"group":b.group_id,"id":oid})).await;
    Ok(Json(json!({"ok":true,"id":oid})))
}
#[derive(Deserialize)] pub struct OverrideDel { pub id: Uuid }
pub async fn oncall_override_del(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideDel>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_overrides WHERE id=$1 AND tenant_id=$2").bind(b.id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "override inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct OverrideUpdate { pub id: Uuid, pub start: String, pub end: String, pub add_user_id: Option<Uuid>, pub remove_user_id: Option<Uuid> }
/// Modifica un override esistente (edit in-place, parità YWAlert).
pub async fn oncall_override_update(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideUpdate>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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

// ---- F3: Temporary schedule (sostituzione a tempo, parità YWAlert) ----
pub async fn temp_schedule_list(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<OverrideQuery>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    // Allow gaps (parità YWAlert): se i blocchi non coprono tutta la finestra e non è consentito, rifiuta.
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let hours = b.hours.unwrap_or(0).clamp(0, 24);
    let exp = if hours > 0 { st.clock.now() + chrono::Duration::hours(hours) } else { st.clock.now() - chrono::Duration::minutes(1) };
    let exp_s = exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // Sorgente di verità NATIVA: il runner nativo (escalation_delivery_run_tenant) sospende
    // le consegne finché now() < maintenance_until. Indipendente da YWAlert.
    let until_db: Option<chrono::DateTime<chrono::Utc>> = if hours > 0 { Some(exp) } else { None };
    sqlx::query("INSERT INTO app.tenant_oncall_cfg(tenant_id,maintenance_until) VALUES($1,$2)
        ON CONFLICT (tenant_id) DO UPDATE SET maintenance_until=EXCLUDED.maintenance_until,updated_at=now()")
        .bind(tid).bind(until_db).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "youworkforce", "oncall_maintenance", json!({"hours":hours,"native":true})).await;
    Ok(Json(json!({"ok":true,"until":if hours>0 {Some(exp_s)} else {None},"active":hours>0})))
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let now = st.clock.now();
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
    let now = st.clock.now();
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

// ============================================================================
// Calendario CORE condiviso (W1): un'unica vista che aggrega gli eventi dei moduli
// Campo attivi (turni/interventi/ronde). Ogni modulo è una "sorgente eventi"; il
// calendario li fonde. Tenant-scoped (RLS). I ruoli ristretti vedono solo i propri.
// ============================================================================
#[derive(Clone)]
struct CalEvent { id: String, source: &'static str, title: String, color: String,
    start: chrono::DateTime<chrono::Utc>, end: chrono::DateTime<chrono::Utc>,
    user_id: Option<Uuid>, user: Option<String>, link: Option<String> }
/// Colore calendario di un utente: quello scelto, altrimenti un colore stabile da palette (per id).
fn user_color(chosen: Option<String>, uid: Uuid) -> String {
    if let Some(c) = chosen { if !c.trim().is_empty() { return c; } }
    const P: &[&str] = &["#2563eb","#16a34a","#9333ea","#dc2626","#d97706","#0891b2","#db2777","#65a30d",
        "#4f46e5","#0d9488","#ea580c","#7c3aed","#059669","#e11d48","#0284c7","#ca8a04"];
    P[(uid.as_bytes()[0] as usize) % P.len()].to_string()
}
fn cal_json(e: &CalEvent) -> Value {
    json!({"id":e.id,"source":e.source,"title":e.title,"color":e.color,
        "start":e.start.to_rfc3339(),"end":e.end.to_rfc3339(),
        "user_id":e.user_id,"user":e.user,"link":e.link})
}
/// Raccoglie gli eventi del tenant nella finestra [from,to] dai moduli attivi. `only_uid` limita
/// a una persona (ruoli ristretti: i propri). Usato sia dall'endpoint JSON sia dall'iCal.
async fn calendar_collect(st: &AppState, tid: Uuid, from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>, only_uid: Option<Uuid>) -> Vec<CalEvent> {
    let mut out: Vec<CalEvent> = Vec::new();
    // --- Forza lavoro e turni: shift_plans ---
    if module_active(st, tid, "workforce").await {
        let rows = sqlx::query(
            "SELECT sp.id, sp.user_id, sp.starts_at, sp.ends_at, sp.state, u.email, u.cal_color, o.title AS order_title,
                    cm.code AS commessa_code, cm.name AS commessa_name
             FROM app.shift_plans sp JOIN app.users u ON u.id=sp.user_id
             LEFT JOIN app.work_orders o ON o.id=sp.work_order_id
             LEFT JOIN app.commesse cm ON cm.id=sp.commessa_id
             WHERE sp.tenant_id=$1 AND sp.ends_at >= $2 AND sp.starts_at <= $3
               AND ($4::uuid IS NULL OR sp.user_id=$4)")
            .bind(tid).bind(from).bind(to).bind(only_uid)
            .fetch_all(&st.owner_pool).await.unwrap_or_default();
        for r in &rows {
            let em: String = r.get("email");
            let uid: Uuid = r.get("user_id");
            let ordt: Option<String> = r.try_get("order_title").ok().flatten();
            let cmc: Option<String> = r.try_get("commessa_code").ok().flatten();
            let cmn: Option<String> = r.try_get("commessa_name").ok().flatten();
            // colore PER LAVORATORE (scelto o da palette) per distinguerli sul calendario
            let ctx = cmn.as_ref().map(|n| match &cmc { Some(c)=>format!("{c} · {n}"), None=>n.clone() }).or(ordt.clone());
            let title = match &ctx { Some(t) => format!("Turno · {em} · {t}"), None => format!("Turno · {em}") };
            out.push(CalEvent{ id: format!("shift-{}", r.get::<Uuid,_>("id")), source:"turno",
                title, color: user_color(r.try_get("cal_color").ok().flatten(), uid),
                start: r.get("starts_at"), end: r.get("ends_at"),
                user_id: Some(uid), user: Some(em), link: ordt });
        }
    }
    // --- Interventi e cantieri: ordini con appuntamento programmato ---
    if module_active(st, tid, "interventi").await {
        let rows = sqlx::query(
            "SELECT o.id, o.title, o.assigned_user, o.scheduled_start, o.scheduled_end, u.email
             FROM app.work_orders o LEFT JOIN app.users u ON u.id=o.assigned_user
             WHERE o.tenant_id=$1 AND o.scheduled_start IS NOT NULL
               AND o.scheduled_start <= $3
               AND COALESCE(o.scheduled_end, o.scheduled_start + interval '1 hour') >= $2
               AND ($4::uuid IS NULL OR o.assigned_user=$4)")
            .bind(tid).bind(from).bind(to).bind(only_uid)
            .fetch_all(&st.owner_pool).await.unwrap_or_default();
        for r in &rows {
            let s0: chrono::DateTime<chrono::Utc> = r.get("scheduled_start");
            let e0: chrono::DateTime<chrono::Utc> = r.try_get("scheduled_end").ok()
                .unwrap_or_else(|| s0 + chrono::Duration::hours(1));
            let title: String = r.get("title");
            out.push(CalEvent{ id: format!("order-{}", r.get::<Uuid,_>("id")), source:"intervento",
                title: format!("Intervento · {title}"), color:"#2563eb".into(),
                start: s0, end: e0, user_id: r.try_get("assigned_user").ok(),
                user: r.try_get::<String,_>("email").ok(), link: None });
        }
    }
    // --- Ronde e punzonature: giri eseguiti (finestra reale) ---
    if module_active(st, tid, "giri").await {
        let rows = sqlx::query(
            "SELECT pr.id, pr.user_id, pr.started_at, pr.ended_at, pr.status, u.email, rt.name AS route
             FROM app.patrol_rounds pr JOIN app.users u ON u.id=pr.user_id
             JOIN app.patrol_routes rt ON rt.id=pr.route_id
             WHERE pr.tenant_id=$1 AND pr.started_at <= $3
               AND COALESCE(pr.ended_at, pr.started_at + interval '2 hours') >= $2
               AND ($4::uuid IS NULL OR pr.user_id=$4)")
            .bind(tid).bind(from).bind(to).bind(only_uid)
            .fetch_all(&st.owner_pool).await.unwrap_or_default();
        for r in &rows {
            let s0: chrono::DateTime<chrono::Utc> = r.get("started_at");
            let e0: chrono::DateTime<chrono::Utc> = r.try_get("ended_at").ok()
                .unwrap_or_else(|| s0 + chrono::Duration::hours(2));
            let route: String = r.get("route");
            out.push(CalEvent{ id: format!("round-{}", r.get::<Uuid,_>("id")), source:"ronda",
                title: format!("Ronda · {route}"), color:"#9333ea".into(),
                start: s0, end: e0, user_id: Some(r.get("user_id")),
                user: r.try_get::<String,_>("email").ok(), link: None });
        }
    }
    out.sort_by_key(|e| e.start);
    out
}
/// GET /api/calendar/events?from&to[&all=1] — eventi del calendario condiviso (JSON per FullCalendar).
pub async fn calendar_events(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let parse = |k:&str, def: chrono::DateTime<chrono::Utc>| q.get(k)
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok().map(|t| t.with_timezone(&chrono::Utc)))
        .unwrap_or(def);
    let now = st.clock.now();
    let from = parse("from", now - chrono::Duration::days(7));
    let to   = parse("to",   now + chrono::Duration::days(31));
    // ruoli ristretti (operatore/lavoratore/reperibile) o chi non pianifica: solo i propri eventi.
    let only_uid = if is_planner(&s) { None } else { Some(s.user_id) };
    let evs = calendar_collect(&st, tid, from, to, only_uid).await;
    Ok(Json(json!(evs.iter().map(cal_json).collect::<Vec<_>>())))
}
/// GET /api/calendar/my-url[?rotate=1] — URL iCal personale (aggrega tutti i moduli attivi).
pub async fn calendar_my_url(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap(); let uid = s.user_id;
    let rotate = q.get("rotate").map(|v| v=="1"||v=="true").unwrap_or(false);
    let existing: Option<String> = sqlx::query_scalar("SELECT token FROM app.calendar_ical_tokens WHERE tenant_id=$1 AND user_id=$2")
        .bind(tid).bind(uid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let token = match (existing, rotate) {
        (Some(t), false) => t,
        _ => { let t = auth::new_token();
            sqlx::query("INSERT INTO app.calendar_ical_tokens(tenant_id,user_id,token) VALUES($1,$2,$3) ON CONFLICT (tenant_id,user_id) DO UPDATE SET token=EXCLUDED.token,created_at=now()")
                .bind(tid).bind(uid).bind(&t).execute(&st.owner_pool).await.map_err(ise)?; t }
    };
    Ok(Json(json!({"url":format!("/api/calendar.ics?token={token}"),"token":token})))
}
#[derive(Deserialize)] pub struct CalIcs { pub token: String }
/// GET /api/calendar.ics?token= — feed iCal PERSONALE (senza sessione; token -> utente/tenant).
pub async fn calendar_ics(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<CalIcs>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT tenant_id,user_id FROM app.calendar_ical_tokens WHERE token=$1")
        .bind(&q.token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "token non valido").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let uid: Uuid = row.get("user_id");
    let now = st.clock.now();
    let evs = calendar_collect(&st, tid, now - chrono::Duration::days(7), now + chrono::Duration::days(60), Some(uid)).await;
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    let esc_ics = |x: &str| x.replace('\\',"\\\\").replace('\n'," ").replace(',', "\\,").replace(';', "\\;");
    let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//YouWorkforce//Calendario//IT\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\nX-WR-CALNAME:YouWorkforce\r\n");
    for e in &evs {
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:{}@youworkforce\r\n", e.id));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(now)));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(e.start)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(e.end)));
        ics.push_str(&format!("SUMMARY:{}\r\n", esc_ics(&e.title)));
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");
    ([(axum::http::header::CONTENT_TYPE, "text/calendar; charset=utf-8")], ics).into_response()
}

/// #5 Sync ack/chiusura da YWAlert -> incidente YW. Se il reperibile prende in carico o
/// chiude via SMS/voce/console YWAlert, lo stato dell'incidente YW lo riflette.
/// Girata periodicamente dal worker. Best-effort: un tenant giu' non blocca gli altri.
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    if !has_cap(&s,"incidents.view") { return err(StatusCode::FORBIDDEN, "ruolo non abilitato").into_response(); }
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.get(traccar_url(&base, "/api/drivers?all=true")).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!([]));
    Ok(Json(v))
}
#[derive(Deserialize)] pub struct DriverReq { pub name: String, pub unique_id: String, pub device_id: Option<Uuid> }
pub async fn traccar_driver_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DriverReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let _ = st.http.delete(traccar_url(&base, &format!("/api/drivers/{id}"))).basic_auth(&user, Some(&pass)).send().await;
    Ok(Json(json!({"ok":true})))
}

// ---- Maintenance (proxy Traccar + link device) ----
pub async fn traccar_maint_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.get(traccar_url(&base, "/api/maintenance?all=true")).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!([]));
    Ok(Json(v))
}
#[derive(Deserialize)] pub struct MaintReqT { pub name: String, #[serde(rename="type")] pub kind: String, pub start: f64, pub period: f64, pub device_id: Option<Uuid> }
pub async fn traccar_maint_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<MaintReqT>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let _ = st.http.delete(traccar_url(&base, &format!("/api/maintenance/{id}"))).basic_auth(&user, Some(&pass)).send().await;
    Ok(Json(json!({"ok":true})))
}

// ---- Computed attributes (proxy Traccar + link device) ----
pub async fn traccar_computed_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let r = st.http.get(traccar_url(&base, "/api/attributes/computed?all=true")).basic_auth(&user, Some(&pass)).send().await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, &format!("Traccar: {e}")))?;
    let v: Value = r.json().await.unwrap_or_else(|_| json!([]));
    Ok(Json(v))
}
#[derive(Deserialize)] pub struct ComputedReq { pub description: String, pub attribute: String, pub expression: String, #[serde(rename="type")] pub kind: String, pub device_id: Option<Uuid> }
pub async fn traccar_computed_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ComputedReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap(); require_module(&st, tid, "traccar").await?;
    let (base, user, pass) = tc_conn_or_err(&st, tid).await?;
    let _ = st.http.delete(traccar_url(&base, &format!("/api/attributes/computed/{id}"))).basic_auth(&user, Some(&pass)).send().await;
    Ok(Json(json!({"ok":true})))
}

// ---- Accumulators (lettura ultima posizione + reset) ----
pub async fn traccar_accumulators_get(State(st): State<AppState>, h: HeaderMap, Path(dev): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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


// ============================================================================
// #4 escalation engine nativo — FASE 1 (shadow). Calcola l'on-call col motore nativo
// (crate::escalation) in parallelo a YWAlert, confronta e logga. NESSUNA consegna:
// YWAlert resta il motore in produzione. Obiettivo: 0 divergenze prima del cutover.
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
/// Costruisce la Schedule nativa dalla config del gruppo (stessi input della sync nativa).
/// Gli override (assenze) sono tirati da YWAlert e innestati a valle da [`esc_goalert_overrides`]
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

/// Risolve il membro on-call della rotation con STATO persistente (semantica YWAlert
/// `rotation_state`): carica/semina lo stato, lo avanza fino a `now`, lo persiste, ritorna il membro.
/// Semina alla prima osservazione dal ground-truth YWAlert (indice del membro on-call corrente).
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
            // SEMINA: preferisci il ground-truth YWAlert (indice del membro on-call ora) per restare
            // allineati finché YWAlert è il motore. Senza YWAlert:
            //  - in CONSEGNA (allow_native_seed=true) semina nativamente alla posizione 0 e PERSISTE;
            //  - in LETTURA (read_transient=true) calcola la posizione 0 ma NON persiste (una GET non
            //    deve mutare lo stato né pre-emptare la semina YWAlert fatta dalla consegna);
            //  - in SHADOW (entrambi false) ritorna None per non generare false divergenze.
            let ga_idx = ga.first().and_then(|e| members.iter().position(|u| email_of.get(u).map(|x| x==e).unwrap_or(false)));
            let (idx, src) = match ga_idx {
                Some(i) => (i, "seed"),
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

/// Runner shadow per un tenant: per ogni gruppo con servizio YWAlert, confronta on-call
/// nativo vs YWAlert e logga. Best-effort, nessun effetto esterno oltre al log.
// ============================================================================
// #4 escalation nativo — FASE 2: CONSEGNA IN OMBRA (sink di prova, YWAlert resta il motore)
// ============================================================================

/// On-call nativo di un gruppo a `now`, risolto con la stessa catena dello shadow (rotation
/// stateful + override), ma restituendo i recapiti (telefono/email) per costruire la ladder.
/// Richiede una sessione YWAlert (ground-truth per semina rotation + override), coerente con la
/// filosofia shadow: YWAlert è il motore in parallelo finché non si prova la parità.
/// On-call nativo di un gruppo a `now`, con i recapiti per la ladder. SELF-SUFFICIENT:
/// gli override/assenze sono quelli NATIVI (app.esc_overrides); la rotation è seminata
/// nativamente (posizione 0 deterministica) quando non c'è una sessione YWAlert.
/// `_ga` è un parametro vestigiale (ground-truth GoAlert esterno, ora sempre None dopo il ritiro):
/// mantenuto nella firma per compat dei chiamanti, non più usato.
/// `seed=true` SOLO nella consegna (persiste la semina rotation). Le letture (on-call-now, slack,
/// status-update, bundle) passano `seed=false`: calcolano l'on-call senza MAI mutare lo stato.
async fn esc_native_oncall_contacts(
    st: &AppState, tid: Uuid, gid: Uuid, _ga: Option<(&str, &str)>, now: chrono::DateTime<chrono::Utc>, seed: bool,
) -> Vec<(Uuid, Option<String>, String)> {
    let Some(g) = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
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
    // override/assenze NATIVE (indipendenti da YWAlert)
    sched.overrides = esc_native_overrides(st, tid, gid, &email_of).await;
    let rot_member = match &sched.rotation {
        Some(rot) => {
            // seed rotation NATIVO (YWAlert ritirato: nessun ground-truth esterno).
            let gae: Vec<String> = vec![];
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
    let Some(g) = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let days: i64 = q.get("days").and_then(|d| d.parse().ok()).unwrap_or(30);
    let now = st.clock.now();
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

/// Temporary schedule attivo (parità YWAlert): se un gruppo ha una pianificazione temporanea che
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

/// Turni di rotazione CALCOLATI di un utente (parità YWAlert calendar): per ogni gruppo 'turni'
/// di cui è membro, cammina i boundary della rotation su `days` giorni e ritorna i segmenti in cui
/// l'utente è on-call. Usa lo stato persistente (esc_rotation_state) se presente, altrimenti semina
/// nativamente alla posizione 0. Indipendente da YWAlert.
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
    let groups = sqlx::query("SELECT g.id,g.name,g.oncall_mode,g.oncall_type,g.oncall_shift,g.coverage_mode,g.cov_start,g.cov_end,g.cov_days,g.handoff_time,g.handoff_day
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
/// Twilio LIVE se configurato, altrimenti sink/dry (dev). Nessuna dipendenza da YWAlert.
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

/// Policy di consegna di un GRUPPO. Parità YWAlert: se il gruppo ha step personalizzati
/// (app.esc_policy_steps) usa quelli (sequenza arbitraria canale+attesa + repeat); altrimenti
/// torna alla ladder derivata dalle impostazioni vocali del tenant (retro-compatibile).
/// Target di uno step di policy: canale + CHI notificare (parità Escalation Policies YWAlert).
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

/// Consegna le rung DOVUTE di uno step per gli on-call. Parità YWAlert a 2 livelli:
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
            // Box::pin: il future di esc_dispatch (client reqwest/TLS) ha frame grande; su heap tiene
            // lo stack del runner escalation shallow al picco di concorrenza.
            let res = Box::pin(esc_dispatch(st, chn, &to, text, &body, tr.live_on, tr.live_cfg, tr.slack_token, tr.relay, tr.sink, tr.public_base)).await;
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
    let res = Box::pin(esc_dispatch(st, Channel::Slack, channel_id, text, &body, tr.live_on, tr.live_cfg, tr.slack_token, tr.relay, tr.sink, tr.public_base)).await;
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

/// Massimo incidenti processati per tenant in UN tick di consegna. Oltre questo si rimanda al tick
/// successivo (il worker gira di continuo): bound esplicito del lavoro/future per giro, così un
/// accumulo di incidenti aperti non fa crescere il carico di un singolo task fino all'overflow.
/// Gli incidenti NON processati in questo giro restano aperti e vengono ripresi al prossimo (ordine
/// per opened_at: i più vecchi prima, nessuna starvation sugli allarmi di sicurezza).
const ESC_MAX_INCIDENTS_PER_TICK: i64 = 200;

/// Processa UN incidente: policy di gruppo, macchina a stati (tick), consegna rung/Slack, avanzamento
/// stato. Estratta dal runner e chiamata `Box::pin` così il frame del runner resta piccolo e ogni
/// incidente è pollato da uno stack shallow (anti stack-overflow sotto accumulo). Ritorna il numero
/// di consegne effettuate.
#[allow(clippy::too_many_arguments)]
async fn esc_process_incident(
    st: &AppState, tid: Uuid, inc: &sqlx::postgres::PgRow,
    voice_on: bool, voice_min: i64, repeat: i64, engine: &str, live_on: bool,
    ga_opt: Option<(&str, &str)>, now: chrono::DateTime<chrono::Utc>, tr: &EscTransport<'_>,
) -> i64 {
    use crate::delivery::Channel;
    use crate::escalation::{self, AlertState, EscAction};
    let iid: Uuid = inc.get("id");
    let gid: Uuid = inc.get("group_id");
    let etype: String = inc.get("etype");
    let istate: String = inc.get("state");
    let mut delivered = 0i64;

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
            if cur_step >= 0 && !matches!(state.status, crate::escalation::EscStatus::Acked | crate::escalation::EscStatus::Closed) {
                let spec = specs.get(cur_step as usize).cloned().unwrap_or(EscStepTarget{ channel:Channel::Sms, kind:"oncall".into(), group:None, user:None, slack:None });
                let (oncall, slack_ch) = esc_step_recipients(st, tid, gid, &spec, ga_opt, now).await;
                let sstr = iid.to_string(); let short = &sstr[..8];
                let text = format!("Allarme {etype} (incidente {short})");
                delivered += esc_deliver_step_rungs(st, tid, iid, cur_step, fired_at, rc, spec.channel, &oncall, now, &text, tr).await;
                if let Some(sc) = slack_ch { delivered += esc_deliver_slack_rung(st, tid, iid, cur_step, rc, &sc, &text, tr).await; }
            }
        }
        EscAction::FireStep(idx, _) => {
            // repeat: tick ritorna FireStep(0) ripartendo dall'ultimo step -> nuovo giro.
            let is_repeat_restart = idx == 0 && cur_step == last_step && cur_step >= 0;
            let new_rc = if is_repeat_restart { rc + 1 } else { rc };
            let attempt = new_rc; // il giro di repeat fa parte della chiave di idempotenza
            let spec = specs.get(idx).cloned().unwrap_or(EscStepTarget{ channel:Channel::Sms, kind:"oncall".into(), group:None, user:None, slack:None });
            let (oncall, slack_ch) = esc_step_recipients(st, tid, gid, &spec, ga_opt, now).await;
            let sstr = iid.to_string(); let short = &sstr[..8];
            let text = format!("Allarme {etype} (incidente {short})");
            // rung per-utente (regole di notifica o fallback al canale dello step), ritardi inclusi.
            delivered += esc_deliver_step_rungs(st, tid, iid, idx as i32, now, attempt, spec.channel, &oncall, now, &text, tr).await;
            // step con target Slack: posta sul canale dello step.
            if let Some(sc) = slack_ch { delivered += esc_deliver_slack_rung(st, tid, iid, idx as i32, attempt, &sc, &text, tr).await; }
            // Slack per-gruppo (canale del gruppo) sul PRIMO step 'oncall' se configurato.
            if idx == 0 && spec.kind == "oncall" {
                if let Some(sr) = sqlx::query("SELECT slack_channel_id,slack_notify FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
                    .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() {
                    let ch: Option<String> = sr.try_get("slack_channel_id").ok().flatten();
                    let notify: String = sr.try_get("slack_notify").unwrap_or_else(|_| "off".into());
                    if notify != "off" { if let Some(ch) = ch.filter(|s| !s.trim().is_empty()) {
                        delivered += esc_deliver_slack_rung(st, tid, iid, idx as i32, attempt, &ch, &text, tr).await;
                    } }
                }
            }
            // avanza/semina lo stato alla rung appena sparata (repeat_count aggiornato)
            let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,repeat_count,status) VALUES($1,$2,$3,$4,$5,'unack') ON CONFLICT (tenant_id,incident_id) DO UPDATE SET step=$3,step_fired_at=$4,repeat_count=$5,updated_at=now()")
                .bind(tid).bind(iid).bind(idx as i32).bind(now).bind(new_rc).execute(&st.owner_pool).await;
            let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_delivery',$3)")
                .bind(tid).bind(iid).bind(json!({"step":idx,"channel":spec.channel.as_str(),"target":spec.kind,"attempt":attempt,"live":live_on,"engine":engine})).execute(&st.owner_pool).await;
        }
    }
    delivered
}

/// Runner di consegna YWAlert (nativo) per un tenant. Motore SEMPRE autoritativo: usa gli adapter
/// live (Twilio/Slack/relay se configurati), altrimenti il sink webhook, altrimenti DRY in dev;
/// l'ack applica la transizione sull'incidente. Idempotenza su `esc_delivery_log`; i fallimenti si
/// ritentano a oltranza sugli invii di sicurezza.
pub async fn escalation_delivery_run_tenant(st: &AppState, tid: Uuid) -> (i64, i64) {
    // MAINTENANCE nativo: finestra attiva -> sospendi le consegne (gli incidenti continuano a
    // essere creati; la ladder riparte a fine finestra). Indipendente da YWAlert.
    if maintenance_active(st, tid).await {
        // segna gli incidenti aperti come "in bundle": a fine finestra verrà inviato UN riepilogo.
        let _ = sqlx::query("UPDATE app.incidents SET bundled_at=now() WHERE tenant_id=$1 AND state<>'chiuso' AND bundled_at IS NULL")
            .bind(tid).execute(&st.owner_pool).await;
        return (0,0);
    }
    // USCITA manutenzione: se ci sono incidenti accumulati, invia UN solo riepilogo (parità YWAlert
    // alert bundling) agli on-call del gruppo di default, poi riprende l'escalation normale.
    let bundled: i64 = sqlx::query_scalar("SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state<>'chiuso' AND bundled_at IS NOT NULL")
        .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    if bundled > 0 {
        let dg: Option<Uuid> = sqlx::query_scalar("SELECT default_group_id FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
            .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
        if let Some(dg) = dg {
            let now0 = st.clock.now();
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
    // Motore nativo autoritativo: nessun ground-truth esterno (YWAlert ritirato). Seed rotation nativo.
    let ga_opt: Option<(&str,&str)> = None;
    // riferimento temporale del motore nativo = orologio controllabile: step_fired_at viene SCRITTO
    // e confrontato con lo stesso `now` (vedi escalation::tick), quindi e' auto-coerente e testabile
    // con TEST_CLOCK (in prod offset=0 -> identico al wall-clock).
    let now = st.clock.now();
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
            AND COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id) IS NOT NULL
          ORDER BY i.opened_at ASC
          LIMIT $2")
        .bind(tid).bind(ESC_MAX_INCIDENTS_PER_TICK).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let tr = EscTransport{ from:&from, sink:&sink, relay:&relay, slack_token:&slack_token, public_base:&public_base, live_on, live_cfg: live_cfg.as_ref() };
    let (mut seen, mut delivered) = (0i64, 0i64);
    // Un incidente per volta, pollato da uno stack shallow: `Box::pin` sposta il future (grande) del
    // processing su heap, così il frame di QUESTO runner resta minimo e la profondità è costante
    // qualunque sia il numero di incidenti (anti stack-overflow sotto accumulo).
    for inc in &incs {
        seen += 1;
        delivered += Box::pin(esc_process_incident(
            st, tid, inc, voice_on, voice_min, repeat, &engine, live_on, ga_opt, now, &tr,
        )).await;
    }
    (seen, delivered)
}

/// F3 — Annunci Slack dell'on-call (parità YWAlert "On-Call Notifications"): per ogni gruppo con
/// `slack_notify` != off e un canale Slack, posta chi è reperibile. `onchange` = a ogni handoff
/// (cambia lo shift_start della rotation); `HH:MM` = una volta al giorno dopo quell'ora.
/// Best-effort: senza token Slack live non fa nulla.
pub async fn slack_announce_scan(st: &AppState) {
    let token = std::env::var("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    if token.trim().is_empty() { return; }
    // wall-clock: annuncio verso Slack (sistema esterno) + cadenza giornaliera HH:MM e
    // slack_last_announce (SQL) sono su tempo reale; il TEST_CLOCK non deve spammare Slack.
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
/// incidenti aperti (qualunque modulo li produca: safety/IoT/Traccar).
pub async fn escalation_delivery_scan(st: &AppState) {
    let tids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT tenant_id FROM app.incidents WHERE state <> 'chiuso'")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    // Per-tenant su task DEDICATO: il future (profondo) del tenant viene pollato dal runtime dalla
    // cima dello stack del task, non annidato nel frame dello scan -> isolamento di profondità.
    // Await sequenziale del JoinHandle: UN tenant alla volta, così non si reintroduce il picco di
    // concorrenza che causava lo stack-overflow.
    for tid in tids {
        let st2 = st.clone();
        let _ = tokio::spawn(async move { escalation_delivery_run_tenant(&st2, tid).await }).await;
    }
}

/// Riepilogo consegne in ombra (admin): conteggi per stato + ultime consegne.
pub async fn escalation_delivery_report(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
        "note":"Consegna FASE 2 in OMBRA: sink di prova, nessun carrier reale. YWAlert resta il motore. 'dry' = nessun sink configurato."})))
}

/// Trigger manuale (admin) di un giro di consegna in ombra sul proprio tenant.
pub async fn escalation_delivery_run(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    let (seen, delivered) = escalation_delivery_run_tenant(&st, tid).await;
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
/// `incidents.state` (YWAlert resta il motore). Col gate `YW_ESC_ACK_APPLY=1` (cutover) applica
/// la transizione reale (preso_in_carico / chiuso).
async fn esc_apply_ack(st: &AppState, tid: Uuid, iid: Uuid, action: &str, via: &str) {
    let status = if action == "close" { "closed" } else { "ack" };
    let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,status) VALUES($1,$2,0,now(),$3) ON CONFLICT (tenant_id,incident_id) DO UPDATE SET status=$3,updated_at=now()")
        .bind(tid).bind(iid).bind(status).execute(&st.owner_pool).await;
    // motore YWAlert nativo: engine='native' (default di prodotto) -> applica la transizione reale.
    // Il gate globale YW_ESC_ACK_APPLY=1 resta per forzarla anche su eventuali tenant non-native.
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
// Parità Services YWAlert — S1 heartbeat monitor, S2 email ingress, S3 bulk, S4 on-call-now (nativi)
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
    // wall-clock: last_ping_at e' scritto con now() SQL (wall); il timeout si misura sul wall.
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
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
    // dedup (parità YWAlert email): chiave custom (+key) oppure firma STABILE dell'oggetto, così email
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
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
    if !has_cap(&s,"incidents.handle") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let acl: Option<Vec<Uuid>> = allowed_sites(&st, &s).await.map(|h| h.into_iter().collect());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // solo gli incidenti 'aperto' visibili all'ACL dell'operatore. Gli incidenti senza sede NON sono
    // visibili a un operatore ristretto (coerente con guard_incident_site): quindi esclusi dal bulk.
    // tenant_id ESPLICITO oltre alla RLS (vedi incidents_close_all).
    let rows = match &acl {
        Some(sites) => sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE tenant_id=$2 AND state='aperto' AND site_id = ANY($1) RETURNING id")
            .bind(sites).bind(tid).fetch_all(&mut *tx).await.map_err(ise)?,
        None => sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE tenant_id=$1 AND state='aperto' RETURNING id")
            .bind(tid).fetch_all(&mut *tx).await.map_err(ise)?,
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
    if !has_cap(&s,"incidents.handle") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let acl: Option<Vec<Uuid>> = allowed_sites(&st, &s).await.map(|h| h.into_iter().collect());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // Incidenti senza sede esclusi per operatore ristretto (coerente con guard_incident_site).
    // tenant_id ESPLICITO oltre alla RLS: un bulk destructive non deve MAI poter uscire dal tenant,
    // nemmeno se il ruolo DB fosse mal configurato (owner/BYPASSRLS).
    let rows = match &acl {
        Some(sites) => sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$1,closed_at=now(),version=version+1 WHERE tenant_id=$3 AND state<>'chiuso' AND site_id = ANY($2) RETURNING id,correlation,etype")
            .bind(&b.outcome).bind(sites).bind(tid).fetch_all(&mut *tx).await.map_err(ise)?,
        None => sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$1,closed_at=now(),version=version+1 WHERE tenant_id=$2 AND state<>'chiuso' RETURNING id,correlation,etype")
            .bind(&b.outcome).bind(tid).fetch_all(&mut *tx).await.map_err(ise)?,
    };
    for r in &rows {
        let iid: Uuid = r.get("id"); let correlation: String = r.get("correlation"); let etype: String = r.get("etype");
        // trail per-incidente come la chiusura singola
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
            .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":b.outcome,"by":s.email,"bulk":true})).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'close',$2)")
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
    let now = st.clock.now();
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
    let oncall = esc_native_oncall_contacts(st, tid, gid, None, st.clock.now(), false).await;
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
