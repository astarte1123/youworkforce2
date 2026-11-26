//! Fase 6 «Pilota» (piano Idealverde, mig 0171, modulo `pianificazione`): la pianificazione automatica
//! si prova su alcune squadre per qualche settimana e si misura contro le settimane prima, con le misure
//! del piano: piani proposti accettati senza modifiche, ore di spostamento (per intervento, stesso
//! modello della pianificazione), straordinario, richieste extra gestite entro lo SLA del contratto,
//! ore di pianificazione risparmiate (dichiarate dal pianificatore ogni settimana). Alla fine si decide:
//! estendere a tutte le squadre o concludere.

use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, require_any_cap, require_module, ise, err, audit};
use crate::planning::{load_settings, travel_min, Settings};
use crate::db;

const ACCEPT_TOLERANCE_MIN: i64 = 30;

fn monday(d: NaiveDate) -> NaiveDate { d - Duration::days(d.weekday().num_days_from_monday() as i64) }
fn local_mid(tz: chrono_tz::Tz, d: NaiveDate) -> DateTime<Utc> {
    tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|x| x.with_timezone(&Utc)).unwrap_or_else(|| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
}

#[derive(Default, Clone, Debug)]
pub(crate) struct WeekM { pub orders: i64, pub from_plan: i64, pub unchanged: i64, pub travel_min: i64, pub overtime_min: i64,
    pub sla_ok: i64, pub sla_late: i64, pub sla_open: i64 }

/// Spostamenti e straordinario di una giornata di squadra (dal deposito, in ordine di orario, rientro).
fn day_route(s: &Settings, tz: chrono_tz::Tz, depot: Option<(f64, f64)>, mut v: Vec<(DateTime<Utc>, DateTime<Utc>, Option<(f64, f64)>)>) -> (i64, i64) {
    v.sort_by_key(|x| x.0);
    let (mut tm, mut loc) = (0i64, depot);
    for (_, _, l) in &v { if let (Some(a), Some(b)) = (loc, *l) { tm += travel_min(s, a, b).0; } if l.is_some() { loc = *l; } }
    let back = match (loc, depot) { (Some(a), Some(b)) => travel_min(s, a, b).0, _ => 0 };
    tm += back;
    let last = v.iter().map(|x| x.1).max();
    let ot = match (last, v.first()) {
        (Some(z), Some(f)) => { let d = f.0.with_timezone(&tz).date_naive();
            let end = tz.from_local_datetime(&d.and_time(s.day_end)).earliest().map(|x| x.with_timezone(&Utc)).unwrap_or(z);
            ((z + Duration::minutes(back)) - end).num_minutes().max(0) }
        _ => 0 };
    (tm, ot)
}

/// Misure di una settimana per le squadre indicate.
pub(crate) async fn week_metrics(st: &AppState, tid: Uuid, teams: &[Uuid], wk: NaiveDate, default_sla: i64) -> Result<WeekM, (StatusCode, String)> {
    let s = load_settings(st, tid).await?;
    let tz = crate::field::tenant_tz(st, tid).await;
    let (a, z) = (local_mid(tz, wk), local_mid(tz, wk + Duration::days(7)));
    let now = st.clock.now();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let depot: Option<(f64, f64)> = match s.depot { Some(d) => sqlx::query("SELECT lat,lon FROM app.sites WHERE id=$1 AND lat IS NOT NULL").bind(d).fetch_optional(&mut *tx).await.map_err(ise)?
        .map(|r| (r.get("lat"), r.get("lon"))), None => None };
    let tdep: HashMap<Uuid, (f64, f64)> = sqlx::query("SELECT g.id, s.lat, s.lon FROM app.user_groups g JOIN app.sites s ON s.id=g.depot_site_id WHERE g.id = ANY($1) AND s.lat IS NOT NULL")
        .bind(teams).fetch_all(&mut *tx).await.map_err(ise)?.iter().map(|r| (r.get::<Uuid,_>("id"), (r.get::<f64,_>("lat"), r.get::<f64,_>("lon")))).collect();
    let rows = sqlx::query("SELECT o.id, o.assigned_group, o.scheduled_start, COALESCE(o.scheduled_end, o.scheduled_start + interval '1 hour') AS e, s.lat, s.lon
         FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id
         WHERE o.assigned_group = ANY($1) AND o.state <> 'annullato' AND o.scheduled_start >= $2 AND o.scheduled_start < $3")
        .bind(teams).bind(a).bind(z).fetch_all(&mut *tx).await.map_err(ise)?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
    // ultima proposta applicata per ogni intervento
    let items = sqlx::query("SELECT DISTINCT ON (i.work_order_id) i.work_order_id, i.group_id, i.starts_at FROM app.planning_run_items i
         JOIN app.planning_runs r ON r.id=i.run_id WHERE i.applied AND i.work_order_id = ANY($1) ORDER BY i.work_order_id, r.applied_at DESC NULLS LAST")
        .bind(&ids).fetch_all(&mut *tx).await.map_err(ise)?;
    let reqs = sqlx::query("SELECT r.created_at, COALESCE(o.arrived_at, o.work_started_at) AS served, o.state,
            COALESCE((SELECT min(c.sla_hours) FROM app.service_contracts c WHERE c.client_id=r.client_id AND c.active AND c.sla_hours IS NOT NULL), $4) AS sla
         FROM app.service_requests r JOIN app.work_orders o ON o.id=r.order_id
         WHERE o.assigned_group = ANY($1) AND r.created_at >= $2 AND r.created_at < $3")
        .bind(teams).bind(a).bind(z).bind(default_sla as i32).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut m = WeekM { orders: rows.len() as i64, ..Default::default() };
    let by_id: HashMap<Uuid, &sqlx::postgres::PgRow> = rows.iter().map(|r| (r.get::<Uuid,_>("id"), r)).collect();
    for it in &items {
        m.from_plan += 1;
        let Some(o) = by_id.get(&it.get::<Uuid,_>("work_order_id")) else { continue };
        let same_team = it.try_get::<Option<Uuid>,_>("group_id").ok().flatten() == o.try_get::<Option<Uuid>,_>("assigned_group").ok().flatten();
        let (ps, os): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = (it.try_get("starts_at").ok().flatten(), o.try_get("scheduled_start").ok().flatten());
        if same_team && matches!((ps, os), (Some(p), Some(x)) if (x - p).num_minutes().abs() <= ACCEPT_TOLERANCE_MIN) { m.unchanged += 1; }
    }
    let mut days: HashMap<(Uuid, NaiveDate), Vec<(DateTime<Utc>, DateTime<Utc>, Option<(f64, f64)>)>> = HashMap::new();
    for r in &rows { let g: Uuid = r.get("assigned_group"); let st0: DateTime<Utc> = r.get("scheduled_start");
        let loc = match (r.try_get::<Option<f64>,_>("lat").ok().flatten(), r.try_get::<Option<f64>,_>("lon").ok().flatten()) { (Some(x), Some(y)) => Some((x, y)), _ => None };
        days.entry((g, st0.with_timezone(&tz).date_naive())).or_default().push((st0, r.get("e"), loc)); }
    for ((g, _), v) in days { let (t, o) = day_route(&s, tz, tdep.get(&g).cloned().or(depot), v); m.travel_min += t; m.overtime_min += o; }
    for r in &reqs {
        let (c, sla): (DateTime<Utc>, i32) = (r.get("created_at"), r.get("sla"));
        let due = c + Duration::hours(sla as i64);
        match r.try_get::<Option<DateTime<Utc>>,_>("served").ok().flatten() {
            Some(sv) => if sv <= due { m.sla_ok += 1 } else { m.sla_late += 1 },
            None => if now > due { m.sla_late += 1 } else { m.sla_open += 1 },
        }
    }
    Ok(m)
}

fn pct(a: i64, b: i64) -> Option<f64> { if b > 0 { Some((1000.0 * a as f64 / b as f64).round() / 10.0) } else { None } }

pub(crate) async fn pilot_report(st: &AppState, tid: Uuid, id: Uuid) -> Result<Value, (StatusCode, String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let p = sqlx::query("SELECT p.*, p.baseline_planning_hours::float8 AS bph, u.email FROM app.planning_pilots p LEFT JOIN app.users u ON u.id=p.created_by WHERE p.id=$1").bind(id)
        .fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "pilota inesistente"))?;
    let teams: Vec<Uuid> = p.get("group_ids");
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM app.user_groups WHERE id = ANY($1) ORDER BY name").bind(&teams).fetch_all(&mut *tx).await.map_err(ise)?;
    let reported: HashMap<NaiveDate, (f64, Option<String>)> = sqlx::query("SELECT week_start, planning_hours::float8 AS h, note FROM app.planning_pilot_weeks WHERE pilot_id=$1").bind(id)
        .fetch_all(&mut *tx).await.map_err(ise)?.iter().map(|r| (r.get::<NaiveDate,_>("week_start"), (r.get::<f64,_>("h"), r.try_get::<Option<String>,_>("note").ok().flatten()))).collect();
    tx.commit().await.map_err(ise)?;
    let tz = crate::field::tenant_tz(st, tid).await;
    let today = st.clock.now().with_timezone(&tz).date_naive();
    let start: NaiveDate = monday(p.get("starts_on"));
    let (weeks, bweeks): (i32, i32) = (p.get("weeks"), p.get("baseline_weeks"));
    let sla_def = p.get::<i32,_>("default_sla_hours") as i64;
    let targets: Value = p.get("targets");
    let mut out = vec![];
    let (mut b, mut q) = (WeekM::default(), WeekM::default()); let mut qn = 0; let mut qh: Vec<f64> = vec![];
    for k in -(bweeks as i64)..(weeks as i64) {
        let wk = start + Duration::days(7 * k);
        let m = week_metrics(st, tid, &teams, wk, sla_def).await?;
        let phase = if k < 0 { "prima" } else { "pilota" };
        let state = if wk > today { "da svolgere" } else if wk + Duration::days(7) > today { "in corso" } else { "svolta" };
        let rep = reported.get(&wk).cloned();
        if k < 0 { b.orders += m.orders; b.travel_min += m.travel_min; b.overtime_min += m.overtime_min; b.sla_ok += m.sla_ok; b.sla_late += m.sla_late; }
        else if state != "da svolgere" { qn += 1; q.orders += m.orders; q.from_plan += m.from_plan; q.unchanged += m.unchanged; q.travel_min += m.travel_min;
            q.overtime_min += m.overtime_min; q.sla_ok += m.sla_ok; q.sla_late += m.sla_late; if let Some((h, _)) = &rep { qh.push(*h); } }
        out.push(json!({"week_start": wk, "phase": phase, "state": state, "orders": m.orders, "from_plan": m.from_plan, "unchanged": m.unchanged,
            "proposed_pct": pct(m.from_plan, m.orders), "accepted_pct": pct(m.unchanged, m.from_plan),
            "travel_hours": (m.travel_min as f64 / 6.0).round() / 10.0, "travel_min_per_order": if m.orders > 0 { Some((10.0 * m.travel_min as f64 / m.orders as f64).round() / 10.0) } else { None },
            "overtime_minutes": m.overtime_min, "sla_ok": m.sla_ok, "sla_late": m.sla_late, "sla_open": m.sla_open, "sla_pct": pct(m.sla_ok, m.sla_ok + m.sla_late),
            "planning_hours": rep.as_ref().map(|r| r.0), "note": rep.and_then(|r| r.1)}));
    }
    let per_order = |w: &WeekM| if w.orders > 0 { Some(w.travel_min as f64 / w.orders as f64) } else { None };
    let tr = match (per_order(&b), per_order(&q)) { (Some(x), Some(y)) if x > 0.0 => Some((1000.0 * (x - y) / x).round() / 10.0), _ => None };
    let base_h = p.try_get::<Option<f64>,_>("bph").ok().flatten();
    let saved = match (base_h, qh.is_empty()) { (Some(bh), false) => Some(((bh - qh.iter().sum::<f64>() / qh.len() as f64) * 10.0).round() / 10.0), _ => None };
    let tget = |k: &str| targets.get(k).and_then(|v| v.as_f64());
    let verdict = |v: Option<f64>, t: Option<f64>| match (v, t) { (Some(v), Some(t)) => Some(v >= t), _ => None };
    let acc = pct(q.unchanged, q.from_plan); let sla = pct(q.sla_ok, q.sla_ok + q.sla_late);
    let measures = json!([
        {"code": "accepted", "label": "Piani proposti accettati senza modifiche", "unit": "%", "baseline": null, "pilot": acc, "target": tget("accepted_pct"), "ok": verdict(acc, tget("accepted_pct")),
         "detail": format!("{} interventi da proposta applicata, {} rimasti come proposti (stessa squadra, entro {ACCEPT_TOLERANCE_MIN} minuti)", q.from_plan, q.unchanged)},
        {"code": "travel", "label": "Spostamenti per intervento", "unit": "% in meno", "baseline": per_order(&b).map(|x| (x * 10.0).round() / 10.0), "pilot": per_order(&q).map(|x| (x * 10.0).round() / 10.0),
         "change": tr, "target": tget("travel_reduction_pct"), "ok": verdict(tr, tget("travel_reduction_pct")), "detail": "minuti di spostamento stimati per intervento, stesso modello della pianificazione"},
        {"code": "overtime", "label": "Straordinario a settimana", "unit": "min", "baseline": if bweeks > 0 { Some(b.overtime_min / bweeks as i64) } else { None },
         "pilot": if qn > 0 { Some(q.overtime_min / qn) } else { None }, "target": null, "ok": null, "detail": "fine dell'ultimo intervento più rientro al deposito oltre la fine giornata"},
        {"code": "sla", "label": "Extra gestiti entro lo SLA", "unit": "%", "baseline": pct(b.sla_ok, b.sla_ok + b.sla_late), "pilot": sla, "target": tget("sla_pct"), "ok": verdict(sla, tget("sla_pct")),
         "detail": format!("richieste extra delle squadre del pilota: arrivo sul posto entro lo SLA del contratto (senza contratto {sla_def} ore)")},
        {"code": "hours", "label": "Ore di pianificazione risparmiate a settimana", "unit": "h", "baseline": base_h, "pilot": if qh.is_empty() { None } else { Some((qh.iter().sum::<f64>() / qh.len() as f64 * 10.0).round() / 10.0) },
         "change": saved, "target": tget("hours_saved"), "ok": verdict(saved, tget("hours_saved")), "detail": "dichiarate dal pianificatore ogni settimana, contro quelle prima del pilota"}
    ]);
    Ok(json!({"id": id, "name": p.get::<String,_>("name"), "teams": names, "group_ids": teams, "starts_on": start, "weeks": weeks, "baseline_weeks": bweeks,
        "default_sla_hours": sla_def, "baseline_planning_hours": base_h, "targets": targets, "status": p.get::<String,_>("status"),
        "decision_note": p.try_get::<Option<String>,_>("decision_note").ok().flatten(), "created_by": p.try_get::<Option<String>,_>("email").ok().flatten(),
        "closed_at": p.try_get::<Option<DateTime<Utc>>,_>("closed_at").ok().flatten(),
        "current_week": if today < start { 0 } else { ((today - start).num_days() / 7 + 1).min(weeks as i64) }, "weeks_detail": out, "measures": measures}))
}

/// GET /api/planning/pilot — il pilota attivo (o l'ultimo) con le misure settimana per settimana.
pub async fn pilot_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_any_cap(&s, &["plan", "orders.view_all"])?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.planning_pilots ORDER BY (status='attivo') DESC, created_at DESC LIMIT 1").fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    match id { None => Ok(Json(json!({"pilot": null}))), Some(id) => Ok(Json(json!({"pilot": pilot_report(&st, tid, id).await?}))) }
}

#[derive(Deserialize)]
pub struct PilotReq { pub name: String, pub group_ids: Vec<Uuid>, pub starts_on: NaiveDate, #[serde(default)] pub weeks: Option<i32>, #[serde(default)] pub baseline_weeks: Option<i32>,
    #[serde(default)] pub default_sla_hours: Option<i32>, #[serde(default)] pub baseline_planning_hours: Option<f64>, #[serde(default)] pub targets: Option<Value> }
fn validate(b: &PilotReq) -> Result<Value, (StatusCode, String)> {
    if b.name.trim().chars().count() < 2 { return Err(err(StatusCode::BAD_REQUEST, "nome del pilota richiesto")); }
    if b.group_ids.is_empty() || b.group_ids.len() > 50 { return Err(err(StatusCode::BAD_REQUEST, "scegli da 1 a 50 squadre")); }
    if b.weeks.map(|w| !(1..=26).contains(&w)).unwrap_or(false) || b.baseline_weeks.map(|w| !(1..=26).contains(&w)).unwrap_or(false) { return Err(err(StatusCode::BAD_REQUEST, "settimane tra 1 e 26")); }
    if b.default_sla_hours.map(|h| !(1..=720).contains(&h)).unwrap_or(false) { return Err(err(StatusCode::BAD_REQUEST, "SLA tra 1 e 720 ore")); }
    if b.baseline_planning_hours.map(|h| !(0.0..=168.0).contains(&h)).unwrap_or(false) { return Err(err(StatusCode::BAD_REQUEST, "ore di pianificazione tra 0 e 168")); }
    let mut t = json!({"accepted_pct": 70, "travel_reduction_pct": 10, "sla_pct": 90, "hours_saved": 4});
    if let Some(Value::Object(m)) = &b.targets { for (k, v) in m {
        if !["accepted_pct", "travel_reduction_pct", "sla_pct", "hours_saved"].contains(&k.as_str()) || !v.is_number() { return Err(err(StatusCode::BAD_REQUEST, "obiettivi: accepted_pct, travel_reduction_pct, sla_pct, hours_saved (numeri)")); }
        t[k] = v.clone(); } }
    Ok(t)
}
pub async fn pilot_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PilotReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let t = validate(&b)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM app.user_groups WHERE id = ANY($1) AND kind='squadra'").bind(&b.group_ids).fetch_one(&mut *tx).await.map_err(ise)?;
    if n != b.group_ids.len() as i64 { return Err(err(StatusCode::BAD_REQUEST, "squadra inesistente")); }
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.planning_pilots(tenant_id,name,group_ids,starts_on,weeks,baseline_weeks,default_sla_hours,baseline_planning_hours,targets,created_by)
            VALUES($1,$2,$3,$4,COALESCE($5,4),COALESCE($6,4),COALESCE($7,48),$8::numeric,$9,$10) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.group_ids).bind(monday(b.starts_on)).bind(b.weeks).bind(b.baseline_weeks).bind(b.default_sla_hours)
        .bind(b.baseline_planning_hours).bind(&t).bind(s.user_id).fetch_one(&mut *tx).await
        .map_err(|e| crate::api::conflict_or(e, "c'è già un pilota in corso: concludilo prima di avviarne un altro"))?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "planning_pilot_create", json!({"id": id})).await;
    Ok(Json(json!({"pilot": pilot_report(&st, tid, id).await?})))
}
pub async fn pilot_update(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PilotReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let t = validate(&b)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.planning_pilots SET name=$2, group_ids=$3, starts_on=$4, weeks=COALESCE($5,weeks), baseline_weeks=COALESCE($6,baseline_weeks),
            default_sla_hours=COALESCE($7,default_sla_hours), baseline_planning_hours=$8::numeric, targets=$9 WHERE id=$1 AND status='attivo'")
        .bind(id).bind(b.name.trim()).bind(&b.group_ids).bind(monday(b.starts_on)).bind(b.weeks).bind(b.baseline_weeks).bind(b.default_sla_hours)
        .bind(b.baseline_planning_hours).bind(&t).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::CONFLICT, "pilota inesistente o già chiuso")); }
    Ok(Json(json!({"pilot": pilot_report(&st, tid, id).await?})))
}
#[derive(Deserialize)]
pub struct WeekReq { pub week_start: NaiveDate, pub planning_hours: f64, #[serde(default)] pub note: Option<String> }
/// POST /api/planning/pilot/:id/weeks — ore dedicate alla pianificazione in una settimana del pilota.
pub async fn pilot_week(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<WeekReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    if !(0.0..=168.0).contains(&b.planning_hours) { return Err(err(StatusCode::BAD_REQUEST, "ore tra 0 e 168")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let p = sqlx::query("SELECT starts_on, weeks FROM app.planning_pilots WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "pilota inesistente"))?;
    let (s0, w): (NaiveDate, i32) = (p.get("starts_on"), p.get("weeks"));
    let wk = monday(b.week_start);
    if wk < s0 || wk >= s0 + Duration::days(7 * w as i64) { return Err(err(StatusCode::BAD_REQUEST, "la settimana non fa parte del pilota")); }
    sqlx::query("INSERT INTO app.planning_pilot_weeks(pilot_id,tenant_id,week_start,planning_hours,note,updated_by) VALUES($1,$2,$3,$4::numeric,$5,$6)
         ON CONFLICT (pilot_id,week_start) DO UPDATE SET planning_hours=EXCLUDED.planning_hours, note=EXCLUDED.note, updated_by=EXCLUDED.updated_by, updated_at=now()")
        .bind(id).bind(tid).bind(wk).bind(b.planning_hours).bind(b.note.as_deref().map(str::trim).filter(|x| !x.is_empty())).bind(s.user_id)
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok": true})))
}
#[derive(Deserialize)]
pub struct CloseReq { pub decision: String, #[serde(default)] pub note: Option<String> }
/// POST /api/planning/pilot/:id/close — decisione finale: «estendi» a tutte le squadre o «concludi».
pub async fn pilot_close(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<CloseReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let status = match b.decision.as_str() { "estendi" => "esteso", "concludi" => "concluso", _ => return Err(err(StatusCode::BAD_REQUEST, "decisione: estendi o concludi")) };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.planning_pilots SET status=$2, decision_note=$3, closed_at=now(), closed_by=$4 WHERE id=$1 AND status='attivo'")
        .bind(id).bind(status).bind(b.note.as_deref().map(str::trim).filter(|x| !x.is_empty())).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::CONFLICT, "pilota inesistente o già chiuso")); }
    audit(&st, tid, &s.email, "planning_pilot_close", json!({"id": id, "decision": b.decision})).await;
    Ok(Json(json!({"pilot": pilot_report(&st, tid, id).await?})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn mondays() {
        let d = NaiveDate::from_ymd_opt(2026, 12, 3).unwrap(); // giovedì
        assert_eq!(monday(d), NaiveDate::from_ymd_opt(2026, 11, 30).unwrap());
        assert_eq!(pct(7, 10), Some(70.0)); assert_eq!(pct(1, 0), None);
    }
    #[test] fn route_overtime() {
        let s = Settings::default(); let tz: chrono_tz::Tz = "Europe/Rome".parse().unwrap();
        let t = |h: u32, m: u32| tz.with_ymd_and_hms(2026, 12, 1, h, m, 0).unwrap().with_timezone(&Utc);
        // un intervento che finisce alle 16:20 a ~10 km dal deposito: rientro ~30 min → ~20 min di straordinario
        let (tm, ot) = day_route(&s, tz, Some((45.50, 9.13)), vec![(t(14, 0), t(16, 20), Some((45.46, 9.19)))]);
        assert!(tm > 40 && tm < 80, "{tm}"); assert!(ot > 0 && ot < 40, "{ot}");
    }
}
