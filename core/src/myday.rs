//! App dell'operaio — M1 «La mia giornata» (Lavoro sul campo, per qualsiasi tenant).
//! Una chiamata con tutto quello che serve a chi va in campo, solo per sé e per le proprie squadre:
//! interventi del giorno nell'ordine di visita (orario), spostamento stimato dal precedente (stesso
//! modello della pianificazione: deposito → visite → rientro), mezzi da ritirare, squadra con gli
//! assenti, meteo per sede e fascia oraria con gli avvisi del tipo di attività (modulo meteo) e il
//! riepilogo per zona, link di navigazione del giro. Sezioni secondo i moduli attivi.

use axum::{extract::{State, Query}, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_module, module_active, ise, err};
use crate::db;

#[derive(Deserialize)]
pub struct DayQ { #[serde(default)] pub date: Option<NaiveDate>, #[serde(default)] pub next: Option<String> }

fn f(r: &sqlx::postgres::PgRow, c: &str) -> Option<f64> { r.try_get::<Option<f64>,_>(c).ok().flatten() }

/// GET /api/my/day?date=YYYY-MM-DD — la giornata dell'utente (oggi se la data manca).
pub async fn my_day(State(st): State<AppState>, h: HeaderMap, Query(q): Query<DayQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let tz = crate::field::tenant_tz(&st, tid).await;
    let now = st.clock.now();
    let today = now.with_timezone(&tz).date_naive();
    let mut day = q.date.unwrap_or(today);
    // «prossima giornata»: il primo giorno dopo oggi con interventi propri o della squadra (entro 14 giorni)
    if matches!(q.next.as_deref(), Some("1") | Some("true")) {
        let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
        let from = tz.from_local_datetime(&(today + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap()).earliest().map(|x| x.with_timezone(&Utc)).unwrap();
        let first: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT min(o.scheduled_start) FROM app.work_orders o
                WHERE o.state <> 'annullato' AND o.scheduled_start >= $2 AND o.scheduled_start < $2 + interval '14 days'
                  AND (o.assigned_user=$1 OR o.assigned_group IN (SELECT group_id FROM app.user_group_members WHERE user_id=$1))")
            .bind(s.user_id).bind(from).fetch_one(&mut *tx).await.map_err(ise)?;
        tx.commit().await.map_err(ise)?;
        day = first.map(|t| t.with_timezone(&tz).date_naive()).unwrap_or(today + Duration::days(1));
    }
    if (day - today).num_days().abs() > 14 { return Err(err(StatusCode::BAD_REQUEST, "giornata tra 14 giorni prima e 14 giorni dopo oggi")); }
    let mid = |d: NaiveDate| tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|x| x.with_timezone(&Utc)).unwrap();
    let (a, z) = (mid(day), mid(day + Duration::days(1)));
    let (mezzi_on, meteo_on) = (module_active(&st, tid, "mezzi").await, module_active(&st, tid, "meteo").await);
    let settings = crate::planning::load_settings(&st, tid).await?;

    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // squadre e gruppi dell'utente
    let groups = sqlx::query("SELECT g.id, g.name, g.kind, ds.lat AS dlat, ds.lon AS dlon, ds.name AS dname
         FROM app.user_group_members m JOIN app.user_groups g ON g.id=m.group_id LEFT JOIN app.sites ds ON ds.id=g.depot_site_id
         WHERE m.user_id=$1 ORDER BY g.name").bind(s.user_id).fetch_all(&mut *tx).await.map_err(ise)?;
    let gids: Vec<Uuid> = groups.iter().map(|g| g.get("id")).collect();
    let orders = sqlx::query("SELECT o.id, o.title, o.state, o.version, o.priority, o.instructions, o.scheduled_start, o.scheduled_end, o.quantity::float8 AS qty,
            o.estimated_minutes, o.arrived_at, o.work_started_at, o.assigned_user, o.assigned_group, o.activity_type_id, o.site_id, o.updated_at,
            s.name AS site, s.address, s.lat, s.lon, s.access_notes, s.access_windows, at.name AS activity, at.unit, g.name AS team, u.email AS assignee, cl.name AS client
         FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.activity_types at ON at.id=o.activity_type_id
         LEFT JOIN app.user_groups g ON g.id=o.assigned_group LEFT JOIN app.users u ON u.id=o.assigned_user LEFT JOIN app.clients cl ON cl.id=o.client_id
         WHERE o.state <> 'annullato' AND o.scheduled_start >= $2 AND o.scheduled_start < $3
           AND (o.assigned_user=$1 OR o.assigned_group = ANY($4))
         ORDER BY o.scheduled_start, o.title").bind(s.user_id).bind(a).bind(z).bind(&gids).fetch_all(&mut *tx).await.map_err(ise)?;
    let oids: Vec<Uuid> = orders.iter().map(|o| o.get("id")).collect();
    let bookings = if mezzi_on { sqlx::query("SELECT b.work_order_id, x.id, x.name, x.kind, x.identifier FROM app.asset_bookings b JOIN app.assets x ON x.id=b.asset_id
            WHERE b.work_order_id = ANY($1) ORDER BY x.name").bind(&oids).fetch_all(&mut *tx).await.map_err(ise)? } else { vec![] };
    // squadre con i membri e chi è assente quel giorno
    let members = sqlx::query("SELECT m.group_id, u.id, u.email,
            EXISTS(SELECT 1 FROM app.worker_absences w WHERE w.user_id=u.id AND w.starts_at < $3 AND w.ends_at > $2) AS absent
         FROM app.user_group_members m JOIN app.users u ON u.id=m.user_id JOIN app.user_groups g ON g.id=m.group_id
         WHERE m.group_id = ANY($1) AND g.kind='squadra' AND u.anonymized_at IS NULL ORDER BY u.email").bind(&gids).bind(a).bind(z).fetch_all(&mut *tx).await.map_err(ise)?;
    let depot = match settings.depot { Some(d) => sqlx::query("SELECT name, lat, lon FROM app.sites WHERE id=$1").bind(d).fetch_optional(&mut *tx).await.map_err(ise)?, None => None };
    tx.commit().await.map_err(ise)?;

    // deposito: quello della (prima) squadra, altrimenti quello generale
    let team_depot = groups.iter().filter(|g| g.get::<String,_>("kind") == "squadra").find_map(|g| match (f(g, "dlat"), f(g, "dlon")) {
        (Some(x), Some(y)) => Some((g.try_get::<Option<String>,_>("dname").ok().flatten().unwrap_or_default(), x, y)), _ => None });
    let depot = team_depot.or_else(|| depot.as_ref().and_then(|d| match (f(d, "lat"), f(d, "lon")) { (Some(x), Some(y)) => Some((d.get::<String,_>("name"), x, y)), _ => None }));
    let mut prev: Option<(f64, f64)> = depot.as_ref().map(|d| (d.1, d.2));
    let (mut tot_travel, mut tot_km, mut tot_work) = (0i64, 0.0f64, 0i64);
    let mut items = vec![];
    let mut zones: BTreeMap<String, Value> = BTreeMap::new();
    for (k, o) in orders.iter().enumerate() {
        let id: Uuid = o.get("id");
        let start: DateTime<Utc> = o.get("scheduled_start");
        let end: DateTime<Utc> = o.try_get::<Option<DateTime<Utc>>,_>("scheduled_end").ok().flatten()
            .unwrap_or(start + Duration::minutes(o.try_get::<Option<i32>,_>("estimated_minutes").ok().flatten().unwrap_or(60) as i64));
        let loc = match (f(o, "lat"), f(o, "lon")) { (Some(x), Some(y)) => Some((x, y)), _ => None };
        let (tmin, km) = match (prev, loc) { (Some(p), Some(l)) => crate::planning::travel_min(&settings, p, l), _ => (0, 0.0) };
        if loc.is_some() { prev = loc; }
        tot_travel += tmin; tot_km += km; tot_work += (end - start).num_minutes().max(0);
        let mut weather = Value::Null; let mut warnings = json!([]);
        if meteo_on {
            if let Some(site) = o.try_get::<Option<Uuid>,_>("site_id").ok().flatten() {
                if let Some(w) = crate::weather::site_slot_weather(&st, tid, site, start, end).await? {
                    if let Some(zn) = w["zone"].as_str() { zones.entry(zn.to_string()).or_insert(Value::Null); }
                    weather = w;
                }
                let vs = crate::weather::weather_warnings(&st, tid, o.try_get("activity_type_id").ok().flatten(), Some(site), Some(start), Some(end)).await?;
                warnings = crate::field::violations_json(&vs);
            }
        }
        let assets: Vec<Value> = bookings.iter().filter(|b| b.get::<Uuid,_>("work_order_id") == id)
            .map(|b| json!({"id": b.get::<Uuid,_>("id"), "name": b.get::<String,_>("name"), "kind": b.try_get::<Option<String>,_>("kind").ok().flatten(),
                "identifier": b.try_get::<Option<String>,_>("identifier").ok().flatten()})).collect();
        items.push(json!({"seq": k + 1, "id": id, "title": o.get::<String,_>("title"), "state": o.get::<String,_>("state"), "version": o.get::<i32,_>("version"),
            "priority": o.try_get::<Option<i32>,_>("priority").ok().flatten(), "instructions": o.try_get::<Option<String>,_>("instructions").ok().flatten(),
            "start": start, "end": end, "minutes": (end - start).num_minutes(), "travel_minutes": tmin, "travel_km": (km * 10.0).round() / 10.0,
            "site": o.try_get::<Option<String>,_>("site").ok().flatten(), "site_address": o.try_get::<Option<String>,_>("address").ok().flatten(),
            "site_lat": loc.map(|l| l.0), "site_lon": loc.map(|l| l.1), "access_notes": o.try_get::<Option<String>,_>("access_notes").ok().flatten(),
            "access_windows": o.try_get::<Option<Value>,_>("access_windows").ok().flatten().unwrap_or(json!([])),
            "activity": o.try_get::<Option<String>,_>("activity").ok().flatten(), "unit": o.try_get::<Option<String>,_>("unit").ok().flatten(), "quantity": f(o, "qty"),
            "team": o.try_get::<Option<String>,_>("team").ok().flatten(), "assignee": o.try_get::<Option<String>,_>("assignee").ok().flatten(),
            "mine": o.try_get::<Option<Uuid>,_>("assigned_user").ok().flatten() == Some(s.user_id), "client": o.try_get::<Option<String>,_>("client").ok().flatten(),
            "arrived_at": o.try_get::<Option<DateTime<Utc>>,_>("arrived_at").ok().flatten(), "work_started_at": o.try_get::<Option<DateTime<Utc>>,_>("work_started_at").ok().flatten(),
            "updated_at": o.get::<DateTime<Utc>,_>("updated_at"), "assets": assets, "weather": weather, "weather_warnings": warnings}));
    }
    // rientro al deposito
    if let (Some(p), Some(d)) = (prev, depot.as_ref()) { if !items.is_empty() { let (m, k) = crate::planning::travel_min(&settings, p, (d.1, d.2)); tot_travel += m; tot_km += k; } }
    // mezzi da ritirare (una volta sola)
    let mut vehicles: BTreeMap<Uuid, Value> = BTreeMap::new();
    for it in &items { for x in it["assets"].as_array().cloned().unwrap_or_default() { if let Some(id) = x["id"].as_str().and_then(|s| s.parse().ok()) { vehicles.entry(id).or_insert(x); } } }
    // riepilogo meteo per zona della giornata
    let day_weather = if meteo_on { crate::weather::zones_day_summary(&st, tid, zones.keys().cloned().collect(), a, z).await? } else { vec![] };
    // giro per la navigazione: deposito → tappe → deposito
    let stops: Vec<String> = items.iter().filter_map(|i| match (i["site_lat"].as_f64(), i["site_lon"].as_f64()) { (Some(x), Some(y)) => Some(format!("{x},{y}")), _ => None }).collect();
    let route_url = if stops.is_empty() { None } else {
        let origin = depot.as_ref().map(|d| format!("{},{}", d.1, d.2));
        let dest = origin.clone().unwrap_or_else(|| stops.last().unwrap().clone());
        let wps: Vec<String> = if origin.is_some() { stops.clone() } else { stops[..stops.len() - 1].to_vec() };
        Some(format!("https://www.google.com/maps/dir/?api=1{}&destination={dest}{}&travelmode=driving",
            origin.map(|o| format!("&origin={o}")).unwrap_or_default(), if wps.is_empty() { String::new() } else { format!("&waypoints={}", wps.join("%7C")) }))
    };
    let teams: Vec<Value> = groups.iter().filter(|g| g.get::<String,_>("kind") == "squadra").map(|g| { let gid: Uuid = g.get("id");
        json!({"id": gid, "name": g.get::<String,_>("name"), "members": members.iter().filter(|m| m.get::<Uuid,_>("group_id") == gid)
            .map(|m| json!({"email": m.get::<String,_>("email"), "me": m.get::<Uuid,_>("id") == s.user_id, "absent": m.get::<bool,_>("absent")})).collect::<Vec<_>>()}) }).collect();
    Ok(Json(json!({"date": day, "today": today, "is_today": day == today, "timezone": tz.name(),
        "depot": depot.map(|d| json!({"name": d.0, "lat": d.1, "lon": d.2})), "teams": teams, "orders": items,
        "vehicles": vehicles.into_values().collect::<Vec<_>>(), "weather": day_weather, "route_url": route_url,
        "totals": {"orders": orders.len(), "work_minutes": tot_work, "travel_minutes": tot_travel, "travel_km": (tot_km * 10.0).round() / 10.0},
        "modules": {"mezzi": mezzi_on, "meteo": meteo_on}})))
}
