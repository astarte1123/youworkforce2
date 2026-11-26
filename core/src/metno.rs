//! Fase 4b: previsioni prese da YW stesso da MET Norway (api.met.no, Locationforecast 2.0 «complete»),
//! per i tenant che scelgono questa fonte (mig 0169, decisione PO 26/11). Gratuito anche per uso
//! commerciale, licenza CC BY 4.0: la console cita la fonte. Regole dei termini di servizio rispettate:
//! - User-Agent che identifica l'applicazione (+ contatto dell'operatore da YW_METNO_CONTACT);
//! - cache: una zona si riscarica solo dopo la scadenza indicata (Expires) e mai prima di 3 ore;
//! - richiesta condizionale If-Modified-Since (304 = niente di nuovo);
//! - coordinate con al massimo 4 decimali; errori → nuovo tentativo dopo 30 minuti (3 ore se 403/429).
//! Le fasce finiscono nella stessa tabella usata dalla chiave di integrazione: avvisi, pagina Meteo e
//! «primo giorno buono» non cambiano. Tempi esterni: orologio di sistema di proposito (cache HTTP).

use axum::{extract::State, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, require_any_cap, require_module, module_active, ise, err, audit};
use crate::db;

pub(crate) const SOURCE: &str = "MET Norway";
const MIN_CACHE_H: i64 = 3;

fn base_url() -> String { std::env::var("YW_METNO_URL").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "https://api.met.no".into()) }
fn user_agent() -> String {
    let v = env!("CARGO_PKG_VERSION");
    match std::env::var("YW_METNO_CONTACT").ok().filter(|s| !s.trim().is_empty()) {
        Some(c) => format!("YouWorkforce/{v} {}", c.trim()),
        None => format!("YouWorkforce/{v} (meteo operativo)"),
    }
}

/// Codice della condizione dal simbolo MET (es. «lightrainshowers_day» → pioggia_debole).
pub(crate) fn condition_of(symbol: &str) -> &'static str {
    let s = symbol.split('_').next().unwrap_or("");
    if s.contains("thunder") { return "temporale"; }
    if s.contains("snow") || s.contains("sleet") { return "neve"; }
    if s.starts_with("light") && s.contains("rain") { return "pioggia_debole"; }
    if s.contains("rain") { return "pioggia"; }
    match s { "clearsky" => "sereno", "fair" | "partlycloudy" => "poco_nuvoloso", "fog" => "nebbia", "cloudy" => "nuvoloso", _ => "nuvoloso" }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Slot { pub from: DateTime<Utc>, pub to: DateTime<Utc>, pub rain_mm: Option<f64>, pub rain_prob: Option<i32>,
    pub wind_kmh: Option<f64>, pub gust_kmh: Option<f64>, pub temp_c: Option<f64>, pub condition: Option<&'static str> }

/// Fasce dal JSON Locationforecast: orarie finché c'è `next_1_hours`, poi da 6 ore; senza sovrapposizioni.
pub(crate) fn parse_slots(v: &Value) -> Vec<Slot> {
    let mut out = vec![];
    let mut cursor: Option<DateTime<Utc>> = None;
    let ts = v.pointer("/properties/timeseries").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    for e in &ts {
        let Some(t) = e.get("time").and_then(|x| x.as_str()).and_then(|x| DateTime::parse_from_rfc3339(x).ok()).map(|x| x.with_timezone(&Utc)) else { continue };
        if cursor.map(|c| t < c).unwrap_or(false) { continue; }
        let inst = e.pointer("/data/instant/details");
        let f = |k: &str| inst.and_then(|d| d.get(k)).and_then(|x| x.as_f64());
        let (next, h) = if let Some(n) = e.pointer("/data/next_1_hours") { (n, 1) } else if let Some(n) = e.pointer("/data/next_6_hours") { (n, 6) } else { continue };
        let det = |k: &str| next.pointer(&format!("/details/{k}")).and_then(|x| x.as_f64());
        let to = t + Duration::hours(h);
        out.push(Slot { from: t, to,
            rain_mm: det("precipitation_amount").map(|r| (r * 10.0).round() / 10.0),
            rain_prob: det("probability_of_precipitation").map(|p| p.round().clamp(0.0, 100.0) as i32),
            wind_kmh: f("wind_speed").map(|w| (w * 36.0).round() / 10.0),
            gust_kmh: f("wind_speed_of_gust").map(|w| (w * 36.0).round() / 10.0),
            temp_c: f("air_temperature"),
            condition: next.pointer("/summary/symbol_code").and_then(|x| x.as_str()).map(condition_of) });
        cursor = Some(to);
    }
    out
}

fn header_time(h: &reqwest::header::HeaderMap, k: &str) -> Option<DateTime<Utc>> {
    h.get(k).and_then(|v| v.to_str().ok()).and_then(|s| DateTime::parse_from_rfc2822(s).ok()).map(|d| d.with_timezone(&Utc))
}

pub(crate) enum Outcome { Updated(usize), NotModified, Failed(String) }

/// Scarica una zona (rispettando cache e 304) e sostituisce le sue fasce future di MET Norway.
pub(crate) async fn fetch_zone(st: &AppState, tid: Uuid, zid: Uuid, lat: f64, lon: f64, last_modified: Option<String>) -> Outcome {
    let now = Utc::now();
    let url = format!("{}/weatherapi/locationforecast/2.0/complete?lat={:.4}&lon={:.4}", base_url().trim_end_matches('/'), lat, lon);
    let mut rq = st.http.get(&url).header(reqwest::header::USER_AGENT, user_agent()).timeout(std::time::Duration::from_secs(30));
    if let Some(lm) = &last_modified { rq = rq.header(reqwest::header::IF_MODIFIED_SINCE, lm); }
    let res = rq.send().await;
    let (outcome, expires, lm) = match res {
        Err(e) => (Outcome::Failed(if e.is_timeout() { "MET Norway non risponde (tempo scaduto)".into() } else { "MET Norway non raggiungibile".into() }), now + Duration::minutes(30), last_modified.clone()),
        Ok(r) => {
            let st_code = r.status();
            let exp = header_time(r.headers(), "expires").unwrap_or(now).max(now + Duration::hours(MIN_CACHE_H));
            let lm = r.headers().get("last-modified").and_then(|v| v.to_str().ok()).map(|s| s.to_string()).or(last_modified.clone());
            if st_code == reqwest::StatusCode::NOT_MODIFIED { (Outcome::NotModified, exp, lm) }
            else if st_code == reqwest::StatusCode::FORBIDDEN || st_code == reqwest::StatusCode::TOO_MANY_REQUESTS {
                (Outcome::Failed(format!("MET Norway ha rifiutato la richiesta, codice {}: controlla il contatto dell'operatore YW_METNO_CONTACT", st_code.as_u16())), now + Duration::hours(MIN_CACHE_H), last_modified.clone())
            } else if !st_code.is_success() {
                (Outcome::Failed(format!("MET Norway ha risposto con l'errore {}", st_code.as_u16())), now + Duration::minutes(30), last_modified.clone())
            } else {
                match r.json::<Value>().await {
                    Err(_) => (Outcome::Failed("risposta di MET Norway non leggibile".into()), now + Duration::minutes(30), last_modified.clone()),
                    Ok(v) => {
                        let slots = parse_slots(&v);
                        if slots.is_empty() { (Outcome::Failed("MET Norway non ha restituito previsioni per questa posizione".into()), now + Duration::minutes(30), last_modified.clone()) }
                        else {
                            match save_slots(st, tid, zid, &slots).await {
                                Ok(n) => (Outcome::Updated(n), exp, lm),
                                Err(_) => (Outcome::Failed("previsioni non salvate".into()), now + Duration::minutes(30), last_modified.clone()),
                            }
                        }
                    }
                }
            }
        }
    };
    let error = if let Outcome::Failed(m) = &outcome { Some(m.clone()) } else { None };
    if let Ok(mut tx) = db::begin_tenant(&st.rt_pool, tid).await {
        let ok = matches!(outcome, Outcome::Updated(_) | Outcome::NotModified);
        let _ = sqlx::query("UPDATE app.weather_zones SET expires_at=$2, last_modified=$3, fetch_error=$4, fetched_at=CASE WHEN $5 THEN $6 ELSE fetched_at END WHERE id=$1")
            .bind(zid).bind(expires).bind(lm).bind(&error).bind(ok).bind(now).execute(&mut *tx).await;
        let _ = tx.commit().await;
    }
    outcome
}

async fn save_slots(st: &AppState, tid: Uuid, zid: Uuid, slots: &[Slot]) -> anyhow::Result<usize> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await?;
    let first = slots[0].from;
    let issued = Utc::now();
    // la nuova emissione sostituisce le fasce future (anche di granularità diversa) della stessa fonte
    sqlx::query("DELETE FROM app.weather_slots WHERE zone_id=$1 AND kind='previsione' AND valid_from >= $2").bind(zid).bind(first).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM app.weather_slots WHERE zone_id=$1 AND kind='previsione' AND valid_from < $2 AND valid_to > $2").bind(zid).bind(first).execute(&mut *tx).await?;
    for s in slots {
        sqlx::query("INSERT INTO app.weather_slots(tenant_id,zone_id,kind,valid_from,valid_to,rain_mm,rain_prob,wind_kmh,gust_kmh,temp_c,condition,source,issued_at)
                VALUES($1,$2,'previsione',$3,$4,$5::numeric,$6,$7::numeric,$8::numeric,$9::numeric,$10,$11,$12)")
            .bind(tid).bind(zid).bind(s.from).bind(s.to).bind(s.rain_mm).bind(s.rain_prob).bind(s.wind_kmh).bind(s.gust_kmh).bind(s.temp_c)
            .bind(s.condition).bind(SOURCE).bind(issued).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(slots.len())
}

async fn provider_of(st: &AppState, tid: Uuid) -> String {
    let Ok(mut tx) = db::begin_tenant(&st.rt_pool, tid).await else { return "integrazione".into() };
    let p: Option<String> = sqlx::query_scalar("SELECT provider FROM app.weather_settings WHERE tenant_id=$1").bind(tid).fetch_optional(&mut *tx).await.ok().flatten();
    let _ = tx.commit().await;
    p.unwrap_or_else(|| "integrazione".into())
}

/// Scarica le zone scadute di un tenant (o tutte quelle senza dati). Ritorna (aggiornate, invariate, errori).
pub(crate) async fn refresh_tenant(st: &AppState, tid: Uuid) -> (usize, usize, usize, usize) {
    let Ok(mut tx) = db::begin_tenant(&st.rt_pool, tid).await else { return (0, 0, 0, 0) };
    let zones = sqlx::query("SELECT id,lat,lon,last_modified FROM app.weather_zones WHERE expires_at IS NULL OR expires_at <= now() ORDER BY expires_at NULLS FIRST LIMIT 50")
        .fetch_all(&mut *tx).await.unwrap_or_default();
    let _ = tx.commit().await;
    let (mut up, mut nm, mut fail, mut slots) = (0, 0, 0, 0);
    for z in zones {
        match fetch_zone(st, tid, z.get("id"), z.get("lat"), z.get("lon"), z.try_get("last_modified").ok().flatten()).await {
            Outcome::Updated(n) => { up += 1; slots += n; }
            Outcome::NotModified => nm += 1,
            Outcome::Failed(_) => fail += 1,
        }
    }
    (up, nm, fail, slots)
}

/// Ciclo separato dal worker (una rete lenta non ferma allarmi e timer): ogni 5 minuti le zone scadute
/// dei tenant con fonte MET Norway e modulo meteo attivo.
pub async fn weather_loop(st: AppState) {
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    loop {
        let tenants: Vec<Uuid> = sqlx::query_scalar("SELECT ws.tenant_id FROM app.weather_settings ws WHERE ws.tenant_id IS NOT NULL AND ws.provider='metno'")
            .fetch_all(&st.owner_pool).await.unwrap_or_default();
        for tid in tenants {
            if !module_active(&st, tid, "meteo").await { continue; }
            let (up, _, fail, n) = refresh_tenant(&st, tid).await;
            if up + fail > 0 { tracing::info!(tenant = %tid, zones = up, failed = fail, slots = n, "meteo MET Norway aggiornato"); }
        }
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    }
}

// =============================================================================================
// Impostazioni per la console
// =============================================================================================
pub async fn weather_settings(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_any_cap(&s, &["plan", "settings.manage", "orders.view_all"])?;
    let provider = provider_of(&st, tid).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let zones = sqlx::query("SELECT id,code,name,fetched_at,expires_at,fetch_error FROM app.weather_zones ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"provider": provider,
        "providers": [
            {"code": "integrazione", "label": "Sistema collegato", "description": "Le previsioni le scrive un sistema autorizzato (es. l'agente Youco) con una chiave di integrazione."},
            {"code": "metno", "label": "MET Norway", "description": "YouWorkforce scarica da solo le previsioni dell'Istituto meteorologico norvegese per ogni zona, ogni 3 ore circa. Servizio gratuito, dati con licenza CC BY 4.0."}],
        "zones": zones.iter().map(|z| json!({"id": z.get::<Uuid,_>("id"), "code": z.get::<String,_>("code"), "name": z.get::<String,_>("name"),
            "fetched_at": z.try_get::<Option<DateTime<Utc>>,_>("fetched_at").ok().flatten(), "next_at": z.try_get::<Option<DateTime<Utc>>,_>("expires_at").ok().flatten(),
            "error": z.try_get::<Option<String>,_>("fetch_error").ok().flatten()})).collect::<Vec<_>>()})))
}
#[derive(Deserialize)]
pub struct WeatherSettingsReq { pub provider: String }
pub async fn put_weather_settings(State(st): State<AppState>, h: HeaderMap, Json(b): Json<WeatherSettingsReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_cap(&s, "settings.manage")?;
    if !["integrazione", "metno"].contains(&b.provider.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "fonte non valida: integrazione o metno")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.weather_settings(tenant_id,provider,updated_by) VALUES($1,$2,$3)
         ON CONFLICT (tenant_id) DO UPDATE SET provider=EXCLUDED.provider, updated_by=EXCLUDED.updated_by, updated_at=now()")
        .bind(tid).bind(&b.provider).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?;
    if b.provider == "metno" { // si scarica subito al prossimo giro
        sqlx::query("UPDATE app.weather_zones SET expires_at=NULL, fetch_error=NULL WHERE fetched_at IS NULL").execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "weather_provider", json!({"provider": b.provider})).await;
    Ok(Json(json!({"ok": true, "provider": b.provider})))
}
/// POST /api/weather/refresh — scarica adesso le zone scadute o mai scaricate (la cache di almeno 3 ore
/// richiesta da MET Norway resta: una zona aggiornata da poco non si riscarica).
pub async fn weather_refresh(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_any_cap(&s, &["plan", "settings.manage"])?;
    if provider_of(&st, tid).await != "metno" { return Err(err(StatusCode::BAD_REQUEST, "la fonte delle previsioni è il sistema collegato: scegli MET Norway per scaricarle da qui")); }
    let (up, nm, fail, n) = refresh_tenant(&st, tid).await;
    Ok(Json(json!({"updated": up, "not_modified": nm, "failed": fail, "slots": n})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn conditions() {
        assert_eq!(condition_of("lightrainshowers_day"), "pioggia_debole");
        assert_eq!(condition_of("heavyrain"), "pioggia");
        assert_eq!(condition_of("rainandthunder"), "temporale");
        assert_eq!(condition_of("clearsky_night"), "sereno");
        assert_eq!(condition_of("lightsleet"), "neve");
        assert_eq!(condition_of("fog"), "nebbia");
    }
    #[test] fn slots_hourly_then_six_hours_without_overlap() {
        let v = json!({"properties": {"timeseries": [
            {"time": "2026-12-02T10:00:00Z", "data": {"instant": {"details": {"air_temperature": 6.2, "wind_speed": 5.0, "wind_speed_of_gust": 12.0}},
                "next_1_hours": {"summary": {"symbol_code": "rain"}, "details": {"precipitation_amount": 1.26, "probability_of_precipitation": 80.4}},
                "next_6_hours": {"summary": {"symbol_code": "rain"}, "details": {"precipitation_amount": 6.0}}}},
            {"time": "2026-12-02T11:00:00Z", "data": {"instant": {"details": {"air_temperature": 6.0, "wind_speed": 4.0}},
                "next_6_hours": {"summary": {"symbol_code": "cloudy"}, "details": {"precipitation_amount": 0.0}}}},
            {"time": "2026-12-02T12:00:00Z", "data": {"instant": {"details": {"air_temperature": 7.0, "wind_speed": 3.0}},
                "next_6_hours": {"summary": {"symbol_code": "fair_day"}, "details": {"precipitation_amount": 0.0}}}},
            {"time": "2026-12-02T17:00:00Z", "data": {"instant": {"details": {"air_temperature": 5.0, "wind_speed": 2.0}},
                "next_6_hours": {"summary": {"symbol_code": "clearsky_night"}, "details": {"precipitation_amount": 0.0}}}},
            {"time": "2026-12-02T18:00:00Z", "data": {"instant": {"details": {"air_temperature": 4.0, "wind_speed": 2.0}}}}
        ]}});
        let s = parse_slots(&v);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].rain_mm, s[0].rain_prob, s[0].wind_kmh, s[0].gust_kmh), (Some(1.3), Some(80), Some(18.0), Some(43.2)));
        assert_eq!(s[0].to - s[0].from, Duration::hours(1));
        assert_eq!(s[1].to - s[1].from, Duration::hours(6));     // 11:00-17:00
        assert_eq!(s[2].from, s[1].to);                            // 12:00 saltata (dentro la fascia precedente)
    }
}
