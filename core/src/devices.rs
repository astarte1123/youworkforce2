//! Dispositivi e localizzazione: ingest nativo (GPS, OsmAnd, Teltonika, JSON), comandi, geofence, report, mappe, beacon e piantine, catalogo, onboarding
//! Estratto da api.rs (split, nessun cambio di comportamento): i gestori restano
//! raggiungibili come `api::nome` grazie al re-export in api.rs.
#![allow(unused_imports)]
use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, Response, sse::{Sse, Event as SseEvent}}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::convert::Infallible;
use uuid::Uuid;
use futures::stream::Stream;
use crate::state::{AppState, Session, TenantEvent};
use crate::{auth, db};
use crate::api::*;

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
        let ext = bound_ext.clone().or_else(|| b.device_external_id.clone());
        let dev_id = match &ext { Some(x) => device_id_by_ext(&st, tid, &source, x).await, None => None };
        if let Some(e) = eval_sensor_rules(&st, tid, dev_id, p).await {
            if CRITICAL.contains(&e.as_str()) { etype = e; }
        }
    }

    persist_ingest(&st, tid, Normalized { device_id: None,
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
/// JSON HTTP, Teltonika, OsmAnd, …) maps onto this shape so the SOS chain stays single-path.
pub struct Normalized {
    /// dispositivo già risolto dal chiamante (ha la precedenza su sorgente + identificativo esterno)
    pub device_id: Option<Uuid>,
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

/// Estrae l'identificatore beacon dagli attributi di una posizione/evento.
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
pub(crate) async fn record_device_assignment(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, device_id: Uuid,
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
pub(crate) async fn resolve_worker_at(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, device_id: Uuid,
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
pub(crate) fn payload_origin_time(p: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
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

// Il futuro di persist_ingest è molto grande (escalation, notifiche, regole): va sul heap, altrimenti
// le catene GPS -> geofence/batteria -> persist_ingest sforano lo stack dei worker tokio (crash 25/11).
pub async fn persist_ingest(st: &AppState, tid: Uuid, n: Normalized) -> R {
    Box::pin(persist_ingest_inner(st, tid, n)).await
}
pub(crate) async fn persist_ingest_inner(st: &AppState, tid: Uuid, n: Normalized) -> R {
    // SA-5: ultima barriera per ogni canale (HTTP, Teltonika, MQTT, worker): un tenant sospeso non riceve.
    if tenant_suspended(st, tid).await { return Err(err(StatusCode::FORBIDDEN, "tenant sospeso")); }
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
    let device_id: Option<Uuid> = if let Some(did) = n.device_id {
        // RLS della transazione: un id di un'altra azienda non viene trovato
        sqlx::query_scalar("SELECT id FROM app.devices WHERE id=$1").bind(did).fetch_optional(&mut *tx).await.map_err(ise)?
    } else if let Some(ext) = &n.device_external_id {
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
        // DEDUP esplicito: se esiste già un incidente APERTO con la stessa chiave
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

// ----------------------------- Ingest JSON HTTP nativo: eventi -----------------------------
/// Riceve eventi di dispositivi GPS/badge in formato JSON `{device:{uniqueId}, event:{type,
/// attributes:{alarm}}, position:{latitude,longitude,attributes}}` (gateway e tracker che parlano
/// HTTP) e li porta sulla catena normalizzata. Il tenant deriva SOLO dal token di ingest (header
/// `X-Ingest-Token` o `?token=`), mai dal corpo. Mappatura allarmi:
///   alarm=sos      -> sos       (critico)
///   alarm=fallDown -> man_down  (critico, caduta del lavoratore isolato)
///   altro alarm    -> device_alarm (registrato, non critico)
///   deviceOffline  -> device_offline
///   altro          -> secondo la policy eventi del tenant (`device_event` di default)
pub async fn ingest_gps_event(
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
    // canale ufficiale del modulo gps: basta un modulo dispositivi attivo (come OsmAnd/Teltonika)
    if !any_device_module(&st, tid).await {
        return Err(err(StatusCode::FORBIDDEN, "nessun modulo dispositivi attivo"));
    }
    let bound_ext = match cred.device_id { Some(did) => bound_device_external(&st, did).await, None => None };

    let ev = b.get("event");
    let dev = b.get("device");
    let pos = b.get("position");

    let unique_id = dev.and_then(|d| d.get("uniqueId")).and_then(|v| v.as_str()).map(|s| s.to_string());
    if let Some(ext) = bound_ext.as_deref().or(unique_id.as_deref()) { touch_device_seen(&st, tid, &source, ext).await; }
    let lat = pos.and_then(|p| p.get("latitude")).and_then(|v| v.as_f64());
    let lon = pos.and_then(|p| p.get("longitude")).and_then(|v| v.as_f64());
    let evt_type = ev.and_then(|e| e.get("type")).and_then(|v| v.as_str()).unwrap_or("");
    let alarm = ev.and_then(|e| e.get("attributes")).and_then(|a| a.get("alarm")).and_then(|v| v.as_str())
        .or_else(|| pos.and_then(|p| p.get("attributes")).and_then(|a| a.get("alarm")).and_then(|v| v.as_str()));
    let evt_id = ev.and_then(|e| e.get("id")).map(|v| v.to_string())
        .or_else(|| pos.and_then(|p| p.get("id")).map(|v| v.to_string()))
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Per-tenant mapping (gps/alarm_map): sos routing, offline severity, soglia batteria.
    let amap = tool_config(&st, tid, "gps", "alarm_map").await;
    // battery level dagli attributi (posizione o evento)
    let battery = pos.and_then(|p| p.get("attributes")).and_then(|a| a.get("batteryLevel")).and_then(|v| v.as_f64())
        .or_else(|| ev.and_then(|e| e.get("attributes")).and_then(|a| a.get("batteryLevel")).and_then(|v| v.as_f64()));
    let etype = map_gps_etype(&amap, evt_type, alarm, battery);

    // Indoor/beacon: se la posizione (o l'evento) porta un beacon, l'allarme e' indoor
    // e lo localizziamo per zona. L'ancora del beacon vince sul GPS (in fabbrica inaffidabile).
    let attrs = pos.and_then(|p| p.get("attributes"))
        .or_else(|| ev.and_then(|e| e.get("attributes")));
    let (indoor, beacon_uid, zone, anchor) = resolve_indoor(&st, tid, attrs).await;
    let (lat, lon) = if indoor { if let Some((a,o)) = anchor { (Some(a), Some(o)) } else { (lat, lon) } } else { (lat, lon) };

    let source_event = format!("gps-evt-{}", evt_id);
    persist_ingest(&st, tid, Normalized { device_id: None,
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

// ----------------------------- Comandi al device (nativi, GPRS Teltonika codec12) -----------------------------
#[derive(Deserialize)]
pub struct DeviceCmd { #[serde(rename="type")] pub kind: String, pub data: Option<String> }
/// Catalogo dei comandi rapidi: (tipo, etichetta per l'utente, comando GPRS Teltonika).
/// `personalizzato` invia il testo libero in `data` (per tecnici).
pub(crate) const DEVICE_COMMANDS: &[(&str, &str, &str)] = &[
    ("posizione", "Chiedi la posizione adesso", "getgps"),
    ("stato",     "Stato del dispositivo",       "getstatus"),
    ("info",      "Informazioni di rete e SIM",  "getinfo"),
    ("versione",  "Versione firmware",           "getver"),
    ("riavvio",   "Riavvia il dispositivo",      "cpureset"),
];
/// Invia un comando al dispositivo sulla connessione TCP nativa (Teltonika codec12) e registra
/// l'esito nello storico. Il device deve essere collegato in quel momento.
pub async fn device_command(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<DeviceCmd>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"devices.manage") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    // testo libero = stesso livello del comando GPRS grezzo (può riconfigurare il device)
    if b.kind == "personalizzato" && !has_cap(&s,"devices.admin") {
        return Err(err(StatusCode::FORBIDDEN, "i comandi personalizzati richiedono l'amministrazione dispositivi"));
    }
    let ext = gps_device_external(&st, tid, id).await?;
    let text: String = if b.kind == "personalizzato" {
        b.data.as_deref().map(str::trim).filter(|t| !t.is_empty())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "scrivi il comando da inviare"))?.to_string()
    } else {
        DEVICE_COMMANDS.iter().find(|c| c.0 == b.kind).map(|c| c.2.to_string())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "comando non disponibile per questo dispositivo"))?
    };
    let (code, result, reply) = match crate::teltonika::send_command(tid, id, &ext, &text).await {
        Ok(resp) => (200i32, "inviato".to_string(), Some(resp)),
        Err(e) => (502i32, format!("non inviato: {e}"), None),
    };
    sqlx::query("INSERT INTO app.device_commands(tenant_id,device_id,ctype,actor,status_code,result) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(tid).bind(id).bind(&b.kind).bind(&s.email).bind(code).bind(&result)
        .execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "device_command", json!({"device":id,"type":b.kind,"status_code":code})).await;
    Ok(Json(json!({"ok": code==200, "status_code":code, "result":result, "response":reply})))
}

/// IMEI di un device del tenant servito dal bridge Teltonika (source='gps'). 404 se il device non
/// esiste nel tenant, 400 se non è un dispositivo GPS (i comandi GPRS valgono solo per quelli).
pub(crate) async fn gps_device_external(st: &AppState, tid: Uuid, id: Uuid) -> Result<String, (StatusCode, String)> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT external_id, source FROM app.devices WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let (ext, src) = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "dispositivo inesistente"))?;
    if src != "gps" { return Err(err(StatusCode::BAD_REQUEST, "i comandi valgono solo per i dispositivi GPS/badge")); }
    Ok(ext)
}

/// Messa in servizio / fuori servizio di un badge GPS (mig 0128). È l'azione che AVVIA il
/// conteggio del silenzio: da questo momento il watchdog protegge il lavoratore assegnato e, se il
/// badge non trasmette entro la soglia (anche senza aver mai trasmesso), apre `perdita_contatto`.
/// `in_servizio` richiede un lavoratore assegnato e il device abilitato.
#[derive(Deserialize)] pub struct ServiceReq { pub state: String }
pub async fn set_device_service(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ServiceReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    require_cap(&s, "devices.manage")?;
    if !matches!(b.state.as_str(), "in_servizio" | "in_attesa" | "fuori_servizio") {
        return Err(err(StatusCode::BAD_REQUEST, "stato non valido (in_servizio, in_attesa, fuori_servizio)"));
    }
    let row = sqlx::query("SELECT source, assigned_user, COALESCE(enabled,true) AS enabled, service_state FROM app.devices WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "dispositivo inesistente"))?;
    if row.get::<String,_>("source") != "gps" {
        return Err(err(StatusCode::BAD_REQUEST, "la messa in servizio vale solo per i dispositivi GPS/badge"));
    }
    if b.state == "in_servizio" {
        if row.try_get::<Option<Uuid>,_>("assigned_user").ok().flatten().is_none() {
            return Err(err(StatusCode::BAD_REQUEST, "assegna prima il dispositivo a un lavoratore: un badge in servizio protegge chi lo porta"));
        }
        if !row.get::<bool,_>("enabled") {
            return Err(err(StatusCode::BAD_REQUEST, "il dispositivo è disabilitato: abilitalo prima di metterlo in servizio"));
        }
    }
    let prev: String = row.get("service_state");
    // la messa in servizio arma il conteggio con l'orologio dell'applicazione (st.clock)
    let out = sqlx::query("UPDATE app.devices SET service_state=$3, updated_at=now(),
            silence_armed_at=CASE WHEN $3='in_servizio' AND service_state<>'in_servizio' THEN $4 ELSE silence_armed_at END
          WHERE id=$1 AND tenant_id=$2 RETURNING service_state, silence_armed_at")
        .bind(id).bind(tid).bind(&b.state).bind(st.clock.now()).fetch_one(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "device_service", json!({"device":id,"from":prev,"to":b.state})).await;
    Ok(Json(json!({"ok":true,"service_state":out.get::<String,_>("service_state"),
        "silence_armed_at":out.try_get::<chrono::DateTime<chrono::Utc>,_>("silence_armed_at").ok()})))
}

/// Comandi rapidi disponibili (per la UI).
pub async fn device_command_types(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"devices.manage") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.devices WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    if exists.is_none() { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente")); }
    let out: Vec<Value> = DEVICE_COMMANDS.iter().map(|c| json!({"type":c.0,"label":c.1})).collect();
    Ok(Json(json!(out)))
}

/// P4: storico comandi inviati a un device (operatore/admin/auditor).
pub async fn list_device_commands(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if !has_cap(&s,"incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let rows = sqlx::query("SELECT ctype,actor,status_code,result,created_at FROM app.device_commands
         WHERE tenant_id=$1 AND device_id=$2 ORDER BY created_at DESC LIMIT 50")
        .bind(tid).bind(id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "ctype": r.get::<String,_>("ctype"),
        "actor": r.get::<String,_>("actor"),
        "status_code": r.try_get::<i32,_>("status_code").ok(),
        "result": r.try_get::<String,_>("result").ok(),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
    })).collect();
    Ok(Json(json!(out)))
}


// ----------------------------- #3 Geofence + presenza -----------------------------
pub(crate) fn haversine_m(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    let r = 6_371_000.0_f64;
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dphi = (b_lat - a_lat).to_radians();
    let dlmb = (b_lon - a_lon).to_radians();
    let h = (dphi/2.0).sin().powi(2) + p1.cos()*p2.cos()*(dlmb/2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}
pub(crate) fn point_in_polygon(lat: f64, lon: f64, pts: &[[f64;2]]) -> bool {
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
pub(crate) fn geofence_contains(geom: &Value, lat: f64, lon: f64) -> bool {
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
pub(crate) fn clean_hex_color(c: Option<&str>) -> String {
    let c = c.unwrap_or("").trim();
    let ok = c.starts_with('#') && (c.len()==7 || c.len()==4) && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if ok { c.to_string() } else { "#16a34a".to_string() }
}
#[derive(Deserialize)]
pub struct GeofenceReq { pub name: String, pub kind: String, pub geom: Value, pub site_id: Option<Uuid>, pub floorplan_id: Option<Uuid>, pub color: Option<String>,
    #[serde(default)] pub alert_on: Option<String> }
pub async fn create_geofence(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GeofenceReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "nome gia' presente"))?;
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
        .map_err(|e| conflict_or(e, "nome gia' presente"))?.rows_affected();
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
pub(crate) fn rep_dates(q: &std::collections::HashMap<String,String>) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), (StatusCode,String)> {
    let to = q.get("to").and_then(|x| parse_ts(x).ok()).unwrap_or_else(chrono::Utc::now);
    let from = q.get("from").and_then(|x| parse_ts(x).ok()).unwrap_or(to - chrono::Duration::days(7));
    Ok((from, to))
}
pub(crate) fn report_role(s: &Session) -> Result<(), (StatusCode,String)> {
    if !has_cap(&s,"incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    Ok(())
}
/// Report percorso: posizioni in intervallo -> distanza totale, durata, soste.
pub async fn report_route(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","gps"]).await?;
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
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?; // AR-1: controllo del modulo dichiarato
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
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","gps"]).await?;
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
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?; // AR-1: controllo del modulo dichiarato
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
pub(crate) fn ise_resp2() -> (StatusCode,String) { (StatusCode::INTERNAL_SERVER_ERROR, "db".into()) }
pub(crate) fn csv_response(csv: String, filename: &str) -> Response {
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
        (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))], csv).into_response()
}
pub(crate) fn text_response(body: String, filename: &str) -> Response {
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
        (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))], body).into_response()
}

// ============ Report Allarmi (DVR) — guidato dal modulo allarmi ============
// Quando il tenant ha un modulo che genera allarmi (loneworker/gps), il report
// principale e' quello degli ALLARMI/incidenti, non percorsi/soste/presenze.
// Descrizioni leggibili per ogni tipo di allarme (DVR, operatori non tecnici).
/// SL-15: il nome viene dal catalogo nel DB (app.event_types, modificabile dal superadmin);
/// questi sono i nomi predefiniti, usati se il catalogo non è caricato o il codice non c'è.
pub fn etype_label(e: &str) -> String {
    crate::catalogs::event_label(e).unwrap_or_else(|| builtin_etype_label(e).to_string())
}
pub fn builtin_etype_label(e: &str) -> &'static str {
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
        "ronda_mancata" => "Ronda mancata (orario pianificato non svolto)",
        // storico (connettore di tracciamento esterno dismesso, mig 0127): resta per i vecchi alert
        "traccar_conn_down" => "Server di tracciamento esterno irraggiungibile (storico)",
        "batteria_critica" => "Batteria in esaurimento (critico)",
        "batteria_bassa" => "Batteria bassa",
        "geofence_ingresso" => "Ingresso in zona vietata (geofence)",
        "geofence_uscita" => "Uscita da area sicura (geofence)",
        _ => "Altro evento",
    }
}
pub(crate) struct AlarmRow {
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
pub(crate) async fn alarms_rows(st: &AppState, s: &Session, q: &std::collections::HashMap<String,String>)
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
pub(crate) async fn tenant_name(st: &AppState, tid: Uuid) -> String {
    sqlx::query_scalar::<_,String>("SELECT name FROM app.tenants WHERE id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or_else(|| "—".into())
}
pub(crate) fn fmt_local(t: &chrono::DateTime<chrono::Utc>) -> String { t.format("%d/%m/%Y %H:%M:%S").to_string() }
pub(crate) fn fmt_resp(sec: Option<i64>) -> String {
    match sec { Some(s) if s >= 60 => format!("{}m {}s", s/60, s%60), Some(s) => format!("{}s", s), None => "—".into() }
}

/// JSON: report allarmi + sintesi (per la UI e per il PDF lato client).
pub async fn report_alarms(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; report_role(&s)?;
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","gps"]).await?;
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
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?; // AR-1: controllo del modulo dichiarato
    let (_from, _to, rows) = alarms_rows(&st, &s, &q).await?;
    let mut csv = String::from("aperto_il,tipo,descrizione,stato,priorita,preso_in_carico_il,tempo_risposta_s,chiuso_il,sede,assegnatario,esito\n");
    for r in &rows {
        csv.push_str(&format!("{},{},{},{},{},{},{},{},{},{},{}\n",
            r.opened_at.to_rfc3339(),
            csv_esc(&r.etype), csv_esc(&etype_label(&r.etype)),
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
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?; // AR-1: controllo del modulo dichiarato
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
    require_any_module(&st, s.tenant_id.unwrap(), &["loneworker","gps"]).await?;
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    require_cap(&s, "devices.admin")?; // TA-4: permesso, non nome del ruolo
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
    require_cap(&s, "devices.admin")?; // TA-4: permesso, non nome del ruolo
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    require_cap(&s, "devices.admin")?; // TA-4: permesso, non nome del ruolo
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    require_cap(&s, "devices.admin")?; // TA-4: permesso, non nome del ruolo
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    require_cap(&s, "devices.admin")?; // TA-4: permesso, non nome del ruolo
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    require_cap(&s, "devices.admin")?; // TA-4: permesso, non nome del ruolo
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    require_cap(&s, "devices.admin")?; // TA-4: permesso, non nome del ruolo
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.devices SET floorplan_id=$2, fp_x=$3, fp_y=$4 WHERE id=$1")
        .bind(id).bind(b.floorplan_id).bind(b.x).bind(b.y).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// TA-15 (decisione PO 25/11/2026): il server non fa più geocoding (prima chiamava Nominatim per
// ogni scheda dispositivo aperta, mandando fuori le posizioni). L'indirizzo lo chiede il browser
// dell'operatore, solo se lo vuole (pulsante «mostra indirizzo»).

// ----------------------------- Ingest JSON HTTP nativo: posizioni -----------------------------
/// Riceve posizioni `{position, device}` e aggiorna l'ultima posizione nota del device + lo
/// storico. Tenant dal token di ingest. Alta frequenza: idempotente per id posizione.
pub async fn ingest_gps_position(
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
    if !any_device_module(&st, tid).await {
        return Err(err(StatusCode::FORBIDDEN, "nessun modulo dispositivi attivo"));
    }
    let pos = b.get("position").or(Some(&b)).unwrap_or(&b);
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
    let out = process_gps_position(&st, tid, pos, ext).await.map_err(ise)?;
    Ok(Json(out))
}

/// Ingest NATIVO OsmAnd/HTTP: smartphone e tracker che parlano il
/// protocollo OsmAnd inviano posizioni e allarmi via HTTP (GET/POST con query string).
/// Autenticazione con token device-bound (anti-spoof, come gli altri ingress). Riusa la pipeline
/// nativa: process_gps_position (posizione/presenza) + persist_ingest (allarme->incidente),
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
        Some(did) => sqlx::query_scalar("SELECT source FROM app.devices WHERE id=$1").bind(did).fetch_optional(&st.owner_pool).await.map_err(ise)?.unwrap_or_else(|| "gps".into()),
        None => "gps".into(),
    };
    // presenza: il pacchetto è autenticato -> il device è vivo, con o senza fix GPS. Alimenta il
    // watchdog del silenzio (che misura l'ultimo CONTATTO, non l'ultima posizione).
    touch_device_seen(&st, tid, &dev_source, &ext).await;
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
        // process_gps_position filtra source='gps': per device con quella sorgente aggiorna,
        // altrimenti ignora (la posizione resta comunque nel payload dell'eventuale allarme).
        if dev_source=="gps" {
            let prev_batt = device_last_battery(&st, tid, &dev_source, &ext).await;
            if alarm.is_none() { gps_battery_check(&st, tid, &dev_source, &ext, prev_batt, battery, lat, lon, ts_rfc.as_deref()).await; }
            position = process_gps_position(&st, tid, &pos, &ext).await.map_err(ise)?;
        }
        else { position = json!({"skipped":"sorgente_non_gps"}); }
    }
    // 2) allarme -> incidente (etype mappato; orario affidabile -> attribuzione temporale)
    let mut incident = json!(null);
    if let Some(al) = alarm.as_deref() {
        let amap = tool_config(&st, tid, "gps", "alarm_map").await;
        let etype = map_gps_etype(&amap, "", Some(al), battery);
        // SL-4: con l'orario del dispositivo la ritrasmissione è riconoscibile (stesso evento);
        // senza orario non lo è e ogni richiesta resta un evento distinto. Correlazione stabile per
        // dispositivo e tipo: le ripetizioni confluiscono nell'incidente ancora aperto.
        let corr = format!("osmand-{ext}-{etype}");
        let src_evt = match ts_rfc.as_deref() {
            Some(ts) => format!("osmand-evt-{ext}-{ts}-{al}"),
            None => format!("osmand-evt-{}", Uuid::new_v4()),
        };
        let payload = json!({"channel":"osmand","alarm":al,"lat":lat,"lon":lon,"battery":battery,"occurred_at":ts_rfc});
        let r = persist_ingest(&st, tid, Normalized{ device_id: None,
            source: dev_source.clone(), source_event: src_evt.clone(), idem: src_evt,
            etype, device_external_id: Some(ext.clone()), site_name:None,
            lat, lon, accuracy_m: accuracy, correlation: corr, payload,
            indoor:false, beacon_uid:None, zone:None }).await?;
        incident = r.0;
    }
    Ok(Json(json!({"ok":true,"external_id":ext,"position":position,"incident":incident})))
}

// ---- Teltonika: comando GPRS (codec12) verso un device connesso ----
#[derive(Deserialize)] pub struct GprsCmdReq { pub command: String }
pub async fn device_gprs_command(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<GprsCmdReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, DEVICE_MODS).await?;
    if b.command.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "comando vuoto")); }
    let imei = gps_device_external(&st, tid, id).await?;
    match crate::teltonika::send_command(tid, id, &imei, b.command.trim()).await {
        Ok(resp) => { audit(&st, tid, &s.email, "gprs_command", json!({"device":id,"command":b.command})).await;
                      Ok(Json(json!({"ok":true,"response":resp}))) }
        Err(e) => Err(err(StatusCode::BAD_GATEWAY, &e)),
    }
}

// ---- Teltonika edge nativo (Codec8/8E su TCP): helper d'ingest riusabili ----
/// Risolve device+tenant+source dall'IMEI (= external_id). owner_pool (bypassa RLS).
/// Esito della risoluzione IMEI -> device del bridge Teltonika.
#[derive(Debug, Clone, PartialEq)]
pub enum ImeiMatch {
    /// Nessun device `gps` con quell'IMEI.
    None,
    /// Esattamente un device: (tenant, device, source).
    One(Uuid, Uuid, String),
    /// Stesso IMEI censito in N tenant: identità contesa, non si associa automaticamente.
    Ambiguous(usize),
}
/// Decide l'esito dai candidati (tenant, device) trovati per un IMEI. Pura: testata in unità.
pub fn imei_match_from(rows: &[(Uuid, Uuid)]) -> ImeiMatch {
    match rows {
        [] => ImeiMatch::None,
        [(t, d)] => ImeiMatch::One(*t, *d, "gps".into()),
        many => {
            let mut ts: Vec<Uuid> = many.iter().map(|r| r.0).collect();
            ts.sort(); ts.dedup();
            ImeiMatch::Ambiguous(ts.len())
        }
    }
}
/// Il bridge TCP Teltonika serve SOLO i dispositivi del modulo `gps` (source='gps'). L'IMEI deve
/// identificare UN SOLO device in tutto il sistema: sconosciuto o conteso tra tenant = rifiuto.
/// Mai `LIMIT 1`: con un IMEI duplicato i dati (SOS compresi) finirebbero in un tenant a caso.
pub async fn teltonika_resolve(st: &AppState, imei: &str) -> ImeiMatch {
    let rows: Vec<(Uuid, Uuid)> = match sqlx::query_as(
        "SELECT tenant_id, id FROM app.devices WHERE external_id=$1 AND source='gps' LIMIT 10")
        .bind(imei).fetch_all(&st.owner_pool).await {
        Ok(r) => r,
        Err(e) => { tracing::warn!("teltonika_resolve {imei}: {e}"); return ImeiMatch::None; }
    };
    imei_match_from(&rows)
}
/// Mappa IO->allarme EFFETTIVA per un device Teltonika: default del MODELLO
/// (device_models.default_config.io_alarm_map) con OVERRIDE per-tenant
/// (tool_config teltonika.io_alarm_map) che vince. Risolta UNA volta per connessione e
/// passata al decoder, così la semantica salva-vita (236->sos, 242->man_down del GH-5200)
/// è un DATO di prodotto configurabile, non hardcoded nel decoder.
pub async fn teltonika_io_alarm_map(st: &AppState, tid: Uuid, did: Uuid) -> std::collections::HashMap<i64,String> {
    let mut out: std::collections::HashMap<i64,String> = std::collections::HashMap::new();
    let ingest = |m: &mut std::collections::HashMap<i64,String>, v: &Value| {
        if let Some(obj) = v.as_object() {
            for (k, val) in obj {
                if let (Ok(id), Some(s)) = (k.parse::<i64>(), val.as_str()) {
                    if !s.trim().is_empty() { m.insert(id, s.trim().to_string()); }
                }
            }
        }
    };
    // 1) default del modello del device
    if let Some(row) = sqlx::query(
        "SELECT m.default_config->'io_alarm_map' AS map
           FROM app.devices d JOIN app.device_models m ON m.model_code=d.model_code
          WHERE d.id=$1 AND d.tenant_id=$2")
        .bind(did).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() {
        if let Ok(Some(map)) = row.try_get::<Option<Value>,_>("map") { ingest(&mut out, &map); }
    }
    // 2) override per-tenant (vince)
    ingest(&mut out, &tool_config(st, tid, "teltonika", "io_alarm_map").await);
    out
}
/// Registro dei tentativi di collegamento RIFIUTATI (mig 0136). Una riga per identità con
/// contatore; al massimo ~2000 righe (oltre si aggiornano solo quelle già presenti, così un
/// flusso di IMEI casuali non gonfia la tabella). L'identità è troncata e ripulita.
pub async fn record_connect_attempt(st: &AppState, protocol: &str, identity: &str, reason: &str, peer: &str) {
    let ident: String = identity.chars().filter(|c| c.is_ascii_graphic()).take(32).collect();
    if ident.is_empty() { return; }
    let now = st.clock.now();
    let upd = sqlx::query("UPDATE app.device_connect_attempts SET reason=$3, peer=$4, last_at=$5, attempts=attempts+1 \
                           WHERE protocol=$1 AND identity=$2")
        .bind(protocol).bind(&ident).bind(reason).bind(peer).bind(now).execute(&st.owner_pool).await;
    if matches!(upd, Ok(ref r) if r.rows_affected() > 0) { return; }
    if let Err(e) = sqlx::query("INSERT INTO app.device_connect_attempts(protocol,identity,reason,peer,first_at,last_at) \
            SELECT $1,$2,$3,$4,$5,$5 WHERE (SELECT count(*) FROM app.device_connect_attempts) < 2000 \
            ON CONFLICT (protocol,identity) DO UPDATE SET reason=EXCLUDED.reason, peer=EXCLUDED.peer, \
              last_at=EXCLUDED.last_at, attempts=app.device_connect_attempts.attempts+1")
        .bind(protocol).bind(&ident).bind(reason).bind(peer).bind(now).execute(&st.owner_pool).await {
        tracing::warn!("record_connect_attempt {ident}: {e}");
    }
}
/// L'identità è stata accettata: il tentativo rifiutato non è più attuale.
pub async fn clear_connect_attempt(st: &AppState, protocol: &str, identity: &str) {
    let _ = sqlx::query("DELETE FROM app.device_connect_attempts WHERE protocol=$1 AND identity=$2")
        .bind(protocol).bind(identity).execute(&st.owner_pool).await;
}
/// Pulizia: tentativi fermi da più di 30 giorni (worker).
pub async fn purge_connect_attempts(st: &AppState) {
    let _ = sqlx::query("DELETE FROM app.device_connect_attempts WHERE last_at < $1 - interval '30 days'")
        .bind(st.clock.now()).execute(&st.owner_pool).await;
}
/// Superadmin: dispositivi che provano a collegarsi e vengono rifiutati, con il tenant in cui
/// l'IMEI risulta censito (se c'è) per capire subito cosa sistemare.
pub async fn super_device_attempts(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query(
        "SELECT a.protocol,a.identity,a.reason,a.peer,a.first_at,a.last_at,a.attempts,
                (SELECT string_agg(t.name, ', ' ORDER BY t.name) FROM app.devices d JOIN app.tenants t ON t.id=d.tenant_id
                  WHERE d.external_id=a.identity AND d.source='gps') AS tenants
           FROM app.device_connect_attempts a ORDER BY a.last_at DESC LIMIT 200")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "protocol": r.get::<String,_>("protocol"), "identity": r.get::<String,_>("identity"),
        "reason": r.get::<String,_>("reason"), "reason_label": connect_reason_label(&r.get::<String,_>("reason")),
        "peer": r.try_get::<String,_>("peer").ok(),
        "first_at": r.get::<chrono::DateTime<chrono::Utc>,_>("first_at").to_rfc3339(),
        "last_at": r.get::<chrono::DateTime<chrono::Utc>,_>("last_at").to_rfc3339(),
        "attempts": r.get::<i32,_>("attempts"), "tenants": r.try_get::<String,_>("tenants").ok() })).collect();
    Ok(Json(json!(out)))
}
/// Campi di collegamento della scheda dispositivo (fuori dal json! grande: evita la ricorsione del macro).
pub(crate) fn device_link_fields(v: &mut Value, tid: Uuid, rejected: &std::collections::HashMap<String,(String,chrono::DateTime<chrono::Utc>,i32)>) {
    let gps = v.get("source").and_then(|x| x.as_str()) == Some("gps");
    let ext = v.get("external_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let since = if gps { crate::teltonika::connected_since(tid, &ext).map(|d| d.to_rfc3339()) } else { None };
    let rej = if gps { rejected.get(&ext).map(|(why, at, n)|
        json!({"reason": why, "reason_label": connect_reason_label(why), "last_at": at.to_rfc3339(), "attempts": n})) } else { None };
    if let Some(o) = v.as_object_mut() { o.insert("connected_since".into(), json!(since)); o.insert("rejected".into(), json!(rej)); }
}
pub fn connect_reason_label(r: &str) -> &'static str {
    match r {
        "sconosciuto" => "IMEI non censito in nessuna azienda",
        "conteso" => "IMEI censito in più aziende: identità contesa",
        "modulo_spento" => "azienda senza modulo dispositivi attivo",
        _ => "rifiutato",
    }
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
    // SL-16: 0,0 = nessun fix GPS (il badge lo manda senza satelliti): non è una posizione
    let fix = !(lat == 0.0 && lon == 0.0);
    if source == "gps" && !fix {
        // niente posizione né controlli di area; il contatto è già registrato (touch_device_seen)
        tracing::debug!("teltonika {ext}: record senza fix GPS, posizione ignorata");
    }
    if source == "gps" && fix {
        let prev_batt = device_last_battery(st, tid, source, ext).await;
        if alarm.is_none() { gps_battery_check(st, tid, source, ext, prev_batt, battery, Some(lat), Some(lon), Some(ts_rfc)).await; }
        let mut attrs = match io_attrs { Value::Object(m) => m, _ => serde_json::Map::new() };
        attrs.insert("batteryLevel".into(), json!(battery));
        let pos = json!({"latitude":lat,"longitude":lon,"speed":speed,"course":course,
            "attributes": Value::Object(attrs),
            "id": format!("telt-{}-{}", ext, ts_rfc), "fixTime": ts_rfc, "source": source});
        let _ = process_gps_position(st, tid, &pos, ext).await;
    }
    if let Some(al) = alarm {
        let amap = tool_config(st, tid, "gps", "alarm_map").await;
        let etype = map_gps_etype(&amap, "", Some(&al), battery);
        // SL-4: chiavi STABILI. Il badge ritrasmette lo stesso record AVL se non riceve l'ACK: stesso
        // IMEI + orario del record + allarme = stesso evento (scartato). La correlazione per
        // dispositivo e tipo fa confluire le pressioni ripetute nell'incidente ancora aperto.
        let src_evt = format!("telt-evt-{ext}-{ts_rfc}-{al}");
        let corr = format!("telt-{ext}-{etype}");
        let _ = persist_ingest(st, tid, Normalized{ device_id: None,
            source: source.into(), source_event: src_evt.clone(), idem: src_evt, etype,
            device_external_id: Some(ext.into()), site_name: None,
            lat: fix.then_some(lat), lon: fix.then_some(lon), accuracy_m: None, correlation: corr,
            payload: json!({"channel":"teltonika","alarm":al,"lat":fix.then_some(lat),"lon":fix.then_some(lon),"no_fix":!fix,"battery":battery,"occurred_at":ts_rfc}),
            indoor: false, beacon_uid: None, zone: None }).await;
    }
}

/// CA-17 (worker, ~1 min): ronde e presenze dimenticate aperte. Ronde sul wall-clock SQL (come
/// started_at e le punzonature); presenze sull orologio unico st.clock (come clock_in/out).
/// - ronda in corso oltre finestra + tolleranza + 60 min (8 ore se il giro non ha finestra):
///   chiusa come «abbandonata» (closed_by='auto') con anomalia, così sparisce dalle «in corso» e
///   il giro torna avviabile;
/// - presenza di cantiere aperta da oltre 16 ore: chiusa a durata ZERO e marcata `auto_closed`
///   (da verificare): niente ore o costi inventati.
pub async fn stale_open_watchdog(st: &AppState) {
    let now = st.clock.now();
    // ronde: wall-clock di proposito (now() SQL), come started_at e le punzonature delle ronde
    let rounds = sqlx::query(
        "UPDATE app.patrol_rounds ro SET status='abbandonata', ended_at=now(), closed_by='auto'
           FROM app.patrol_routes rt
          WHERE rt.id=ro.route_id AND ro.status='in_corso'
            AND ro.started_at < now() - make_interval(mins => CASE WHEN rt.window_minutes IS NULL THEN 480
                                                               ELSE rt.window_minutes + COALESCE(rt.grace_min,0) + 60 END)
          RETURNING ro.id, ro.tenant_id, ro.started_at")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for r in rounds {
        let (rid, tid): (Uuid, Uuid) = (r.get("id"), r.get("tenant_id"));
        let started: chrono::DateTime<chrono::Utc> = r.get("started_at");
        let _ = sqlx::query("INSERT INTO app.patrol_anomalies(tenant_id,round_id,kind,detail) VALUES($1,$2,'abbandonata',$3)")
            .bind(tid).bind(rid).bind(format!("Ronda avviata alle {} e mai chiusa: chiusa automaticamente", started.format("%H:%M UTC")))
            .execute(&st.owner_pool).await;
        audit(st, tid, "sistema", "round_auto_close", json!({"round_id":rid})).await;
    }
    let pres = sqlx::query(
        "UPDATE app.commessa_presenze SET clock_out=clock_in, auto_closed=true
          WHERE clock_out IS NULL AND clock_in < $1 - interval '16 hours' RETURNING id, tenant_id, user_id")
        .bind(now).fetch_all(&st.owner_pool).await.unwrap_or_default();
    for p in pres {
        let tid: Uuid = p.get("tenant_id");
        audit(st, tid, "sistema", "presenza_auto_close", json!({"presenza_id":p.get::<Uuid,_>("id"),"user_id":p.get::<Uuid,_>("user_id")})).await;
    }
}
/// SL-17 (25/11/2026): soglia batteria valutata su OGNI record di posizione (Teltonika, OsmAnd),
/// non solo sugli allarmi espliciti del dispositivo. Segnala una volta per discesa: solo quando il
/// valore passa da sopra (o ignoto) a sotto la soglia `battery_min`; la policy `battery_as`
/// (off / warning / critico) decide se è un evento o un allarme con escalation.
pub async fn gps_battery_check(st: &AppState, tid: Uuid, source: &str, ext: &str, prev: Option<f64>, battery: Option<f64>,
    lat: Option<f64>, lon: Option<f64>, ts: Option<&str>) {
    let Some(b) = battery else { return };
    // l'ultimo valore letto si salva SEMPRE (i filtri di movimento della posizione possono non
    // aggiornarlo se il badge è fermo): serve a riconoscere la discesa sotto soglia una volta sola
    let _ = sqlx::query("UPDATE app.devices SET last_battery=$4 WHERE tenant_id=$1 AND source=$2 AND external_id=$3")
        .bind(tid).bind(source).bind(ext).bind(b).execute(&st.owner_pool).await;
    let amap = tool_config(st, tid, "gps", "alarm_map").await;
    let min = amap.get("battery_min").and_then(|v| v.as_i64()).unwrap_or(15) as f64;
    if !(b > 0.0 && b < min) { return; }
    if let Some(p) = prev { if p > 0.0 && p < min { return; } } // era già sotto: già segnalato
    let etype = map_gps_etype(&amap, "", None, Some(b));
    if etype == "device_event" || etype == "ignore" { return; }
    let corr = format!("batt-{ext}");
    let ev = format!("batt-{ext}-{}", ts.map(|x| x.to_string()).unwrap_or_else(|| st.clock.now().timestamp_millis().to_string()));
    let _ = persist_ingest(st, tid, Normalized{ device_id: None,
        source: source.into(), source_event: ev.clone(), idem: ev, etype,
        device_external_id: Some(ext.into()), site_name: None, lat, lon, accuracy_m: None, correlation: corr,
        payload: json!({"channel":"batteria","battery":b,"soglia":min,"occurred_at":ts}),
        indoor: false, beacon_uid: None, zone: None }).await;
}
pub(crate) async fn device_last_battery(st: &AppState, tid: Uuid, source: &str, ext: &str) -> Option<f64> {
    sqlx::query_scalar::<_, Option<f64>>("SELECT last_battery FROM app.devices WHERE tenant_id=$1 AND source=$2 AND external_id=$3")
        .bind(tid).bind(source).bind(ext).fetch_optional(&st.owner_pool).await.ok().flatten().flatten()
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
            sqlx::query("INSERT INTO app.patrol_punches(tenant_id,round_id,checkpoint_id,user_id,method,lat,lon,ok,note,entry) \
                VALUES($1,$2,$3,$4,'gps',$5,$6,true,'auto da dispositivo','auto') \
                ON CONFLICT (round_id,checkpoint_id) DO UPDATE SET ok=true,method='gps',lat=EXCLUDED.lat,lon=EXCLUDED.lon,punched_at=now(),note='auto da dispositivo',entry='auto',attempts=patrol_punches.attempts+1")
                .bind(tid).bind(round_id).bind(cid).bind(uid).bind(lat).bind(lon).execute(&mut **tx).await?;
            n += 1;
        }
    }
    Ok(n)
}
/// Pipeline posizione condivisa da webhook (push) e poller API (pull): filtri #7,
/// storico, ultima posizione, presenza geofence #3, primo-fix->attivo. Idempotente
/// per (tenant,device,source_id) cosi' push e pull convergono senza duplicare.
/// Registra l'ultimo contatto con un dispositivo (qualsiasi pacchetto, anche senza fix GPS).
/// Alimenta il watchdog nativo del silenzio (`worker::gps_silence_watchdog`).
pub async fn touch_device_seen(st: &AppState, tid: Uuid, source: &str, ext: &str) {
    let now = st.clock.now();
    // ritorna il contatto PRECEDENTE: se il dispositivo era silente, chiude la perdita di contatto
    let prev = sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "WITH o AS (SELECT id, last_seen_at AS old FROM app.devices WHERE tenant_id=$1 AND source=$2 AND external_id=$3 FOR UPDATE)
         UPDATE app.devices d SET last_seen_at=$4 FROM o WHERE d.id=o.id RETURNING o.old")
        .bind(tid).bind(source).bind(ext).bind(now).fetch_optional(&st.owner_pool).await;
    match prev {
        Err(e) => tracing::warn!("touch_device_seen {ext}: {e}"),
        Ok(Some(old)) => if old.map(|o| (now - o).num_seconds() > 30).unwrap_or(true) {
            // stesso prefisso di correlazione del watchdog (worker.rs): badge "gps-", app "app-"
            let ch = if source == "pwa" { "app" } else { source };
            silence_auto_close(st, tid, &format!("{ch}-silence-{ext}-")).await;
        },
        Ok(None) => {}
    }
}
/// Q4 (PO 24/11/2026): la perdita di contatto si chiude da sola quando il dispositivo torna in
/// linea. Chiude gli incidenti `perdita_contatto` aperti dal watchdog per quel dispositivo
/// (correlazione `<canale>-silence-<id>-…`), con traccia in timeline e avviso a chi era stato
/// coinvolto. Sempre con tenant_id esplicito (owner_pool bypassa RLS).
pub async fn silence_auto_close(st: &AppState, tid: Uuid, prefix: &str) {
    let now = st.clock.now();
    let outcome = "rientrato: il dispositivo è di nuovo in linea";
    let rows = match sqlx::query(
        "UPDATE app.incidents SET state='chiuso', outcome=$3, closed_at=$4, version=version+1
          WHERE tenant_id=$1 AND etype='perdita_contatto' AND state<>'chiuso' AND left(correlation, length($2))=$2
          RETURNING id, correlation")
        .bind(tid).bind(prefix).bind(outcome).bind(now).fetch_all(&st.owner_pool).await {
        Ok(r) => r, Err(e) => { tracing::warn!("silence_auto_close {prefix}: {e}"); return; } };
    for r in rows {
        let iid: Uuid = r.get("id"); let corr: String = r.get("correlation");
        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
            .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":outcome,"by":"sistema","auto":true,"back_at":now}))
            .execute(&st.owner_pool).await;
        let _ = sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'close',$2)")
            .bind(tid).bind(json!({"incident_id":iid,"summary":format!("close {corr}"),"dedup":corr,"etype":"perdita_contatto"}))
            .execute(&st.owner_pool).await;
        audit(st, tid, "sistema", "incident_auto_close", json!({"incident_id":iid,"motivo":"dispositivo di nuovo in linea"})).await;
        publish_event(st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_closed","incident_id":iid,"auto":true})}).await;
        esc_status_update_notify(st, tid, iid, None, "sistema (dispositivo di nuovo in linea)", "close").await;
    }
}
/// Segno di vita dell'app del lavoratore: aggiorna l'ultimo contatto del SUO dispositivo app
/// (mai di un badge, che ha la sua sorveglianza) e chiude un'eventuale perdita di contatto.
pub(crate) async fn app_seen(st: &AppState, tid: Uuid, dev_id: Uuid) {
    let ext: Option<String> = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE tenant_id=$1 AND id=$2 AND source='pwa'")
        .bind(tid).bind(dev_id).fetch_optional(&st.owner_pool).await.ok().flatten();
    if let Some(ext) = ext { touch_device_seen(st, tid, "pwa", &ext).await; }
}
/// App del lavoratore: battito (~ogni minuto). Dice anche se l'app è sorvegliata adesso.
pub async fn worker_heartbeat(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, crate::modules::feature_modules("app_heartbeat")).await?; // SL-20
    let (dev_id, _ext) = worker_app_device(&st, tid, s.user_id, &s.email).await?;
    app_seen(&st, tid, dev_id).await;
    let in_shift: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.shifts WHERE tenant_id=$1 AND user_id=$2 AND state='in_corso')")
        .bind(tid).bind(s.user_id).fetch_one(&st.owner_pool).await.map_err(ise)?;
    let badge: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.devices WHERE tenant_id=$1 AND assigned_user=$2 AND source='gps' AND service_state='in_servizio' AND COALESCE(enabled,true))")
        .bind(tid).bind(s.user_id).fetch_one(&st.owner_pool).await.map_err(ise)?;
    let after = tool_config(&st, tid, "loneworker", "app_silence").await
        .get("app_silence_after_s").and_then(|v| v.as_i64()).unwrap_or(900);
    Ok(Json(json!({"ok":true, "monitored": in_shift && !badge && after > 0, "in_shift": in_shift,
        "badge_in_service": badge, "silence_after_s": after})))
}

pub async fn process_gps_position(st: &AppState, tid: Uuid, pos: &Value, ext: &str) -> anyhow::Result<Value> {
    touch_device_seen(st, tid, "gps", ext).await;
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
    let fixed_at = clamp_future(st, fixed_at); // IO-10
    // #7 filtri posizione (config per-tenant)
    let fcfg = tool_config(st, tid, "gps", "alarm_map").await;
    let max_jump = fcfg.get("max_jump_kmh").and_then(|v| v.as_f64()).unwrap_or(300.0);
    let min_move = fcfg.get("min_move_m").and_then(|v| v.as_f64()).unwrap_or(5.0);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await?;
    let drow = sqlx::query("SELECT id,last_lat,last_lon,last_pos_at FROM app.devices WHERE source='gps' AND external_id=$1")
        .bind(ext).fetch_optional(&mut *tx).await?;
    let Some(drow) = drow else {
        tx.commit().await?;
        return Ok(json!({"ignored":"device non registrato","external_id":ext}));
    };
    let dev_id: Uuid = drow.get("id");
    // SL-2: aggiorna lo stato di movimento PRIMA dei filtri (una posizione scartata perché troppo
    // vicina è proprio il segnale che il lavoratore è fermo)
    immobility_note_movement(st, tid, dev_id, lat, lon, pos.get("attributes"), fixed_at).await;
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
    // SL-7: se l'area ha dispositivi associati, l'allarme vale solo per quelli
    let assoc: Vec<Uuid> = sqlx::query_scalar("SELECT device_id FROM app.geofence_devices WHERE tenant_id=$1 AND geofence_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if !assoc.is_empty() && !assoc.contains(&dev_id) { return; }
    // SL-7: sorgente REALE del dispositivo (prima "geofence": il dispositivo non veniva ritrovato e
    // l'allarme usciva senza badge né lavoratore)
    let dev = match sqlx::query("SELECT external_id, source FROM app.devices WHERE id=$1 AND tenant_id=$2").bind(dev_id).bind(tid)
        .fetch_optional(&st.owner_pool).await.ok().flatten() { Some(r)=>r, None=>return };
    let (ext, source): (String, String) = (dev.get("external_id"), dev.get("source"));
    let mut etypes: Vec<String> = sqlx::query_scalar("SELECT etype FROM app.geofence_events WHERE geofence_id=$1")
        .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if etypes.is_empty() { etypes.push(if inside { "geofence_ingresso".into() } else { "geofence_uscita".into() }); }
    let at = st.clock.now();
    for et in etypes {
        // SL-7 isteresi: correlazione STABILE per (area, dispositivo, direzione, evento): finché
        // l'incidente è aperto, gli attraversamenti ripetuti al confine confluiscono in quello
        let corr = format!("gf-{gid}-{dev_id}-{dir}-{et}");
        let ev = format!("{corr}-{}", at.timestamp_millis());
        let _ = persist_ingest(st, tid, Normalized{ device_id: None,
            source: source.clone(), source_event: ev.clone(), idem: ev, etype: et,
            device_external_id: Some(ext.clone()), site_name: None,
            lat, lon, accuracy_m: None, correlation: corr,
            payload: json!({"geofence": name, "direction": dir, "channel": "geofence"}),
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


// ---------------- Dispositivi ----------------
pub async fn list_devices(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
    let acl = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT d.id,d.external_id,d.source,d.label,d.enabled,d.config,d.site_id,d.assigned_user,
                d.last_lat,d.last_lon,d.last_battery,d.last_pos_at,d.last_attributes,d.model_code,d.assigned_asset,
                d.last_seen_at,d.floorplan_id,d.fp_x,d.fp_y,d.service_state,d.silence_armed_at,
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
    // soglia di silenzio REALE del tenant (la stessa del watchdog): la scheda la usa per "in linea"
    let silence_after_s = tool_config(&st, s.tenant_id.unwrap(), "gps", "alarm_map").await
        .get("silence_after_s").and_then(|v| v.as_i64()).unwrap_or(600);
    // tentativi rifiutati dei badge DI QUESTO tenant (solo i suoi IMEI: niente fuga di dati altrui)
    let gps_ids: Vec<String> = rows.iter().filter(|r| r.get::<String,_>("source") == "gps")
        .map(|r| r.get::<String,_>("external_id")).collect();
    let rejected: std::collections::HashMap<String,(String,chrono::DateTime<chrono::Utc>,i32)> = if gps_ids.is_empty() { Default::default() } else {
        sqlx::query("SELECT identity,reason,last_at,attempts FROM app.device_connect_attempts WHERE protocol='teltonika' AND identity = ANY($1)")
            .bind(&gps_ids).fetch_all(&st.owner_pool).await.unwrap_or_default().iter()
            .map(|r| (r.get::<String,_>("identity"), (r.get::<String,_>("reason"), r.get::<chrono::DateTime<chrono::Utc>,_>("last_at"), r.get::<i32,_>("attempts"))))
            .collect() };
    let tid0 = s.tenant_id.unwrap();
    // Q4: lavoratori in turno adesso (turno aperto o pianificato): la scheda dice se il silenzio è sorvegliato
    let now0 = st.clock.now();
    let on_shift: std::collections::HashSet<Uuid> = sqlx::query_scalar::<_, Uuid>(
        "SELECT user_id FROM app.shifts WHERE tenant_id=$1 AND state='in_corso'
         UNION SELECT user_id FROM app.shift_plans WHERE tenant_id=$1 AND state<>'annullato' AND starts_at <= $2 AND ends_at > $2")
        .bind(tid0).bind(now0).fetch_all(&st.owner_pool).await.unwrap_or_default().into_iter().collect();
    let shift_only = tool_config(&st, tid0, "gps", "alarm_map").await
        .get("silence_shift_only").and_then(|v| v.as_str()).map(|s| s == "si").unwrap_or(false); // PO 25/11: opt-in
    let sup = is_supervisor(&s); // CA-11: posizioni dei colleghi solo a chi supervisiona
    let out: Vec<Value> = rows.iter()
        .filter(|r| site_allowed(&acl, r.try_get::<Uuid,_>("site_id").ok()))
        .filter(|r| sup || r.try_get::<Uuid,_>("assigned_user").ok() == Some(s.user_id))
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
        "last_seen_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("last_seen_at").ok(),
        "floorplan_id": r.try_get::<Uuid,_>("floorplan_id").ok(),
        "fp_x": r.try_get::<f64,_>("fp_x").ok(), "fp_y": r.try_get::<f64,_>("fp_y").ok(),
        "silence_after_s": silence_after_s,
        "service_state": r.try_get::<String,_>("service_state").ok(),
        "silence_armed_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("silence_armed_at").ok(),
        "prov_step": r.try_get::<String,_>("prov_step").ok() })).map(|mut v| { device_link_fields(&mut v, tid0, &rejected);
            let on = v.get("assigned_user").and_then(|x| x.as_str()).and_then(|x| Uuid::parse_str(x).ok()).map(|u| on_shift.contains(&u)).unwrap_or(false);
            if let Some(o) = v.as_object_mut() { o.insert("in_shift".into(), json!(on)); o.insert("silence_shift_only".into(), json!(shift_only)); }
            v }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct DeviceReq { pub external_id: String, pub source: String, pub label: Option<String>,
    pub site_id: Option<Uuid>, pub assigned_user: Option<Uuid>, pub enabled: Option<bool>, pub config: Option<Value> }
pub async fn create_device(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DeviceReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    let tid = s.tenant_id.unwrap();
    if b.external_id.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "external_id richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,assigned_user,enabled,config)
         VALUES($1,$2,$3,$4,$5,$6,COALESCE($7,true),COALESCE($8,'{}'::jsonb)) RETURNING id")
        .bind(tid).bind(b.external_id.trim()).bind(&b.source).bind(&b.label).bind(b.site_id).bind(b.assigned_user)
        .bind(b.enabled).bind(b.config.clone())
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "dispositivo gia' presente (source+external_id)"))?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "dispositivo gia' presente"))?.get::<Uuid,_>("id");
    if b.assigned_user.is_some() { record_device_assignment(&mut tx, tid, id, None, b.assigned_user, st.clock.now()).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_device", json!({"external_id":b.external_id,"source":b.source})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn update_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<DeviceReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let old: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.devices WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
    let n = sqlx::query(
        "UPDATE app.devices SET external_id=$2,source=$3,label=$4,site_id=$5,assigned_user=$6,
            enabled=COALESCE($7,enabled),config=COALESCE($8,config),updated_at=now(),
            -- passaggio di consegne di un badge in servizio: il conteggio riparte (orologio app)
            silence_armed_at=CASE WHEN assigned_user IS DISTINCT FROM $6 THEN $9 ELSE silence_armed_at END
          WHERE id=$1")
        .bind(id).bind(b.external_id.trim()).bind(&b.source).bind(&b.label).bind(b.site_id).bind(b.assigned_user)
        .bind(b.enabled).bind(b.config.clone()).bind(st.clock.now()).execute(&mut *tx).await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23505" => err(StatusCode::CONFLICT, "identificativo già registrato (anche in un altro tenant per i dispositivi GPS)"),
            _ => ise(e),
        })?.rows_affected();
    if n>0 { record_device_assignment(&mut tx, tid, id, old, b.assigned_user, st.clock.now()).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente")); }
    audit(&st, tid, &s.email, "update_device", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_device(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
    require_any_module(&st, tid, DEVICE_MODS).await?;
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
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    const SRC: &[&str] = &["gps","iot"];
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
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?; // AR-1: controllo del modulo dichiarato
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

// ================= Fase A — Catalogo modelli dispositivo =================
pub async fn list_catalog(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), DEVICE_MODS).await?;
    let term = q.get("q").filter(|s| !s.is_empty()).map(|s| format!("%{}%", s.to_lowercase()));
    let manu = q.get("manufacturer").filter(|s| !s.is_empty()).cloned();
    let cat = q.get("category").filter(|s| !s.is_empty()).cloned();
    // scope = perimetro: 'iot' -> solo sensori; 'safety' -> tracker sicurezza/campo (il dump GPS
    // 'altro', 2174 modelli GPS generici, compare solo se si CERCA, per non annegare il catalogo curato).
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
    let s_ = require_tenant(&st, &h).await?;
    require_any_module(&st, s_.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    // facets coerenti col perimetro; il bucket 'altro' (catalogo GPS generico) è sempre escluso dai filtri
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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
    if b.user_id.is_some() && b.asset_id.is_some() { return Err(err(StatusCode::BAD_REQUEST, "assegna a utente OPPURE asset, non entrambi")); }
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let old: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.devices WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.flatten();
    let n = sqlx::query("UPDATE app.devices SET assigned_user=$2, assigned_asset=$3, updated_at=now(),
            silence_armed_at=CASE WHEN assigned_user IS DISTINCT FROM $2 THEN $4 ELSE silence_armed_at END WHERE id=$1")
        .bind(id).bind(b.user_id).bind(b.asset_id).bind(st.clock.now()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
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
pub(crate) fn dok_cap_to_etype(cap: &str) -> Option<&'static str> {
    match cap.to_ascii_lowercase().as_str() {
        "sos" => Some("sos"),
        "fall" | "falldown" | "man_down" | "mandown" => Some("man_down"),
        "movement" | "motion" | "immobility" | "immobilita" => Some("immobilita"),
        "battery" => Some("batteria_bassa"),
        _ => None, // gps/ignition/temperature/immobilizer/ecc.: non sono allarmi di sicurezza diretti
    }
}
pub(crate) struct DokPlan { needs_checkin: bool, suggest_profile: bool, label: &'static str }
pub(crate) fn dok_category_plan(cat: &str) -> DokPlan {
    match cat {
        "personale"  => DokPlan{needs_checkin:true,  suggest_profile:true,  label:"Lavoratore isolato"},
        "smartphone" => DokPlan{needs_checkin:true,  suggest_profile:true,  label:"Smartphone / app"},
        "veicolo"    => DokPlan{needs_checkin:false, suggest_profile:false, label:"Veicolo / flotta"},
        "asset"      => DokPlan{needs_checkin:false, suggest_profile:false, label:"Asset / attrezzatura"},
        "sensore"    => DokPlan{needs_checkin:false, suggest_profile:false, label:"Sensore IoT"},
        _            => DokPlan{needs_checkin:false, suggest_profile:false, label:"Generico"},
    }
}
pub(crate) fn dok_ingress_base() -> String { std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_else(|_| "http://localhost:8080".into()) }

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
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
         VALUES($1,$2,'gps',$3,$4,$5,true,$6,$7) RETURNING id")
        .bind(tid).bind(b.external_id.trim()).bind(&label).bind(b.site_id).bind(b.user_id).bind(&dcfg).bind(&b.model_code)
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "dispositivo gia' presente (uniqueId)"))?;
    let dev_id = drow.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id");
    if b.user_id.is_some() { record_device_assignment(&mut tx, tid, dev_id, None, b.user_id, now).await.map_err(ise)?; }
    // 2) gruppo di escalation (se fornito). SL-11 (25/11/2026): prima si SOVRASCRIVEVA il routing di
    // tutto il tenant per quei tipi di allarme (gli SOS di tutti finivano al gruppo del nuovo badge).
    // Ora: se si crea il profilo del lavoratore il gruppo vale solo per lui (profilo); altrimenti si
    // instradano solo i tipi che non hanno ancora un gruppo, senza toccare quelli già instradati.
    let mut routed: Vec<String> = vec![];
    let mut routing_kept: Vec<String> = vec![];
    let will_profile = plan.suggest_profile && b.make_profile.unwrap_or(false) && b.user_id.is_some();
    if let Some(g) = b.group_id {
        let okg = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(g).bind(tid).fetch_optional(&mut *tx).await.map_err(ise)?.is_some();
        if !okg { return Err(err(StatusCode::BAD_REQUEST, "gruppo inesistente")); }
        if !will_profile {
            // SL-19: si instradano solo i tipi che aprono un allarme (la batteria bassa no)
            for et in &etypes {
                if !CRITICAL.contains(&et.as_str()) { continue; }
                let n = sqlx::query("INSERT INTO app.alarm_routing(tenant_id,etype,group_id) VALUES($1,$2,$3)
                     ON CONFLICT (tenant_id,etype) DO NOTHING")
                    .bind(tid).bind(et).bind(g).execute(&mut *tx).await.map_err(ise)?.rows_affected();
                if n > 0 { routed.push(et.clone()); } else { routing_kept.push(et.clone()); }
            }
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
                "geofence": true }); // SL-18: allarmi di area attivi come gli altri trigger
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
    let secret = format!("dev-{}", &auth::new_token()[..24]);
    let hash = hash_token(&secret);
    let handle = format!("dev-{}", &secret[4..12]);
    sqlx::query("INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id,label) VALUES($1,$2,$3,'gps',$4,$5)")
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
        "routing_kept": routing_kept,
        "token": secret, "handle": handle,
        // Dati per la guida di collegamento (la console costruisce i passi per protocollo).
        "server_host": host, "teltonika_port": tport,
        "ingress_url": format!("{base}/api/ingress/gps?token={secret}"),
        "osmand_url": format!("{base}/api/ingress/osmand?token={secret}&id={}&lat={{lat}}&lon={{lon}}&timestamp={{unix}}", b.external_id.trim()),
    }))
}

/// Readiness/idoneità del device per i suoi segnali: dichiarato (dalle capacità del modello) vs
/// verificato (abbiamo mai ricevuto quel segnale). Stati: registrazione · assegnazione · connessione · protezione.
pub async fn device_readiness(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "devices.admin")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("devices")).await?; // AR-4: servizio abilitato dai moduli
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
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    let ext: Option<String> = sqlx::query_scalar("SELECT external_id FROM app.devices WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let Some(ext) = ext else { return Err(err(StatusCode::NOT_FOUND, "device inesistente")); };
    let corr = format!("TEST-SOS-{}", &Uuid::new_v4().to_string()[..8]);
    let src_evt = format!("test-sos-{}", Uuid::new_v4());
    let r = persist_ingest(&st, tid, Normalized{ device_id: None,
        source:"gps".into(), source_event: src_evt.clone(), idem: src_evt,
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
    pub user_id: Option<Uuid>, pub asset_id: Option<Uuid> }
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
    // crea device (source 'gps': decodifica nativa Teltonika / OsmAnd / JSON HTTP)
    let dcfg = json!({"model_code": b.model_code, "capabilities": caps, "protocol": protocol});
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let drow = sqlx::query(
        "INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,assigned_user,assigned_asset,enabled,config,model_code)
         VALUES($1,$2,'gps',$3,$4,$5,$6,true,$7,$8) RETURNING id")
        .bind(tid).bind(b.external_id.trim()).bind(&b.label).bind(b.site_id).bind(b.user_id).bind(b.asset_id)
        .bind(&dcfg).bind(&b.model_code)
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "dispositivo gia' presente (uniqueId)"))?;
    let dev_id = drow.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id");
    if b.user_id.is_some() { record_device_assignment(&mut tx, tid, dev_id, None, b.user_id, st.clock.now()).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    // default alarm_map dal modello SOLO se il tenant non ha gia' una config propria
    let has_cfg: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='gps' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    if has_cfg.is_none() && def_cfg.is_object() && !def_cfg.as_object().unwrap().is_empty() {
        let mut ttx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let _ = sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'gps','alarm_map',$2,$3) ON CONFLICT DO NOTHING")
            .bind(tid).bind(&def_cfg).bind(s.user_id).execute(&mut *ttx).await;
        let _ = ttx.commit().await;
    }
    let step = "in_attesa_primo_fix".to_string();
    sqlx::query("INSERT INTO app.device_provisioning(device_id,tenant_id,step,last_error) VALUES($1,$2,$3,NULL)
        ON CONFLICT (device_id) DO UPDATE SET step=EXCLUDED.step,last_error=NULL,updated_at=now()")
        .bind(dev_id).bind(tid).bind(&step).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "provision_device", json!({"model":b.model_code,"external_id":b.external_id})).await;
    Ok(Json(json!({"device_id":dev_id,"step":step,
        "install":{"protocol":protocol,"port":port,"note":"Configura il dispositivo verso l'indirizzo e la porta di YouWorkforce (Teltonika: porta TCP nativa; smartphone: app OsmAnd)."}})))
}


