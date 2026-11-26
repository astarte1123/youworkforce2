//! Fase 3 «Analisi e stime» (piano Idealverde, mig 0167, modulo `analisi`). Tutto calcolato dai dati
//! di YW, in modo deterministico e spiegabile (nessun modello opaco):
//! - DURATE REALI per tipo di attività (e per sede o squadra): quanti, media, mediana, 80° percentile,
//!   scostamento dalla durata standard del catalogo;
//! - STIMA DALLO STORICO per un nuovo intervento: interventi simili nella stessa sede, poi della stessa
//!   squadra, poi del tipo (scalata sulla quantità quando c'è), poi la durata standard; dice sempre su
//!   quanti interventi si basa;
//! - PRECISIONE della stima, misurata sul passato: per ogni intervento chiuso si calcola la stima con i
//!   soli dati precedenti e la si confronta con il tempo reale e con la durata standard;
//! - RICORRENZE reali dei piani di manutenzione contro la frequenza a contratto;
//! - EXTRA: volume per mese, cliente e sede, e richieste in arrivo;
//! - PIANO CONTRO REALTÀ per settimana.

use axum::{extract::{State, Query}, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Datelike, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_any_cap, require_module, ise, err, parse_ts};
use crate::db;

const MIN_SITE: usize = 3;   // interventi simili nella stessa sede per usarli
const MIN_TEAM: usize = 5;
const MIN_TYPE: usize = 5;

fn median(v: &mut Vec<f64>) -> f64 { pct(v, 0.5) }
fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() { return 0.0; }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let k = (v.len() as f64 - 1.0) * p; let (lo, hi) = (k.floor() as usize, k.ceil() as usize);
    v[lo] + (v[hi] - v[lo]) * (k - lo as f64)
}

/// Intervento chiuso con il tempo reale (base per stime e precisione).
#[derive(Clone)]
pub(crate) struct Done { pub at: Uuid, pub site: Option<Uuid>, pub team: Option<Uuid>, pub qty: Option<f64>, pub minutes: f64, pub when: DateTime<Utc> }

pub(crate) async fn load_done(st: &AppState, tid: Uuid) -> Result<Vec<Done>, (StatusCode, String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT activity_type_id, site_id, assigned_group, quantity::float8 AS qty, work_minutes,
            COALESCE(work_started_at, scheduled_start, updated_at) AS t
         FROM app.work_orders WHERE state='completato' AND activity_type_id IS NOT NULL AND work_minutes > 0
         ORDER BY 6 LIMIT 50000").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(rows.iter().map(|r| Done { at: r.get("activity_type_id"), site: r.try_get("site_id").ok(), team: r.try_get("assigned_group").ok(),
        qty: r.try_get::<Option<f64>,_>("qty").ok().flatten(), minutes: r.get::<i32,_>("work_minutes") as f64, when: r.get("t") }).collect())
}

/// Stima dallo storico. `hist` = interventi chiusi già noti (per la precisione: solo i precedenti).
/// Restituisce (minuti, base della stima, quanti interventi, 80° percentile).
pub(crate) fn estimate_from(hist: &[&Done], at: Uuid, site: Option<Uuid>, team: Option<Uuid>, qty: Option<f64>) -> Option<(f64, &'static str, usize, f64)> {
    let of_type: Vec<&&Done> = hist.iter().filter(|d| d.at == at).collect();
    // quantità: scala la mediana sul rapporto tra la quantità chiesta e quella tipica (tra 0,5× e 2×)
    let scale = |set: &[&&Done], m: f64| -> f64 {
        match qty { Some(q) if q > 0.0 => {
            let mut qs: Vec<f64> = set.iter().filter_map(|d| d.qty).filter(|x| *x > 0.0).collect();
            if qs.len() >= 3 { let mq = median(&mut qs); if mq > 0.0 { return m * (q / mq).clamp(0.5, 2.0); } }
            m }
          _ => m }
    };
    let pick = |set: Vec<&&Done>, basis: &'static str| -> (f64, &'static str, usize, f64) {
        let mut mins: Vec<f64> = set.iter().map(|d| d.minutes).collect(); let n = mins.len();
        let m = median(&mut mins); let p80 = pct(&mut mins, 0.8);
        (scale(&set, m), basis, n, scale(&set, p80))
    };
    if let Some(sid) = site {
        let s: Vec<&&Done> = of_type.iter().filter(|d| d.site == Some(sid)).cloned().collect();
        if s.len() >= MIN_SITE { return Some(pick(s, "sede")); }
    }
    if let Some(g) = team {
        let s: Vec<&&Done> = of_type.iter().filter(|d| d.team == Some(g)).cloned().collect();
        if s.len() >= MIN_TEAM { return Some(pick(s, "squadra")); }
    }
    if of_type.len() >= MIN_TYPE { return Some(pick(of_type, "tipo")); }
    None
}

#[derive(Deserialize)]
pub struct EstimateQ { pub activity_type_id: Uuid, #[serde(default)] pub site_id: Option<Uuid>, #[serde(default)] pub group_id: Option<Uuid>, #[serde(default)] pub quantity: Option<f64> }
/// GET /api/analytics/estimate — durata stimata dallo storico, con la sua base in chiaro.
pub async fn analytics_estimate(State(st): State<AppState>, h: HeaderMap, Query(q): Query<EstimateQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "analisi").await?; require_any_cap(&s, &["plan", "orders.view_all", "costs.view"])?;
    Ok(Json(history_estimate(&st, tid, q.activity_type_id, q.site_id, q.group_id, q.quantity).await?))
}
pub(crate) async fn history_estimate(st: &AppState, tid: Uuid, at: Uuid, site: Option<Uuid>, team: Option<Uuid>, qty: Option<f64>) -> Result<Value, (StatusCode, String)> {
    let done = load_done(st, tid).await?;
    let refs: Vec<&Done> = done.iter().collect();
    let standard = crate::constraints::activity_estimate(st, tid, Some(at), qty).await?;
    Ok(match estimate_from(&refs, at, site, team, qty) {
        Some((m, basis, n, p80)) => json!({"minutes": m.round() as i64, "p80_minutes": p80.round() as i64, "basis": basis, "count": n,
            "standard_minutes": standard,
            "explanation": format!("{} {} di questo tipo {}", n, if n == 1 { "intervento" } else { "interventi" },
                match basis { "sede" => "in questa sede", "squadra" => "della stessa squadra", _ => "in tutte le sedi" })}),
        None => json!({"minutes": standard, "basis": "standard", "count": 0, "standard_minutes": standard,
            "explanation": "storico insufficiente: si usa la durata standard del tipo di attività"}),
    })
}

#[derive(Deserialize)]
pub struct RangeQ { #[serde(default)] pub from: Option<String>, #[serde(default)] pub to: Option<String>,
    #[serde(default)] pub by: Option<String>, #[serde(default)] pub months: Option<i64>, #[serde(default)] pub weeks: Option<i64>, #[serde(default)] pub days: Option<i64> }

/// GET /api/analytics/durations?by=type|site|team&from&to — durate reali per tipo di attività.
pub async fn analytics_durations(State(st): State<AppState>, h: HeaderMap, Query(q): Query<RangeQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "analisi").await?; require_any_cap(&s, &["plan", "orders.view_all", "costs.view"])?;
    let now = st.clock.now();
    let from = q.from.as_deref().map(parse_ts).transpose()?.unwrap_or(now - chrono::Duration::days(365));
    let to = q.to.as_deref().map(parse_ts).transpose()?.unwrap_or(now);
    let by = q.by.as_deref().unwrap_or("type");
    let (key, join, label) = match by {
        "type" => ("NULL::uuid", "", "NULL::text"),
        "site" => ("o.site_id", "LEFT JOIN app.sites x ON x.id=o.site_id", "x.name"),
        "team" => ("o.assigned_group", "LEFT JOIN app.user_groups x ON x.id=o.assigned_group", "x.name"),
        _ => return Err(err(StatusCode::BAD_REQUEST, "by: type, site o team")),
    };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(&format!("SELECT a.id AS at, a.name AS at_name, {key} AS k, {label} AS k_name, count(*)::int AS n,
            round(avg(o.work_minutes))::int AS avg_min,
            percentile_cont(0.5) WITHIN GROUP (ORDER BY o.work_minutes)::float8 AS med,
            percentile_cont(0.8) WITHIN GROUP (ORDER BY o.work_minutes)::float8 AS p80,
            round(avg(COALESCE(o.estimated_minutes, a.base_minutes + a.minutes_per_unit*COALESCE(o.quantity,0))))::int AS std_min
         FROM app.work_orders o JOIN app.activity_types a ON a.id=o.activity_type_id {join}
         WHERE o.state='completato' AND o.work_minutes > 0 AND COALESCE(o.work_started_at,o.scheduled_start,o.updated_at) BETWEEN $1 AND $2
         GROUP BY a.id, a.name, {key}, {label} HAVING count(*) >= 1 ORDER BY a.name, n DESC"))
        .bind(from).bind(to).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"by": by, "from": from, "to": to, "rows": rows.iter().map(|r| {
        let (med, std): (f64, i32) = (r.get("med"), r.try_get("std_min").unwrap_or(0));
        json!({"activity_type_id": r.get::<Uuid,_>("at"), "activity_type": r.get::<String,_>("at_name"),
            "key_id": r.try_get::<Option<Uuid>,_>("k").ok().flatten(), "key": r.try_get::<Option<String>,_>("k_name").ok().flatten(),
            "count": r.get::<i32,_>("n"), "avg_minutes": r.get::<i32,_>("avg_min"), "median_minutes": med.round() as i64,
            "p80_minutes": r.get::<f64,_>("p80").round() as i64, "standard_minutes": std,
            "deviation_pct": if std > 0 { Some(((med - std as f64) / std as f64 * 100.0).round() as i64) } else { None }})
    }).collect::<Vec<_>>()})))
}

/// GET /api/analytics/accuracy?days=90 — quanto sbaglia la stima dallo storico rispetto alla durata
/// standard, sugli interventi chiusi del periodo (ognuno stimato con i soli dati precedenti).
pub async fn analytics_accuracy(State(st): State<AppState>, h: HeaderMap, Query(q): Query<RangeQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "analisi").await?; require_any_cap(&s, &["plan", "orders.view_all", "costs.view"])?;
    let days = q.days.unwrap_or(90).clamp(7, 730);
    let since = st.clock.now() - chrono::Duration::days(days);
    let done = load_done(&st, tid).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let types: HashMap<Uuid, (String, i32, f64)> = sqlx::query("SELECT id,name,base_minutes,minutes_per_unit::float8 AS mpu FROM app.activity_types")
        .fetch_all(&mut *tx).await.map_err(ise)?.iter().map(|r| (r.get("id"), (r.get("name"), r.get("base_minutes"), r.get("mpu")))).collect();
    tx.commit().await.map_err(ise)?;
    let (mut n, mut e_hist, mut e_std, mut in20_h, mut in20_s) = (0usize, 0f64, 0f64, 0usize, 0usize);
    let mut per: HashMap<Uuid, (usize, f64, f64)> = HashMap::new();
    for (i, d) in done.iter().enumerate() {
        if d.when < since { continue; }
        let Some((_, base, mpu)) = types.get(&d.at) else { continue };
        let prev: Vec<&Done> = done[..i].iter().collect();
        let std = crate::constraints::estimate_minutes(*base, *mpu, d.qty) as f64;
        let hist = estimate_from(&prev, d.at, d.site, d.team, d.qty).map(|x| x.0).unwrap_or(std);
        let (eh, es) = ((hist - d.minutes).abs(), (std - d.minutes).abs());
        n += 1; e_hist += eh; e_std += es;
        if eh <= 0.2 * d.minutes { in20_h += 1; } if es <= 0.2 * d.minutes { in20_s += 1; }
        let p = per.entry(d.at).or_insert((0, 0.0, 0.0)); p.0 += 1; p.1 += eh; p.2 += es;
    }
    let f = |x: f64| if n > 0 { (x / n as f64).round() as i64 } else { 0 };
    let pctn = |x: usize| if n > 0 { (x as f64 * 100.0 / n as f64).round() as i64 } else { 0 };
    let mut by_type: Vec<Value> = per.iter().map(|(k, (c, h, s))| json!({"activity_type_id": k, "activity_type": types.get(k).map(|t| t.0.clone()),
        "count": c, "history_mae_minutes": (h / *c as f64).round() as i64, "standard_mae_minutes": (s / *c as f64).round() as i64})).collect();
    by_type.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    Ok(Json(json!({"days": days, "count": n, "history_mae_minutes": f(e_hist), "standard_mae_minutes": f(e_std),
        "history_within_20pct": pctn(in20_h), "standard_within_20pct": pctn(in20_s), "by_type": by_type,
        "verdict": if n == 0 { "nessun intervento chiuso nel periodo".to_string() } else if e_hist < e_std {
            format!("la stima dallo storico sbaglia in media {} min, la durata standard {} min", f(e_hist), f(e_std)) }
            else { format!("la durata standard è ancora più precisa (in media {} min contro {} min dallo storico)", f(e_std), f(e_hist)) }})))
}

/// GET /api/analytics/extras?months=12 — lavori extra (fuori programma) e richieste per mese,
/// i clienti e le sedi che ne generano di più.
pub async fn analytics_extras(State(st): State<AppState>, h: HeaderMap, Query(q): Query<RangeQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "analisi").await?; require_any_cap(&s, &["plan", "orders.view_all", "costs.view"])?;
    let months = q.months.unwrap_or(12).clamp(1, 36);
    let tz = crate::field::tenant_tz(&st, tid).await;
    let since = st.clock.now() - chrono::Duration::days(31 * months);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let tzs = tz.name().to_string();
    let monthly = sqlx::query("SELECT to_char(date_trunc('month', COALESCE(o.scheduled_start,o.created_at) AT TIME ZONE $2),'YYYY-MM') AS m,
            count(*) FILTER (WHERE o.is_extra)::int AS extra, count(*)::int AS total,
            COALESCE(sum(o.work_minutes) FILTER (WHERE o.is_extra),0)::int AS extra_min
         FROM app.work_orders o WHERE COALESCE(o.scheduled_start,o.created_at) >= $1 AND COALESCE(o.scheduled_start,o.created_at) <= $3
           AND o.state<>'annullato' GROUP BY 1 ORDER BY 1")
        .bind(since).bind(&tzs).bind(st.clock.now()).fetch_all(&mut *tx).await.map_err(ise)?;
    let reqs: HashMap<String, i32> = sqlx::query("SELECT to_char(date_trunc('month', created_at AT TIME ZONE $2),'YYYY-MM') AS m, count(*)::int AS n
         FROM app.service_requests WHERE created_at >= $1 GROUP BY 1").bind(since).bind(&tzs)
        .fetch_all(&mut *tx).await.map_err(ise)?.iter().map(|r| (r.get("m"), r.get("n"))).collect();
    let top_clients = sqlx::query("SELECT c.name, count(*)::int AS n, COALESCE(sum(o.work_minutes),0)::int AS min FROM app.work_orders o JOIN app.clients c ON c.id=o.client_id
         WHERE o.is_extra AND COALESCE(o.scheduled_start,o.created_at) BETWEEN $1 AND $2 GROUP BY c.name ORDER BY n DESC, c.name LIMIT 10")
        .bind(since).bind(st.clock.now()).fetch_all(&mut *tx).await.map_err(ise)?;
    let top_sites = sqlx::query("SELECT s.name, count(*)::int AS n FROM app.work_orders o JOIN app.sites s ON s.id=o.site_id
         WHERE o.is_extra AND COALESCE(o.scheduled_start,o.created_at) BETWEEN $1 AND $2 GROUP BY s.name ORDER BY n DESC, s.name LIMIT 10")
        .bind(since).bind(st.clock.now()).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // serie completa dei mesi (anche quelli con sole richieste o senza nulla), dal più vecchio a oggi
    let mut series: Vec<String> = vec![];
    let today = st.clock.now().with_timezone(&tz).date_naive();
    let mut d = today.with_day(1).unwrap();
    for _ in 0..months { series.push(d.format("%Y-%m").to_string()); d = (d - chrono::Duration::days(1)).with_day(1).unwrap(); }
    series.reverse();
    let by_m: HashMap<String, (i32, i32, i32)> = monthly.iter().map(|r| (r.get::<String,_>("m"), (r.get("extra"), r.get("total"), r.get("extra_min")))).collect();
    let tot_extra: i32 = monthly.iter().map(|r| r.get::<i32,_>("extra")).sum();
    let tot: i32 = monthly.iter().map(|r| r.get::<i32,_>("total")).sum();
    Ok(Json(json!({"months": months, "extra": tot_extra, "total": tot,
        "extra_pct": if tot > 0 { (tot_extra as f64 * 100.0 / tot as f64).round() as i64 } else { 0 },
        "by_month": series.iter().map(|m| { let (e, t, em) = by_m.get(m).cloned().unwrap_or((0, 0, 0));
            json!({"month": m, "extra": e, "total": t, "extra_minutes": em, "requests": reqs.get(m).cloned().unwrap_or(0)}) }).collect::<Vec<_>>(),
        "top_clients": top_clients.iter().map(|r| json!({"name": r.get::<String,_>("name"), "count": r.get::<i32,_>("n"), "minutes": r.get::<i32,_>("min")})).collect::<Vec<_>>(),
        "top_sites": top_sites.iter().map(|r| json!({"name": r.get::<String,_>("name"), "count": r.get::<i32,_>("n")})).collect::<Vec<_>>()})))
}

/// GET /api/analytics/recurrences — ogni piano di manutenzione attivo: frequenza a contratto contro
/// intervallo reale tra gli interventi chiusi dello stesso tipo nella stessa sede (ultimo anno).
pub async fn analytics_recurrences(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "analisi").await?; require_any_cap(&s, &["plan", "orders.view_all", "costs.view"])?;
    let now = st.clock.now();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let plans = sqlx::query("SELECT p.id,p.title,p.every_days,p.next_date,p.site_id,p.activity_type_id,s.name AS site,c.name AS client
         FROM app.maintenance_plans p LEFT JOIN app.sites s ON s.id=p.site_id LEFT JOIN app.clients c ON c.id=p.client_id
         WHERE p.active AND p.site_id IS NOT NULL AND p.activity_type_id IS NOT NULL ORDER BY c.name, p.title").fetch_all(&mut *tx).await.map_err(ise)?;
    let done = sqlx::query("SELECT site_id, activity_type_id, COALESCE(work_started_at,scheduled_start) AS t FROM app.work_orders
         WHERE state='completato' AND activity_type_id IS NOT NULL AND site_id IS NOT NULL AND COALESCE(work_started_at,scheduled_start) >= $1 ORDER BY 3")
        .bind(now - chrono::Duration::days(365)).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut by: HashMap<(Uuid, Uuid), Vec<DateTime<Utc>>> = HashMap::new();
    for r in &done { by.entry((r.get("site_id"), r.get("activity_type_id"))).or_default().push(r.get("t")); }
    let out: Vec<Value> = plans.iter().map(|p| {
        let every: i32 = p.get("every_days");
        let ts = by.get(&(p.get("site_id"), p.get("activity_type_id"))).cloned().unwrap_or_default();
        let gaps: Vec<f64> = ts.windows(2).map(|w| (w[1] - w[0]).num_hours() as f64 / 24.0).filter(|g| *g >= 1.0).collect();
        let avg = if gaps.is_empty() { None } else { Some(gaps.iter().sum::<f64>() / gaps.len() as f64) };
        let last = ts.last().cloned();
        let since_last = last.map(|l| (now - l).num_days());
        let status = match (avg, since_last) {
            (_, None) => "nessun intervento svolto",
            (_, Some(d)) if d as f64 > every as f64 * 1.5 => "in ritardo",
            (Some(a), _) if a > every as f64 * 1.25 => "meno frequente del contratto",
            (Some(a), _) if a < every as f64 * 0.75 => "più frequente del contratto",
            (None, _) => "un solo intervento",
            _ => "in linea" };
        json!({"plan_id": p.get::<Uuid,_>("id"), "title": p.get::<String,_>("title"), "client": p.try_get::<Option<String>,_>("client").ok().flatten(),
            "site": p.try_get::<Option<String>,_>("site").ok().flatten(), "every_days": every, "done_count": ts.len(),
            "actual_avg_days": avg.map(|a| (a * 10.0).round() / 10.0), "last_done": last, "days_since_last": since_last,
            "next_date": p.get::<chrono::NaiveDate,_>("next_date").to_string(), "status": status})
    }).collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for o in &out { *counts.entry(o["status"].as_str().unwrap_or("").to_string()).or_default() += 1; }
    Ok(Json(json!({"plans": out, "summary": counts})))
}

/// GET /api/analytics/plan-vs-actual?weeks=8 — per settimana: in programma, chiusi, annullati,
/// scaduti aperti, minuti pianificati e lavorati, avviati il giorno previsto.
pub async fn analytics_plan_vs_actual(State(st): State<AppState>, h: HeaderMap, Query(q): Query<RangeQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "analisi").await?; require_any_cap(&s, &["plan", "orders.view_all", "costs.view"])?;
    let weeks = q.weeks.unwrap_or(8).clamp(1, 52);
    let tz = crate::field::tenant_tz(&st, tid).await;
    let now = st.clock.now();
    let since = now - chrono::Duration::weeks(weeks);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT scheduled_start, scheduled_end, work_started_at, work_minutes, state, estimated_minutes FROM app.work_orders
         WHERE scheduled_start >= $1 AND scheduled_start < $2").bind(since).bind(now).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    #[derive(Default)] struct W { sched: i64, done: i64, canc: i64, open_late: i64, plan_min: i64, work_min: i64, same_day: i64 }
    let mut ws: std::collections::BTreeMap<String, W> = Default::default();
    for r in &rows {
        let s: DateTime<Utc> = r.get("scheduled_start");
        let ls = s.with_timezone(&tz); let iw = ls.iso_week();
        let w = ws.entry(format!("{}-W{:02}", iw.year(), iw.week())).or_default();
        w.sched += 1;
        let state: String = r.get("state");
        let pm = r.try_get::<Option<i32>,_>("estimated_minutes").ok().flatten().map(|x| x as i64)
            .or_else(|| r.try_get::<Option<DateTime<Utc>>,_>("scheduled_end").ok().flatten().map(|e| (e - s).num_minutes()));
        w.plan_min += pm.unwrap_or(0);
        match state.as_str() {
            "completato" => { w.done += 1; w.work_min += r.try_get::<Option<i32>,_>("work_minutes").ok().flatten().unwrap_or(0) as i64;
                if let Some(ws_) = r.try_get::<Option<DateTime<Utc>>,_>("work_started_at").ok().flatten() {
                    if ws_.with_timezone(&tz).date_naive() == ls.date_naive() { w.same_day += 1; } } }
            "annullato" => w.canc += 1,
            _ => w.open_late += 1,
        }
    }
    Ok(Json(json!({"weeks": ws.iter().map(|(k, w)| json!({"week": k, "scheduled": w.sched, "completed": w.done, "cancelled": w.canc,
        "open_overdue": w.open_late, "planned_minutes": w.plan_min, "worked_minutes": w.work_min,
        "on_day_pct": if w.done > 0 { (w.same_day as f64 * 100.0 / w.done as f64).round() as i64 } else { 0 }})).collect::<Vec<_>>()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(at: Uuid, site: Option<Uuid>, m: f64, q: Option<f64>) -> Done { Done { at, site, team: None, qty: q, minutes: m, when: Utc::now() } }
    #[test] fn site_history_wins_when_enough() {
        let (at, s1, s2) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let v = vec![d(at, Some(s1), 200.0, None), d(at, Some(s1), 210.0, None), d(at, Some(s1), 190.0, None),
                     d(at, Some(s2), 60.0, None), d(at, Some(s2), 70.0, None), d(at, Some(s2), 65.0, None)];
        let r: Vec<&Done> = v.iter().collect();
        let (m, basis, n, _) = estimate_from(&r, at, Some(s1), None, None).unwrap();
        assert_eq!((m as i64, basis, n), (200, "sede", 3));
        // sede nuova: tutte le sedi del tipo
        let (_, basis, n, _) = estimate_from(&r, at, Some(Uuid::new_v4()), None, None).unwrap();
        assert_eq!((basis, n), ("tipo", 6));
    }
    #[test] fn quantity_scales_and_is_capped() {
        let (at, s) = (Uuid::new_v4(), Uuid::new_v4());
        let v: Vec<Done> = (0..5).map(|_| d(at, Some(s), 100.0, Some(1000.0))).collect();
        let r: Vec<&Done> = v.iter().collect();
        assert_eq!(estimate_from(&r, at, Some(s), None, Some(1500.0)).unwrap().0 as i64, 150);
        assert_eq!(estimate_from(&r, at, Some(s), None, Some(9000.0)).unwrap().0 as i64, 200); // massimo 2×
    }
    #[test] fn too_little_history_gives_none() {
        let at = Uuid::new_v4();
        let v = vec![d(at, None, 100.0, None)];
        let r: Vec<&Done> = v.iter().collect();
        assert!(estimate_from(&r, at, None, None, None).is_none());
    }
}
