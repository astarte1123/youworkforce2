use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, Response, sse::{Sse, Event as SseEvent}}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::convert::Infallible;
use uuid::Uuid;
use futures::stream::Stream;
use crate::state::{AppState, Session, TenantEvent};
use crate::{auth, db};
pub(crate) use crate::superadmin::*; // split: Piano di controllo (superadmin): tenant, moduli, monitoraggio di sistema, chiave dei segreti, canali di consegna, account e MFA, impersonazione
pub(crate) use crate::devices::*; // split: Dispositivi e localizzazione: ingest nativo (GPS, OsmAnd, Teltonika, JSON), comandi, geofence, report, mappe, beacon e piantine, catalogo, onboarding
pub(crate) use crate::iot::*; // split: IoT nativo: telemetria, attributi, comandi, blueprint, MQTT, alert e monitoraggio IoT, scenari
pub(crate) use crate::field::*; // split: Lavoro sul campo: clienti, interventi, listino, consuntivi, registro ore, ronde, forza lavoro e turni, cantieri
pub(crate) use crate::ai::*; // split: Assistente e processi AI (youworker.ai): assistente, processi nei moduli, agente con strumenti, report AI, connessione e funzioni AI
pub(crate) use crate::oncall::*; // split: Escalation nativa e reperibilità: impostazioni, recapiti, assenze, manutenzione, calendari, consegna, ack Twilio, servizi

pub(crate) type R = Result<Json<Value>, (StatusCode, String)>;
pub(crate) fn err(c: StatusCode, m: &str) -> (StatusCode, String) { (c, m.to_string()) }

/// SL-13: allarmi SALVA-VITA. Non si sospendono con la manutenzione e non si chiudono in blocco senza
/// una conferma esplicita: dietro c'è una persona che potrebbe essere in pericolo.
pub(crate) const LIFE_SAFETY: &[&str] = &["sos", "man_down", "immobilita", "mancato_checkin", "perdita_contatto"];
pub(crate) const CRITICAL: &[&str] = &["sos", "man_down", "immobilita", "mancato_checkin", "sensore_critico",
    "manomissione_badge", "perdita_alimentazione", "jamming", "perdita_contatto",
    "batteria_critica", "heartbeat_down", "email_alarm", "geofence_ingresso", "geofence_uscita",
    "ronda_mancata"];

/// Allarme del dispositivo GPS/badge -> etype normalizzato. Safety-first defaults for lone workers.
/// `sos_as` lets a tenant route the SOS alarm to `sos` or `man_down`.
/// Returns the normalized etype; whether it is critical is decided by CRITICAL.
pub(crate) fn gps_alarm_etype(alarm: &str, sos_as: &str) -> String {
    match alarm {
        "sos" | "panic" | "sosButton" => sos_as.to_string(),
        // caduta / uomo a terra: accetta sia i token dei protocolli GPS sia l'etype canonico già risolto
        // (es. GH-5200 io_alarm_map 242->"man_down"), così la semantica salva-vita non si perde.
        "fallDown" | "accident" | "man_down" | "mandown" | "falldown" => "man_down".into(),
        // immobilità / assenza di movimento
        "immobilita" | "immobility" | "noMovement" | "no_movement" | "deadman" => "immobilita".into(),
        "removing" | "tampering" | "bonnet" => "manomissione_badge".into(), // badge rimosso/manomesso
        "powerCut" | "powerOff" => "perdita_alimentazione".into(),
        "jamming" | "gpsAntennaCut" => "jamming".into(),
        "lowBattery" | "lowPower" => "batteria_bassa".into(),       // warning, non critico
        "" => "device_alarm".into(),
        _ => "device_alarm".into(),                                  // registrato, non critico
    }
}

/// Default handling + etype base per tipo evento dei dispositivi GPS (nomi standard del
/// formato JSON di ingest). Safety-first per il lavoratore isolato.
/// Ritorna (etype_base, default_handling ∈ ignora|registra|critico).
pub(crate) fn gps_event_default(evt_type: &str) -> (&'static str, &'static str) {
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
        "deviceOnline" | "deviceUnknown" | "queuedCommandSent" => ("device_event", "ignora"),
        _ => ("device_event", "registra"),
    }
}
/// Tipi evento esposti nella policy (per la UI). Esclusi quelli con
/// gestione dedicata propria (alarm, deviceOffline, deviceStopped, geofenceEnter/Exit).
pub const GPS_POLICY_EVENTS: &[&str] = &[
    "unaccompaniedMotion","deviceInactive","proximityEnter","proximityExit","deviceOverspeed",
    "deviceFuelDrop","deviceFuelIncrease","ignitionOn","ignitionOff","maintenance","driverChanged",
    "media","deviceMoving","geofenceCrossed","commandResult","deviceOnline","deviceUnknown"];

/// Mappa un evento GPS (tipo + eventuale alarm + batteria) nell'etype normalizzato,
/// applicando la config per-tenant `gps/alarm_map` (incl. `events`). Usata da tutti i canali
/// d'ingest nativi (JSON HTTP, OsmAnd, Teltonika), così producono lo stesso incidente. Ritorna "ignore"
/// quando la policy è `ignora` (il chiamante salta la persistenza).
pub fn map_gps_etype(amap: &Value, evt_type: &str, alarm: Option<&str>, battery: Option<f64>) -> String {
    let sos_as = amap.get("sos_as").and_then(|v| v.as_str()).unwrap_or("sos");
    let offline_critical = amap.get("offline_critical").and_then(|v| v.as_str()).map(|s| s=="si").unwrap_or(false);
    let stopped_immobilita = amap.get("stopped_as_immobilita").and_then(|v| v.as_str()).map(|s| s=="si").unwrap_or(false);
    let battery_min = amap.get("battery_min").and_then(|v| v.as_i64()).unwrap_or(15);
    // policy batteria configurabile dall'admin: off (ignora) | warning (evento, default) | critico (allarme)
    let battery_as = amap.get("battery_as").and_then(|v| v.as_str()).unwrap_or("warning");
    let mut etype: String = if let Some(a) = alarm {
        gps_alarm_etype(a, sos_as)
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
        let (base, def) = gps_event_default(evt_type);
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
            "off" => "device_event".into(),      // non segnalata
            "critico" => "batteria_critica".into(), // allarme + escalation (in CRITICAL)
            _ => "batteria_bassa".into(),          // warning registrato, non critico
        };
    }
    etype
}

// ----------------------------- helpers -----------------------------
pub(crate) fn get_cookie(h: &HeaderMap, name: &str) -> Option<String> {
    let raw = h.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let p = part.trim();
        if let Some(v) = p.strip_prefix(&format!("{name}=")) { return Some(v.to_string()); }
    }
    None
}

pub(crate) async fn session_of(state: &AppState, h: &HeaderMap) -> Option<Session> {
    let tok = get_cookie(h, "yw_session")?;
    // persistent sessions: survive core restarts; expired rows are ignored.
    let r = sqlx::query(
        // TA-2: per le sessioni di tenant ruolo ed esistenza dell'utente si rileggono a OGNI richiesta
        // (prima il ruolo era copiato al login e valeva 12 h anche dopo retrocessione o eliminazione).
        // Utente eliminato o anonimizzato -> sessione non valida. Impersonazione: ruolo tenant_admin.
        "SELECT s.user_id, s.tenant_id, s.email, s.is_super, s.impersonator,
                CASE WHEN s.tenant_id IS NULL OR s.impersonator IS NOT NULL THEN s.role ELSE u.role END AS role
         FROM app.sessions s
         LEFT JOIN app.tenants t ON t.id = s.tenant_id
         LEFT JOIN app.users u ON u.id = s.user_id AND u.tenant_id = s.tenant_id
         WHERE s.token=$1 AND s.expires_at > now()
           AND (s.tenant_id IS NULL OR s.impersonator IS NOT NULL OR t.status IS DISTINCT FROM 'sospeso')
           AND (s.tenant_id IS NULL OR (u.id IS NOT NULL AND u.anonymized_at IS NULL))
           -- SA-12: superadmin (anche in attivazione MFA) disattivato -> sessione non valida
           AND (s.tenant_id IS NOT NULL OR EXISTS (SELECT 1 FROM app.superadmins sa WHERE sa.id=s.user_id AND NOT sa.disabled))")
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
pub(crate) async fn persist_session(state: &AppState, tok: &str, s: &Session) {
    let _ = sqlx::query("DELETE FROM app.sessions WHERE expires_at <= now()")
        .execute(&state.owner_pool).await;
    let _ = sqlx::query(
        "INSERT INTO app.sessions(token,user_id,tenant_id,email,role,is_super,impersonator,expires_at)
         VALUES($1,$2,$3,$4,$5,$6,$7, now() + interval '12 hours')")
        .bind(tok).bind(s.user_id).bind(s.tenant_id).bind(&s.email).bind(&s.role).bind(s.is_super).bind(&s.impersonator)
        .execute(&state.owner_pool).await;
}

/// Session cookie (HttpOnly) + CSRF cookie (readable by JS for the double-submit header).
/// SA-9: attributo `Secure` sui cookie quando la console è servita in HTTPS (produzione/staging).
/// `YW_COOKIE_SECURE=1` lo attiva; in locale (http://localhost) resta spento per i test e la webview.
pub(crate) fn cookie_secure() -> &'static str {
    static S: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    S.get_or_init(|| match std::env::var("YW_COOKIE_SECURE").ok().as_deref() {
        Some("1") | Some("true") | Some("yes") => "; Secure",
        _ => "",
    })
}
pub(crate) fn set_cookie_header(tok: &str, csrf: &str) -> HeaderMap {
    let mut m = HeaderMap::new();
    let sec = cookie_secure();
    m.append(header::SET_COOKIE,
        format!("yw_session={tok}; Path=/; HttpOnly; SameSite=Lax{sec}").parse().unwrap());
    m.append(header::SET_COOKIE,
        format!("yw_csrf={csrf}; Path=/; SameSite=Lax{sec}").parse().unwrap());
    m
}

pub(crate) fn cookie_val(raw: &str, name: &str) -> Option<String> {
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
        // SA-15: tutti gli endpoint di test allo stesso modo (rispondono 403 se TEST_CLOCK è spento)
        || path.starts_with("/api/_test/") || path == "/health";
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
// Due chiavi per tentativo (SA-4):
//   - account+IP  `[slug|]email|ip`  5 fallimenti / 300 s -> blocco 300 s (rallenta il singolo client);
//   - account     `acct|[slug|]email` 20 fallimenti / 300 s -> blocco 300 s, INDIPENDENTE dall'IP:
//     cambiare indirizzo (o falsificare X-Forwarded-For) non dà tentativi illimitati sulla stessa utenza.
// L'IP non si legge MAI da intestazioni del client: lo calcola `client_ip_mw` dal peer TCP.
pub(crate) const RL_IP_MAX: i32 = 5;
pub(crate) const RL_ACCT_MAX: i32 = 20;
/// Intestazione INTERNA con l'IP del client; quella eventualmente inviata dal client è scartata.
pub const CLIENT_IP_HDR: &str = "x-yw-client-ip";

/// IP del client: il peer TCP; solo se il peer è un proxy fidato (loopback = console YW, oppure
/// `YW_TRUSTED_PROXIES`, elenco separato da virgole) si usa l'ULTIMO elemento di X-Forwarded-For,
/// cioè quello aggiunto dal proxy stesso (gli elementi precedenti li scrive il client).
pub(crate) fn resolve_client_ip(peer: Option<std::net::IpAddr>, h: &HeaderMap, trusted: &[std::net::IpAddr]) -> String {
    let Some(p) = peer else { return String::new() };
    if p.is_loopback() || trusted.contains(&p) {
        let last = h.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok())
            .flat_map(|s| s.split(',')).map(|s| s.trim()).filter(|s| !s.is_empty()).last();
        if let Some(ip) = last.and_then(|s| s.parse::<std::net::IpAddr>().ok()) { return ip.to_string(); }
    }
    p.to_string()
}
pub(crate) fn trusted_proxies() -> &'static Vec<std::net::IpAddr> {
    static T: std::sync::OnceLock<Vec<std::net::IpAddr>> = std::sync::OnceLock::new();
    T.get_or_init(|| std::env::var("YW_TRUSTED_PROXIES").unwrap_or_default()
        .split(',').filter_map(|s| s.trim().parse().ok()).collect())
}
pub async fn client_ip_mw(mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let peer = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip());
    let ip = resolve_client_ip(peer, req.headers(), trusted_proxies());
    req.headers_mut().remove(CLIENT_IP_HDR);
    if let Ok(v) = axum::http::HeaderValue::from_str(&ip) { req.headers_mut().insert(CLIENT_IP_HDR, v); }
    next.run(req).await
}

/// [chiave account+IP, chiave account]. `scope` = "" (superadmin) o "slug|" (tenant).
pub(crate) fn rl_keys(h: &HeaderMap, scope: &str, email: &str) -> [String; 2] {
    let e = email.trim().to_lowercase();
    let ip = h.get(CLIENT_IP_HDR).and_then(|v| v.to_str().ok()).unwrap_or_default();
    [format!("{scope}{e}|{ip}"), format!("acct|{scope}{e}")]
}

/// Some(retry_after_secs) se una delle chiavi è bloccata.
pub(crate) async fn rl_check_all(st: &AppState, keys: &[String; 2]) -> Option<u64> {
    let mut out: Option<u64> = None;
    for k in keys { if let Some(r) = rl_check(st, k).await { out = Some(out.map_or(r, |o| o.max(r))); } }
    out
}
pub(crate) async fn rl_fail_all(st: &AppState, keys: &[String; 2]) {
    rl_fail(st, &keys[0], RL_IP_MAX).await;
    rl_fail(st, &keys[1], RL_ACCT_MAX).await;
}
pub(crate) async fn rl_clear_all(st: &AppState, keys: &[String; 2]) {
    for k in keys { rl_clear(st, k).await; }
}

/// Some(retry_after_secs) if the key is currently blocked.
pub(crate) async fn rl_check(st: &AppState, key: &str) -> Option<u64> {
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

pub(crate) async fn rl_fail(st: &AppState, key: &str, max: i32) {
    let _ = sqlx::query(
        "INSERT INTO app.login_throttle(key,count,reset_at,blocked_until)
         VALUES($1, 1, now() + interval '300 seconds', NULL)
         ON CONFLICT(key) DO UPDATE SET
           count = CASE WHEN now() > login_throttle.reset_at THEN 1 ELSE login_throttle.count + 1 END,
           reset_at = CASE WHEN now() > login_throttle.reset_at THEN now() + interval '300 seconds' ELSE login_throttle.reset_at END,
           blocked_until = CASE WHEN (CASE WHEN now() > login_throttle.reset_at THEN 1 ELSE login_throttle.count + 1 END) >= $2
                                THEN now() + interval '300 seconds' ELSE login_throttle.blocked_until END,
           updated_at = now()")
        .bind(key).bind(max).execute(&st.owner_pool).await;
}

pub(crate) async fn rl_clear(st: &AppState, key: &str) {
    let _ = sqlx::query("DELETE FROM app.login_throttle WHERE key=$1").bind(key).execute(&st.owner_pool).await;
}

pub(crate) fn too_many(retry_after: u64) -> Response {
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

pub(crate) async fn require_super(state: &AppState, h: &HeaderMap) -> Result<Session, (StatusCode, String)> {
    let s = session_of(state, h).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no session"))?;
    if !s.is_super { return Err(err(StatusCode::FORBIDDEN, "superadmin required")); }
    Ok(s)
}

/// SA-12: MFA obbligatorio per i superadmin. Solo in sviluppo (YW_SEED_DEMO=1) si può non imporlo
/// (YW_SUPERADMIN_MFA diverso da "on"), così i test automatici entrano con la sola password; un
/// account che ha attivato l'MFA lo usa comunque sempre.
pub fn superadmin_mfa_required() -> bool {
    let dev = std::env::var("YW_SEED_DEMO").ok().as_deref() == Some("1");
    !dev || std::env::var("YW_SUPERADMIN_MFA").ok().as_deref() == Some("on")
}
/// Superadmin completo OPPURE in attivazione dell'MFA (password già verificata).
pub(crate) async fn require_super_or_setup(state: &AppState, h: &HeaderMap) -> Result<Session, (StatusCode, String)> {
    let s = session_of(state, h).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no session"))?;
    if s.is_super || (s.tenant_id.is_none() && s.role == "superadmin_setup") { Ok(s) }
    else { Err(err(StatusCode::FORBIDDEN, "superadmin required")) }
}
pub(crate) async fn sa_audit(st: &AppState, actor: &str, action: &str, detail: Value) {
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,$2,$3)")
        .bind(format!("superadmin:{actor}")).bind(action).bind(detail).execute(&st.owner_pool).await;
}
/// Stato MFA dell'account corrente.
pub async fn super_mfa_status(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_super_or_setup(&st, &h).await?;
    let r = sqlx::query("SELECT totp_secret IS NOT NULL AS on, totp_enabled_at FROM app.superadmins WHERE id=$1").bind(s.user_id)
        .fetch_one(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"enabled": r.get::<bool,_>("on"), "enabled_at": r.try_get::<Option<Dt>,_>("totp_enabled_at").ok().flatten().map(|d| d.to_rfc3339()),
        "required": superadmin_mfa_required(), "setup_session": !s.is_super, "secrets_locked": !st.secrets.enabled(), "email": s.email})))
}
/// Avvia l'attivazione: nuovo segreto (cifrato, in attesa) + URI otpauth per l'app di autenticazione.
pub async fn super_mfa_setup(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_super_or_setup(&st, &h).await?;
    let secret = auth::totp_new_secret();
    let enc = st.secrets.encrypt(&secret).map_err(|_| err(StatusCode::CONFLICT, "chiave dei segreti bloccata: sbloccala prima di attivare l'MFA"))?;
    sqlx::query("UPDATE app.superadmins SET totp_pending=$2 WHERE id=$1").bind(s.user_id).bind(&enc).execute(&st.owner_pool).await.map_err(ise)?;
    let issuer = "YouWorkforce";
    let uri = format!("otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        issuer, urlenc(&s.email), secret, issuer);
    Ok(Json(json!({"secret": secret, "otpauth": uri})))
}
pub(crate) fn urlenc(v: &str) -> String { v.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{:02X}", b) }).collect() }
#[derive(Deserialize)] pub struct OtpReq { pub code: String }
/// Conferma l'attivazione con un codice dell'app. Una sessione «in attivazione» diventa completa.
pub async fn super_mfa_enable(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OtpReq>) -> axum::response::Response {
    let s = match require_super_or_setup(&st, &h).await { Ok(s) => s, Err(e) => return e.into_response() };
    let pend: Option<String> = sqlx::query_scalar("SELECT totp_pending FROM app.superadmins WHERE id=$1").bind(s.user_id)
        .fetch_one(&st.owner_pool).await.ok().flatten();
    let Some(pend) = pend else { return err(StatusCode::BAD_REQUEST, "avvia prima l'attivazione").into_response() };
    let secret = st.secrets.decrypt(&pend);
    let Some(step) = auth::totp_verify(&secret, &b.code, st.clock.now().timestamp(), None) else {
        return err(StatusCode::BAD_REQUEST, "codice non valido: controlla l'ora del telefono e riprova").into_response() };
    if sqlx::query("UPDATE app.superadmins SET totp_secret=totp_pending, totp_pending=NULL, totp_enabled_at=now(), totp_last_step=$2 WHERE id=$1")
        .bind(s.user_id).bind(step).execute(&st.owner_pool).await.is_err() { return err(StatusCode::INTERNAL_SERVER_ERROR, "salvataggio non riuscito").into_response(); }
    sa_audit(&st, &s.email, "superadmin_mfa_enabled", json!({"account":s.email})).await;
    if s.is_super { return Json(json!({"ok":true})).into_response(); }
    // sessione di attivazione -> nuova sessione completa (password + codice verificati)
    let (tok, csrf) = (auth::new_token(), auth::new_token());
    let sess = Session { user_id: s.user_id, tenant_id: None, email: s.email.clone(), role: "superadmin".into(), is_super: true, impersonator: None, caps: vec![] };
    if let Some(old) = get_cookie(&h, "yw_session") { let _ = sqlx::query("DELETE FROM app.sessions WHERE token=$1").bind(old).execute(&st.owner_pool).await; }
    persist_session(&st, &tok, &sess).await;
    (StatusCode::OK, set_cookie_header(&tok, &csrf), Json(json!({"ok":true,"csrf":csrf}))).into_response()
}
/// Elenco degli account superadmin.
pub async fn super_admins_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let me = require_super(&st, &h).await?;
    let rows = sqlx::query("SELECT id,email,disabled,created_at,created_by,last_login_at,totp_secret IS NOT NULL AS mfa FROM app.superadmins ORDER BY email")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"me": me.user_id, "mfa_required": superadmin_mfa_required(), "admins": rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "email": r.get::<String,_>("email"), "disabled": r.get::<bool,_>("disabled"),
        "mfa": r.get::<bool,_>("mfa"), "created_at": r.get::<Dt,_>("created_at").to_rfc3339(),
        "created_by": r.try_get::<Option<String>,_>("created_by").ok().flatten(),
        "last_login_at": r.try_get::<Option<Dt>,_>("last_login_at").ok().flatten().map(|d| d.to_rfc3339())})).collect::<Vec<_>>()})))
}
pub(crate) fn sa_pass_ok(p: &str) -> Result<(), (StatusCode,String)> {
    if p.chars().count() < 12 { return Err(err(StatusCode::BAD_REQUEST, "password di almeno 12 caratteri")); }
    Ok(())
}
#[derive(Deserialize)] pub struct NewSuper { pub email: String, pub password: String }
pub async fn super_admins_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewSuper>) -> R {
    let me = require_super(&st, &h).await?;
    let email = b.email.trim().to_lowercase();
    if !valid_email(&email) { return Err(err(StatusCode::BAD_REQUEST, "email non valida")); }
    sa_pass_ok(&b.password)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.superadmins(email,pass_hash,created_by) VALUES($1,$2,$3) RETURNING id")
        .bind(&email).bind(auth::hash_password(&b.password)).bind(format!("superadmin:{}", me.email))
        .fetch_one(&st.owner_pool).await.map_err(|e| conflict_or(e, "esiste già un superadmin con questa email"))?;
    sa_audit(&st, &me.email, "superadmin_create", json!({"account":email})).await;
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)] pub struct SuperUpd { #[serde(default)] pub disabled: Option<bool>, #[serde(default)] pub reset_mfa: Option<bool>, #[serde(default)] pub password: Option<String> }
pub async fn super_admins_update(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<SuperUpd>) -> R {
    let me = require_super(&st, &h).await?;
    let email: String = sqlx::query_scalar("SELECT email FROM app.superadmins WHERE id=$1").bind(id)
        .fetch_optional(&st.owner_pool).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "account inesistente"))?;
    if let Some(d) = b.disabled {
        if d && id == me.user_id { return Err(err(StatusCode::BAD_REQUEST, "non puoi disattivare il tuo account")); }
        if d {
            let active: i64 = sqlx::query_scalar("SELECT count(*) FROM app.superadmins WHERE NOT disabled AND id<>$1").bind(id).fetch_one(&st.owner_pool).await.map_err(ise)?;
            if active == 0 { return Err(err(StatusCode::BAD_REQUEST, "deve restare almeno un superadmin attivo")); }
        }
        sqlx::query("UPDATE app.superadmins SET disabled=$2 WHERE id=$1").bind(id).bind(d).execute(&st.owner_pool).await.map_err(ise)?;
        if d { let _ = sqlx::query("DELETE FROM app.sessions WHERE user_id=$1 AND tenant_id IS NULL").bind(id).execute(&st.owner_pool).await; }
        sa_audit(&st, &me.email, if d { "superadmin_disable" } else { "superadmin_enable" }, json!({"account":email})).await;
    }
    if b.reset_mfa == Some(true) {
        if id == me.user_id { return Err(err(StatusCode::BAD_REQUEST, "l'MFA del tuo account la cambi riattivandola, non azzerandola")); }
        sqlx::query("UPDATE app.superadmins SET totp_secret=NULL, totp_pending=NULL, totp_enabled_at=NULL, totp_last_step=NULL WHERE id=$1")
            .bind(id).execute(&st.owner_pool).await.map_err(ise)?;
        let _ = sqlx::query("DELETE FROM app.sessions WHERE user_id=$1 AND tenant_id IS NULL").bind(id).execute(&st.owner_pool).await;
        sa_audit(&st, &me.email, "superadmin_mfa_reset", json!({"account":email})).await;
    }
    if let Some(p) = b.password.as_deref() {
        if id == me.user_id { return Err(err(StatusCode::BAD_REQUEST, "per la tua password usa «cambia password»")); }
        sa_pass_ok(p)?;
        sqlx::query("UPDATE app.superadmins SET pass_hash=$2 WHERE id=$1").bind(id).bind(auth::hash_password(p)).execute(&st.owner_pool).await.map_err(ise)?;
        let _ = sqlx::query("DELETE FROM app.sessions WHERE user_id=$1 AND tenant_id IS NULL").bind(id).execute(&st.owner_pool).await;
        sa_audit(&st, &me.email, "superadmin_password_reset", json!({"account":email})).await;
    }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct OwnPass { pub current: String, pub new: String }
pub async fn super_own_password(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OwnPass>) -> R {
    let me = require_super(&st, &h).await?;
    let hash: String = sqlx::query_scalar("SELECT pass_hash FROM app.superadmins WHERE id=$1").bind(me.user_id).fetch_one(&st.owner_pool).await.map_err(ise)?;
    if !auth::verify_password(&b.current, &hash) { return Err(err(StatusCode::FORBIDDEN, "password attuale errata")); }
    sa_pass_ok(&b.new)?;
    sqlx::query("UPDATE app.superadmins SET pass_hash=$2 WHERE id=$1").bind(me.user_id).bind(auth::hash_password(&b.new)).execute(&st.owner_pool).await.map_err(ise)?;
    // le altre sessioni dello stesso account si chiudono
    if let Some(cur) = get_cookie(&h, "yw_session") {
        let _ = sqlx::query("DELETE FROM app.sessions WHERE user_id=$1 AND tenant_id IS NULL AND token<>$2").bind(me.user_id).bind(cur).execute(&st.owner_pool).await;
    }
    sa_audit(&st, &me.email, "superadmin_password_change", json!({"account":me.email})).await;
    Ok(Json(json!({"ok":true})))
}

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
    let key = rl_keys(&h, &format!("{slug}|"), &b.email);
    if let Some(ra) = rl_check_all(&st, &key).await { return too_many(ra); }
    // user lookup under tenant context
    let mut tx = match db::begin_tenant(&st.rt_pool, tid).await { Ok(t)=>t, Err(_)=>return ise_resp() };
    let urow = sqlx::query("SELECT id,pass_hash,role FROM app.users WHERE email=$1")
        .bind(&b.email).fetch_optional(&mut *tx).await.ok().flatten();
    let _ = tx.commit().await;
    match urow {
        Some(r) if auth::verify_password(&b.password, &r.get::<String,_>("pass_hash")) => {
            rl_clear_all(&st, &key).await;
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
            rl_fail_all(&st, &key).await;
            (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response()
        }
    }
}

pub async fn me(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = session_of(&st, &h).await.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no session"))?;
    // SA-5: sospensione programmata -> avviso all'amministratore (data + note del superadmin).
    let mut notice = Value::Null;
    if let (Some(tid), true) = (s.tenant_id, has_cap(&s, "admin") || s.impersonator.is_some()) {
        if let Ok(Some(r)) = sqlx::query("SELECT suspend_at, suspend_note FROM app.tenants WHERE id=$1 AND status='attivo' AND suspend_at IS NOT NULL")
            .bind(tid).fetch_optional(&st.owner_pool).await {
            notice = json!({"suspend_at": r.get::<chrono::DateTime<chrono::Utc>,_>("suspend_at"),
                            "note": r.try_get::<Option<String>,_>("suspend_note").ok().flatten()});
        }
    }
    // SA-6: avviso (mai blocco) se il tenant supera le licenze; solo a chi amministra
    let license_notice = match s.tenant_id {
        Some(tid) if has_cap(&s, "admin") || s.impersonator.is_some() => json!(license_overage(&st, tid).await),
        _ => json!([]),
    };
    Ok(Json(json!({"email":s.email,"role":s.role,"is_super":s.is_super,"tenant_id":s.tenant_id,"impersonator":s.impersonator,"caps":s.caps,"suspension_notice":notice,"license_notice":license_notice})))
}

pub async fn logout(State(st): State<AppState>, h: HeaderMap) -> impl IntoResponse {
    if let Some(tok) = get_cookie(&h, "yw_session") {
        let _ = sqlx::query("DELETE FROM app.sessions WHERE token=$1").bind(&tok).execute(&st.owner_pool).await;
    }
    let mut m = HeaderMap::new();
    let sec = cookie_secure();
    m.append(header::SET_COOKIE, format!("yw_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{sec}").parse().unwrap());
    m.append(header::SET_COOKIE, format!("yw_csrf=; Path=/; SameSite=Lax; Max-Age=0{sec}").parse().unwrap());
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
        // giri: la ronda mancata è un incidente (CA-12) -> Centrale ed escalation anche senza Sicurezza
        "incidents.view" | "incidents.handle" => &["loneworker","gps","iot","giri"],
        "escalation.manage"                    => &["loneworker","gps","iot","giri"],
        "devices.manage" | "devices.admin"     => &["loneworker","gps","iot"],
        "iot.manage"                           => &["iot","gps"],
        "plan"                                 => &["interventi","cantieri","giri","workforce"],
        "approve" | "timesheets.view_all"      => &["consuntivi"],
        "orders.view_all"                      => &["interventi"],
        "costs.view"                           => &["cantieri","interventi"],
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
    // CA-10/CA-11 (25/11/2026): visibilità prima legata al nome del ruolo «lavoratore»
    ("orders.view_all",      "Interventi: lettura di tutti gli ordini delle proprie sedi (non solo i propri)"),
    ("costs.view",           "Costi: budget, costi, SAL, fatture e prezzi dei materiali"),
    ("config.view",          "Lettura configurazione e diagnostica"),
    // Fase 4 (Tappa 2): aree amministrative, split di `admin`. Nessun ruolo le ha di default
    // (solo `tenant_admin` via `admin` super); assegnabili a ruoli custom per delega mirata.
    ("users.manage",         "Amministrazione: utenti, gruppi di persone, permessi, ruoli custom, sedi/squadre/asset"),
    ("devices.admin",        "Amministrazione dispositivi: censimento, token/onboarding, geofence, provisioning"),
    ("iot.manage",           "Dispositivi e IoT: allarmi dei dispositivi, regole sensori, allarmi da email"),
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
        "responsabile"  => vec!["approve","incidents.view","incidents.handle","timesheets.view_all","orders.view_all","costs.view"],
        "auditor"       => vec!["incidents.view","timesheets.view_all","config.view","orders.view_all","costs.view"],
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
/// TA-1: NON SI CONCEDE CIÒ CHE NON SI POSSIEDE. Chi gestisce utenti/permessi può assegnare (a un
/// ruolo, a un ruolo personalizzato, a un utente) solo permessi che ha già. `admin` li copre tutti:
/// solo un amministratore crea altri amministratori o tocca chi ha più permessi di lui.
pub(crate) fn require_grantable(actor: &Session, caps: &[String]) -> Result<(), (StatusCode, String)> {
    let missing: Vec<&str> = caps.iter().map(|c| c.as_str()).filter(|c| !has_cap(actor, c)).collect();
    if missing.is_empty() { return Ok(()); }
    let labels: Vec<String> = missing.iter().map(|c| cap_label(c)).collect();
    Err(err(StatusCode::FORBIDDEN, &format!("non puoi concedere permessi che non hai: {}", labels.join(", "))))
}
/// Etichetta leggibile di una capability (mai il codice grezzo in UI/errori).
pub(crate) fn cap_label(c: &str) -> String {
    CAPABILITIES.iter().find(|(k,_)| *k == c).map(|(_,d)| d.to_string()).unwrap_or_else(|| c.to_string())
}
/// Permessi effettivi di un utente del tenant (ruolo + override + ruoli personalizzati).
pub(crate) async fn user_caps(st: &AppState, tid: Uuid, uid: Uuid) -> Option<(String, Vec<String>)> {
    let role: String = sqlx::query_scalar("SELECT role FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let caps = effective_caps_user(st, tid, &role, uid).await;
    Some((role, caps))
}

pub(crate) const USER_ROLES: &[&str] = &["tenant_admin","operatore","pianificatore","responsabile","lavoratore","reperibile","auditor"];

/// #8 ACL: sedi visibili all'utente. None = tutte (admin/auditor/pianificatore o
/// operatore senza restrizioni); Some(set) = solo quelle sedi.
/// Sedi consentite all'utente. CA-9/CA-10 (25/11/2026): prima la restrizione valeva solo per i nomi
/// di ruolo operatore/lavoratore/reperibile, quindi un responsabile o un ruolo personalizzato con
/// sedi assegnate vedeva e approvava tutto. Ora: chiunque abbia sedi assegnate è ristretto a
/// quelle, tranne l'amministratore (`admin`). Nessuna sede assegnata = nessuna restrizione.
pub(crate) async fn allowed_sites(st: &AppState, s: &Session) -> Option<std::collections::HashSet<Uuid>> {
    if has_cap(s, "admin") { return None; }
    let rows = match sqlx::query_scalar::<_, Uuid>("SELECT site_id FROM app.operator_sites WHERE tenant_id=$2 AND user_id=$1")
        .bind(s.user_id).bind(s.tenant_id).fetch_all(&st.owner_pool).await {
        Ok(r) => r,
        // errore del DB: si chiude (nessuna sede), non si apre a tutte. Restano visibili solo gli
        // allarmi senza sede (SL-6), così un allarme non sparisce del tutto.
        Err(e) => { tracing::warn!("allowed_sites: lettura sedi fallita ({e}): accesso ristretto"); return Some(Default::default()); }
    };
    if rows.is_empty() { None } else { Some(rows.into_iter().collect()) }
}
/// SL-6: un incidente SENZA sede (SOS dall'app, check-in mancato di un lavoratore senza sede
/// assegnata) è visibile a TUTTI gli operatori, anche a quelli ristretti per sede: meglio un allarme
/// visto da più persone che un allarme che nessuno vede.
pub(crate) fn site_allowed(set: &Option<std::collections::HashSet<Uuid>>, site_id: Option<Uuid>) -> bool {
    match set { None => true, Some(s) => site_id.map(|x| s.contains(&x)).unwrap_or(true) }
}
/// Sede del lavoratore quando è univoca (una sola sede assegnata), altrimenti None.
pub(crate) async fn user_single_site(st: &AppState, tid: Uuid, uid: Uuid) -> Option<Uuid> {
    let v: Vec<Uuid> = sqlx::query_scalar("SELECT site_id FROM app.operator_sites WHERE tenant_id=$1 AND user_id=$2 LIMIT 2")
        .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if v.len() == 1 { Some(v[0]) } else { None }
}
/// ACL per-sito sulle AZIONI di incidente: un operatore ristretto a certe sedi non può
/// ack/chiudere/assegnare/annotare/leggere la timeline di incidenti di altre sedi (coerente
/// col filtro di lettura su list_incidents). Nessun effetto per ruoli non ristretti.
pub(crate) async fn guard_incident_site(st: &AppState, s: &Session, iid: Uuid) -> Result<(), (StatusCode,String)> {
    let allowed = allowed_sites(st, s).await;
    if allowed.is_none() { return Ok(()); }
    let site_id: Option<Uuid> = sqlx::query_scalar("SELECT site_id FROM app.incidents WHERE tenant_id=$1 AND id=$2")
        .bind(s.tenant_id.unwrap()).bind(iid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    if site_allowed(&allowed, site_id) { Ok(()) } else { Err(err(StatusCode::FORBIDDEN, "incidente fuori dalle tue sedi")) }
}

// UserSitesReq/get_user_sites/set_user_sites estratti in core/src/users.rs (Fase B).

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
    // SL-19: i tipi instradabili sono TUTTI quelli che aprono un allarme (prima la console ne elencava 11)
    let routable: Vec<Value> = CRITICAL.iter().map(|e| json!({"etype": e, "label": etype_label(e)})).collect();
    Ok(Json(json!({"groups":out,"routing":routes,"routable":routable})))
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
        .bind(tid).bind(b.name.trim()).fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "gruppo gia' presente"))?;
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
pub(crate) fn valid_hhmm(s: &str) -> bool { parse_hhmm(s).is_some() }
// Turni + copertura + slack del gruppo (effetto immediato sul motore nativo).
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
// ---- F3: policy di escalation multi-step del gruppo ----
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

// ---- F3: label/metadata sui gruppi (motore nativo labels) + ricerca ----
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
/// Ricerca gruppi per label (key, key+value) o testo nel nome. 
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

// ---- R1: Set Active — imposta SUBITO il reperibile di turno ----
#[derive(Deserialize)] pub struct SetActiveReq { pub user_id: Uuid }
pub async fn rotation_set_active(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, Json(b): Json<SetActiveReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // indice del membro nell'ordine della rotation (ORDER BY position)
    // AR-2: il gruppo arriva dal percorso -> sempre filtrato per tenant sul pool privilegiato
    let members: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM app.alarm_group_members WHERE group_id=$1 AND tenant_id=$2 ORDER BY position")
        .bind(gid).bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
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
    require_any_module(&st, s.tenant_id.unwrap(), &["workforce"]).await?; // AR-1: controllo del modulo dichiarato
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
    require_any_module(&st, s.tenant_id.unwrap(), &["workforce"]).await?; // AR-1: controllo del modulo dichiarato
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
    require_any_module(&st, s.tenant_id.unwrap(), &["workforce"]).await?; // AR-1: controllo del modulo dichiarato
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.oncall WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "reperibilita' inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---------------- Motore regole sensori (parametrico, da YouWorkforce) ----------------
pub(crate) fn rule_num(v: &Value) -> Option<f64> { v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())) }
pub(crate) fn rule_truthy(v: &Value) -> bool {
    v.as_bool().unwrap_or_else(|| v.as_f64().map(|n| n != 0.0)
        .unwrap_or_else(|| matches!(v.as_str().map(|s| s.to_ascii_lowercase()).as_deref(), Some("true")|Some("1")|Some("on")|Some("yes")|Some("si"))))
}
pub(crate) fn rule_matches(op: &str, thr: Option<f64>, v: &Value) -> bool {
    match op {
        "is_true" => rule_truthy(v),
        "is_false" => !rule_truthy(v),
        _ => match (thr, rule_num(v)) {
            (Some(t), Some(x)) => match op { ">=" => x>=t, "<=" => x<=t, ">" => x>t, "<" => x<t, "==" => (x-t).abs()<1e-9, _ => false },
            _ => false,
        }
    }
}
/// IO-1: gravità di un evento prodotto da una regola sensori (più alto = più grave).
pub fn rule_etype_rank(e: &str) -> i32 {
    match e {
        "man_down" => 50, "sensore_critico" => 40, "perdita_alimentazione" => 30, "batteria_critica" => 20,
        "telemetria" => 0,
        x if CRITICAL.contains(&x) || x.ends_with("_critico") => 30,
        _ => 10,
    }
}
/// A quale dispositivo si applica una valutazione (per l'ambito delle regole).
#[derive(Debug, Clone, Default)]
pub struct RuleCtx { pub device_id: Option<Uuid>, pub model: Option<String>, pub site: Option<Uuid>, pub scenario: Option<String> }
/// Una regola come la vede il motore (pura, testabile).
#[derive(Debug, Clone)]
pub struct RuleRow { pub scope_kind: String, pub scope_ref: Option<String>, pub metric: String, pub op: String,
    pub threshold: Option<f64>, pub etype: String, pub label: Option<String> }
/// Esito: la regola che ha deciso l'evento, con il valore letto.
#[derive(Debug, Clone)]
pub struct RuleHit { pub etype: String, pub metric: String, pub threshold: Option<f64>, pub value: Option<f64>, pub label: Option<String> }
pub fn rule_in_scope(kind: &str, sref: Option<&str>, ctx: &RuleCtx) -> bool {
    match (kind, sref) {
        ("tenant", _) => true,
        ("device", Some(r)) => ctx.device_id.map(|d| d.to_string() == r).unwrap_or(false),
        ("model", Some(r)) => ctx.model.as_deref() == Some(r),
        ("site", Some(r)) => ctx.site.map(|s| s.to_string() == r).unwrap_or(false),
        ("scenario", Some(r)) => ctx.scenario.as_deref() == Some(r),
        _ => false,
    }
}
/// Valuta TUTTE le regole applicabili (in ordine) e tiene la più grave; a parità vince la prima.
pub fn select_rule(rules: &[RuleRow], ctx: &RuleCtx, payload: &Value) -> Option<RuleHit> {
    let mut best: Option<(i32, RuleHit)> = None;
    for r in rules {
        if !rule_in_scope(&r.scope_kind, r.scope_ref.as_deref(), ctx) { continue; }
        let Some(v) = payload.get(&r.metric) else { continue; };
        if !rule_matches(&r.op, r.threshold, v) { continue; }
        let rank = rule_etype_rank(&r.etype);
        if best.as_ref().map(|(b, _)| rank > *b).unwrap_or(true) {
            best = Some((rank, RuleHit{ etype: r.etype.clone(), metric: r.metric.clone(), threshold: r.threshold,
                value: rule_num(v), label: r.label.clone() }));
        }
    }
    best.map(|(_, h)| h)
}
/// Contesto di un dispositivo (modello, sede, scenario) per l'ambito delle regole.
pub async fn rule_ctx(st: &AppState, tid: Uuid, dev: Option<Uuid>) -> RuleCtx {
    let Some(d) = dev else { return RuleCtx::default(); };
    match sqlx::query("SELECT model_code, site_id, config->>'scenario' AS scn FROM app.devices WHERE tenant_id=$1 AND id=$2")
        .bind(tid).bind(d).fetch_optional(&st.owner_pool).await.ok().flatten() {
        Some(r) => RuleCtx{ device_id: Some(d), model: r.try_get("model_code").ok().flatten(),
            site: r.try_get("site_id").ok().flatten(), scenario: r.try_get("scn").ok().flatten() },
        None => RuleCtx{ device_id: Some(d), ..Default::default() },
    }
}
/// Id del dispositivo (tenant, source, external_id), per dare il contesto alle regole.
pub async fn device_id_by_ext(st: &AppState, tid: Uuid, source: &str, ext: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT id FROM app.devices WHERE tenant_id=$1 AND source=$2 AND external_id=$3")
        .bind(tid).bind(source).bind(ext).fetch_optional(&st.owner_pool).await.ok().flatten()
}
/// Valuta le regole sensori del tenant sul payload telemetria per il dispositivo `dev` (se noto).
/// IO-1: tutte le regole abilitate nel loro ambito; vince la più grave. System scope.
pub async fn eval_sensor_rules_hit(st: &AppState, tid: Uuid, dev: Option<Uuid>, payload: &Value) -> Option<RuleHit> {
    let rows = sqlx::query("SELECT scope_kind,scope_ref,metric,op,threshold,etype,label FROM app.sensor_rules WHERE tenant_id=$1 AND enabled=true ORDER BY sort,created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.ok()?;
    let rules: Vec<RuleRow> = rows.iter().map(|r| RuleRow{
        scope_kind: r.get("scope_kind"), scope_ref: r.try_get("scope_ref").ok().flatten(),
        metric: r.get("metric"), op: r.get("op"), threshold: r.try_get("threshold").ok().flatten(),
        etype: r.get("etype"), label: r.try_get("label").ok().flatten() }).collect();
    let ctx = rule_ctx(st, tid, dev).await;
    select_rule(&rules, &ctx, payload)
}
pub async fn eval_sensor_rules(st: &AppState, tid: Uuid, dev: Option<Uuid>, payload: &Value) -> Option<String> {
    eval_sensor_rules_hit(st, tid, dev, payload).await.map(|h| h.etype)
}
/// Valida l'ambito di una regola; ritorna lo scope_ref normalizzato.
pub(crate) async fn validate_rule_scope(st: &AppState, tid: Uuid, kind: &str, sref: Option<&str>) -> Result<Option<String>, (StatusCode,String)> {
    let r = sref.map(|x| x.trim()).filter(|x| !x.is_empty());
    let bad = |m: &str| Err(err(StatusCode::BAD_REQUEST, m));
    match kind {
        "tenant" => Ok(None),
        "device" | "site" => {
            let Some(id) = r.and_then(|x| Uuid::parse_str(x).ok()) else { return bad("scegli il dispositivo o la sede a cui si applica la regola"); };
            let q = if kind == "device" { "SELECT EXISTS(SELECT 1 FROM app.devices WHERE tenant_id=$1 AND id=$2)" }
                    else { "SELECT EXISTS(SELECT 1 FROM app.sites WHERE tenant_id=$1 AND id=$2)" };
            let ok: bool = sqlx::query_scalar(q).bind(tid).bind(id).fetch_one(&st.owner_pool).await.map_err(ise)?;
            if ok { Ok(Some(id.to_string())) } else { bad("dispositivo o sede inesistente") }
        }
        "model" => {
            let Some(m) = r else { return bad("scegli il modello di dispositivo"); };
            let ok: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.device_models WHERE model_code=$1)").bind(m)
                .fetch_one(&st.owner_pool).await.map_err(ise)?;
            if ok { Ok(Some(m.to_string())) } else { bad("modello di dispositivo sconosciuto") }
        }
        "scenario" => {
            let Some(c) = r else { return bad("scegli lo scenario"); };
            if scenario_catalog(st).await.iter().any(|s| s.code == c) { Ok(Some(c.to_string())) } else { bad("scenario sconosciuto") }
        }
        _ => bad("ambito non valido"),
    }
}
pub(crate) const RULE_ETYPES: &[&str] = &["sensore_critico","batteria_critica","man_down","perdita_alimentazione","telemetria"];
pub(crate) const RULE_OPS: &[&str] = &[">=","<=",">","<","==","is_true","is_false"];
#[derive(Deserialize)]
pub struct SensorRuleReq { pub metric: String, pub op: String, pub threshold: Option<f64>, pub etype: String, pub label: Option<String>, pub sort: Option<i32>,
    /// IO-1: tenant (default) | device | model | site | scenario
    #[serde(default)] pub scope_kind: Option<String>, #[serde(default)] pub scope_ref: Option<String> }
pub async fn list_sensor_rules(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT r.id,r.metric,r.op,r.threshold,r.etype,r.label,r.enabled,r.sort,r.scope_kind,r.scope_ref,
            COALESCE(d.label,d.external_id) AS dev_name, si.name AS site_name, m.manufacturer||' '||m.model_name AS model_name
          FROM app.sensor_rules r
          LEFT JOIN app.devices d ON r.scope_kind='device' AND d.id::text=r.scope_ref
          LEFT JOIN app.sites si ON r.scope_kind='site' AND si.id::text=r.scope_ref
          LEFT JOIN app.device_models m ON r.scope_kind='model' AND m.model_code=r.scope_ref
          ORDER BY r.sort,r.created_at")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let cat = scenario_catalog(&st).await;
    let out: Vec<Value> = rows.iter().map(|r| {
        let kind: String = r.get("scope_kind"); let sref: Option<String> = r.try_get("scope_ref").ok().flatten();
        let scope_label = match kind.as_str() {
            "tenant" => "Tutti i dispositivi".to_string(),
            "device" => format!("Dispositivo: {}", r.try_get::<String,_>("dev_name").ok().unwrap_or_else(|| "(eliminato)".into())),
            "site" => format!("Sede: {}", r.try_get::<String,_>("site_name").ok().unwrap_or_else(|| "(eliminata)".into())),
            "model" => format!("Modello: {}", r.try_get::<String,_>("model_name").ok().unwrap_or_else(|| sref.clone().unwrap_or_default())),
            "scenario" => format!("Scenario: {}", sref.as_deref().and_then(|c| cat.iter().find(|s| s.code==c)).map(|s| s.name.to_string()).unwrap_or_else(|| sref.clone().unwrap_or_default())),
            _ => kind.clone() };
        json!({
        "id": r.get::<Uuid,_>("id"), "metric": r.get::<String,_>("metric"), "op": r.get::<String,_>("op"),
        "threshold": r.try_get::<f64,_>("threshold").ok(), "etype": r.get::<String,_>("etype"),
        "label": r.try_get::<String,_>("label").ok(), "enabled": r.get::<bool,_>("enabled"), "sort": r.get::<i32,_>("sort"),
        "scope_kind": kind, "scope_ref": sref, "scope_label": scope_label }) }).collect();
    Ok(Json(json!(out)))
}
pub async fn create_sensor_rule(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SensorRuleReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    if b.metric.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "metrica richiesta")); }
    if !RULE_OPS.contains(&b.op.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "operatore non valido")); }
    if !RULE_ETYPES.contains(&b.etype.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "etype non ammesso")); }
    let numeric = !matches!(b.op.as_str(), "is_true"|"is_false");
    if numeric && b.threshold.is_none() { return Err(err(StatusCode::BAD_REQUEST, "soglia richiesta per questo operatore")); }
    let kind = b.scope_kind.clone().unwrap_or_else(|| "tenant".into());
    let sref = validate_rule_scope(&st, tid, &kind, b.scope_ref.as_deref()).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.sensor_rules(tenant_id,metric,op,threshold,etype,label,sort,scope_kind,scope_ref) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(b.metric.trim()).bind(&b.op).bind(if numeric { b.threshold } else { None })
        .bind(&b.etype).bind(&b.label).bind(b.sort.unwrap_or(100)).bind(&kind).bind(&sref).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "sensor_rule_create", json!({"metric":b.metric,"op":b.op,"etype":b.etype})).await;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id")})))
}
pub async fn update_sensor_rule(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<SensorRuleReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    if b.metric.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "metrica richiesta")); }
    if !RULE_OPS.contains(&b.op.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "operatore non valido")); }
    if !RULE_ETYPES.contains(&b.etype.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "etype non ammesso")); }
    let numeric = !matches!(b.op.as_str(), "is_true"|"is_false");
    if numeric && b.threshold.is_none() { return Err(err(StatusCode::BAD_REQUEST, "soglia richiesta per questo operatore")); }
    let scope = match &b.scope_kind { Some(k) => Some((k.clone(), validate_rule_scope(&st, tid, k, b.scope_ref.as_deref()).await?)), None => None };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.sensor_rules SET metric=$2,op=$3,threshold=$4,etype=$5,label=$6,sort=COALESCE($7,sort),
                         scope_kind=CASE WHEN $8 THEN $9 ELSE scope_kind END, scope_ref=CASE WHEN $8 THEN $10 ELSE scope_ref END WHERE id=$1")
        .bind(id).bind(b.metric.trim()).bind(&b.op).bind(if numeric { b.threshold } else { None })
        .bind(&b.etype).bind(&b.label).bind(b.sort)
        .bind(scope.is_some()).bind(scope.as_ref().map(|s| s.0.clone())).bind(scope.and_then(|s| s.1))
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "regola inesistente")); }
    audit(&st, tid, &s.email, "sensor_rule_update", json!({"metric":b.metric,"op":b.op,"etype":b.etype})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_sensor_rule(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?; // AR-1: controllo del modulo dichiarato
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
                b.zone_name, b.floor, b.floorplan_id, b.x AS fp_x, b.y AS fp_y, fp.name AS floorplan_name,
                -- FE-1: l'utente corrente è stato avvisato per questo incidente (telefono, email o recapito)
                EXISTS (SELECT 1 FROM app.esc_delivery_log l
                         WHERE l.tenant_id=i.tenant_id AND l.incident_id=i.id
                           AND (l.to_value = me.email OR l.to_value = me.phone
                                OR l.to_value IN (SELECT cm.value FROM app.user_contact_methods cm
                                                   WHERE cm.tenant_id=i.tenant_id AND cm.user_id=me.id))) AS notified_me
         FROM app.incidents i
           LEFT JOIN app.users me ON me.id=$1 AND me.tenant_id=i.tenant_id
           LEFT JOIN app.sites s ON s.id=i.site_id
           LEFT JOIN app.users u ON u.id=i.assigned_user
           LEFT JOIN app.users w ON w.id=i.worker_user_id
           LEFT JOIN app.events e ON e.id=i.event_id
           LEFT JOIN app.devices d ON d.id=e.device_id
           LEFT JOIN app.users du ON du.id=d.assigned_user
           LEFT JOIN app.beacons b ON b.beacon_uid=e.beacon_uid AND b.tenant_id=i.tenant_id
           LEFT JOIN app.floorplans fp ON fp.id=b.floorplan_id
         -- SL-22: gli APERTI sempre tutti (un allarme non si nasconde mai); i chiusi solo i 300 più
         -- recenti (lo storico completo è nei report): l'elenco non cresce più senza limite
         WHERE i.state<>'chiuso'
            OR i.id IN (SELECT c.id FROM app.incidents c WHERE c.state='chiuso' ORDER BY c.opened_at DESC LIMIT 300)
         ORDER BY i.state<>'chiuso' DESC, i.priority DESC, i.opened_at DESC")
        .bind(s.user_id).fetch_all(&mut *tx).await.map_err(ise)?;
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
        "outcome": r.try_get::<String,_>("outcome").ok(),
        "notified_me": r.try_get::<bool,_>("notified_me").unwrap_or(false),
        "assigned_to_me": r.try_get::<Uuid,_>("assigned_user").ok() == Some(s.user_id) })).collect();
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
pub struct CloseReq { pub outcome: String, #[serde(default)] pub include_life_safety: bool }
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
    // chiusura: il runner nativo smette di notificare (riga outbox «close», stesso dedup = correlation)
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
    app_seen(&st, tid, dev_id).await; // Q4: segno di vita dell'app
    immobility_note_movement(&st, tid, dev_id, lat, lon, None, now).await; // SL-2
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
    // presenza geofence (dentro/fuori)
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
    // SL-20: l'SOS va accettato solo se qualcuno lo vede in Centrale (matrice modules::FEATURE_MODULES)
    if !any_module_active(&st, tid, crate::modules::feature_modules("app_sos")).await {
        return Err(err(StatusCode::FORBIDDEN, "SOS dall'app non attivo per la tua azienda: in caso di pericolo chiama subito i soccorsi"));
    }
    let lat = b.get("lat").and_then(|v| v.as_f64());
    let lon = b.get("lon").and_then(|v| v.as_f64());
    let accuracy = b.get("accuracy").and_then(|v| v.as_f64());
    let correlation = Uuid::new_v4().to_string();
    let now = st.clock.now();
    let (pri, repeat) = sos_escalation(&st, tid).await;
    // Fase B: l'SOS è attribuito al device del lavoratore (bind sessione↔device).
    let dev_id = worker_app_device(&st, tid, s.user_id, &s.email).await.ok().map(|(id,_)| id);
    // SL-10: l'app manda una chiave stabile per ogni pressione (`client_key`) e la ripete nei nuovi
    // tentativi dopo un errore di rete: stesso SOS, nessun doppione. Senza chiave: un evento per richiesta.
    let client_key: Option<String> = b.get("client_key").and_then(|v| v.as_str())
        .map(|k| k.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c=='-').take(64).collect::<String>())
        .filter(|k| !k.is_empty());
    let idem = match &client_key { Some(k) => format!("sos-{}-{}", s.user_id, k), None => format!("sos-{}-{}", s.user_id, now.timestamp_millis()) };
    // SL-6: lavoratore e (se univoca) sede sull'incidente: l'operatore sa chi soccorrere e dove
    let site_id = user_single_site(&st, tid, s.user_id).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ev = sqlx::query(
        "INSERT INTO app.events(tenant_id,source,source_event,idem_key,etype,origin_ts,received_ts,lat,lon,accuracy_m,device_id,correlation,site_id)
         VALUES($1,'pwa',$2,$2,'sos',$3,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (tenant_id,source,idem_key) DO NOTHING RETURNING id")
        .bind(tid).bind(&idem)
        .bind(now).bind(lat).bind(lon).bind(accuracy).bind(dev_id).bind(&correlation).bind(site_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some(ev) = ev else {
        // ritentativo dello stesso SOS: si restituisce l'incidente già aperto
        let prev = sqlx::query("SELECT i.id, i.correlation FROM app.events e JOIN app.incidents i ON i.event_id=e.id
                                WHERE e.tenant_id=$1 AND e.source='pwa' AND e.idem_key=$2 LIMIT 1")
            .bind(tid).bind(&idem).fetch_optional(&mut *tx).await.map_err(ise)?;
        tx.commit().await.map_err(ise)?;
        return Ok(Json(json!({"incident_id": prev.as_ref().map(|r| r.get::<Uuid,_>("id")),
            "correlation": prev.as_ref().map(|r| r.get::<String,_>("correlation")), "deduplicated": true})));
    };
    let eid: Uuid = ev.get("id");
    // Fase C gating: un profilo attivo del lavoratore può disabilitare il trigger SOS (fail-open).
    if trigger_suppressed(&st, tid, Some(s.user_id), None, "sos").await {
        tx.commit().await.map_err(ise)?;
        audit(&st, tid, &s.email, "trigger_suppressed_by_profile", json!({"etype":"sos","correlation":correlation})).await;
        return Ok(Json(json!({"event_id":eid,"suppressed_by_profile":true,"etype":"sos","correlation":correlation})));
    }
    let irow = sqlx::query(
        "INSERT INTO app.incidents(tenant_id,event_id,etype,correlation,priority,worker_user_id,site_id) VALUES($1,$2,'sos',$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(eid).bind(&correlation).bind(pri).bind(s.user_id).bind(site_id).fetch_one(&mut *tx).await.map_err(ise)?;
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

/// SL-10: il check-in ATTIVO del lavoratore (per ripristinarlo dopo un ricaricamento dell'app).
pub async fn my_active_checkin(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "loneworker").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = sqlx::query("SELECT id,due_at FROM app.checkins WHERE user_id=$1 AND state='attivo' ORDER BY due_at DESC LIMIT 1")
        .bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(match r {
        Some(r) => json!({"id":r.get::<Uuid,_>("id"),"due_at":r.get::<chrono::DateTime<chrono::Utc>,_>("due_at")}),
        None => Value::Null,
    }))
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
pub(crate) type Dt = chrono::DateTime<chrono::Utc>;

pub async fn shift_start(State(st): State<AppState>, h: HeaderMap, Json(b): Json<serde_json::Value>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    // Decisione PO 25/11 sera: il turno nell'app esiste solo con Gestione turni (matrice SL-20)
    if !any_module_active(&st, tid, crate::modules::feature_modules("app_shift")).await {
        return Err(err(StatusCode::FORBIDDEN, "turni non disponibili: serve il modulo Gestione turni")); }
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
pub(crate) async fn shift_max_hours(st: &AppState, tid: Uuid) -> i64 {
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

// SSE stream helper module
pub(crate) mod async_stream {
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
/// Errore di scrittura che di solito è un duplicato (vincolo UNIQUE): se invece è un riferimento a un
/// dato di un'altra azienda (TA-11, mig 0144) lo dice, invece di un fuorviante «già presente».
pub(crate) fn conflict_or<E: std::fmt::Display>(e: E, msg: &str) -> (StatusCode, String) {
    if e.to_string().contains("YW_TENANT_MISMATCH") { ise(e) } else { (StatusCode::CONFLICT, msg.to_string()) }
}
pub(crate) fn ise<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    let m = e.to_string();
    // TA-11/CA-19 (mig 0144): riferimento a un dato di un'altra azienda -> richiesta non valida
    if m.contains("YW_TENANT_MISMATCH") { return (StatusCode::BAD_REQUEST, "riferimento non valido: il dato indicato non appartiene alla tua azienda".into()); }
    (StatusCode::INTERNAL_SERVER_ERROR, m)
}
pub(crate) fn ise_resp() -> axum::response::Response { (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"db"}))).into_response() }

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
/// CA-11: budget, costi, SAL, fatture, prezzi. Chi pianifica (crea commesse e ne fissa il budget)
/// li vede; gli altri solo con `costs.view`.
pub(crate) fn can_see_costs(s: &Session) -> bool { has_cap(s, "plan") || has_cap(s, "costs.view") }
/// CA-11: posizioni dei colleghi solo a chi supervisiona (centrale, pianificazione, dispositivi).
pub(crate) fn is_supervisor(s: &Session) -> bool {
    ["incidents.view","plan","devices.manage","devices.admin","iot.manage"].iter().any(|c| has_cap(s, c))
}

pub async fn module_active(st: &AppState, tid: Uuid, code: &str) -> bool {
    // SA-5: tenant sospeso => nessun modulo attivo (ingress, allarmi, funzioni di modulo fermi).
    sqlx::query_scalar::<_, String>("SELECT m.state FROM app.tenant_modules m JOIN app.tenants t ON t.id=m.tenant_id
        WHERE m.tenant_id=$1 AND m.module_code=$2 AND t.status<>'sospeso'")
        .bind(tid).bind(code).fetch_optional(&st.owner_pool).await.ok().flatten()
        .map(|s| s == "attivo").unwrap_or(false)
}

/// Stato della configurazione del tenant: checklist "cosa manca per essere operativi", calcolata
/// dallo stato REALE del DB e condizionata ai moduli attivi. Unica fonte per il wizard di primo
/// avvio e per la checklist persistente. Solo tenant_admin.
pub async fn setup_status(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_cap(&s, "settings.manage")?; // TA-4: permesso, non nome del ruolo
    let tid = s.tenant_id.unwrap();
    let pool = st.owner_pool.clone();
    let cnt = |sql: &'static str| {
        let pool = pool.clone();
        async move { sqlx::query_scalar::<_, i64>(sql).bind(tid).fetch_one(&pool).await.unwrap_or(0) }
    };
    // moduli attivi (per decidere quali item mostrare)
    let lw = module_active(&st, tid, "loneworker").await;
    let iot = module_active(&st, tid, "iot").await;
    let gps = module_active(&st, tid, "gps").await;
    let cantieri = module_active(&st, tid, "cantieri").await;
    let interventi = module_active(&st, tid, "interventi").await;
    let workforce = module_active(&st, tid, "workforce").await;
    let safety = lw || iot || gps;        // moduli che generano allarmi/escalation
    let devices_mod = lw || iot || gps;   // moduli con dispositivi
    let _ = cantieri;

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
        format!("{n_verified} recapiti verificati"), "gruppi", "Verifica recapiti"); // TA-12: la verifica è in Escalation e reperibilità
    push("routing", lw, n_routing>0, "Instrada gli allarmi",
        "Definisci a quale gruppo va ogni tipo di allarme (SOS, uomo a terra…).",
        format!("{n_routing} regole di instradamento"), "gruppi", "Aggiungi instradamento"); // TA-12: l'instradamento è nei gruppi
    push("rules", iot, n_rules>0, "Imposta le regole dei sensori",
        "Le soglie decidono quando un dato IoT diventa un incidente.",
        format!("{n_rules} regole sensore"), "connettori", "Aggiungi regola"); // TA-12: le regole sono in Collegamento dispositivi (anche senza Lone Worker)
    push("devices", devices_mod, n_devices>0, "Collega i dispositivi",
        "Badge, tracker GPS o sensori IoT: la procedura guidata ti accompagna.",
        format!("{n_devices} dispositivi"), "dispositivi", "Aggiungi dispositivo");
    // TA-12: l'anagrafica clienti è una pagina di Interventi (le commesse hanno il committente a testo)
    push("clients", interventi, n_clients>0, "Aggiungi i clienti",
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
        // TA-12: solo voci di moduli attivi (la pagina di destinazione deve esistere nel menu)
        if module_active(&st, tid, "interventi").await {
            if o > 0 { items.push(json!({"title":"Interventi attivi","detail":format!("{o} da gestire"),"link":"ordini","tone":"info"})); } }
        let tt = c1("SELECT count(*) FROM app.shift_plans WHERE tenant_id=$1 AND state='pianificato'").await;
        if module_active(&st, tid, "workforce").await {
            if tt > 0 { items.push(json!({"title":"Turni da confermare","detail":format!("{tt} pianificati"),"link":"turni","tone":"info"})); } }
    }
    // Lavoratore — i propri interventi assegnati
    if role == "lavoratore" {
        let mine = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM app.work_orders WHERE tenant_id=$1 AND assigned_user=$2 AND state IN ('assegnato','in_corso')")
            .bind(tid).bind(uid).fetch_one(&pool).await.unwrap_or(0);
        if mine > 0 && module_active(&st, tid, "interventi").await { items.push(json!({"title":"I tuoi interventi","detail":format!("{mine} assegnati"),"link":"ordini","tone":"info"})); }
    }
    Ok(Json(json!({"items":items})))
}
/// Modulo spento dal superadmin -> la gestione sparisce anche via API.
pub(crate) async fn require_module(st: &AppState, tid: Uuid, code: &str) -> Result<(), (StatusCode, String)> {
    if module_active(st, tid, code).await { Ok(()) }
    else { Err(err(StatusCode::FORBIDDEN, &format!("modulo {code} non abilitato"))) }
}
pub(crate) async fn any_module_active(st: &AppState, tid: Uuid, codes: &[&str]) -> bool {
    for c in codes { if module_active(st, tid, c).await { return true; } }
    false
}
pub(crate) async fn require_any_module(st: &AppState, tid: Uuid, codes: &[&str]) -> Result<(), (StatusCode, String)> {
    for c in codes { if module_active(st, tid, c).await { return Ok(()); } }
    Err(err(StatusCode::FORBIDDEN, "nessun modulo abilitato per questa funzione"))
}
pub(crate) const DEVICE_MODS: &[&str] = &["loneworker","iot","gps"]; // suite self-contained: niente connettori esterni
pub(crate) const IOT_MODS: &[&str] = &["iot"]; // funzioni riservate al modulo IoT
/// Moduli che possono PRODURRE incidenti → l'escalation/centrale è condivisa fra tutti.
/// La configurazione e la visibilità escalation (gruppi, routing, on-call, recapiti,
/// override, maintenance, centrale allarmi) non devono sparire se "Sicurezza lavoratore"
/// (loneworker) è spento ma resta attivo un altro modulo che genera allarmi.
pub(crate) const ESCALATION_MODS: &[&str] = &["loneworker","iot","gps","giri"]; // moduli nativi che producono allarmi (gps: SOS/caduta/silenzio badge anche senza Lone Worker)

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
/// Deriva dalle scelte dell'admin sul tool gps/alarm_map: quali voci di
/// menu mappa esporre. Default parametrici: Mappa on, Piantine off.
pub async fn ui_features(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    let c = tool_config(&st, tid, "gps", "alarm_map").await;
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
    let existing: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='gps' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let mut obj = match existing { Some(Value::Object(m)) => m, _ => serde_json::Map::new() };
    obj.insert("geofence_alarms".into(), json!(if b.geofence_alarms {"si"} else {"no"}));
    let cfg = Value::Object(obj);
    sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'gps','alarm_map',$2,NULL)
         ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config, updated_at=now()")
        .bind(tid).bind(&cfg).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "geofence_alarms_set", json!({"geofence_alarms":b.geofence_alarms})).await;
    Ok(Json(json!({"ok":true,"geofence_alarms":b.geofence_alarms})))
}

/// Visibilità voci di menu mappa (superadmin): legge/scrive i flag view_mappa/view_piantine
/// del tool gps/alarm_map. Il superadmin decide se mostrare Mappa, Piantine, entrambe o nessuna.
pub async fn super_map_visibility_get(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let c = tool_config(&st, tid, "gps", "alarm_map").await;
    let on = |k: &str, d: bool| c.get(k).and_then(|v| v.as_str()).map(|v| v == "si").unwrap_or(d);
    Ok(Json(json!({"map_outdoor": on("view_mappa", true), "map_indoor": on("view_piantine", false)})))
}
#[derive(Deserialize)] pub struct MapVisibility { pub map_outdoor: bool, pub map_indoor: bool }
pub async fn super_map_visibility_set(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<MapVisibility>) -> R {
    let sa_ = require_super(&st, &h).await?;
    // merge preservando gli altri campi del tool alarm_map
    let existing: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='gps' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let mut obj = match existing { Some(Value::Object(m)) => m, _ => serde_json::Map::new() };
    obj.insert("view_mappa".into(), json!(if b.map_outdoor {"si"} else {"no"}));
    obj.insert("view_piantine".into(), json!(if b.map_indoor {"si"} else {"no"}));
    let cfg = Value::Object(obj);
    sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'gps','alarm_map',$2,NULL)
         ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config, updated_at=now()")
        .bind(tid).bind(&cfg).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &format!("superadmin:{}", sa_.email), "map_visibility_set", json!({"map_outdoor":b.map_outdoor,"map_indoor":b.map_indoor})).await;
    Ok(Json(json!({"ok":true,"map_outdoor":b.map_outdoor,"map_indoor":b.map_indoor})))
}

/// Trigger di sicurezza (Worker Safety) — vista unica dei trigger critici del perimetro lavoratore
/// e della loro configurazione per-tenant. Aggrega config già esistenti (gps/alarm_map,
/// loneworker/sos_escalation, loneworker/checkin_timer). Solo tenant_admin, richiede loneworker.
pub async fn safety_triggers_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tenant mancante"))?;
    require_module(&st, tid, "loneworker").await?;
    let amap = tool_config(&st, tid, "gps", "alarm_map").await;
    let sos_esc = tool_config(&st, tid, "loneworker", "sos_escalation").await;
    let chk = tool_config(&st, tid, "loneworker", "checkin_timer").await;
    let sos_as = amap.get("sos_as").and_then(|v| v.as_str()).unwrap_or("sos").to_string();
    let stopped = amap.get("stopped_as_immobilita").and_then(|v| v.as_str()).map(|x| x=="si").unwrap_or(false);
    // area fuori-geofence: critica solo se mappata a un etype critico via alarm_map.events
    let geo_crit = amap.get("events").and_then(|v| v.as_object())
        .map(|m| m.values().any(|v| v.as_str()==Some("critico"))).unwrap_or(false);
    let triggers = json!([
        {"etype":"sos","label":"SOS","desc":"Pulsante antipanico dall'app lavoratore o dal dispositivo","sources":["App lavoratore (/api/sos)","Dispositivo: pulsante SOS"],"critical":true},
        {"etype":"man_down","label":"Uomo a terra","desc":"Rilevamento caduta/incidente","sources":["Dispositivo: caduta / uomo a terra"],"critical":true},
        {"etype":"immobilita","label":"Immobilità","desc":"Dispositivo fermo oltre soglia","sources":["Dispositivo: fermo prolungato"],"critical":stopped},
        {"etype":"mancato_checkin","label":"Check-in mancato","desc":"Dead-man timer: il lavoratore non conferma entro i minuti previsti","sources":["Timer check-in"],"critical":true},
        {"etype":"movimento_non_accompagnato","label":"Movimento non accompagnato","desc":"Moto senza presenza attesa","sources":["Dispositivo: movimento non accompagnato"],"critical":true},
        {"etype":"attraversamento_geofence","label":"Attraversamento area","desc":"Ingresso/uscita da un'area; critico solo se mappato","sources":["Dispositivo: ingresso/uscita area"],"critical":geo_crit}
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
    merge_tool(&st, tid, "gps", "alarm_map", vec![
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
        "SELECT p.id,p.name,p.descr,p.escalation_group_id,g.name AS group_name,p.triggers,p.checkin_minutes,p.enabled,p.updated_at,
                p.immobility_enabled,p.immobility_after_s,p.immobility_prewarn_s,p.immobility_min_move_m,p.immobility_shift_only
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
            "immobility": json!({"enabled": r.get::<bool,_>("immobility_enabled"), "after_s": r.get::<i32,_>("immobility_after_s"),
                "prewarn_s": r.get::<i32,_>("immobility_prewarn_s"), "min_move_m": r.get::<i32,_>("immobility_min_move_m"),
                "shift_only": r.get::<bool,_>("immobility_shift_only")}),
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
    // SL-2: immobilità (tutti i parametri configurabili; default: spenta, 5 min, preavviso 60 s, 15 m, solo in turno)
    #[serde(default)] pub immobility_enabled: bool,
    #[serde(default = "d_imm_after")] pub immobility_after_s: i32,
    #[serde(default = "d_imm_pre")] pub immobility_prewarn_s: i32,
    #[serde(default = "d_imm_move")] pub immobility_min_move_m: i32,
    #[serde(default = "default_true")] pub immobility_shift_only: bool,
}
pub(crate) fn default_true() -> bool { true }
pub(crate) fn d_imm_after() -> i32 { 300 }
pub(crate) fn d_imm_pre() -> i32 { 60 }
pub(crate) fn d_imm_move() -> i32 { 15 }
pub(crate) fn validate_immobility(b: &ProfileReq) -> Result<(), (StatusCode, String)> {
    if !(60..=7200).contains(&b.immobility_after_s) { return Err(err(StatusCode::BAD_REQUEST, "immobilità: soglia tra 1 e 120 minuti")); }
    if !(0..=600).contains(&b.immobility_prewarn_s) || b.immobility_prewarn_s >= b.immobility_after_s {
        return Err(err(StatusCode::BAD_REQUEST, "immobilità: preavviso tra 0 e 600 secondi e minore della soglia")); }
    if !(2..=500).contains(&b.immobility_min_move_m) { return Err(err(StatusCode::BAD_REQUEST, "immobilità: spostamento minimo tra 2 e 500 metri")); }
    Ok(())
}

/// Valida che un gruppo (se indicato) appartenga al tenant.
pub(crate) async fn group_belongs(st: &AppState, tid: Uuid, gid: Option<Uuid>) -> Result<(), (StatusCode,String)> {
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
    validate_immobility(&b)?;
    let triggers = b.triggers.clone().unwrap_or_else(|| json!({"sos":true,"man_down":true,"immobilita":true,"mancato_checkin":true,"geofence":true}));
    let row = sqlx::query(
        "INSERT INTO app.protection_profiles(tenant_id,name,descr,escalation_group_id,triggers,checkin_minutes,enabled,
            immobility_enabled,immobility_after_s,immobility_prewarn_s,immobility_min_move_m,immobility_shift_only)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING id")
        .bind(tid).bind(name).bind(b.descr.trim()).bind(b.escalation_group_id).bind(&triggers)
        .bind(b.checkin_minutes).bind(b.enabled)
        .bind(b.immobility_enabled).bind(b.immobility_after_s).bind(b.immobility_prewarn_s).bind(b.immobility_min_move_m).bind(b.immobility_shift_only)
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
    validate_immobility(&b)?;
    let triggers = b.triggers.clone().unwrap_or_else(|| json!({"sos":true,"man_down":true,"immobilita":true,"mancato_checkin":true,"geofence":true}));
    let n = sqlx::query(
        "UPDATE app.protection_profiles SET name=$3,descr=$4,escalation_group_id=$5,triggers=$6,checkin_minutes=$7,enabled=$8,
            immobility_enabled=$9,immobility_after_s=$10,immobility_prewarn_s=$11,immobility_min_move_m=$12,immobility_shift_only=$13,updated_at=now()
          WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).bind(name).bind(b.descr.trim()).bind(b.escalation_group_id).bind(&triggers)
        .bind(b.checkin_minutes).bind(b.enabled)
        .bind(b.immobility_enabled).bind(b.immobility_after_s).bind(b.immobility_prewarn_s).bind(b.immobility_min_move_m).bind(b.immobility_shift_only)
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
        // SL-18: tipi REALI degli allarmi di area (prima «attraversamento_geofence», inesistente)
        "geofence_ingresso" | "geofence_uscita" | "ingresso_sito" | "uscita_sito" | "attraversamento_geofence" => "geofence",
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
/// priority "alta"->3, "media"->2. repeat_seconds = cadenza di ripetizione dell'escalation nativa.
pub(crate) async fn sos_escalation(st: &AppState, tid: Uuid) -> (i32, i64) {
    let c = tool_config(st, tid, "loneworker", "sos_escalation").await;
    let p = match c.get("priority").and_then(|v| v.as_str()) { Some("media") => 2, _ => 3 };
    let r = c.get("repeat_seconds").and_then(|v| v.as_i64()).unwrap_or(0);
    (p, r)
}

pub(crate) async fn tool_enabled(st: &AppState, tid: Uuid, module: &str, tool: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT enabled FROM app.tenant_tool_enabled WHERE tenant_id=$1 AND module_code=$2 AND tool_code=$3")
        .bind(tid).bind(module).bind(tool).fetch_optional(&st.owner_pool).await.ok().flatten()
        .unwrap_or(true) // absent => available
}

/// Validate an incoming config object against a tool's JSON schema.
/// Rejects unknown keys and out-of-range/enum-violating values. Returns a
/// cleaned object containing only schema-declared keys.
/// Salvataggio del form di uno strumento: il form è PROPRIETARIO dei campi dichiarati nello schema
/// (un campo omesso viene rimosso e torna al default). Le chiavi NON dichiarate nello schema sono
/// gestite da altri endpoint (es. `events` della policy eventi GPS, i flag mappa) e si preservano.
pub(crate) fn merge_tool_form(schema: &Value, existing: Option<&Value>, clean: &Value) -> Value {
    let declared: std::collections::HashSet<&str> = schema.get("fields").and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|f| f.get("key").and_then(|k| k.as_str())).collect()).unwrap_or_default();
    let mut out = serde_json::Map::new();
    if let Some(Value::Object(prev)) = existing {
        for (k, v) in prev { if !declared.contains(k.as_str()) { out.insert(k.clone(), v.clone()); } }
    }
    if let Value::Object(form) = clean { for (k, v) in form { out.insert(k.clone(), v.clone()); } }
    Value::Object(out)
}

pub(crate) fn validate_config(schema: &Value, input: &Value) -> Result<Value, String> {
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
    // TA-4: chi può modificare (settings.manage) deve anche poter leggere
    if !has_cap(&s,"config.view") && !has_cap(&s,"settings.manage") {
        return Err(err(StatusCode::FORBIDDEN, "permesso richiesto: lettura configurazione"));
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
            "editable": has_cap(&s, "settings.manage"),
        }));
    }
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct SetToolConfig { pub config: Value }

// ---- tenant_admin: save config for one tool ----
pub async fn set_module_tool(State(st): State<AppState>, h: HeaderMap, Path((code, tool)): Path<(String, String)>, Json(b): Json<SetToolConfig>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_cap(&s, "settings.manage")?; // TA-4: permesso, non nome del ruolo
    let tid = s.tenant_id.unwrap();
    let schema: Option<Value> = sqlx::query_scalar("SELECT config_schema FROM app.module_tools WHERE module_code=$1 AND tool_code=$2")
        .bind(&code).bind(&tool).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let schema = schema.ok_or_else(|| err(StatusCode::NOT_FOUND, "strumento inesistente"))?;
    if !tool_enabled(&st, tid, &code, &tool).await {
        return Err(err(StatusCode::FORBIDDEN, "strumento non abilitato per questo tenant"));
    }
    let clean = validate_config(&schema, &b.config).map_err(|m| err(StatusCode::BAD_REQUEST, &m))?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let existing: Option<Value> = sqlx::query_scalar(
        "SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code=$2 AND tool_code=$3 FOR UPDATE")
        .bind(tid).bind(&code).bind(&tool).fetch_optional(&mut *tx).await.map_err(ise)?;
    let stored = merge_tool_form(&schema, existing.as_ref(), &clean);
    sqlx::query(
        "INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by)
         VALUES($1,$2,$3,$4,$5)
         ON CONFLICT (tenant_id,module_code,tool_code)
         DO UPDATE SET config=EXCLUDED.config, updated_by=EXCLUDED.updated_by, updated_at=now()")
        .bind(tid).bind(&code).bind(&tool).bind(&stored).bind(s.user_id)
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'set_tool_config',$3)")
        .bind(tid).bind(&s.email).bind(json!({"module":code,"tool":tool})).execute(&st.owner_pool).await.map_err(ise)?;
    // si restituisce ciò che è stato davvero salvato
    Ok(Json(json!({"ok":true,"config":stored})))
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
pub(crate) async fn user_in_group(st: &AppState, user_id: Uuid, group_id: Uuid) -> bool {
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
    if module_active(&st, tid, "interventi").await { order_open(&st, tid, id).await?; } // CA-13
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
    // CA-10: tutti gli ordini solo a chi pianifica o ha `orders.view_all` (prima: chiunque non si
    // chiamasse «lavoratore», compresi i ruoli custom); in ogni caso filtrati per sedi consentite.
    let see_all = is_planner(&s) || has_cap(&s, "orders.view_all");
    let acl = allowed_sites(&st, &s).await;
    let my_groups: Vec<Uuid> = sqlx::query_scalar("SELECT group_id FROM app.user_group_members WHERE tenant_id=$1 AND user_id=$2")
        .bind(s.tenant_id.unwrap()).bind(s.user_id).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let rows = if !see_all {
        // tecnico: i propri ordini + quelli assegnati a un gruppo di persone di cui è membro
        sqlx::query(&format!("SELECT {cols} {joins}
            WHERE o.assigned_user=$1 OR o.assigned_group IN (SELECT group_id FROM app.user_group_members WHERE user_id=$1)
            ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC")).bind(s.user_id).fetch_all(&mut *tx).await
    } else {
        sqlx::query(&format!("SELECT {cols} {joins}
            ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC")).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mine = |r: &sqlx::postgres::PgRow| r.try_get::<Uuid,_>("assigned_user").ok() == Some(s.user_id)
        || r.try_get::<Uuid,_>("assigned_group").ok().map(|g| my_groups.contains(&g)).unwrap_or(false);
    let out: Vec<Value> = rows.iter().filter(|r| mine(r) || site_allowed(&acl, r.try_get::<Uuid,_>("site_id").ok())).map(|r| json!({
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
        "site_access": if is_planner(&s) || mine(r) { r.try_get::<String,_>("site_access").ok() } else { None },
        "client_id": r.try_get::<Uuid,_>("client_id").ok(), "client_name": r.try_get::<String,_>("client_name").ok()
    })).collect();
    Ok(Json(json!(out)))
}
#[cfg(test)]
mod device_identity_tests {
    use super::*;

    fn u(n: u128) -> Uuid { Uuid::from_u128(n) }

    #[test]
    fn imei_unknown_is_none() {
        assert_eq!(imei_match_from(&[]), ImeiMatch::None);
    }

    #[test]
    fn imei_single_device_is_accepted() {
        assert_eq!(imei_match_from(&[(u(1), u(10))]), ImeiMatch::One(u(1), u(10), "gps".into()));
    }

    #[test]
    fn imei_in_two_tenants_is_ambiguous_never_first_row() {
        // l'ordine delle righe non deve decidere a chi vanno dati e SOS
        assert_eq!(imei_match_from(&[(u(1), u(10)), (u(2), u(20))]), ImeiMatch::Ambiguous(2));
        assert_eq!(imei_match_from(&[(u(2), u(20)), (u(1), u(10))]), ImeiMatch::Ambiguous(2));
        assert_eq!(imei_match_from(&[(u(1), u(10)), (u(2), u(20)), (u(2), u(21))]), ImeiMatch::Ambiguous(2));
    }

    fn schema() -> Value {
        json!({"fields":[{"key":"silence_after_s","type":"int"},{"key":"sos_as","type":"enum"}]})
    }

    #[test]
    fn tool_form_omitted_field_falls_back_to_default() {
        let prev = json!({"silence_after_s": 120, "sos_as": "sos"});
        let out = merge_tool_form(&schema(), Some(&prev), &json!({"sos_as": "man_down"}));
        assert_eq!(out, json!({"sos_as": "man_down"}), "campo dello schema omesso = rimosso (torna al default)");
    }

    #[test]
    fn tool_form_preserves_keys_managed_elsewhere() {
        let prev = json!({"silence_after_s": 120, "events": {"deviceOverspeed": "critico"}, "view_mappa": "si"});
        let out = merge_tool_form(&schema(), Some(&prev), &json!({"silence_after_s": 300}));
        assert_eq!(out, json!({"silence_after_s": 300, "events": {"deviceOverspeed": "critico"}, "view_mappa": "si"}));
    }

    #[test]
    fn tool_form_first_save_without_existing_row() {
        let out = merge_tool_form(&schema(), None, &json!({"silence_after_s": 0}));
        assert_eq!(out, json!({"silence_after_s": 0}));
    }
}

#[cfg(test)]
mod client_ip_tests {
    use super::*;
    use std::net::IpAddr;
    fn hx(xff: &str) -> HeaderMap { let mut h = HeaderMap::new(); h.insert("x-forwarded-for", xff.parse().unwrap()); h }
    fn ip(s: &str) -> IpAddr { s.parse().unwrap() }

    #[test]
    fn untrusted_peer_ignores_forwarded_for() {
        // client diretto: X-Forwarded-For falsificato non cambia l'IP usato dal rate limit
        assert_eq!(resolve_client_ip(Some(ip("203.0.113.7")), &hx("1.2.3.4"), &[]), "203.0.113.7");
    }
    #[test]
    fn trusted_proxy_uses_last_hop() {
        // la console (loopback) APPENDE l'IP reale: il primo elemento è scritto dal client e va ignorato
        assert_eq!(resolve_client_ip(Some(ip("127.0.0.1")), &hx("6.6.6.6, 198.51.100.9"), &[]), "198.51.100.9");
        assert_eq!(resolve_client_ip(Some(ip("10.0.0.5")), &hx("198.51.100.9"), &[ip("10.0.0.5")]), "198.51.100.9");
    }
    #[test]
    fn trusted_proxy_without_or_bad_header_uses_peer() {
        assert_eq!(resolve_client_ip(Some(ip("127.0.0.1")), &HeaderMap::new(), &[]), "127.0.0.1");
        assert_eq!(resolve_client_ip(Some(ip("127.0.0.1")), &hx("non-un-ip"), &[]), "127.0.0.1");
        assert_eq!(resolve_client_ip(None, &hx("1.2.3.4"), &[]), "");
    }
    #[test]
    fn account_key_is_ip_independent() {
        let mut a = HeaderMap::new(); a.insert(CLIENT_IP_HDR, "1.1.1.1".parse().unwrap());
        let mut b = HeaderMap::new(); b.insert(CLIENT_IP_HDR, "2.2.2.2".parse().unwrap());
        let (ka, kb) = (rl_keys(&a, "acme|", " Mario@X.it"), rl_keys(&b, "acme|", "mario@x.it"));
        assert_ne!(ka[0], kb[0]);
        assert_eq!(ka[1], kb[1]);
        assert_eq!(ka[1], "acct|acme|mario@x.it");
    }
}

#[cfg(test)]
mod logo_tests {
    use super::*;
    #[test]
    fn logo_accepts_only_raster_images() {
        assert_eq!(logo_kind(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]), Some("image/png"));
        assert_eq!(logo_kind(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(logo_kind(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(logo_kind(b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>"), None);
        assert_eq!(logo_kind(b"<html><script>alert(1)</script></html>"), None);
        assert_eq!(logo_kind(b""), None);
    }
}

#[cfg(test)]
mod sms_reply_tests {
    use super::*;
    #[test]
    fn keyword_must_be_first_word() {
        assert_eq!(parse_sms_reply("ok").0, Some("ack"));
        assert_eq!(parse_sms_reply("  Preso ").0, Some("ack"));
        assert_eq!(parse_sms_reply("NON OK").0, None);
        assert_eq!(parse_sms_reply("non chiudere").0, None);
        assert_eq!(parse_sms_reply("chiudi").0, Some("close"));
    }
    #[test]
    fn incident_code_is_parsed() {
        assert_eq!(parse_sms_reply("OK 1a2b3c4d"), (Some("ack"), Some("1a2b3c4d".into())));
        assert_eq!(parse_sms_reply("chiudi 1A2B3C4D"), (Some("close"), Some("1a2b3c4d".into())));
        assert_eq!(parse_sms_reply("OK grazie"), (Some("ack"), None));
    }
}

#[cfg(test)]
mod sensor_rule_scope_tests {
    use super::*;
    fn rule(kind: &str, sref: Option<&str>, metric: &str, op: &str, thr: f64, etype: &str) -> RuleRow {
        RuleRow{ scope_kind: kind.into(), scope_ref: sref.map(|s| s.into()), metric: metric.into(), op: op.into(),
            threshold: Some(thr), etype: etype.into(), label: None }
    }
    #[test]
    fn minor_rule_first_does_not_hide_critical() {
        // IO-1: una «telemetria» prima nell'ordine non nasconde più una «critica» sulla stessa metrica
        let rules = vec![rule("tenant", None, "gas", ">=", 10.0, "telemetria"), rule("tenant", None, "gas", ">=", 50.0, "sensore_critico")];
        let hit = select_rule(&rules, &RuleCtx::default(), &json!({"gas": 80})).unwrap();
        assert_eq!(hit.etype, "sensore_critico");
        assert_eq!(hit.threshold, Some(50.0));
        let low = select_rule(&rules, &RuleCtx::default(), &json!({"gas": 20})).unwrap();
        assert_eq!(low.etype, "telemetria");
    }
    #[test]
    fn scope_limits_rule_to_its_device_model_site_scenario() {
        let d = Uuid::new_v4(); let s = Uuid::new_v4();
        let rules = vec![rule("device", Some(&d.to_string()), "t", ">", 30.0, "sensore_critico")];
        assert!(select_rule(&rules, &RuleCtx::default(), &json!({"t": 40})).is_none(), "senza dispositivo non si applica");
        assert!(select_rule(&rules, &RuleCtx{device_id: Some(Uuid::new_v4()), ..Default::default()}, &json!({"t": 40})).is_none());
        assert!(select_rule(&rules, &RuleCtx{device_id: Some(d), ..Default::default()}, &json!({"t": 40})).is_some());
        let site = vec![rule("site", Some(&s.to_string()), "t", ">", 30.0, "sensore_critico")];
        assert!(select_rule(&site, &RuleCtx{site: Some(s), ..Default::default()}, &json!({"t": 40})).is_some());
        let model = vec![rule("model", Some("GH5200"), "t", ">", 30.0, "batteria_critica")];
        assert!(select_rule(&model, &RuleCtx{model: Some("GH5200".into()), ..Default::default()}, &json!({"t": 40})).is_some());
        assert!(select_rule(&model, &RuleCtx{model: Some("FMB920".into()), ..Default::default()}, &json!({"t": 40})).is_none());
        let scn = vec![rule("scenario", Some("smart_energy"), "t", ">", 30.0, "sensore_critico")];
        assert!(select_rule(&scn, &RuleCtx{scenario: Some("smart_energy".into()), ..Default::default()}, &json!({"t": 40})).is_some());
    }
    #[test]
    fn same_severity_first_in_order_wins() {
        let rules = vec![rule("tenant", None, "t", ">", 30.0, "sensore_critico"), rule("tenant", None, "t", ">", 35.0, "sensore_critico")];
        assert_eq!(select_rule(&rules, &RuleCtx::default(), &json!({"t": 40})).unwrap().threshold, Some(30.0));
    }
    #[test]
    fn man_down_beats_sensor_critical() {
        let rules = vec![rule("tenant", None, "t", ">", 30.0, "sensore_critico"), rule("tenant", None, "fall", ">=", 1.0, "man_down")];
        assert_eq!(select_rule(&rules, &RuleCtx::default(), &json!({"t": 40, "fall": 1})).unwrap().etype, "man_down");
    }
}
