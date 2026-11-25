//! IoT nativo: telemetria, attributi, comandi, blueprint, MQTT, alert e monitoraggio IoT, scenari
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

// ===== IoT nativo F1: telemetria time-series =====
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
/// IO-10: orario dichiarato dal dispositivo oltre 2 minuti nel futuro -> ora di ricezione.
pub fn clamp_future(st: &AppState, ts: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    let now = st.clock.now();
    if ts > now + chrono::Duration::minutes(2) { tracing::debug!("orario dispositivo nel futuro ({ts}): uso l'ora di ricezione"); now } else { ts }
}
pub async fn persist_telemetry(st: &AppState, tid: Uuid, dev_id: Uuid,
    ts: chrono::DateTime<chrono::Utc>, values: &serde_json::Map<String,Value>,
    attrs: Option<&serde_json::Map<String,Value>>) -> Result<(usize, Value, Value),(StatusCode,String)> {
    // IO-10: un orario nel futuro (orologio del dispositivo sbagliato) lo farebbe risultare sempre
    // in linea e sfalserebbe le serie: oltre 2 minuti di anticipo vale l'ora di ricezione
    let ts = clamp_future(st, ts);
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
    // IO-1: «telemetria» = dato registrato, nessun allarme (come dice la UI)
    if let Some(hit) = eval_sensor_rules_hit(st, tid, Some(dev_id), &payload).await.filter(|h| h.etype != "telemetria") {
        let etype = hit.etype.clone();
        // 1) alert IoT in VISTA SEPARATA (sempre): è il mestiere IoT, non la sicurezza del lavoratore.
        alert = raise_iot_soglia_alert(st, tid, dev_id, &hit).await;
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
        // IO-13: il segreto va in un'intestazione HTTP (non nell'URL, che finisce nei log); il
        // parametro ?token= resta accettato per i dispositivi già configurati
        "lorawan" => json!({"protocol":"lorawan",
            "webhook_url": "/api/ingress/lorawan",
            "header_name": "x-ingest-token", "header_value": secret,
            "note":"Imposta questo URL come webhook di uplink su TTN/ChirpStack e aggiungi l'intestazione indicata (serve un decoder a monte che produca il payload decodificato)."}),
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
    // IO-12: metodo non più libero. Per i dispositivi IoT un nome di metodo (lettere, cifre, _ . : -,
    // max 64) e parametri contenuti; il testo GPRS grezzo verso i badge resta riservato a devices.admin.
    let m = b.method.trim();
    let gps_text_ok = m.len() <= 200 && !m.chars().any(|c| c.is_control());
    let rpc_ok = m.len() <= 64 && m.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c));
    if b.params.as_ref().map(|v| v.to_string().len()).unwrap_or(0) > 4096 { return Err(err(StatusCode::BAD_REQUEST, "parametri troppo grandi (max 4 KB)")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let drow = sqlx::query("SELECT source,external_id FROM app.devices WHERE id=$1").bind(dev).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"device inesistente"))?;
    let (src, ext): (String, String) = (drow.get("source"), drow.try_get::<String,_>("external_id").unwrap_or_default());
    // un device GPS riceve il metodo come comando GPRS grezzo: stesso permesso del comando libero
    if src == "gps" && !has_cap(&s, "devices.admin") {
        return Err(err(StatusCode::FORBIDDEN, "i comandi verso dispositivi GPS richiedono l'amministrazione dispositivi"));
    }
    if (src == "gps" && !gps_text_ok) || (src != "gps" && !rpc_ok) {
        return Err(err(StatusCode::BAD_REQUEST, "metodo non valido: usa un nome di comando (lettere, cifre, _ . : -)"));
    }
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.iot_commands(tenant_id,device_id,method,params,created_by) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(dev).bind(b.method.trim()).bind(&b.params).bind(s.user_id).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // consegna immediata se il device è sul bridge Teltonika (source gps) di QUESTO tenant;
    // altrimenti resta in coda (il device fa pull)
    let mut status="pending"; let mut result: Option<String> = None;
    if src=="gps" && !ext.is_empty() {
        match crate::teltonika::send_command(tid, dev, &ext, b.method.trim()).await {
            Ok(r)=>{ status="done"; result=Some(r); }, Err(e)=>{ status="error"; result=Some(e); }
        }
        let mut tx2 = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        sqlx::query("UPDATE app.iot_commands SET status=$2,result=$3,sent_at=now() WHERE id=$1").bind(id).bind(status).bind(&result).execute(&mut *tx2).await.map_err(ise)?;
        tx2.commit().await.map_err(ise)?;
    }
    audit(&st, tid, &s.email, "iot_command", json!({"device":dev,"command_id":id,"method":m,"status":status})).await; // IO-12
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
pub(crate) fn mqtt_topic_ext(topic: &str, tid: Uuid) -> Result<Option<String>, String> {
    let p: Vec<&str> = topic.split('/').collect();
    if p.len() >= 3 && p[0] == "yw" {
        if p[1] != tid.to_string() { return Err("il tenant del topic non combacia con la credenziale".into()); }
        return Ok(Some(p[2].to_string()));
    }
    Ok(None)
}
/// Risolve il device dal token (bound) o dall'external_id del topic, con difesa in profondità:
/// se il token è vincolato a un device E il topic porta un external_id, DEVONO combaciare.
pub(crate) async fn mqtt_device(st: &AppState, tid: Uuid, bound: Option<Uuid>, ext_topic: Option<&str>) -> Result<Uuid, String> {
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
    // una sola istruzione: i comandi marcati «inviati» sono esattamente quelli restituiti (niente
    // comandi inseriti fra lettura e aggiornamento persi). Se la scrittura sul socket fallisce,
    // mqtt_commands_unsent li rimette in coda.
    let mut cmds = sqlx::query("UPDATE app.iot_commands SET status='sent',sent_at=now() WHERE device_id=$1 AND status='pending' RETURNING id,method,params,created_at")
        .bind(dev_id).fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    cmds.sort_by_key(|r| r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"));
    Ok(cmds.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"method":r.get::<String,_>("method"),"params":r.try_get::<Value,_>("params").ok()})).collect())
}
/// Rimette in coda i comandi che non è stato possibile scrivere al dispositivo.
pub async fn mqtt_commands_unsent(st: &AppState, tid: Uuid, ids: &[Uuid]) {
    if ids.is_empty() { return; }
    let _ = sqlx::query("UPDATE app.iot_commands SET status='pending',sent_at=NULL WHERE tenant_id=$1 AND id=ANY($2) AND status='sent'")
        .bind(tid).bind(ids).execute(&st.owner_pool).await;
}

// ================= IoT Fase 3 — alert IoT in VISTA SEPARATA =================
// Gli alert di processo IoT (soglia / offline / ripristino) vivono in app.iot_alerts, con un ciclo
// di vita PROPRIO e distinto dagli incidenti di sicurezza (SOS/man_down + escalation). Notifica
// nativa = SSE 'iot_alert' + record persistente consultabile dalla vista "Alert IoT".
pub(crate) fn iot_severity_for(etype: &str) -> &'static str {
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
pub(crate) async fn raise_iot_soglia_alert(st: &AppState, tid: Uuid, device_id: Uuid, hit: &RuleHit) -> Value {
    // IO-1: la regola è quella che ha deciso (nel suo ambito, la più grave), non la prima con lo stesso evento
    let etype = hit.etype.as_str();
    let (metric, threshold, value, label) = (hit.metric.clone(), hit.threshold, hit.value, hit.label.clone());
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
         JOIN app.tenants tn ON tn.id=d.tenant_id AND tn.status<>'sospeso'
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
/// Hook di test (solo TEST_CLOCK): forza una passata del watchdog del silenzio dei dispositivi GPS.
pub async fn test_gps_silence_watchdog(State(st): State<AppState>) -> R {
    if !st.clock.test_mode { return Err(err(StatusCode::FORBIDDEN,"test clock disabled")); }
    crate::worker::gps_silence_watchdog(&st).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Hook di test (solo TEST_CLOCK): una passata del controllo ronde/presenze dimenticate (CA-17).
pub async fn test_stale_open_watchdog(State(st): State<AppState>) -> R {
    if !st.clock.test_mode { return Err(err(StatusCode::FORBIDDEN,"test clock disabled")); }
    stale_open_watchdog(&st).await;
    Ok(Json(json!({"ok":true})))
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
    require_any_module(&st, tid, IOT_MODS).await?;
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
    require_any_module(&st, tid, IOT_MODS).await?;
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
    require_any_module(&st, s.tenant_id.unwrap(), IOT_MODS).await?; // AR-1: controllo del modulo dichiarato
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
    require_any_module(&st, s.tenant_id.unwrap(), IOT_MODS).await?; // AR-1: controllo del modulo dichiarato
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
    require_any_module(&st, tid, IOT_MODS).await?;
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


// ================= Ambienti / casi d'uso IoT (scenari attivabili) =================
// Scenari attivabili dal superadmin su QUALSIASI tenant. Applicare uno scenario provisiona,
// riusando le primitive esistenti: abilita i moduli giusti, inserisce un bundle di regole
// sensori (metrica/operatore/soglia -> etype) e, se richiesto, crea device + sito demo.
// Reversibile (rimozione pulita di regole e demo). Catalogo parametrico nel codice.

pub(crate) struct BScnRule { metric: &'static str, op: &'static str, threshold: Option<f64>, etype: &'static str }
pub(crate) struct BScnDev { suffix: &'static str, label: &'static str }
pub(crate) struct BScenario { code: &'static str, name: &'static str, sector: &'static str, desc: &'static str,
    modules: Vec<&'static str>, rules: Vec<BScnRule>, devices: Vec<BScnDev> }

/// Scenari PREDEFINITI: sorgente del seed di app.iot_scenarios (IO-4). A runtime vale il DB.
pub(crate) fn builtin_scenarios() -> Vec<BScenario> {
    // operatori numerici con soglia, oppure is_true/is_false (threshold None).
    let r = |metric, op, threshold, etype| BScnRule{ metric, op, threshold, etype };
    let d = |suffix, label| BScnDev{ suffix, label };
    let crit = "sensore_critico"; let tele = "telemetria";
    vec![
      BScenario{ code:"smart_energy", name:"Smart Energy", sector:"Energia", desc:"Consumo elettrico per piano con uno smart meter per piano: tensione, corrente, frequenza, energia; allarmi su sovra/sotto-tensione e calo di frequenza.",
        modules:vec!["iot"],
        rules:vec![ r("voltage",">=",Some(253.0),crit), r("voltage","<=",Some(207.0),crit), r("frequency","<=",Some(49.5),crit), r("current",">=",Some(32.0),tele), r("energy_total",">=",Some(1000.0),tele) ],
        devices:vec![ d("meter-a","Smart Meter A (Piano 1)"), d("meter-b","Smart Meter B (Piano 2)"), d("meter-c","Smart Meter C (Piano 3)") ] },
      BScenario{ code:"environment_monitoring", name:"Environment Monitoring", sector:"Ambiente", desc:"Monitoraggio ambientale indoor/outdoor con sensori multipli.",
        modules:vec!["iot"],
        rules:vec![ r("temperature",">=",Some(35.0),crit), r("humidity",">=",Some(85.0),tele), r("co2",">=",Some(1500.0),crit) ],
        devices:vec![ d("env-1","Sensore ambientale 1") ] },
      BScenario{ code:"smart_office", name:"Smart Office", sector:"Edifici", desc:"Clima indoor ufficio, salute/sicurezza dipendenti, consumo risorse.",
        modules:vec!["iot"],
        rules:vec![ r("co2",">=",Some(1200.0),crit), r("temperature",">=",Some(30.0),tele), r("smoke","is_true",None,crit) ],
        devices:vec![ d("office-1","Sensore ufficio 1") ] },
      BScenario{ code:"water_metering", name:"Water Metering", sector:"Utility", desc:"Consumo acqua dai contatori con rilevazione perdite/frodi.",
        modules:vec!["iot"],
        rules:vec![ r("leak","is_true",None,crit), r("flow",">=",Some(100.0),tele), r("fraud","is_true",None,crit) ],
        devices:vec![ d("water-1","Contatore acqua 1") ] },
      BScenario{ code:"smart_retail", name:"Smart Retail (catena freddo)", sector:"Retail", desc:"Frigo/freezer e asset di supermercato: qualita' alimentare e disponibilita'.",
        modules:vec!["iot"],
        rules:vec![ r("temperature",">=",Some(8.0),crit), r("temperature","<=",Some(-25.0),crit), r("door_open","is_true",None,tele) ],
        devices:vec![ d("chiller-1","Frigo 1"), d("freezer-1","Freezer 1") ] },
      BScenario{ code:"smart_farming", name:"Smart Farming", sector:"Agricoltura", desc:"Stoccaggio cereali in silos: livello, temperatura, umidità grano, aerazione. Allarmi su surriscaldamento, grano umido (muffa) e scorta bassa.",
        modules:vec!["iot"],
        rules:vec![ r("temperature",">=",Some(30.0),crit), r("moisture",">=",Some(25.0),crit), r("humidity",">=",Some(80.0),crit), r("crop_level","<=",Some(15.0),tele) ],
        devices:vec![ d("silo-a","Silo A"), d("silo-b","Silo B"), d("silo-c","Silo C") ] },
      BScenario{ code:"site_fleet_tracking", name:"Site Fleet Tracking", sector:"Mezzi", desc:"Escavatori, camion e mezzi in cantiere/miniera: posizione e carburante.",
        modules:vec!["loneworker","iot"],
        rules:vec![ r("fuel","<=",Some(10.0),crit), r("engine_temp",">=",Some(110.0),crit) ],
        devices:vec![ d("vehicle-1","Mezzo 1") ] },
      BScenario{ code:"smart_metering", name:"Smart Metering", sector:"Utility", desc:"Dati smart meter per consumo risorse: perdite, anomalie, frodi.",
        modules:vec!["iot"],
        rules:vec![ r("leak","is_true",None,crit), r("fraud","is_true",None,crit), r("consumption",">=",Some(1000.0),tele) ],
        devices:vec![ d("smartmeter-1","Smart meter 1") ] },
      BScenario{ code:"health_care", name:"Health Care (assisted living)", sector:"Sanita'", desc:"Metriche salute paziente da device connessi, allerta su variazioni critiche.",
        modules:vec!["loneworker","iot"],
        rules:vec![ r("heart_rate",">=",Some(120.0),crit), r("heart_rate","<=",Some(40.0),crit), r("fall","is_true",None,"man_down") ],
        devices:vec![ d("wearable-1","Wearable paziente 1") ] },
      BScenario{ code:"air_quality", name:"Air Quality Monitoring", sector:"Ambiente", desc:"Qualita' dell'aria su piu' sedi: PM2.5, CO2, umidita', temperatura.",
        modules:vec!["iot"],
        rules:vec![ r("pm25",">=",Some(35.0),crit), r("co2",">=",Some(1000.0),crit), r("humidity",">=",Some(90.0),tele), r("temperature",">=",Some(40.0),tele) ],
        devices:vec![ d("air-1","Centralina aria 1") ] },
      BScenario{ code:"smart_irrigation", name:"Smart Irrigation", sector:"Agricoltura", desc:"Impianto di irrigazione e campi: umidita' suolo, posizione, statistiche.",
        modules:vec!["iot"],
        rules:vec![ r("soil_moisture","<=",Some(15.0),crit), r("flow","<=",Some(1.0),tele) ],
        devices:vec![ d("irrig-1","Nodo irrigazione 1") ] },
      BScenario{ code:"waste_management", name:"Waste Management", sector:"Utility", desc:"Cassonetti e raccolta: livello di riempimento e pattern di raccolta.",
        modules:vec!["iot"],
        rules:vec![ r("fill_level",">=",Some(90.0),crit), r("fill_level",">=",Some(70.0),tele) ],
        devices:vec![ d("bin-1","Cassonetto 1") ] },
      BScenario{ code:"tank_level", name:"Tank Level Monitoring", sector:"Industria", desc:"Serbatoi carburante/acqua/chimici: livelli e trend di consumo.",
        modules:vec!["iot"],
        rules:vec![ r("level","<=",Some(10.0),crit), r("level",">=",Some(95.0),crit) ],
        devices:vec![ d("tank-1","Serbatoio 1") ] },
      BScenario{ code:"scada_pool", name:"SCADA Swimming Pool", sector:"SCADA", desc:"Sistemi piscina e processi industriali in tempo reale.",
        modules:vec!["iot"],
        rules:vec![ r("ph","<=",Some(6.8),crit), r("ph",">=",Some(7.8),crit), r("chlorine","<=",Some(0.5),crit), r("temperature",">=",Some(32.0),tele) ],
        devices:vec![ d("pool-1","Centralina piscina 1") ] },
      BScenario{ code:"scada_oil_gas", name:"SCADA Oil & Gas Drilling", sector:"SCADA", desc:"Perforazione: parametri chiave (rig, pompe, BOP) con focus sicurezza.",
        modules:vec!["iot"],
        rules:vec![ r("pressure",">=",Some(350.0),crit), r("gas",">=",Some(20.0),crit), r("vibration",">=",Some(80.0),crit) ],
        devices:vec![ d("rig-1","Rig 1") ] },
      BScenario{ code:"scada_energy", name:"SCADA Energy Management", sector:"SCADA", desc:"Flussi energetici e infrastruttura: efficienza, picchi di domanda, costi.",
        modules:vec!["iot"],
        rules:vec![ r("demand_kw",">=",Some(500.0),crit), r("power_factor","<=",Some(0.8),tele) ],
        devices:vec![ d("grid-1","Nodo rete 1") ] },
    ]
}

// ---- IO-4 (25/11/2026): catalogo scenari nel DATABASE (app.iot_scenarios, mig 0143) ----
/// Spec dei widget INCORPORATE nel binario (prima lette da file con percorso relativo).
pub(crate) const BUILTIN_SPECS: &[(&str, &str)] = &[
    ("air_quality", include_str!("../scenario_specs/air_quality.json")),
    ("environment_monitoring", include_str!("../scenario_specs/environment_monitoring.json")),
    ("health_care", include_str!("../scenario_specs/health_care.json")),
    ("scada_energy", include_str!("../scenario_specs/scada_energy.json")),
    ("scada_oil_gas", include_str!("../scenario_specs/scada_oil_gas.json")),
    ("scada_pool", include_str!("../scenario_specs/scada_pool.json")),
    ("site_fleet_tracking", include_str!("../scenario_specs/site_fleet_tracking.json")),
    ("smart_energy", include_str!("../scenario_specs/smart_energy.json")),
    ("smart_farming", include_str!("../scenario_specs/smart_farming.json")),
    ("smart_irrigation", include_str!("../scenario_specs/smart_irrigation.json")),
    ("smart_metering", include_str!("../scenario_specs/smart_metering.json")),
    ("smart_office", include_str!("../scenario_specs/smart_office.json")),
    ("smart_retail", include_str!("../scenario_specs/smart_retail.json")),
    ("tank_level", include_str!("../scenario_specs/tank_level.json")),
    ("waste_management", include_str!("../scenario_specs/waste_management.json")),
    ("water_metering", include_str!("../scenario_specs/water_metering.json")),
];
#[derive(Clone, Debug)]
pub(crate) struct ScnRule { metric: String, op: String, threshold: Option<f64>, etype: String }
#[derive(Clone, Debug)]
pub(crate) struct ScnDev { suffix: String, label: String }
#[derive(Clone, Debug)]
pub(crate) struct Scenario { pub(crate) code: String, pub(crate) name: String, sector: String, desc: String, modules: Vec<String>,
    rules: Vec<ScnRule>, devices: Vec<ScnDev>, spec: Option<Value>, ai_focus: Option<String>, builtin: bool, enabled: bool }
impl Scenario {
    fn focus(&self) -> String { self.ai_focus.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| scenario_ai_focus(&self.code).to_string()) }
    fn from_row(r: &sqlx::postgres::PgRow) -> Scenario {
        let rules: Vec<ScnRule> = r.try_get::<Value,_>("rules").ok().and_then(|v| v.as_array().cloned()).unwrap_or_default().iter()
            .filter_map(|x| Some(ScnRule{ metric: x.get("metric")?.as_str()?.to_string(), op: x.get("op")?.as_str()?.to_string(),
                threshold: x.get("threshold").and_then(|t| t.as_f64()), etype: x.get("etype")?.as_str()?.to_string() })).collect();
        let devices: Vec<ScnDev> = r.try_get::<Value,_>("devices").ok().and_then(|v| v.as_array().cloned()).unwrap_or_default().iter()
            .filter_map(|x| Some(ScnDev{ suffix: x.get("suffix")?.as_str()?.to_string(), label: x.get("label")?.as_str()?.to_string() })).collect();
        Scenario{ code: r.get("code"), name: r.get("name"), sector: r.get("sector"), desc: r.get("descr"),
            modules: r.try_get::<Vec<String>,_>("modules").unwrap_or_default(), rules, devices,
            spec: r.try_get::<Option<Value>,_>("spec").ok().flatten(), ai_focus: r.try_get::<Option<String>,_>("ai_focus").ok().flatten(),
            builtin: r.get("builtin"), enabled: r.get("enabled") }
    }
}
/// Catalogo corrente dal DB (anche gli scenari disattivati: `enabled` dice se si possono applicare).
pub(crate) async fn scenario_catalog(st: &AppState) -> Vec<Scenario> {
    sqlx::query("SELECT code,name,sector,descr,modules,rules,devices,spec,ai_focus,builtin,enabled FROM app.iot_scenarios ORDER BY builtin DESC, name")
        .fetch_all(&st.owner_pool).await.unwrap_or_default().iter().map(Scenario::from_row).collect()
}
/// Avvio: inserisce gli scenari predefiniti mancanti (mai sovrascrive quelli modificati dal superadmin).
pub async fn seed_scenarios(st: &AppState) {
    for b in builtin_scenarios() {
        let spec: Option<Value> = BUILTIN_SPECS.iter().find(|(c, _)| *c == b.code).and_then(|(_, t)| serde_json::from_str(t).ok());
        let rules: Vec<Value> = b.rules.iter().map(|r| json!({"metric":r.metric,"op":r.op,"threshold":r.threshold,"etype":r.etype})).collect();
        let devs: Vec<Value> = b.devices.iter().map(|d| json!({"suffix":d.suffix,"label":d.label})).collect();
        let mods: Vec<String> = b.modules.iter().map(|m| m.to_string()).collect();
        if let Err(e) = sqlx::query("INSERT INTO app.iot_scenarios(code,name,sector,descr,modules,rules,devices,spec,builtin)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,true) ON CONFLICT (code) DO NOTHING")
            .bind(b.code).bind(b.name).bind(b.sector).bind(b.desc).bind(&mods).bind(json!(rules)).bind(json!(devs)).bind(spec)
            .execute(&st.owner_pool).await { tracing::warn!("seed scenario {}: {e}", b.code); }
    }
}
pub async fn super_scenarios_list(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let out: Vec<Value> = scenario_catalog(&st).await.iter().map(|s| json!({
        "code":s.code, "name":s.name, "sector":s.sector, "desc":s.desc,
        "modules":s.modules, "rule_count":s.rules.len(), "device_count":s.devices.len(),
        "builtin":s.builtin, "enabled":s.enabled })).collect();
    Ok(Json(json!(out)))
}
// ---- IO-4: gestione del catalogo scenari (superadmin) ----
#[derive(Deserialize)]
pub struct ScenarioReq { pub code: Option<String>, pub name: String, #[serde(default)] pub sector: String,
    #[serde(default)] pub descr: String, #[serde(default)] pub modules: Vec<String>, #[serde(default)] pub rules: Vec<Value>,
    #[serde(default)] pub devices: Vec<Value>, #[serde(default)] pub spec: Option<Value>, #[serde(default)] pub ai_focus: Option<String>,
    #[serde(default)] pub enabled: Option<bool> }
/// Valida e normalizza moduli, regole e dispositivi demo di uno scenario.
pub(crate) async fn scenario_validate(st: &AppState, b: &ScenarioReq) -> Result<(Vec<String>, Value, Value), (StatusCode,String)> {
    let bad = |m: String| Err(err(StatusCode::BAD_REQUEST, &m));
    if b.name.trim().is_empty() { return bad("nome richiesto".into()); }
    let known: Vec<String> = sqlx::query_scalar("SELECT code FROM app.modules_catalog").fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut mods: Vec<String> = b.modules.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    if mods.is_empty() { mods.push("iot".into()); }
    for m in &mods { if !known.contains(m) { return bad(format!("modulo sconosciuto: {m}")); } }
    let mut rules = Vec::new();
    for (i, r) in b.rules.iter().enumerate() {
        let metric = r.get("metric").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let op = r.get("op").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let etype = r.get("etype").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let thr = r.get("threshold").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|x| x.trim().parse().ok())));
        if metric.is_empty() { return bad(format!("regola {}: metrica richiesta", i+1)); }
        if !RULE_OPS.contains(&op.as_str()) { return bad(format!("regola {}: operatore non valido", i+1)); }
        if !RULE_ETYPES.contains(&etype.as_str()) { return bad(format!("regola {}: evento non ammesso", i+1)); }
        let numeric = !matches!(op.as_str(), "is_true"|"is_false");
        if numeric && thr.is_none() { return bad(format!("regola {}: soglia richiesta", i+1)); }
        rules.push(json!({"metric":metric,"op":op,"threshold": if numeric { thr } else { None },"etype":etype}));
    }
    let mut devs = Vec::new();
    for (i, d) in b.devices.iter().enumerate() {
        let suffix = d.get("suffix").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
        let label = d.get("label").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || suffix.len() > 30 {
            return bad(format!("dispositivo demo {}: sigla con sole lettere, cifre e trattini", i+1)); }
        if label.is_empty() { return bad(format!("dispositivo demo {}: nome richiesto", i+1)); }
        devs.push(json!({"suffix":suffix,"label":label}));
    }
    if let Some(sp) = &b.spec { if !sp.is_object() && !sp.is_null() { return bad("la spec deve essere un oggetto JSON".into()); } }
    Ok((mods, json!(rules), json!(devs)))
}
pub async fn super_scenario_get(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>) -> R {
    require_super(&st, &h).await?;
    let r = sqlx::query("SELECT code,name,sector,descr,modules,rules,devices,spec,ai_focus,builtin,enabled,
            (SELECT count(*) FROM app.tenant_scenarios t WHERE t.scenario_code=s.code) AS tenants FROM app.iot_scenarios s WHERE code=$1")
        .bind(&code).fetch_optional(&st.owner_pool).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "scenario inesistente"))?;
    Ok(Json(json!({"code":r.get::<String,_>("code"),"name":r.get::<String,_>("name"),"sector":r.get::<String,_>("sector"),
        "descr":r.get::<String,_>("descr"),"modules":r.get::<Vec<String>,_>("modules"),"rules":r.get::<Value,_>("rules"),
        "devices":r.get::<Value,_>("devices"),"spec":r.try_get::<Option<Value>,_>("spec").ok().flatten(),
        "ai_focus":r.try_get::<Option<String>,_>("ai_focus").ok().flatten(),"builtin":r.get::<bool,_>("builtin"),
        "enabled":r.get::<bool,_>("enabled"),"tenants":r.get::<i64,_>("tenants")})))
}
pub async fn super_scenario_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ScenarioReq>) -> R {
    let s = require_super(&st, &h).await?;
    let code = b.code.clone().unwrap_or_default().trim().to_lowercase();
    if code.len() < 2 || code.len() > 40 || !code.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err(err(StatusCode::BAD_REQUEST, "codice: 2-40 caratteri, lettere minuscole, cifre e _")); }
    let (mods, rules, devs) = scenario_validate(&st, &b).await?;
    let n = sqlx::query("INSERT INTO app.iot_scenarios(code,name,sector,descr,modules,rules,devices,spec,ai_focus,enabled)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,COALESCE($10,true)) ON CONFLICT (code) DO NOTHING")
        .bind(&code).bind(b.name.trim()).bind(b.sector.trim()).bind(b.descr.trim()).bind(&mods).bind(&rules).bind(&devs)
        .bind(&b.spec).bind(&b.ai_focus).bind(b.enabled).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::CONFLICT, "codice scenario già usato")); }
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'scenario_catalog_create',$2)")
        .bind(&s.email).bind(json!({"code":code})).execute(&st.owner_pool).await;
    Ok(Json(json!({"ok":true,"code":code})))
}
pub async fn super_scenario_update(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>, Json(b): Json<ScenarioReq>) -> R {
    let s = require_super(&st, &h).await?;
    let (mods, rules, devs) = scenario_validate(&st, &b).await?;
    let n = sqlx::query("UPDATE app.iot_scenarios SET name=$2,sector=$3,descr=$4,modules=$5,rules=$6,devices=$7,spec=$8,ai_focus=$9,
            enabled=COALESCE($10,enabled),updated_at=now() WHERE code=$1")
        .bind(&code).bind(b.name.trim()).bind(b.sector.trim()).bind(b.descr.trim()).bind(&mods).bind(&rules).bind(&devs)
        .bind(&b.spec).bind(&b.ai_focus).bind(b.enabled).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "scenario inesistente")); }
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'scenario_catalog_update',$2)")
        .bind(&s.email).bind(json!({"code":code,"enabled":b.enabled})).execute(&st.owner_pool).await;
    Ok(Json(json!({"ok":true,"note":"le regole già applicate ai tenant non cambiano: riapplica lo scenario per aggiornarle"})))
}
pub async fn super_scenario_delete(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>) -> R {
    let s = require_super(&st, &h).await?;
    let r = sqlx::query("SELECT builtin, (SELECT count(*) FROM app.tenant_scenarios t WHERE t.scenario_code=s.code) AS n FROM app.iot_scenarios s WHERE code=$1")
        .bind(&code).fetch_optional(&st.owner_pool).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "scenario inesistente"))?;
    if r.get::<bool,_>("builtin") { return Err(err(StatusCode::CONFLICT, "scenario predefinito: si disattiva, non si elimina")); }
    if r.get::<i64,_>("n") > 0 { return Err(err(StatusCode::CONFLICT, "scenario attivo su alcuni tenant: rimuovilo prima da quei tenant")); }
    sqlx::query("DELETE FROM app.iot_scenarios WHERE code=$1").bind(&code).execute(&st.owner_pool).await.map_err(ise)?;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'scenario_catalog_delete',$2)")
        .bind(&s.email).bind(json!({"code":code})).execute(&st.owner_pool).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct DeviceScenarioReq { pub code: Option<String> }
/// IO-4: collega un sensore REALE del tenant a uno scenario attivo (il cruscotto ne mostra la telemetria).
pub async fn device_scenario_set(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<DeviceScenarioReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "iot").await?; // IO-17
    if !has_cap(&s, "iot.manage") && !has_cap(&s, "devices.admin") { return Err(err(StatusCode::FORBIDDEN, "permesso richiesto: IoT o amministrazione dispositivi")); }
    let code = b.code.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty());
    if let Some(c) = code {
        let ok: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.tenant_scenarios WHERE tenant_id=$1 AND scenario_code=$2)")
            .bind(tid).bind(c).fetch_one(&st.owner_pool).await.map_err(ise)?;
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "scenario non attivo per l'azienda")); }
    }
    let n = sqlx::query("UPDATE app.devices SET config = CASE WHEN $3::text IS NULL THEN config - 'scenario' ELSE config || jsonb_build_object('scenario',$3::text) END,
            updated_at=now() WHERE id=$1 AND tenant_id=$2 AND source<>'scenario'")
        .bind(id).bind(tid).bind(code).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente (o demo)")); }
    audit(&st, tid, &s.email, "device_scenario_set", json!({"device":id,"scenario":code})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn super_tenant_scenarios(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query("SELECT scenario_code,with_demo,applied_by,applied_at FROM app.tenant_scenarios WHERE tenant_id=$1 ORDER BY applied_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cat = scenario_catalog(&st).await;
    let out: Vec<Value> = rows.iter().map(|r| { let code: String = r.get("scenario_code");
        let name = cat.iter().find(|s| s.code==code).map(|s| s.name.clone()).unwrap_or_else(|| "?".into());
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
pub(crate) async fn scenario_apply_inner(st: &AppState, tid: Uuid, code: &str, with_demo: bool) -> Result<(String, i64, i64, Vec<String>), (StatusCode, String)> {
    let cat = scenario_catalog(st).await;
    let scn = cat.iter().find(|s| s.code==code).ok_or_else(|| err(StatusCode::BAD_REQUEST, "scenario sconosciuto"))?;
    if !scn.enabled { return Err(err(StatusCode::CONFLICT, "scenario disattivato nel catalogo")); }
    // 1) moduli
    for m in &scn.modules {
        sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES($1,$2,'attivo')
            ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo', updated_at=now()")
            .bind(tid).bind(m).execute(&st.owner_pool).await.map_err(ise)?;
    }
    // 2) regole (idempotenti per label scn:<code>:<metric>:<op>)
    let mut rules_added = 0i64;
    for ru in &scn.rules {
        // IO-9: la soglia entra nell'etichetta (due regole sulla stessa metrica e operatore con soglie
        // diverse non si scartano più a vicenda); la prima forma resta per le regole già inserite
        let label = match ru.threshold { Some(t) => format!("scn:{}:{}:{}:{}", scn.code, ru.metric, ru.op, t), None => format!("scn:{}:{}:{}", scn.code, ru.metric, ru.op) };
        let n = sqlx::query("INSERT INTO app.sensor_rules(tenant_id,metric,op,threshold,etype,label,sort)
            SELECT $1,$2,$3,$4,$5,$6,100 WHERE NOT EXISTS(SELECT 1 FROM app.sensor_rules WHERE tenant_id=$1 AND label=$6)")
            .bind(tid).bind(&ru.metric).bind(&ru.op).bind(ru.threshold).bind(&ru.etype).bind(&label)
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
            // SA-13: la sede demo porta il codice dello scenario (la rimozione tocca solo lei)
            None => sqlx::query_scalar("INSERT INTO app.sites(tenant_id,name,scenario_code) VALUES($1,$2,$3) RETURNING id")
                .bind(tid).bind(&site_name).bind(&scn.code).fetch_one(&st.owner_pool).await.map_err(ise)?,
        };
        // coordinate demo sparse attorno a una base (Milano) per popolare la mappa del cruscotto
        for (i, dev) in scn.devices.iter().enumerate() {
            let ext = format!("scn-{}-{}", scn.code, dev.suffix);
            let lat = 45.464 + ((i as f64)*0.9 + (scn.code.len() as f64)*0.13).sin() * 0.03;
            let lon = 9.190 + ((i as f64)*1.3 + (scn.code.len() as f64)*0.21).cos() * 0.03;
            let n = sqlx::query("INSERT INTO app.devices(tenant_id,external_id,source,label,site_id,enabled,config,last_lat,last_lon,updated_at)
                VALUES($1,$2,'scenario',$3,$4,true,$5,$6,$7,now())
                ON CONFLICT (tenant_id,source,external_id) DO NOTHING")
                .bind(tid).bind(&ext).bind(&dev.label).bind(site_id).bind(json!({"scenario":scn.code})).bind(lat).bind(lon)
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
pub(crate) async fn scenario_remove_inner(st: &AppState, tid: Uuid, code: &str) -> Result<(u64, u64), (StatusCode, String)> {
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
    // 4) sede demo DI QUESTO scenario (SA-13: legame esplicito), se rimasta vuota.
    let _ = sqlx::query("DELETE FROM app.sites s WHERE s.tenant_id=$1 AND s.scenario_code=$2
            AND NOT EXISTS (SELECT 1 FROM app.devices d WHERE d.site_id=s.id)
            AND NOT EXISTS (SELECT 1 FROM app.incidents i WHERE i.site_id=s.id)")
        .bind(tid).bind(code).execute(&st.owner_pool).await;
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
            for s in scenario_catalog(&st).await.into_iter().filter(|s| s.enabled) {
                let (_c, r, d, _m) = scenario_apply_inner(&st, tid, &s.code, with_demo).await?;
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
pub(crate) fn demo_alarm_value(op: &str, thr: Option<f64>) -> Value {
    match op {
        ">=" | ">" => json!(thr.map(|t| t + (t.abs()*0.1).max(1.0)).unwrap_or(1.0)),
        "<=" | "<" => json!(thr.map(|t| t - (t.abs()*0.1).max(1.0)).unwrap_or(-1.0)),
        "==" => thr.map(|t| json!(t)).unwrap_or(json!(0.0)),
        "is_true" => json!(true),
        "is_false" => json!(false),
        _ => json!(thr.unwrap_or(0.0)),
    }
}
pub(crate) fn demo_normal_value(op: &str, thr: Option<f64>) -> Value {
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
    let sa_ = require_super(&st, &h).await?;
    let cat = scenario_catalog(&st).await;
    let scn = cat.iter().find(|s| s.code==code).ok_or_else(|| err(StatusCode::BAD_REQUEST, "scenario sconosciuto"))?;
    // nomi device demo (se lo scenario non ha device demo, usa un nome sintetico)
    let devs: Vec<String> = if scn.devices.is_empty() { vec![format!("scn-{}-demo", scn.code)] }
        else { scn.devices.iter().map(|d| format!("scn-{}-{}", scn.code, d.suffix)).collect() };
    // Telemetria demo: spec-driven (realistica, multi-metrica) se esiste la spec generata,
    // altrimenti derivata dalle regole (comportamento storico).
    let spec = scn.spec.clone();
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
            if let Some(rule) = scn.rules.iter().find(|r| r.metric==k && CRITICAL.contains(&r.etype.as_str())) {
                let mut p = serde_json::Map::new();
                for mm in ms { if let Some(kk)=mm.get("key").and_then(|v| v.as_str()) { p.insert(kk.into(), normal_val(mm, &mut rng)); } }
                let av = m.get("alarm").and_then(|v| v.as_f64()).map(|x| json!(x)).unwrap_or_else(|| demo_alarm_value(&rule.op, rule.threshold));
                p.insert(k.into(), av);
                payloads.push(Value::Object(p));
                na += 1;
            }
        }
    } else {
        let mut normal = serde_json::Map::new();
        for ru in &scn.rules { normal.insert(ru.metric.to_string(), demo_normal_value(&ru.op, ru.threshold)); }
        payloads.push(Value::Object(normal.clone()));
        for ru in scn.rules.iter().filter(|r| CRITICAL.contains(&r.etype.as_str())).take(3) {
            let mut p = normal.clone();
            p.insert(ru.metric.to_string(), demo_alarm_value(&ru.op, ru.threshold));
            payloads.push(Value::Object(p));
        }
    }
    let mut events = 0i64; let mut incidents = 0i64;
    for (i, payload) in payloads.iter().enumerate() {
        let device = devs[i % devs.len()].clone();
        let dev_id = device_id_by_ext(&st, tid, "scenario", &device).await;
        let etype = eval_sensor_rules(&st, tid, dev_id, payload).await.unwrap_or_else(|| "telemetria".into());
        let is_crit = CRITICAL.contains(&etype.as_str());
        let corr = format!("scndemo-{}-{}", scn.code, Uuid::new_v4());
        let _ = persist_ingest(&st, tid, Normalized{
            source:"scenario".into(), source_event:corr.clone(), idem:corr.clone(),
            etype, device_external_id:Some(device), site_name:None,
            lat:None, lon:None, accuracy_m:None, correlation:corr, payload:payload.clone(),
            indoor:false, beacon_uid:None, zone:None }).await;
        events += 1; if is_crit { incidents += 1; }
    }
    audit(&st, tid, &format!("superadmin:{}", sa_.email), "scenario_demo", json!({"code":scn.code,"events":events,"incidents":incidents})).await;
    Ok(Json(json!({"ok":true,"events":events,"incidents":incidents})))
}

// ---- Motore dashboard parametrico per scenario (widget derivati dalle regole) ----
pub(crate) fn metric_unit(m: &str) -> &'static str {
    match m {
        "temperature"|"engine_temp"|"temp_out" => "°C", "humidity"|"soil_moisture"|"moisture"|"crop_level"|"fill_level"|"power_factor" => "%",
        "co2" => "ppm", "pm25" => "µg/m³", "power_kw"|"demand_kw" => "kW", "energy_kwh"|"consumption" => "kWh",
        "voltage" => "V", "pressure" => "bar", "flow" => "L/min", "level" => "%", "fuel" => "%",
        "current" => "A", "frequency" => "Hz", "energy_total" => "kWh",
        "heart_rate" => "bpm", "gas" => "%LEL", "chlorine" => "mg/L", "ph" => "pH", "vibration" => "mm/s",
        _ => "",
    }
}
pub(crate) fn metric_label(m: &str) -> String {
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
pub(crate) fn scenario_widgets(scn: &Scenario) -> Vec<Value> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for ru in &scn.rules {
        if seen.contains(&ru.metric.as_str()) { continue; }
        seen.push(&ru.metric);
        let rules_m: Vec<&ScnRule> = scn.rules.iter().filter(|r| r.metric==ru.metric).collect();
        let is_bool = rules_m.iter().any(|r| matches!(r.op.as_str(), "is_true"|"is_false"));
        let crit = rules_m.iter().find(|r| CRITICAL.contains(&r.etype.as_str())).and_then(|r| r.threshold);
        let any_thr: Vec<f64> = rules_m.iter().filter_map(|r| r.threshold).collect();
        let op = rules_m.iter().find(|r| CRITICAL.contains(&r.etype.as_str())).map(|r| r.op.as_str()).unwrap_or(rules_m[0].op.as_str());
        let (min, max) = if is_bool { (0.0, 1.0) } else {
            let maxt = any_thr.iter().cloned().fold(f64::MIN, f64::max);
            let mint = any_thr.iter().cloned().fold(f64::MAX, f64::min);
            let lo = if mint < 0.0 { mint - mint.abs()*0.3 } else { 0.0 };
            let hi = if maxt > 0.0 { maxt * 1.4 } else { maxt * 0.6 + 10.0 };
            (lo, hi)
        };
        out.push(json!({
            "metric": ru.metric, "label": metric_label(&ru.metric), "unit": metric_unit(&ru.metric),
            "kind": if is_bool { "status" } else { "gauge" }, "op": op, "threshold": crit,
            "min": min, "max": max,
            "critical": rules_m.iter().any(|r| CRITICAL.contains(&r.etype.as_str())),
        }));
    }
    out
}

// IO-4: la spec dei widget (prima letta da SCENARIO_SPECS_DIR con percorso relativo) è nel campo
// `spec` di app.iot_scenarios; i file in core/scenario_specs sono incorporati solo per il seed.
/// Widget dal spec (se presente) altrimenti derivati dalle regole. Il spec definisce
/// metrica/etichetta/unità/tipo/range; la soglia/direzione d'allarme viene dalle regole.
pub(crate) fn scenario_widgets_spec(scn: &Scenario, spec: Option<&Value>) -> Vec<Value> {
    if let Some(ms) = spec.and_then(|s| s.get("metrics")).and_then(|m| m.as_array()) {
        return ms.iter().map(|m| {
            let key = m.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let rules_m: Vec<&ScnRule> = scn.rules.iter().filter(|r| r.metric==key).collect();
            let critr = rules_m.iter().find(|r| CRITICAL.contains(&r.etype.as_str()));
            let op = critr.map(|r| r.op.as_str()).or_else(|| rules_m.first().map(|r| r.op.as_str())).unwrap_or("");
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
    // IO-17: senza modulo IoT nessun cruscotto (lista vuota: la console la chiede a ogni avvio)
    if !module_active(&st, tid, "iot").await { return Ok(Json(json!([]))); }
    let rows = sqlx::query("SELECT scenario_code FROM app.tenant_scenarios WHERE tenant_id=$1 ORDER BY applied_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cat = scenario_catalog(&st).await;
    let out: Vec<Value> = rows.iter().filter_map(|r| { let code: String = r.get("scenario_code");
        cat.iter().find(|s| s.code==code).map(|s| json!({"code":s.code,"name":s.name,"sector":s.sector})) }).collect();
    Ok(Json(json!(out)))
}

/// Angolo di analisi AI specifico per scenario (preset).
pub(crate) fn scenario_ai_focus(code: &str) -> &'static str {
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
pub(crate) async fn scenario_applied<'a>(st: &AppState, tid: Uuid, code: &str, cat: &'a [Scenario]) -> Result<&'a Scenario, (StatusCode,String)> {
    let scn = cat.iter().find(|x| x.code==code).ok_or_else(|| err(StatusCode::NOT_FOUND, "scenario sconosciuto"))?;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM app.tenant_scenarios WHERE tenant_id=$1 AND scenario_code=$2")
        .bind(tid).bind(code).fetch_one(&st.owner_pool).await.map_err(ise)?;
    if applied == 0 { return Err(err(StatusCode::FORBIDDEN, "scenario non attivo per questo tenant")); }
    Ok(scn)
}

/// Dati completi del cruscotto: widget (valore/serie), KPI aggregati, device (coord mappa),
/// multi-metrica, incidenti recenti. Riusato dal cruscotto e dall'analisi AI.
pub(crate) async fn scenario_dash_json(st: &AppState, tid: Uuid, scn: &Scenario) -> Result<Value, (StatusCode,String)> {
    let spec = scn.spec.clone();
    let widgets = scenario_widgets_spec(scn, spec.as_ref());
    let narrative = spec.as_ref().and_then(|s| s.get("narrative")).and_then(|v| v.as_str()).map(|s| s.to_string());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // IO-4: dispositivi demo dello scenario + sensori REALI del tenant collegati allo scenario
    let devs = sqlx::query("SELECT id,external_id,label,last_lat,last_lon,config,source FROM app.devices
            WHERE (source='scenario' AND external_id LIKE $1) OR (source<>'scenario' AND config->>'scenario'=$2) ORDER BY source<>'scenario' DESC, external_id")
        .bind(format!("scn-{}-%", scn.code)).bind(&scn.code).fetch_all(&mut *tx).await.map_err(ise)?;
    let ids: Vec<Uuid> = devs.iter().map(|r| r.get::<Uuid,_>("id")).collect();
    let real_ids: Vec<Uuid> = devs.iter().filter(|r| r.get::<String,_>("source") != "scenario").map(|r| r.get::<Uuid,_>("id")).collect();
    let mut evs: Vec<(Uuid, Value, Dt)> = if ids.is_empty() { Vec::new() } else {
        sqlx::query("SELECT device_id,payload_ref,origin_ts FROM app.events WHERE device_id = ANY($1) AND payload_ref IS NOT NULL ORDER BY origin_ts DESC LIMIT 400")
            .bind(&ids).fetch_all(&mut *tx).await.map_err(ise)?.iter()
            .filter_map(|r| Some((r.try_get::<Uuid,_>("device_id").ok()?, r.try_get::<Value,_>("payload_ref").unwrap_or(json!({})), r.get::<Dt,_>("origin_ts")))).collect() };
    // telemetria reale (time-series nativa): ogni lettura diventa un «payload» {metrica: valore}
    if !real_ids.is_empty() {
        let tel = sqlx::query("SELECT device_id, COALESCE(metric,key) AS m, num, str, ts FROM app.device_telemetry
                WHERE device_id = ANY($1) ORDER BY ts DESC LIMIT 800").bind(&real_ids).fetch_all(&mut *tx).await.map_err(ise)?;
        for r in &tel {
            let v = r.try_get::<Option<f64>,_>("num").ok().flatten().map(|n| json!(n))
                .or_else(|| r.try_get::<Option<String>,_>("str").ok().flatten().map(|x| json!(x))).unwrap_or(Value::Null);
            let mut m = serde_json::Map::new(); m.insert(r.get::<String,_>("m"), v);
            evs.push((r.get("device_id"), Value::Object(m), r.get("ts")));
        }
        evs.sort_by(|a, b| b.2.cmp(&a.2));
    }
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
    let payloads: Vec<(Value, Dt)> = evs.iter().map(|(_, p, t)| (p.clone(), *t)).collect();
    // storico per-device (per il raggruppamento a silos)
    let mut by_dev: std::collections::HashMap<Uuid, Vec<(Value, Dt)>> = std::collections::HashMap::new();
    for (did, p, t) in &evs { by_dev.entry(*did).or_default().push((p.clone(), *t)); }
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
        "label":r.try_get::<String,_>("label").ok(),"lat":r.try_get::<f64,_>("last_lat").ok(),"lon":r.try_get::<f64,_>("last_lon").ok(),
        "real": r.get::<String,_>("source") != "scenario"})).collect();
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
            let rule = scn.rules.iter().find(|rr| rr.metric==k.as_str() && CRITICAL.contains(&rr.etype.as_str()));
            let tripped = if let (Some(v), Some(ru)) = (val.as_ref().and_then(|x| x.as_f64()), rule) {
                match ru.op.as_str() { ">="=>ru.threshold.map(|t|v>=t).unwrap_or(false), ">"=>ru.threshold.map(|t|v>t).unwrap_or(false),
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
        "code":scn.code, "name":scn.name, "sector":scn.sector, "ai_focus":scn.focus(),
        "narrative": narrative, "generated": spec.is_some(),
        "grouped": grouped, "group_label": group_label, "primary_metric": primary, "silos": silos,
        "device_count": devs.len(), "real_device_count": real_ids.len(), "widgets": widgets_out, "incidents": incidents, "devices": devices,
        "kpis": {"device_count":devs.len(),"open_incidents":open_inc,"events_24h":ev24,"metrics_tracked":widgets_out.len(),"alarms_now":alarms_now}
    }))
}

/// Dati del cruscotto di uno scenario (tenant, scenario attivo).
pub async fn scenario_dashboard(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "iot").await?; // IO-17
    let cat = scenario_catalog(&st).await;
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
    require_module(&st, tid, "iot").await?; // IO-17
    let cat = scenario_catalog(&st).await;
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
    require_module(&st, tid, "iot").await?; // IO-17
    let cat = scenario_catalog(&st).await;
    let scn = scenario_applied(&st, tid, &code, &cat).await?;
    if !module_active(&st, tid, "assistant").await { return Err(err(StatusCode::FORBIDDEN, "modulo assistant non abilitato")); }
    let c = ai_conn(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "provider AI non configurato (superadmin)"))?;
    let facts = scenario_dash_json(&st, tid, scn).await?;
    let sys = format!("Sei un analista IoT di YouWorkforce per lo scenario «{}» (settore {}). {} \
        Usa SOLO i fatti forniti (widget con valori/serie, KPI, incidenti). Non inventare numeri. \
        Rispondi in italiano, breve e strutturato (stato, anomalie, azioni consigliate). Non agire \
        sulla catena di sicurezza: resta manuale.", scn.name, scn.sector, scn.focus());
    let user = format!("Dati cruscotto: {}", serde_json::to_string(&facts).unwrap_or_default());
    let analysis = ai_chat(&st, &c, &sys, &user).await.map_err(|e| err(StatusCode::BAD_GATEWAY, &e))?;
    let mut report_id: Option<Uuid> = None;
    if b.save.unwrap_or(false) {
        let title = format!("Analisi {} — {}", scn.name, st.clock.now().format("%Y-%m-%d %H:%M"));
        report_id = Some(ai_report_save(&st, &s, &scn.code, &title, &analysis, json!({"scenario":scn.code})).await?);
    }
    audit(&st, tid, &s.email, "scenario_ai_analysis", json!({"code":scn.code,"saved":report_id.is_some()})).await;
    Ok(Json(json!({"provider":c.provider,"analysis":analysis,"ai_focus":scn.focus(),"report_id":report_id})))
}

