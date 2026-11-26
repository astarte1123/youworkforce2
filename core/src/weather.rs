//! Fase 4 «Meteo operativo» (piano Idealverde, mig 0168, modulo `meteo`). Decisione PO: le previsioni
//! le legge Youco (o un altro sistema autorizzato) e le SCRIVE in YW con una chiave di integrazione;
//! YW non chiama servizi meteo.
//! - zone meteo con posizione; ogni sede usa la zona indicata o la più vicina;
//! - fasce di previsione (o di meteo osservato) per zona: pioggia, probabilità, vento, raffiche, temperatura;
//! - limiti meteo per tipo di attività (pioggia, vento, temperatura, ore asciutte dopo);
//! - AVVISI sull'intervento (mai blocchi: una previsione è incerta) e interventi a rischio con il
//!   primo giorno buono a pari orario.

use axum::{extract::{State, Path, Query}, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, require_any_cap, require_module, module_active, ise, err, audit, conflict_or};
use crate::field::parse_ts;
use crate::scheduling::Violation;
use crate::db;

fn num(r: &sqlx::postgres::PgRow, c: &str) -> Option<f64> { r.try_get::<Option<f64>,_>(c).ok().flatten() }
fn it(x: f64) -> String { format!("{x:.1}").replace('.', ",").trim_end_matches(",0").to_string() }

/// Zona della sede: quella indicata, altrimenti la più vicina (per posizione).
pub(crate) async fn zone_of_site(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, site: Uuid) -> Result<Option<(Uuid, String)>, (StatusCode, String)> {
    let r = sqlx::query("SELECT z.id, z.name FROM app.sites s JOIN app.weather_zones z ON z.id = COALESCE(s.weather_zone_id,
            (SELECT z2.id FROM app.weather_zones z2 WHERE s.lat IS NOT NULL AND s.lon IS NOT NULL
              ORDER BY (z2.lat - s.lat)^2 + ((z2.lon - s.lon) * cos(radians(s.lat)))^2 LIMIT 1))
         WHERE s.id=$1").bind(site).fetch_optional(&mut **tx).await.map_err(ise)?;
    Ok(r.map(|r| (r.get("id"), r.get("name"))))
}

/// La previsione copre la fascia oraria (almeno il 90%)? Senza previsione non si propone un giorno.
pub(crate) async fn forecast_covers(st: &AppState, tid: Uuid, site: Uuid, a: DateTime<Utc>, z: DateTime<Utc>) -> Result<bool, (StatusCode, String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let Some((zid, _)) = zone_of_site(&mut tx, site).await? else { return Ok(false) };
    let secs: Option<f64> = sqlx::query_scalar("SELECT sum(extract(epoch FROM LEAST(valid_to,$3) - GREATEST(valid_from,$2)))::float8
         FROM app.weather_slots WHERE zone_id=$1 AND kind='previsione' AND valid_from < $3 AND valid_to > $2")
        .bind(zid).bind(a).bind(z).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(secs.unwrap_or(0.0) >= 0.9 * (z - a).num_seconds() as f64)
}

/// Avvisi meteo per un intervento di quel tipo, in quella sede, in quella fascia oraria.
pub(crate) async fn weather_warnings(st: &AppState, tid: Uuid, at: Option<Uuid>, site: Option<Uuid>, start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> Result<Vec<Violation>, (StatusCode, String)> {
    let (Some(at), Some(site), Some(a), Some(z)) = (at, site, start, end) else { return Ok(vec![]) };
    if !module_active(st, tid, "meteo").await { return Ok(vec![]); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let lim = sqlx::query("SELECT name, rain_max_mm::float8 AS rain, wind_max_kmh, temp_min_c, temp_max_c, dry_hours_after FROM app.activity_types WHERE id=$1")
        .bind(at).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some(lim) = lim else { return Ok(vec![]) };
    let (rain_max, wind_max, tmin, tmax, dry) = (num(&lim, "rain"), lim.try_get::<Option<i32>,_>("wind_max_kmh").ok().flatten(),
        lim.try_get::<Option<i32>,_>("temp_min_c").ok().flatten(), lim.try_get::<Option<i32>,_>("temp_max_c").ok().flatten(),
        lim.try_get::<Option<i32>,_>("dry_hours_after").ok().flatten());
    if rain_max.is_none() && wind_max.is_none() && tmin.is_none() && tmax.is_none() && dry.is_none() { return Ok(vec![]); }
    let Some((zid, zname)) = zone_of_site(&mut tx, site).await? else { return Ok(vec![]) };
    let name: String = lim.get("name");
    let tz = crate::field::tenant_tz(st, tid).await;
    let when = format!("{} {}–{}", a.with_timezone(&tz).format("%d/%m"), a.with_timezone(&tz).format("%H:%M"), z.with_timezone(&tz).format("%H:%M"));
    let agg = |from: DateTime<Utc>, to: DateTime<Utc>| sqlx::query(
        "SELECT sum(rain_mm * extract(epoch FROM LEAST(valid_to,$3) - GREATEST(valid_from,$2)) / extract(epoch FROM valid_to - valid_from))::float8 AS rain, max(rain_prob) AS prob, max(GREATEST(COALESCE(wind_kmh,0),COALESCE(gust_kmh,0)))::float8 AS wind,
                min(temp_c)::float8 AS tmin, max(temp_c)::float8 AS tmax, count(*)::int AS n
         FROM app.weather_slots WHERE zone_id=$1 AND kind='previsione' AND valid_from < $3 AND valid_to > $2").bind(zid).bind(from).bind(to);
    let w = agg(a, z).fetch_one(&mut *tx).await.map_err(ise)?;
    let mut out = vec![];
    if w.get::<i32,_>("n") > 0 {
        let prob = w.try_get::<Option<i32>,_>("prob").ok().flatten();
        if let (Some(mx), Some(r)) = (rain_max, num(&w, "rain")) { if r > mx {
            let message = match prob {
                Some(p) => format!("Meteo {zname}, {when}: previsti {} mm di pioggia, probabilità {p}%. «{name}» si fa con al massimo {} mm.", it(r), it(mx)),
                None => format!("Meteo {zname}, {when}: previsti {} mm di pioggia. «{name}» si fa con al massimo {} mm.", it(r), it(mx)) };
            out.push(Violation { code: "weather_rain", hard: false, message }); } }
        if let (Some(mx), Some(v)) = (wind_max, num(&w, "wind")) { if v > mx as f64 {
            out.push(Violation { code: "weather_wind", hard: false, message: format!("Meteo {zname}, {when}: vento fino a {} km/h. «{name}» si fa con vento fino a {mx} km/h.", v.round()) }); } }
        if let (Some(mn), Some(t)) = (tmin, num(&w, "tmin")) { if t < mn as f64 {
            out.push(Violation { code: "weather_cold", hard: false, message: format!("Meteo {zname}, {when}: {} °C. «{name}» richiede almeno {mn} °C.", it(t)) }); } }
        if let (Some(mx), Some(t)) = (tmax, num(&w, "tmax")) { if t > mx as f64 {
            out.push(Violation { code: "weather_heat", hard: false, message: format!("Meteo {zname}, {when}: {} °C. «{name}» si fa fino a {mx} °C.", it(t)) }); } }
    }
    if let Some(h) = dry.filter(|h| *h > 0) {
        let w2 = agg(z, z + chrono::Duration::hours(h as i64)).fetch_one(&mut *tx).await.map_err(ise)?;
        if let Some(r) = num(&w2, "rain") { if r >= 0.5 {
            out.push(Violation { code: "weather_dry_after", hard: false, message: format!("Meteo {zname}: previsti {} mm di pioggia nelle {h} ore dopo «{name}», che richiede tempo asciutto.", it(r)) }); } }
    }
    tx.commit().await.map_err(ise)?;
    Ok(out)
}

// =============================================================================================
// Scrittura da Youco / sistemi autorizzati (chiave di integrazione) e dalla console
// =============================================================================================
#[derive(Deserialize)]
pub struct ZoneReq { pub name: String, pub lat: f64, pub lon: f64 }
pub async fn put_weather_zone(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>, Json(b): Json<ZoneReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_any_cap(&s, &["plan", "settings.manage"])?;
    let code = code.trim().to_string();
    if code.is_empty() || code.chars().count() > 60 { return Err(err(StatusCode::BAD_REQUEST, "codice della zona: da 1 a 60 caratteri")); }
    if b.name.trim().chars().count() < 2 { return Err(err(StatusCode::BAD_REQUEST, "nome della zona richiesto")); }
    if !(-90.0..=90.0).contains(&b.lat) || !(-180.0..=180.0).contains(&b.lon) { return Err(err(StatusCode::BAD_REQUEST, "posizione non valida")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = sqlx::query("INSERT INTO app.weather_zones(tenant_id,code,name,lat,lon) VALUES($1,$2,$3,$4,$5)
         ON CONFLICT (tenant_id,code) DO UPDATE SET name=EXCLUDED.name, lat=EXCLUDED.lat, lon=EXCLUDED.lon RETURNING id, (xmax=0) AS created")
        .bind(tid).bind(&code).bind(b.name.trim()).bind(b.lat).bind(b.lon).fetch_one(&mut *tx).await.map_err(|e| conflict_or(e, "zona non valida"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id": r.get::<Uuid,_>("id"), "code": code, "created": r.get::<bool,_>("created")})))
}
pub async fn delete_weather_zone(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_cap(&s, "settings.manage")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.weather_zones WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "zona inesistente")); }
    audit(&st, tid, &s.email, "weather_zone_delete", json!({"id": id})).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct ZoneSitesReq { pub site_ids: Vec<Uuid> }
/// POST /api/weather/zones/:id/sites — fissa le sedi su questa zona (le altre sedi usano la più vicina).
/// L'elenco sostituisce le sedi fissate prima sulla zona.
pub async fn set_zone_sites(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ZoneSitesReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_any_cap(&s, &["plan", "settings.manage"])?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ok: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.weather_zones WHERE id=$1)").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    if !ok { return Err(err(StatusCode::NOT_FOUND, "zona inesistente")); }
    sqlx::query("UPDATE app.sites SET weather_zone_id=NULL WHERE weather_zone_id=$1 AND NOT (id = ANY($2))").bind(id).bind(&b.site_ids).execute(&mut *tx).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.sites SET weather_zone_id=$1 WHERE id = ANY($2)").bind(id).bind(&b.site_ids).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "weather_zone_sites", json!({"id": id, "sites": n})).await;
    Ok(Json(json!({"ok": true, "sites": n})))
}

#[derive(Deserialize)]
pub struct SlotIn { pub from: String, pub to: String, #[serde(default)] pub rain_mm: Option<f64>, #[serde(default)] pub rain_prob: Option<i32>,
    #[serde(default)] pub wind_kmh: Option<f64>, #[serde(default)] pub gust_kmh: Option<f64>, #[serde(default)] pub temp_c: Option<f64>,
    #[serde(default)] pub condition: Option<String> }
#[derive(Deserialize)]
pub struct SlotsReq { pub zone: String, #[serde(default)] pub kind: Option<String>, #[serde(default)] pub source: Option<String>,
    #[serde(default)] pub issued_at: Option<String>, pub slots: Vec<SlotIn> }
/// POST /api/integration/weather/slots — fasce di previsione (o di meteo osservato) per una zona,
/// inserite o aggiornate per inizio fascia: una nuova emissione sostituisce la precedente.
pub async fn post_weather_slots(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SlotsReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_any_cap(&s, &["plan", "settings.manage"])?;
    let kind = b.kind.as_deref().unwrap_or("previsione");
    if !["previsione", "osservato"].contains(&kind) { return Err(err(StatusCode::BAD_REQUEST, "kind: previsione o osservato")); }
    if b.slots.is_empty() || b.slots.len() > 2000 { return Err(err(StatusCode::BAD_REQUEST, "da 1 a 2000 fasce per invio")); }
    let issued = b.issued_at.as_deref().map(parse_ts).transpose()?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let zid: Uuid = sqlx::query_scalar("SELECT id FROM app.weather_zones WHERE code=$1").bind(b.zone.trim()).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, &format!("zona «{}» inesistente: creala prima con PUT /api/integration/weather/zones/{{codice}}", b.zone)))?;
    let mut n = 0;
    for (i, x) in b.slots.iter().enumerate() {
        let (f, t) = (parse_ts(&x.from)?, parse_ts(&x.to)?);
        if t <= f || t - f > chrono::Duration::hours(24) { return Err(err(StatusCode::BAD_REQUEST, &format!("fascia {}: fine dopo l'inizio, al massimo 24 ore", i + 1))); }
        if x.rain_prob.map(|p| !(0..=100).contains(&p)).unwrap_or(false) || x.rain_mm.map(|r| r < 0.0).unwrap_or(false) || x.wind_kmh.map(|w| w < 0.0).unwrap_or(false) {
            return Err(err(StatusCode::BAD_REQUEST, &format!("fascia {}: valori non validi", i + 1))); }
        sqlx::query("INSERT INTO app.weather_slots(tenant_id,zone_id,kind,valid_from,valid_to,rain_mm,rain_prob,wind_kmh,gust_kmh,temp_c,condition,source,issued_at)
                VALUES($1,$2,$3,$4,$5,$6::numeric,$7,$8::numeric,$9::numeric,$10::numeric,$11,$12,$13)
             ON CONFLICT (zone_id,kind,valid_from) DO UPDATE SET valid_to=EXCLUDED.valid_to, rain_mm=EXCLUDED.rain_mm, rain_prob=EXCLUDED.rain_prob,
                wind_kmh=EXCLUDED.wind_kmh, gust_kmh=EXCLUDED.gust_kmh, temp_c=EXCLUDED.temp_c, condition=EXCLUDED.condition,
                source=EXCLUDED.source, issued_at=EXCLUDED.issued_at, updated_at=now()")
            .bind(tid).bind(zid).bind(kind).bind(f).bind(t).bind(x.rain_mm).bind(x.rain_prob).bind(x.wind_kmh).bind(x.gust_kmh).bind(x.temp_c)
            .bind(x.condition.as_deref().map(|c| c.chars().take(60).collect::<String>())).bind(&b.source).bind(issued)
            .execute(&mut *tx).await.map_err(|e| conflict_or(e, "fascia meteo non valida"))?;
        n += 1;
    }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"zone": b.zone, "kind": kind, "slots": n})))
}

// =============================================================================================
// Lettura per la console
// =============================================================================================
#[derive(Deserialize)]
pub struct WQ { #[serde(default)] pub zone_id: Option<Uuid>, #[serde(default)] pub days: Option<i64> }
/// GET /api/weather/forecast?days=7 — per zona e per giorno: pioggia totale, probabilità, vento,
/// temperatura minima e massima, condizione prevalente; quali sedi usano la zona. Un giorno con meno di
/// 6 ore di previsione (la coda dell'ultima emissione) non si mostra: sarebbe fuorviante.
pub async fn weather_forecast(State(st): State<AppState>, h: HeaderMap, Query(q): Query<WQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?;
    let days = q.days.unwrap_or(7).clamp(1, 16);
    let tz = crate::field::tenant_tz(&st, tid).await;
    let now = st.clock.now();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let zones = sqlx::query("SELECT z.id,z.code,z.name,z.lat,z.lon,
            (SELECT count(*) FROM app.sites s WHERE s.weather_zone_id=z.id)::int AS sites_fixed,
            (SELECT max(issued_at) FROM app.weather_slots w WHERE w.zone_id=z.id) AS issued, (SELECT max(source) FROM app.weather_slots w WHERE w.zone_id=z.id) AS source
         FROM app.weather_zones z WHERE ($1::uuid IS NULL OR z.id=$1) ORDER BY z.name").bind(q.zone_id).fetch_all(&mut *tx).await.map_err(ise)?;
    let tzs = tz.name().to_string();
    let daily = sqlx::query("SELECT zone_id, to_char(valid_from AT TIME ZONE $3,'YYYY-MM-DD') AS d, sum(rain_mm)::float8 AS rain, max(rain_prob) AS prob,
            max(GREATEST(COALESCE(wind_kmh,0),COALESCE(gust_kmh,0)))::float8 AS wind, min(temp_c)::float8 AS tmin, max(temp_c)::float8 AS tmax,
            mode() WITHIN GROUP (ORDER BY condition) AS cond
         FROM app.weather_slots WHERE kind='previsione' AND valid_from >= $1 AND valid_from < $2 GROUP BY zone_id, 2
         HAVING sum(extract(epoch FROM valid_to - valid_from)) >= 6*3600 ORDER BY 2")
        .bind(now - chrono::Duration::hours(12)).bind(now + chrono::Duration::days(days)).bind(&tzs).fetch_all(&mut *tx).await.map_err(ise)?;
    let sites = sqlx::query("SELECT s.id, s.name, s.weather_zone_id IS NOT NULL AS fixed, COALESCE(s.weather_zone_id,
            (SELECT z2.id FROM app.weather_zones z2 WHERE s.lat IS NOT NULL AND s.lon IS NOT NULL
              ORDER BY (z2.lat - s.lat)^2 + ((z2.lon - s.lon) * cos(radians(s.lat)))^2 LIMIT 1)) AS zone_id
         FROM app.sites s ORDER BY s.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"days": days, "zones": zones.iter().map(|z| { let zid: Uuid = z.get("id");
        let zs: Vec<_> = sites.iter().filter(|x| x.try_get::<Option<Uuid>,_>("zone_id").ok().flatten() == Some(zid))
            .map(|x| json!({"id": x.get::<Uuid,_>("id"), "name": x.get::<String,_>("name"), "fixed": x.get::<bool,_>("fixed")})).collect();
        json!({"id": zid, "code": z.get::<String,_>("code"), "name": z.get::<String,_>("name"), "lat": z.get::<f64,_>("lat"), "lon": z.get::<f64,_>("lon"),
            "sites_fixed": z.get::<i32,_>("sites_fixed"), "sites": zs, "issued_at": z.try_get::<Option<DateTime<Utc>>,_>("issued").ok().flatten(),
            "source": z.try_get::<Option<String>,_>("source").ok().flatten(),
            "days": daily.iter().filter(|d| d.get::<Uuid,_>("zone_id") == zid).map(|d| json!({"date": d.get::<String,_>("d"),
                "rain_mm": num(d, "rain"), "rain_prob": d.try_get::<Option<i32>,_>("prob").ok().flatten(), "wind_kmh": num(d, "wind"),
                "temp_min": num(d, "tmin"), "temp_max": num(d, "tmax"), "condition": d.try_get::<Option<String>,_>("cond").ok().flatten()})).collect::<Vec<_>>()})
    }).collect::<Vec<_>>()})))
}

/// GET /api/weather/at-risk?days=7 — interventi aperti dei prossimi giorni con un avviso meteo, e il
/// primo giorno (stesso orario, entro 7 giorni, lavorativo, coperto dalla previsione) in cui la previsione lo permette.
pub async fn weather_at_risk(State(st): State<AppState>, h: HeaderMap, Query(q): Query<WQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "meteo").await?; require_any_cap(&s, &["plan", "orders.view_all"])?;
    let days = q.days.unwrap_or(7).clamp(1, 16);
    let now = st.clock.now();
    let tz = crate::field::tenant_tz(&st, tid).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT o.id,o.title,o.version,o.site_id,o.activity_type_id,o.scheduled_start,o.scheduled_end,g.name AS team,u.email
         FROM app.work_orders o LEFT JOIN app.user_groups g ON g.id=o.assigned_group LEFT JOIN app.users u ON u.id=o.assigned_user
         WHERE o.state NOT IN ('completato','annullato','in_corso') AND o.scheduled_start >= $1 AND o.scheduled_start < $2
           AND o.activity_type_id IS NOT NULL AND o.site_id IS NOT NULL ORDER BY o.scheduled_start LIMIT 800")
        .bind(now).bind(now + chrono::Duration::days(days)).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut out = vec![];
    for r in &rows {
        let (at, site): (Uuid, Uuid) = (r.get("activity_type_id"), r.get("site_id"));
        let (a, z): (DateTime<Utc>, Option<DateTime<Utc>>) = (r.get("scheduled_start"), r.try_get("scheduled_end").ok());
        let z = z.unwrap_or(a + chrono::Duration::hours(1));
        let w = weather_warnings(&st, tid, Some(at), Some(site), Some(a), Some(z)).await?;
        if w.is_empty() { continue; }
        // primo giorno buono a pari orario (salta sabato e domenica), entro 7 giorni
        let mut better: Option<DateTime<Utc>> = None;
        for d in 1..=7 {
            let a2 = a + chrono::Duration::days(d);
            let wd = chrono::Datelike::weekday(&a2.with_timezone(&tz));
            if matches!(wd, chrono::Weekday::Sat | chrono::Weekday::Sun) { continue; }
            if !forecast_covers(&st, tid, site, a2, a2 + (z - a)).await? { break; } // oltre la previsione non si propone
            if weather_warnings(&st, tid, Some(at), Some(site), Some(a2), Some(a2 + (z - a))).await?.is_empty() { better = Some(a2); break; }
        }
        out.push(json!({"id": r.get::<Uuid,_>("id"), "title": r.get::<String,_>("title"), "version": r.get::<i32,_>("version"),
            "scheduled_start": a, "scheduled_end": z, "team": r.try_get::<Option<String>,_>("team").ok().flatten(),
            "assignee": r.try_get::<Option<String>,_>("email").ok().flatten(), "warnings": crate::field::violations_json(&w),
            "suggested_start": better, "suggested_end": better.map(|b| b + (z - a))}));
    }
    Ok(Json(json!({"days": days, "items": out})))
}

#[cfg(test)]
mod tests {
    #[test] fn italian_numbers() { assert_eq!(super::it(6.0), "6"); assert_eq!(super::it(2.5), "2,5"); }
}
