//! Report di tracciamento NATIVI (percorsi, soste, riepilogo, tracciato) calcolati dallo storico
//! posizioni del dispositivo (`app.device_positions`), con la stessa semantica per tutti i
//! protocolli (Teltonika, OsmAnd, JSON HTTP).
//!
//! Le velocità sono ricavate da distanza/tempo tra fix consecutivi (non dal campo `speed` del
//! device, che cambia unità di misura da protocollo a protocollo: nodi, km/h, m/s).

use axum::{extract::{Path, Query, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::api::{err, has_cap, ise, require_any_module, require_tenant, DEVICE_MODS};
use crate::state::AppState;

/// Raggio entro cui il dispositivo è considerato fermo nello stesso punto.
pub const STOP_RADIUS_M: f64 = 50.0;
/// Durata minima di una sosta.
pub const MIN_STOP_S: i64 = 300;
/// Distanza minima perché un tragitto sia significativo (sotto è rumore GPS).
pub const MIN_TRIP_M: f64 = 100.0;
/// Velocità oltre la quale un segmento è un salto implausibile e viene ignorato.
pub const MAX_PLAUSIBLE_KMH: f64 = 300.0;
/// Intervallo massimo consentito per un report.
pub const MAX_RANGE_DAYS: i64 = 31;
/// Punti massimi letti per un report.
pub const MAX_POINTS: i64 = 50_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix { pub lat: f64, pub lon: f64, pub at: DateTime<Utc> }

#[derive(Clone, Debug, PartialEq)]
pub struct Stop { pub start: DateTime<Utc>, pub end: DateTime<Utc>, pub lat: f64, pub lon: f64 }
impl Stop { pub fn duration_s(&self) -> i64 { (self.end - self.start).num_seconds() } }

#[derive(Clone, Debug, PartialEq)]
pub struct Trip { pub start: DateTime<Utc>, pub end: DateTime<Utc>, pub distance_m: f64, pub max_kmh: f64 }

#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub points: usize,
    pub distance_m: f64,
    pub moving_s: i64,
    pub max_kmh: f64,
    pub avg_moving_kmh: f64,
    pub stops: usize,
    pub trips: usize,
}

pub fn haversine_m(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    let r = 6_371_000.0_f64;
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dphi = (b_lat - a_lat).to_radians();
    let dlmb = (b_lon - a_lon).to_radians();
    let h = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlmb / 2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}

/// Distanza e velocità di un segmento; `None` se il segmento è un salto implausibile o ha
/// durata nulla/negativa (fix duplicati o fuori ordine).
fn segment(a: &Fix, b: &Fix) -> Option<(f64, f64, i64)> {
    let dt = (b.at - a.at).num_seconds();
    if dt <= 0 { return None; }
    let d = haversine_m(a.lat, a.lon, b.lat, b.lon);
    let kmh = d / dt as f64 * 3.6;
    if kmh > MAX_PLAUSIBLE_KMH { return None; }
    Some((d, kmh, dt))
}

/// Individua le soste: il dispositivo resta entro `STOP_RADIUS_M` dal punto d'ancoraggio per
/// almeno `MIN_STOP_S`. I fix devono essere ordinati per tempo.
pub fn stops(fixes: &[Fix]) -> Vec<Stop> {
    let mut out = Vec::new();
    if fixes.is_empty() { return out; }
    let mut anchor = 0usize;
    let mut last_inside = 0usize;
    for i in 1..fixes.len() {
        let a = &fixes[anchor];
        if haversine_m(a.lat, a.lon, fixes[i].lat, fixes[i].lon) <= STOP_RADIUS_M {
            last_inside = i;
            continue;
        }
        push_stop(&mut out, fixes, anchor, last_inside);
        anchor = i;
        last_inside = i;
    }
    push_stop(&mut out, fixes, anchor, last_inside);
    out
}

fn push_stop(out: &mut Vec<Stop>, fixes: &[Fix], from: usize, to: usize) {
    if to <= from { return; }
    let (s, e) = (&fixes[from], &fixes[to]);
    if (e.at - s.at).num_seconds() >= MIN_STOP_S {
        out.push(Stop { start: s.at, end: e.at, lat: s.lat, lon: s.lon });
    }
}

/// Tragitti = intervalli in movimento tra una sosta e la successiva (più l'eventuale tratto prima
/// della prima sosta e dopo l'ultima). Scarta i tragitti sotto `MIN_TRIP_M`.
pub fn trips(fixes: &[Fix], stops: &[Stop]) -> Vec<Trip> {
    let mut out = Vec::new();
    if fixes.len() < 2 { return out; }
    let first = fixes[0].at;
    let last = fixes[fixes.len() - 1].at;
    // finestre di movimento: [inizio, prima sosta), (fine sosta i, inizio sosta i+1), (ultima sosta, fine]
    let mut windows: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    let mut cursor = first;
    for s in stops {
        if s.start > cursor { windows.push((cursor, s.start)); }
        cursor = s.end;
    }
    if last > cursor { windows.push((cursor, last)); }
    for (ws, we) in windows {
        let mut dist = 0.0; let mut max_kmh = 0.0_f64;
        let pts: Vec<&Fix> = fixes.iter().filter(|f| f.at >= ws && f.at <= we).collect();
        for w in pts.windows(2) {
            if let Some((d, kmh, _)) = segment(w[0], w[1]) { dist += d; max_kmh = max_kmh.max(kmh); }
        }
        if dist >= MIN_TRIP_M {
            out.push(Trip { start: ws, end: we, distance_m: dist, max_kmh });
        }
    }
    out
}

pub fn summary(fixes: &[Fix], stops: &[Stop], trips: &[Trip]) -> Summary {
    let mut dist = 0.0; let mut max_kmh = 0.0_f64; let mut moving_s = 0i64;
    for w in fixes.windows(2) {
        if let Some((d, kmh, dt)) = segment(&w[0], &w[1]) {
            dist += d; max_kmh = max_kmh.max(kmh);
            // tempo in movimento: segmenti sopra 1 km/h (sotto è deriva del GPS da fermo)
            if kmh >= 1.0 { moving_s += dt; }
        }
    }
    let avg = if moving_s > 0 { dist / moving_s as f64 * 3.6 } else { 0.0 };
    Summary { points: fixes.len(), distance_m: dist, moving_s, max_kmh, avg_moving_kmh: avg,
              stops: stops.len(), trips: trips.len() }
}

// ------------------------------------------------------------------------------------------
// HTTP: GET /api/devices/:id/report/:kind?from=..&to=..[&csv=true]
// kind ∈ summary | trips | stops | route
// ------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ReportQuery { pub from: String, pub to: String, pub csv: Option<bool> }

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
        .or_else(|| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0)).map(|n| n.and_utc()))
}

pub async fn device_report(State(st): State<AppState>, h: HeaderMap, Path((id, kind)): Path<(Uuid, String)>,
    Query(q): Query<ReportQuery>) -> Response {
    match build_report(&st, &h, id, &kind, &q).await {
        Ok((true, body)) => ([(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")], body).into_response(),
        Ok((false, body)) => ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn build_report(st: &AppState, h: &HeaderMap, id: Uuid, kind: &str, q: &ReportQuery)
    -> Result<(bool, String), (StatusCode, String)> {
    let s = require_tenant(st, h).await?;
    if !has_cap(&s, "incidents.view") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.ok_or_else(|| err(StatusCode::FORBIDDEN, "tenant richiesto"))?;
    require_any_module(st, tid, DEVICE_MODS).await?;
    if !matches!(kind, "summary" | "trips" | "stops" | "route") {
        return Err(err(StatusCode::BAD_REQUEST, "report non valido (summary, trips, stops, route)"));
    }
    let from = parse_ts(&q.from).ok_or_else(|| err(StatusCode::BAD_REQUEST, "data di inizio non valida"))?;
    let to = parse_ts(&q.to).ok_or_else(|| err(StatusCode::BAD_REQUEST, "data di fine non valida"))?;
    if to <= from { return Err(err(StatusCode::BAD_REQUEST, "la fine deve essere successiva all'inizio")); }
    if (to - from).num_days() > MAX_RANGE_DAYS {
        return Err(err(StatusCode::BAD_REQUEST, &format!("intervallo massimo {MAX_RANGE_DAYS} giorni")));
    }
    let mut tx = crate::db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.devices WHERE id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    if exists.is_none() { return Err(err(StatusCode::NOT_FOUND, "dispositivo inesistente")); }
    let rows = sqlx::query("SELECT lat,lon,fixed_at FROM app.device_positions
         WHERE device_id=$1 AND fixed_at>=$2 AND fixed_at<=$3 ORDER BY fixed_at LIMIT $4")
        .bind(id).bind(from).bind(to).bind(MAX_POINTS).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let fixes: Vec<Fix> = rows.iter().map(|r| Fix { lat: r.get("lat"), lon: r.get("lon"), at: r.get("fixed_at") }).collect();
    let st_list = stops(&fixes);
    let tr_list = trips(&fixes, &st_list);
    let csv = q.csv.unwrap_or(false);
    let fmt = |t: &DateTime<Utc>| t.to_rfc3339();
    let body = match (kind, csv) {
        ("summary", false) => {
            let s = summary(&fixes, &st_list, &tr_list);
            json!({"points":s.points,"distance_m":s.distance_m.round(),"moving_s":s.moving_s,
                   "max_kmh":s.max_kmh.round(),"avg_moving_kmh":s.avg_moving_kmh.round(),
                   "stops":s.stops,"trips":s.trips,"from":fmt(&from),"to":fmt(&to)}).to_string()
        }
        ("summary", true) => {
            let s = summary(&fixes, &st_list, &tr_list);
            format!("punti;distanza_km;tempo_in_movimento_min;vel_max_kmh;vel_media_kmh;soste;tragitti\n{};{:.2};{};{:.0};{:.0};{};{}\n",
                s.points, s.distance_m / 1000.0, s.moving_s / 60, s.max_kmh, s.avg_moving_kmh, s.stops, s.trips)
        }
        ("stops", false) => Value::Array(st_list.iter().map(|x| json!({"start":fmt(&x.start),"end":fmt(&x.end),
            "duration_s":x.duration_s(),"lat":x.lat,"lon":x.lon})).collect()).to_string(),
        ("stops", true) => {
            let mut o = String::from("inizio;fine;durata_min;lat;lon\n");
            for x in &st_list { o += &format!("{};{};{};{:.6};{:.6}\n", fmt(&x.start), fmt(&x.end), x.duration_s() / 60, x.lat, x.lon); }
            o
        }
        ("trips", false) => Value::Array(tr_list.iter().map(|x| json!({"start":fmt(&x.start),"end":fmt(&x.end),
            "distance_m":x.distance_m.round(),"max_kmh":x.max_kmh.round()})).collect()).to_string(),
        ("trips", true) => {
            let mut o = String::from("inizio;fine;distanza_km;vel_max_kmh\n");
            for x in &tr_list { o += &format!("{};{};{:.2};{:.0}\n", fmt(&x.start), fmt(&x.end), x.distance_m / 1000.0, x.max_kmh); }
            o
        }
        (_, false) => Value::Array(fixes.iter().map(|f| json!({"lat":f.lat,"lon":f.lon,"at":fmt(&f.at)})).collect()).to_string(),
        (_, true) => {
            let mut o = String::from("orario;lat;lon\n");
            for f in &fixes { o += &format!("{};{:.6};{:.6}\n", fmt(&f.at), f.lat, f.lon); }
            o
        }
    };
    Ok((csv, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(min: i64) -> DateTime<Utc> { Utc.with_ymd_and_hms(2026, 1, 1, 8, 0, 0).unwrap() + chrono::Duration::minutes(min) }
    /// ~111 m per 0.001° di latitudine.
    fn fix(lat_off_milli: f64, min: i64) -> Fix { Fix { lat: 45.0 + lat_off_milli / 1000.0, lon: 9.0, at: t(min) } }

    #[test]
    fn haversine_known_distance() {
        let d = haversine_m(45.0, 9.0, 45.001, 9.0);
        assert!((d - 111.2).abs() < 1.0, "{d}");
    }

    #[test]
    fn no_points_no_report() {
        assert!(stops(&[]).is_empty());
        assert!(trips(&[], &[]).is_empty());
        let s = summary(&[], &[], &[]);
        assert_eq!(s.points, 0); assert_eq!(s.distance_m, 0.0);
    }

    #[test]
    fn stationary_device_is_one_stop() {
        let f: Vec<Fix> = (0..=20).map(|m| fix(0.0, m)).collect();
        let s = stops(&f);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].duration_s(), 20 * 60);
        assert!(trips(&f, &s).is_empty());
    }

    #[test]
    fn short_pause_is_not_a_stop() {
        let f = vec![fix(0.0, 0), fix(0.0, 2), fix(0.0, 4)]; // 4 minuti < 5
        assert!(stops(&f).is_empty());
    }

    #[test]
    fn stop_trip_stop() {
        // fermo 10 min, poi si sposta ~1.1 km in 5 min, poi fermo 10 min
        let mut f: Vec<Fix> = (0..=10).map(|m| fix(0.0, m)).collect();
        for (i, m) in (11..=15).enumerate() { f.push(fix(2.0 * (i as f64 + 1.0), m)); }
        for m in 16..=26 { f.push(fix(10.0, m)); }
        let s = stops(&f);
        assert_eq!(s.len(), 2, "{s:?}");
        let tr = trips(&f, &s);
        assert_eq!(tr.len(), 1, "{tr:?}");
        assert!(tr[0].distance_m > 1000.0 && tr[0].distance_m < 1200.0, "{}", tr[0].distance_m);
        let sm = summary(&f, &s, &tr);
        assert_eq!(sm.stops, 2); assert_eq!(sm.trips, 1);
        assert!(sm.max_kmh > 10.0 && sm.max_kmh < 20.0, "{}", sm.max_kmh);
    }

    #[test]
    fn implausible_jump_is_ignored() {
        // salto di ~111 km in 1 minuto: scartato dal conteggio distanza
        let f = vec![fix(0.0, 0), fix(1000.0, 1), fix(0.0, 2)];
        let sm = summary(&f, &[], &[]);
        assert_eq!(sm.distance_m, 0.0);
        assert_eq!(sm.max_kmh, 0.0);
    }

    #[test]
    fn duplicate_timestamps_are_ignored() {
        let f = vec![fix(0.0, 0), fix(1.0, 0), fix(2.0, 1)];
        let sm = summary(&f, &[], &[]);
        assert!(sm.distance_m > 100.0 && sm.distance_m < 120.0, "{}", sm.distance_m);
    }

    #[test]
    fn parse_dates() {
        assert!(parse_ts("2026-01-01T08:00:00Z").is_some());
        assert!(parse_ts("2026-01-01").is_some());
        assert!(parse_ts("ieri").is_none());
    }
}
