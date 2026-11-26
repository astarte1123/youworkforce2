//! Fase 5 «Pianificazione automatica» (piano Idealverde, mig 0170, modulo `pianificazione`).
//! Decisioni PO: YW espone il problema (contratto dati `yw-planning/1`), valida OGNI proposta con gli
//! stessi vincoli della pianificazione a mano e ha un motore di base nativo; un motore esterno (Youco)
//! propone sullo stesso contratto. Il motore propone, la persona decide: la proposta è una bozza.
//!
//! Motore di base (`yw-base-1`), deterministico e spiegabile:
//! 1. interventi da pianificare in ordine di priorità, data voluta, durata;
//! 2. per ognuno, ogni squadra idonea (persone presenti, patentini validi quel giorno, anche per i mezzi)
//!    × ogni giorno lavorativo × ogni posizione nella giornata della squadra: orari ricalcolati dal
//!    deposito con i tempi di spostamento, finestre orarie della sede, fine giornata (+ straordinario
//!    ammesso), mezzi del tipo richiesto liberi; si sceglie il costo minore (minuti di spostamento in
//!    più + spostamento dalla data voluta + cambio di squadra + meteo sfavorevole + straordinario);
//! 3. miglioramento: ogni intervento si toglie e si reinserisce dove costa meno, finché migliora.
//! Tempi di spostamento: distanza in linea d'aria × fattore di percorso / velocità media + minuti di
//! sistemazione (decisione 3, nessun servizio esterno).

use axum::{extract::{State, Path, Query}, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, require_any_cap, require_module, module_active, ise, err, audit};
use crate::constraints::{check_order, OrderPlan, book_order_assets, move_order_bookings};
use crate::field::parse_ts;
use crate::weather::{WSlot, WLimits, weather_issue, load_weather};
use crate::db;

pub(crate) const ENGINE: &str = "yw-base-1";
const CONTRACT: &str = "yw-planning/1";

// =============================================================================================
// Impostazioni
// =============================================================================================
#[derive(Clone, Debug)]
pub(crate) struct Settings { pub depot: Option<Uuid>, pub day_start: NaiveTime, pub day_end: NaiveTime, pub workdays: [bool; 7],
    pub kmh: f64, pub detour: f64, pub setup: i64, pub overtime: i64, pub weather_avoid: bool }
impl Default for Settings {
    fn default() -> Self { Settings { depot: None, day_start: NaiveTime::from_hms_opt(7, 30, 0).unwrap(), day_end: NaiveTime::from_hms_opt(16, 30, 0).unwrap(),
        workdays: [true, true, true, true, true, false, false], kmh: 25.0, detour: 1.3, setup: 5, overtime: 0, weather_avoid: true } }
}
fn wd_mask(s: &str) -> [bool; 7] { let mut m = [false; 7]; for (i, c) in s.chars().take(7).enumerate() { m[i] = c == '1'; } m }
fn mask_str(m: &[bool; 7]) -> String { m.iter().map(|b| if *b { '1' } else { '0' }).collect() }

pub(crate) async fn load_settings(st: &AppState, tid: Uuid) -> Result<Settings, (StatusCode, String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = sqlx::query("SELECT depot_site_id, day_start, day_end, workdays, travel_kmh, detour_factor::float8 AS detour, setup_minutes, max_overtime_minutes, weather_policy
         FROM app.planning_settings WHERE tenant_id=$1").bind(tid).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(match r { None => Settings::default(), Some(r) => Settings {
        depot: r.try_get("depot_site_id").ok().flatten(), day_start: r.get("day_start"), day_end: r.get("day_end"),
        workdays: wd_mask(&r.get::<String,_>("workdays")), kmh: r.get::<i32,_>("travel_kmh") as f64, detour: r.get("detour"),
        setup: r.get::<i32,_>("setup_minutes") as i64, overtime: r.get::<i32,_>("max_overtime_minutes") as i64,
        weather_avoid: r.get::<String,_>("weather_policy") == "evita" } })
}

/// Minuti di spostamento tra due punti: linea d'aria × fattore / velocità + sistemazione (0 se stesso posto).
pub(crate) fn travel_min(s: &Settings, a: (f64, f64), b: (f64, f64)) -> (i64, f64) {
    let km = haversine_km(a, b) * s.detour;
    if km < 0.05 { return (0, 0.0); }
    (((km / s.kmh) * 60.0).ceil() as i64 + s.setup, km)
}
pub(crate) fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    6371.0 * 2.0 * h.sqrt().asin()
}

// =============================================================================================
// Il problema
// =============================================================================================
#[derive(Clone, Debug)]
pub(crate) struct Team { pub id: Uuid, pub name: String, pub depot: Option<(f64, f64)>, pub members: Vec<Uuid> }
#[derive(Clone, Debug)]
pub(crate) struct Ord {
    pub id: Uuid, pub version: i32, pub title: String, pub site: Option<Uuid>, pub site_name: String, pub loc: Option<(f64, f64)>,
    pub windows: Value, pub at: Option<Uuid>, pub dur: i64, pub crew: usize, pub quals: Vec<(Uuid, String)>, pub kinds: Vec<(String, usize)>,
    pub priority: i32, pub pref: Option<DateTime<Utc>>, pub prev_group: Option<Uuid>, pub prev_user: Option<Uuid>, pub prev_end: Option<DateTime<Utc>>,
    pub fixed: bool, pub fixed_assets: Vec<Uuid>, pub wlim: WLimits,
}
#[derive(Clone, Debug)]
pub(crate) struct Asset { pub id: Uuid, pub name: String, pub kind: String, pub qual: Option<Uuid>, pub busy: Vec<(DateTime<Utc>, DateTime<Utc>)> }
pub(crate) struct Problem {
    pub s: Settings, pub tz: chrono_tz::Tz, pub from: DateTime<Utc>, pub to: DateTime<Utc>, pub now: DateTime<Utc>,
    pub depot: Option<(f64, f64)>, pub teams: Vec<Team>, pub orders: Vec<Ord>, pub assets: Vec<Asset>,
    pub quals: HashMap<Uuid, Vec<(Uuid, Option<DateTime<Utc>>)>>, pub absences: HashMap<Uuid, Vec<(DateTime<Utc>, DateTime<Utc>)>>,
    pub days: Vec<NaiveDate>, pub weather: HashMap<Uuid, Vec<WSlot>>, pub site_zone: HashMap<Uuid, Uuid>, pub qual_names: HashMap<Uuid, String>,
}

/// Carica il problema del periodo. `replan`: anche gli interventi già pianificati e assegnati (non
/// bloccati, non avviati) si possono spostare; altrimenti restano dove sono e occupano tempo e mezzi.
/// `history`: prova sullo storico — gli interventi COMPLETATI del periodo passato si ripianificano da
/// capo come se fosse il primo giorno (sola lettura, per confrontare con il piano fatto a mano).
pub(crate) async fn load_problem(st: &AppState, tid: Uuid, from: DateTime<Utc>, to: DateTime<Utc>, replan: bool, teams_filter: Option<&[Uuid]>) -> Result<Problem, (StatusCode, String)> {
    load_problem_ex(st, tid, from, to, replan, teams_filter, false).await
}
pub(crate) async fn load_problem_ex(st: &AppState, tid: Uuid, from: DateTime<Utc>, to: DateTime<Utc>, replan: bool, teams_filter: Option<&[Uuid]>, history: bool) -> Result<Problem, (StatusCode, String)> {
    let s = load_settings(st, tid).await?;
    let tz = crate::field::tenant_tz(st, tid).await;
    let now = if history { from } else { st.clock.now() };
    let quals_on = module_active(st, tid, "qualifiche").await;
    let mezzi_on = module_active(st, tid, "mezzi").await;
    let meteo_on = module_active(st, tid, "meteo").await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let depot = match s.depot { Some(d) => sqlx::query("SELECT lat,lon FROM app.sites WHERE id=$1 AND lat IS NOT NULL AND lon IS NOT NULL").bind(d).fetch_optional(&mut *tx).await.map_err(ise)?
        .map(|r| (r.get::<f64,_>("lat"), r.get::<f64,_>("lon"))), None => None };
    let trows = sqlx::query("SELECT g.id, g.name, s.lat, s.lon FROM app.user_groups g LEFT JOIN app.sites s ON s.id=g.depot_site_id WHERE g.kind='squadra' ORDER BY g.name")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    let mem = sqlx::query("SELECT m.group_id, m.user_id FROM app.user_group_members m JOIN app.users u ON u.id=m.user_id WHERE u.anonymized_at IS NULL")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    let mut teams: Vec<Team> = trows.iter().map(|r| { let id: Uuid = r.get("id");
        Team { id, name: r.get("name"), depot: match (r.try_get::<Option<f64>,_>("lat").ok().flatten(), r.try_get::<Option<f64>,_>("lon").ok().flatten()) { (Some(a), Some(b)) => Some((a, b)), _ => None },
            members: mem.iter().filter(|m| m.get::<Uuid,_>("group_id") == id).map(|m| m.get("user_id")).collect() } }).collect();
    if let Some(f) = teams_filter { if !f.is_empty() { teams.retain(|t| f.contains(&t.id)); } }
    let mut quals: HashMap<Uuid, Vec<(Uuid, Option<DateTime<Utc>>)>> = HashMap::new();
    let mut qual_names = HashMap::new();
    if quals_on {
        for r in sqlx::query("SELECT user_id, qualification_id, expires_at FROM app.user_qualifications").fetch_all(&mut *tx).await.map_err(ise)? {
            quals.entry(r.get("user_id")).or_default().push((r.get("qualification_id"), r.try_get("expires_at").ok().flatten())); }
        for r in sqlx::query("SELECT id,name FROM app.qualifications").fetch_all(&mut *tx).await.map_err(ise)? { qual_names.insert(r.get::<Uuid,_>("id"), r.get::<String,_>("name")); }
    }
    let mut absences: HashMap<Uuid, Vec<(DateTime<Utc>, DateTime<Utc>)>> = HashMap::new();
    for r in sqlx::query("SELECT user_id, starts_at, ends_at FROM app.worker_absences WHERE ends_at > $1 AND starts_at < $2").bind(from).bind(to).fetch_all(&mut *tx).await.map_err(ise)? {
        absences.entry(r.get("user_id")).or_default().push((r.get("starts_at"), r.get("ends_at"))); }
    // interventi aperti: nel periodo, oppure senza data o senza assegnazione
    let rows = sqlx::query("SELECT o.id,o.version,o.title,o.site_id,s.name AS site_name,s.lat,s.lon,s.access_windows,o.activity_type_id,o.quantity::float8 AS qty,
            o.estimated_minutes,o.priority,o.scheduled_start,o.scheduled_end,o.assigned_group,o.assigned_user,o.plan_locked,o.state,
            a.crew_size,a.base_minutes,a.minutes_per_unit::float8 AS mpu,a.rain_max_mm::float8 AS rain_max,a.wind_max_kmh,a.temp_min_c,a.temp_max_c,a.dry_hours_after
         FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.activity_types a ON a.id=o.activity_type_id
         WHERE CASE WHEN $4 THEN o.state='completato' AND o.scheduled_start >= $1 AND o.scheduled_start < $2 AND o.assigned_group IS NOT NULL
               ELSE o.state NOT IN ('completato','annullato')
                AND ((o.scheduled_start >= $1 AND o.scheduled_start < $2) OR o.scheduled_start IS NULL OR (o.assigned_group IS NULL AND o.assigned_user IS NULL AND o.scheduled_start >= $3)) END
         ORDER BY o.scheduled_start NULLS LAST LIMIT 3000").bind(from).bind(to).bind(now - Duration::days(1)).bind(history).fetch_all(&mut *tx).await.map_err(ise)?;
    let tq = sqlx::query("SELECT aq.activity_type_id, aq.qualification_id, q.name FROM app.activity_type_qualifications aq JOIN app.qualifications q ON q.id=aq.qualification_id")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    let ta = sqlx::query("SELECT activity_type_id, lower(asset_kind) AS k, qty FROM app.activity_type_assets").fetch_all(&mut *tx).await.map_err(ise)?;
    let booked = sqlx::query("SELECT work_order_id, asset_id FROM app.asset_bookings WHERE work_order_id IS NOT NULL AND ends_at > $1 AND starts_at < $2")
        .bind(from - Duration::days(1)).bind(to + Duration::days(1)).fetch_all(&mut *tx).await.map_err(ise)?;
    let mut orders = vec![];
    for r in &rows {
        let at: Option<Uuid> = r.try_get("activity_type_id").ok().flatten();
        let start: Option<DateTime<Utc>> = r.try_get("scheduled_start").ok().flatten();
        let end: Option<DateTime<Utc>> = r.try_get("scheduled_end").ok().flatten();
        let group: Option<Uuid> = r.try_get("assigned_group").ok().flatten();
        let user: Option<Uuid> = r.try_get("assigned_user").ok().flatten();
        let in_period = start.map(|x| x >= from && x < to).unwrap_or(false);
        let locked: bool = r.get("plan_locked");
        let started = r.get::<String,_>("state") == "in_corso";
        // fisso: bloccato, avviato, già passato, oppure (senza «ripianifica») già pianificato e assegnato
        let fixed = !history && in_period && (locked || started || start.map(|x| x < now).unwrap_or(false) || (!replan && (group.is_some() || user.is_some())));
        if !in_period && start.is_some() && (group.is_some() || user.is_some()) { continue; } // fuori periodo e già sistemato
        // squadre scelte (es. il pilota): solo i loro interventi e quelli senza squadra né persona
        if let Some(f) = teams_filter.filter(|f| !f.is_empty()) {
            match group { Some(g) if !f.contains(&g) => continue, None if user.is_some() => continue, _ => {} }
        }
        if fixed && group.is_none() { continue; } // interventi di una sola persona: occupano la persona, non una squadra
        let qty: Option<f64> = r.try_get("qty").ok().flatten();
        let dur = r.try_get::<Option<i32>,_>("estimated_minutes").ok().flatten().map(|m| m as i64)
            .or_else(|| r.try_get::<Option<i32>,_>("base_minutes").ok().flatten().map(|b| crate::constraints::estimate_minutes(b, r.try_get::<Option<f64>,_>("mpu").ok().flatten().unwrap_or(0.0), qty) as i64))
            .or_else(|| match (start, end) { (Some(a), Some(z)) if z > a => Some((z - a).num_minutes()), _ => None }).unwrap_or(60).clamp(15, 600);
        let dur = if fixed { match (start, end) { (Some(a), Some(z)) if z > a => (z - a).num_minutes(), _ => dur } } else { dur };
        let id: Uuid = r.get("id");
        orders.push(Ord { id, version: r.get("version"), title: r.get("title"), site: r.try_get("site_id").ok().flatten(),
            site_name: r.try_get::<Option<String>,_>("site_name").ok().flatten().unwrap_or_default(),
            loc: match (r.try_get::<Option<f64>,_>("lat").ok().flatten(), r.try_get::<Option<f64>,_>("lon").ok().flatten()) { (Some(a), Some(b)) => Some((a, b)), _ => None },
            windows: r.try_get::<Option<Value>,_>("access_windows").ok().flatten().unwrap_or(json!([])), at, dur,
            crew: r.try_get::<Option<i32>,_>("crew_size").ok().flatten().unwrap_or(1).max(1) as usize,
            quals: if quals_on { at.map(|a| tq.iter().filter(|x| x.get::<Uuid,_>("activity_type_id") == a).map(|x| (x.get("qualification_id"), x.get("name"))).collect()).unwrap_or_default() } else { vec![] },
            kinds: if mezzi_on { at.map(|a| ta.iter().filter(|x| x.get::<Uuid,_>("activity_type_id") == a).map(|x| (x.get::<String,_>("k"), x.get::<i32,_>("qty").max(1) as usize)).collect()).unwrap_or_default() } else { vec![] },
            priority: r.try_get::<Option<i32>,_>("priority").ok().flatten().unwrap_or(2), pref: start, prev_group: group, prev_user: user, prev_end: end,
            fixed, fixed_assets: booked.iter().filter(|b| b.get::<Uuid,_>("work_order_id") == id).map(|b| b.get("asset_id")).collect(),
            wlim: if meteo_on { WLimits { rain_max: r.try_get::<Option<f64>,_>("rain_max").ok().flatten(), wind_max: r.try_get::<Option<i32>,_>("wind_max_kmh").ok().flatten().map(|x| x as f64),
                tmin: r.try_get::<Option<i32>,_>("temp_min_c").ok().flatten().map(|x| x as f64), tmax: r.try_get::<Option<i32>,_>("temp_max_c").ok().flatten().map(|x| x as f64),
                dry_h: r.try_get::<Option<i32>,_>("dry_hours_after").ok().flatten().map(|x| x as i64) } } else { WLimits::default() } });
    }
    // mezzi disponibili con le prenotazioni che NON appartengono a interventi da ripianificare
    let movable: HashSet<Uuid> = orders.iter().filter(|o| !o.fixed).map(|o| o.id).collect();
    let mut assets = vec![];
    if mezzi_on {
        let ar = sqlx::query("SELECT id,name,lower(kind) AS kind,required_qualification_id FROM app.assets WHERE status='disponibile' ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
        let bk = sqlx::query("SELECT asset_id, work_order_id, starts_at, ends_at FROM app.asset_bookings WHERE ends_at > $1 AND starts_at < $2")
            .bind(from - Duration::days(1)).bind(to + Duration::days(1)).fetch_all(&mut *tx).await.map_err(ise)?;
        let fixed_ids: HashSet<Uuid> = orders.iter().filter(|o| o.fixed).map(|o| o.id).collect();
        for a in &ar {
            let id: Uuid = a.get("id");
            assets.push(Asset { id, name: a.get("name"), kind: a.try_get::<Option<String>,_>("kind").ok().flatten().unwrap_or_default(),
                qual: if quals_on { a.try_get("required_qualification_id").ok().flatten() } else { None },
                busy: bk.iter().filter(|b| b.get::<Uuid,_>("asset_id") == id)
                    .filter(|b| b.try_get::<Option<Uuid>,_>("work_order_id").ok().flatten().map(|w| !movable.contains(&w) && !fixed_ids.contains(&w)).unwrap_or(true))
                    .map(|b| (b.get("starts_at"), b.get("ends_at"))).collect() });
        }
    }
    tx.commit().await.map_err(ise)?;
    let (weather, site_zone) = if meteo_on { load_weather(st, tid, from, to + Duration::days(1)).await? } else { (HashMap::new(), HashMap::new()) };
    // giorni lavorativi del periodo (da oggi in poi)
    let mut days = vec![];
    let (mut d, last) = (from.with_timezone(&tz).date_naive(), (to - Duration::seconds(1)).with_timezone(&tz).date_naive());
    let today = now.with_timezone(&tz).date_naive();
    while d <= last && days.len() < 31 { if d >= today && s.workdays[d.weekday().num_days_from_monday() as usize] { days.push(d); } d = d.succ_opt().unwrap(); }
    Ok(Problem { s, tz, from, to, now, depot, teams, orders, assets, quals, absences, days, weather, site_zone, qual_names })
}

// =============================================================================================
// Il motore
// =============================================================================================
#[derive(Clone, Debug)]
pub(crate) struct Stop { pub o: usize, pub start: DateTime<Utc>, pub end: DateTime<Utc>, pub assets: Vec<Uuid> }
#[derive(Clone, Debug, Default)]
pub(crate) struct RouteEval { pub stops: Vec<Stop>, pub travel: i64, pub km: f64, pub overtime: i64, pub legs: Vec<(i64, f64)> }

pub(crate) struct Engine<'a> {
    pub p: &'a Problem,
    pub routes: HashMap<(usize, usize), Vec<Stop>>,          // (squadra, giorno) → visite in ordine
    pub booked: HashMap<Uuid, Vec<(DateTime<Utc>, DateTime<Utc>, usize)>>, // mezzo → (inizio, fine, intervento)
    present: HashMap<(usize, usize), Vec<Uuid>>,
}

fn local(p: &Problem, d: NaiveDate, t: NaiveTime) -> DateTime<Utc> {
    p.tz.from_local_datetime(&d.and_time(t)).earliest().map(|x| x.with_timezone(&Utc)).unwrap_or_else(|| Utc.from_utc_datetime(&d.and_time(t)))
}
fn hhmm(s: &str) -> Option<NaiveTime> { NaiveTime::parse_from_str(s.trim(), "%H:%M").ok() }
/// Finestre della sede nel giorno (ora locale); vuoto = nessun limite (None).
fn windows_on(p: &Problem, ws: &Value, d: NaiveDate) -> Option<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
    let arr = ws.as_array()?; if arr.is_empty() { return None; }
    let wd = d.weekday().num_days_from_monday() as usize;
    let mut out: Vec<_> = arr.iter().filter(|w| w["days"].as_str().unwrap_or("").chars().nth(wd) == Some('1'))
        .filter_map(|w| Some((local(p, d, hhmm(w["from"].as_str()?)?), local(p, d, hhmm(w["to"].as_str()?)?)))).collect();
    out.sort(); Some(out)
}

impl<'a> Engine<'a> {
    pub fn new(p: &'a Problem) -> Self {
        let mut present = HashMap::new();
        for (ti, t) in p.teams.iter().enumerate() { for (di, d) in p.days.iter().enumerate() {
            let (a, z) = (local(p, *d, p.s.day_start), local(p, *d, p.s.day_end));
            present.insert((ti, di), t.members.iter().filter(|u| !p.absences.get(u).map(|v| v.iter().any(|(x, y)| *x < z && *y > a)).unwrap_or(false)).cloned().collect());
        } }
        let mut e = Engine { p, routes: HashMap::new(), booked: HashMap::new(), present };
        // interventi fissi: occupano la giornata della loro squadra e i loro mezzi
        for (oi, o) in p.orders.iter().enumerate().filter(|(_, o)| o.fixed) {
            let (Some(g), Some(a)) = (o.prev_group, o.pref) else { continue };
            let Some(ti) = p.teams.iter().position(|t| t.id == g) else { continue };
            let Some(di) = p.days.iter().position(|d| *d == a.with_timezone(&p.tz).date_naive()) else { continue };
            let z = o.prev_end.filter(|z| *z > a).unwrap_or(a + Duration::minutes(o.dur));
            for x in &o.fixed_assets { e.booked.entry(*x).or_default().push((a, z, oi)); }
            let r = e.routes.entry((ti, di)).or_default(); r.push(Stop { o: oi, start: a, end: z, assets: o.fixed_assets.clone() }); r.sort_by_key(|s| s.start);
        }
        e
    }
    fn depot(&self, ti: usize) -> Option<(f64, f64)> { self.p.teams[ti].depot.or(self.p.depot) }
    fn has_qual(&self, people: &[Uuid], q: Uuid, at: DateTime<Utc>) -> bool {
        people.iter().any(|u| self.p.quals.get(u).map(|v| v.iter().any(|(qq, exp)| *qq == q && exp.map(|e| e > at).unwrap_or(true))).unwrap_or(false))
    }
    /// La squadra può fare l'intervento quel giorno? Err(motivo) per spiegare i non pianificati.
    fn team_ok(&self, oi: usize, ti: usize, di: usize) -> Result<(), String> {
        let o = &self.p.orders[oi]; let pr = &self.present[&(ti, di)];
        if pr.len() < o.crew { return Err(format!("crew:{}", o.crew)); }
        let at = local(self.p, self.p.days[di], self.p.s.day_start);
        for (q, n) in &o.quals { if !self.has_qual(pr, *q, at) { return Err(format!("qual:{n}")); } }
        Ok(())
    }
    fn asset_free(&self, a: Uuid, s: DateTime<Utc>, z: DateTime<Utc>, except: usize, extra: &[(Uuid, DateTime<Utc>, DateTime<Utc>)]) -> bool {
        let p_busy = self.p.assets.iter().find(|x| x.id == a).map(|x| x.busy.iter().any(|(x0, x1)| *x0 < z && *x1 > s)).unwrap_or(true);
        if p_busy { return false; }
        if self.booked.get(&a).map(|v| v.iter().any(|(x0, x1, o)| *o != except && *x0 < z && *x1 > s)).unwrap_or(false) { return false; }
        !extra.iter().any(|(x, x0, x1)| *x == a && *x0 < z && *x1 > s)
    }
    /// Mezzi per l'intervento in [s,z): per ogni tipo la quantità richiesta, usabili dalla squadra.
    fn pick_assets(&self, oi: usize, ti: usize, di: usize, s: DateTime<Utc>, z: DateTime<Utc>, extra: &[(Uuid, DateTime<Utc>, DateTime<Utc>)]) -> Result<Vec<Uuid>, String> {
        let o = &self.p.orders[oi]; let pr = &self.present[&(ti, di)];
        let mut out = vec![];
        for (k, n) in &o.kinds {
            let free: Vec<Uuid> = self.p.assets.iter().filter(|a| a.kind == *k && a.qual.map(|q| self.has_qual(pr, q, s)).unwrap_or(true))
                .filter(|a| self.asset_free(a.id, s, z, oi, extra)).map(|a| a.id).take(*n).collect();
            if free.len() < *n { return Err(format!("asset:{k}")); }
            out.extend(free);
        }
        Ok(out)
    }
    /// Ricalcola gli orari di una giornata con le visite in quell'ordine. Err(motivo) se non sta.
    pub fn eval_route(&self, ti: usize, di: usize, seq: &[usize], prev_assets: &HashMap<usize, Vec<Uuid>>) -> Result<RouteEval, String> {
        let p = self.p; let d = p.days[di];
        let (day_a, day_z) = (local(p, d, p.s.day_start), local(p, d, p.s.day_end) + Duration::minutes(p.s.overtime));
        let mut t = if d == p.now.with_timezone(&p.tz).date_naive() { day_a.max(p.now) } else { day_a };
        let depot = self.depot(ti);
        let mut loc = depot;
        let mut ev = RouteEval::default();
        let mut extra: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>)> = vec![];
        for &oi in seq {
            let o = &p.orders[oi];
            let (tm, km) = match (loc, o.loc) { (Some(a), Some(b)) => travel_min(&p.s, a, b), _ => (if loc.is_some() || o.loc.is_some() { p.s.setup } else { 0 }, 0.0) };
            let arrive = t + Duration::minutes(tm);
            let (start, end) = if o.fixed {
                let a = o.pref.unwrap(); let z = o.prev_end.filter(|z| *z > a).unwrap_or(a + Duration::minutes(o.dur));
                if arrive > a { return Err("time".into()); }
                (a, z)
            } else {
                let dur = Duration::minutes(o.dur);
                let wins: Vec<(DateTime<Utc>, DateTime<Utc>)> = windows_on(p, &o.windows, d).unwrap_or_else(|| vec![(day_a, day_z)]);
                let first = wins.iter().find_map(|(w0, w1)| { let s = arrive.max(*w0); if s + dur <= *w1 && s + dur <= day_z { Some(s) } else { None } });
                let mut s = first.ok_or_else(|| "window".to_string())?;
                // meteo sfavorevole all'ora più presto: si prova più tardi (a passi di 30 minuti) nella giornata
                if p.s.weather_avoid && o.wlim.any() {
                    if let Some(sl) = o.site.and_then(|x| p.site_zone.get(&x)).and_then(|zid| p.weather.get(zid)) {
                        if weather_issue(sl, &o.wlim, s, s + dur).is_some() {
                            let later = wins.iter().flat_map(|(w0, w1)| { let (w0, w1) = (*w0, (*w1).min(day_z));
                                (0..48).map(move |k| arrive.max(w0) + Duration::minutes(30 * k)).take_while(move |x| *x + dur <= w1) })
                                .find(|x| weather_issue(sl, &o.wlim, *x, *x + dur).is_none());
                            if let Some(x) = later { s = x; }
                        }
                    }
                }
                (s, s + dur)
            };
            if end > day_z && !o.fixed { return Err("time".into()); }
            let assets = if o.fixed { o.fixed_assets.clone() } else {
                match prev_assets.get(&oi) { Some(v) if v.iter().all(|a| self.asset_free(*a, start, end, oi, &extra)) && !v.is_empty() => v.clone(), _ => self.pick_assets(oi, ti, di, start, end, &extra)? }
            };
            for a in &assets { extra.push((*a, start, end)); }
            ev.travel += tm; ev.km += km; ev.legs.push((tm, km));
            ev.stops.push(Stop { o: oi, start, end, assets });
            t = end; loc = o.loc.or(loc);
        }
        if let (Some(a), Some(b)) = (loc, depot) { let (tm, km) = travel_min(&p.s, a, b); ev.travel += tm; ev.km += km; t = t + Duration::minutes(tm); }
        let base_end = local(p, d, p.s.day_end);
        if t > day_z { return Err("time".into()); }
        ev.overtime = (t - base_end).num_minutes().max(0);
        Ok(ev)
    }
    fn route_cost(ev: &RouteEval) -> f64 { ev.travel as f64 + 2.0 * ev.overtime as f64 }
    /// Costo «di contesto» dell'intervento in quella squadra e giorno: data voluta, cambio di squadra, meteo.
    fn place_cost(&self, oi: usize, ti: usize, di: usize, s: DateTime<Utc>, z: DateTime<Utc>) -> f64 {
        let p = self.p; let o = &p.orders[oi];
        let mut c = 0.0;
        if let Some(pr) = o.pref { let dd = (p.days[di] - pr.with_timezone(&p.tz).date_naive()).num_days().abs() as f64; c += 45.0 * dd; }
        if o.prev_group.map(|g| g != p.teams[ti].id).unwrap_or(false) { c += 20.0; }
        if p.s.weather_avoid && o.wlim.any() {
            if let Some(zid) = o.site.and_then(|s| p.site_zone.get(&s)) { if let Some(sl) = p.weather.get(zid) { if weather_issue(sl, &o.wlim, s, z).is_some() { c += 600.0; } } }
        }
        c
    }
    fn seq_of(&self, ti: usize, di: usize) -> Vec<usize> { self.routes.get(&(ti, di)).map(|r| r.iter().map(|s| s.o).collect()).unwrap_or_default() }
    fn assets_of(&self, ti: usize, di: usize) -> HashMap<usize, Vec<Uuid>> { self.routes.get(&(ti, di)).map(|r| r.iter().map(|s| (s.o, s.assets.clone())).collect()).unwrap_or_default() }
    fn commit(&mut self, ti: usize, di: usize, ev: RouteEval) {
        if let Some(old) = self.routes.get(&(ti, di)) { for s in old { for a in &s.assets { if let Some(v) = self.booked.get_mut(a) { v.retain(|x| x.2 != s.o); } } } }
        for s in &ev.stops { for a in &s.assets { self.booked.entry(*a).or_default().push((s.start, s.end, s.o)); } }
        self.routes.insert((ti, di), ev.stops);
    }
    /// Miglior inserimento: (costo, squadra, giorno, valutazione) oppure il motivo per cui non entra.
    fn best_insert(&self, oi: usize) -> Result<(f64, usize, usize, RouteEval), String> {
        let p = self.p; let o = &p.orders[oi];
        if o.loc.is_none() && (p.depot.is_some() || p.teams.iter().any(|t| t.depot.is_some())) { return Err("loc".into()); }
        let mut best: Option<(f64, usize, usize, RouteEval)> = None;
        let mut why: HashMap<String, usize> = HashMap::new();
        for ti in 0..p.teams.len() { for di in 0..p.days.len() {
            if let Err(e) = self.team_ok(oi, ti, di) { *why.entry(e).or_default() += 1; continue; }
            let seq = self.seq_of(ti, di); let pa = self.assets_of(ti, di);
            let base = if seq.is_empty() { 0.0 } else { match self.eval_route(ti, di, &seq, &pa) { Ok(e) => Self::route_cost(&e), Err(_) => continue } };
            let mut local_why: HashMap<String, usize> = HashMap::new(); let mut fits = false;
            for pos in 0..=seq.len() {
                let mut s2 = seq.clone(); s2.insert(pos, oi);
                match self.eval_route(ti, di, &s2, &pa) {
                    Ok(ev) => {
                        fits = true;
                        let st = ev.stops.iter().find(|s| s.o == oi).unwrap();
                        let c = Self::route_cost(&ev) - base + self.place_cost(oi, ti, di, st.start, st.end);
                        if best.as_ref().map(|b| c < b.0 - 1e-9).unwrap_or(true) { best = Some((c, ti, di, ev)); }
                    }
                    Err(e) => { *local_why.entry(e).or_default() += 1; }
                }
            }
            // un motivo per squadra e giorno (il più frequente tra le posizioni provate)
            if !fits { if let Some((k, _)) = local_why.into_iter().max_by_key(|x| x.1) { *why.entry(k).or_default() += 1; } }
        } }
        best.ok_or_else(|| {
            if p.teams.is_empty() { return "noteam".to_string(); }
            if p.days.is_empty() { return "nodays".to_string(); }
            // il motivo più frequente tra tutti i tentativi (a parità, quello più specifico)
            why.into_iter().max_by_key(|(k, n)| (*n, if k.starts_with("qual") || k.starts_with("crew") || k.starts_with("asset") { 1 } else { 0 })).map(|x| x.0).unwrap_or_else(|| "time".into())
        })
    }
    fn remove(&mut self, oi: usize) -> Option<(usize, usize)> {
        let key = self.routes.iter().find(|(_, r)| r.iter().any(|s| s.o == oi)).map(|(k, _)| *k)?;
        let seq: Vec<usize> = self.seq_of(key.0, key.1).into_iter().filter(|x| *x != oi).collect();
        let mut pa = self.assets_of(key.0, key.1); pa.remove(&oi);
        match self.eval_route(key.0, key.1, &seq, &pa) {
            Ok(ev) => { self.commit(key.0, key.1, ev); Some(key) }
            Err(_) => None, // togliere una visita non rende mai una giornata impossibile; per sicurezza non tocca
        }
    }
    fn total_cost(&self) -> f64 {
        let mut c = 0.0;
        for ((ti, di), r) in &self.routes {
            let seq: Vec<usize> = r.iter().map(|s| s.o).collect();
            if let Ok(ev) = self.eval_route(*ti, *di, &seq, &self.assets_of(*ti, *di)) {
                c += Self::route_cost(&ev);
                for s in &ev.stops { if !self.p.orders[s.o].fixed { c += self.place_cost(s.o, *ti, *di, s.start, s.end); } }
            }
        }
        c
    }
    /// Pianifica: inserimento in ordine, poi miglioramento entro il tempo concesso.
    pub fn solve(&mut self, budget: std::time::Duration) -> HashMap<usize, String> {
        let t0 = std::time::Instant::now();
        let mut todo: Vec<usize> = (0..self.p.orders.len()).filter(|i| !self.p.orders[*i].fixed).collect();
        todo.sort_by(|a, b| { let (x, y) = (&self.p.orders[*a], &self.p.orders[*b]);
            y.priority.cmp(&x.priority).then(x.pref.map(|t| t.timestamp()).unwrap_or(i64::MAX).cmp(&y.pref.map(|t| t.timestamp()).unwrap_or(i64::MAX))).then(y.dur.cmp(&x.dur)) });
        let mut unplanned = HashMap::new();
        for oi in todo.clone() {
            match self.best_insert(oi) { Ok((_, ti, di, ev)) => self.commit(ti, di, ev), Err(e) => { unplanned.insert(oi, e); } }
        }
        // miglioramento: togli e reinserisci dove costa meno
        let mut improved = true; let mut rounds = 0;
        while improved && rounds < 6 && t0.elapsed() < budget {
            improved = false; rounds += 1;
            for oi in todo.iter().cloned().filter(|o| !unplanned.contains_key(o)) {
                if t0.elapsed() >= budget { break; }
                let before = self.total_cost();
                let snapshot = (self.routes.clone(), self.booked.clone());
                if self.remove(oi).is_none() { continue; }
                match self.best_insert(oi) {
                    Ok((_, ti, di, ev)) => { self.commit(ti, di, ev); if self.total_cost() < before - 0.5 { improved = true; } else { self.routes = snapshot.0; self.booked = snapshot.1; } }
                    Err(_) => { self.routes = snapshot.0; self.booked = snapshot.1; }
                }
            }
            // chi non era entrato riprova dopo i miglioramenti
            for oi in unplanned.keys().cloned().collect::<Vec<_>>() {
                if let Ok((_, ti, di, ev)) = self.best_insert(oi) { self.commit(ti, di, ev); unplanned.remove(&oi); improved = true; }
            }
        }
        unplanned
    }
}

/// Spostamenti di un insieme di giornate (stesso modello): per confrontare con il piano attuale.
fn day_travel(p: &Problem, depot: Option<(f64, f64)>, stops: &[(DateTime<Utc>, Option<(f64, f64)>)]) -> (i64, f64) {
    let mut v: Vec<&(DateTime<Utc>, Option<(f64, f64)>)> = stops.iter().collect(); v.sort_by_key(|x| x.0);
    let (mut tm, mut km, mut loc) = (0i64, 0.0, depot);
    for (_, l) in v { if let (Some(a), Some(b)) = (loc, *l) { let (m, k) = travel_min(&p.s, a, b); tm += m; km += k; } if l.is_some() { loc = *l; } }
    if let (Some(a), Some(b)) = (loc, depot) { let (m, k) = travel_min(&p.s, a, b); tm += m; km += k; }
    (tm, km)
}

/// Straordinario di una giornata: fine dell'ultimo intervento + rientro al deposito oltre la fine giornata.
fn day_overtime(p: &Problem, depot: Option<(f64, f64)>, stops: &[(DateTime<Utc>, DateTime<Utc>, Option<(f64, f64)>)]) -> i64 {
    let Some(last) = stops.iter().max_by_key(|x| x.1) else { return 0 };
    let back = match (last.2, depot) { (Some(a), Some(b)) => travel_min(&p.s, a, b).0, _ => 0 };
    let d = last.0.with_timezone(&p.tz).date_naive();
    ((last.1 + Duration::minutes(back)) - local(p, d, p.s.day_end)).num_minutes().max(0)
}

fn reason_text(p: &Problem, code: &str) -> String {
    if code == "loc" { return "La sede non ha la posizione: non si possono stimare gli spostamenti.".into(); }
    if code == "noteam" { return "Nessuna squadra da pianificare.".into(); }
    if code == "nodays" { return "Nessun giorno lavorativo nel periodo.".into(); }
    if let Some(n) = code.strip_prefix("qual:") { return format!("Nessuna squadra ha «{n}» valida nei giorni del periodo."); }
    if let Some(n) = code.strip_prefix("crew:") { return format!("Nessuna squadra ha {n} persone presenti nei giorni del periodo."); }
    if let Some(k) = code.strip_prefix("asset:") { return format!("Nessun mezzo «{k}» libero quando servirebbe."); }
    if code == "window" { return "Non entra negli orari della sede nei giorni del periodo.".into(); }
    let _ = p; "Non entra nelle giornate delle squadre del periodo (orari e spostamenti).".into()
}

// =============================================================================================
// API
// =============================================================================================
#[derive(Deserialize)]
pub struct RunReq { pub from: String, pub to: String, #[serde(default)] pub replan: Option<bool>, #[serde(default)] pub group_ids: Option<Vec<Uuid>> }

fn period(from: &str, to: &str) -> Result<(DateTime<Utc>, DateTime<Utc>), (StatusCode, String)> {
    let (a, z) = (parse_ts(from)?, parse_ts(to)?);
    if z <= a || z - a > Duration::days(31) { return Err(err(StatusCode::BAD_REQUEST, "periodo tra 1 giorno e 31 giorni")); }
    Ok((a, z))
}

/// POST /api/planning/runs — il motore di base propone una bozza per il periodo.
pub async fn planning_run(State(st): State<AppState>, h: HeaderMap, Json(b): Json<RunReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let (from, to) = period(&b.from, &b.to)?;
    let replan = b.replan.unwrap_or(false);
    let p = load_problem(&st, tid, from, to, replan, b.group_ids.as_deref()).await?;
    let t0 = std::time::Instant::now();
    let mut e = Engine::new(&p);
    let unplanned = e.solve(std::time::Duration::from_secs(20));
    let ms = t0.elapsed().as_millis() as i64;
    let mut items: Vec<Item> = vec![];
    for ((ti, di), r) in &e.routes {
        let seq: Vec<usize> = r.iter().map(|x| x.o).collect();
        let Ok(ev) = e.eval_route(*ti, *di, &seq, &e.assets_of(*ti, *di)) else { continue };
        for (k, stp) in ev.stops.iter().enumerate() {
            let o = &p.orders[stp.o]; if o.fixed { continue; }
            items.push(Item { order: o.id, version: o.version, group: Some(p.teams[*ti].id), user: None, start: Some(stp.start), end: Some(stp.end),
                assets: stp.assets.clone(), seq: Some(k as i32 + 1), travel_min: Some(ev.legs[k].0 as i32), travel_km: Some(ev.legs[k].1), reason: None,
                previous: json!({"start": o.pref, "end": o.prev_end, "group_id": o.prev_group, "user_id": o.prev_user}), violations: json!([]) });
        }
    }
    for (oi, code) in &unplanned { let o = &p.orders[*oi];
        items.push(Item { order: o.id, version: o.version, group: None, user: None, start: None, end: None, assets: vec![], seq: None, travel_min: None, travel_km: None,
            reason: Some(reason_text(&p, code)), previous: json!({"start": o.pref, "end": o.prev_end, "group_id": o.prev_group, "user_id": o.prev_user}), violations: json!([]) }); }
    let summary = summarize(&st, tid, &p, &mut items, Some(&e)).await?;
    let run = save_run(&st, tid, s.user_id, "nativo", ENGINE, from, to, json!({"replan": replan, "group_ids": b.group_ids, "ms": ms}), summary, &items).await?;
    audit(&st, tid, &s.email, "planning_run", json!({"id": run, "items": items.len()})).await;
    run_detail(&st, tid, run).await.map(Json)
}

pub(crate) struct Item { pub order: Uuid, pub version: i32, pub group: Option<Uuid>, pub user: Option<Uuid>, pub start: Option<DateTime<Utc>>, pub end: Option<DateTime<Utc>>,
    pub assets: Vec<Uuid>, pub seq: Option<i32>, pub travel_min: Option<i32>, pub travel_km: Option<f64>, pub reason: Option<String>, pub previous: Value, pub violations: Value }

/// Valida ogni voce con i vincoli di YW (come la pianificazione a mano) + sovrapposizioni e mezzi
/// doppi DENTRO la proposta; confronta gli spostamenti con il piano attuale.
async fn summarize(st: &AppState, tid: Uuid, p: &Problem, items: &mut [Item], _e: Option<&Engine<'_>>) -> Result<Value, (StatusCode, String)> {
    let mut hard = 0; let mut warn = 0;
    // sovrapposizioni nella proposta (stessa squadra) e mezzi doppi
    let mut by_team: HashMap<Uuid, Vec<(DateTime<Utc>, DateTime<Utc>, usize)>> = HashMap::new();
    let mut by_asset: HashMap<Uuid, Vec<(DateTime<Utc>, DateTime<Utc>, usize)>> = HashMap::new();
    for (i, it) in items.iter().enumerate() { if let (Some(a), Some(z)) = (it.start, it.end) {
        if let Some(g) = it.group { by_team.entry(g).or_default().push((a, z, i)); }
        for x in &it.assets { by_asset.entry(*x).or_default().push((a, z, i)); } } }
    // anche gli interventi fissi (squadra e mezzi)
    for o in p.orders.iter().filter(|o| o.fixed) { if let Some(a) = o.pref { let z = o.prev_end.unwrap_or(a + Duration::minutes(o.dur));
        if let Some(g) = o.prev_group { by_team.entry(g).or_default().push((a, z, usize::MAX)); }
        for x in &o.fixed_assets { by_asset.entry(*x).or_default().push((a, z, usize::MAX)); } } }
    let mut extra: HashMap<usize, Vec<Value>> = HashMap::new();
    for v in by_team.values().chain(by_asset.values()) {
        for x in v { for y in v { if x.2 != y.2 && x.2 != usize::MAX && x.0 < y.1 && y.0 < x.1 {
            extra.entry(x.2).or_default().push(json!({"code": "plan_overlap", "hard": true, "message": "Si sovrappone a un altro intervento della stessa squadra o allo stesso mezzo nella proposta."})); } } }
    }
    for (i, it) in items.iter_mut().enumerate() {
        let (Some(a), Some(z)) = (it.start, it.end) else { continue };
        let o = p.orders.iter().find(|o| o.id == it.order);
        let plan = OrderPlan { order_id: Some(it.order), site_id: o.and_then(|o| o.site), activity_type_id: o.and_then(|o| o.at), start: Some(a), end: Some(z),
            user: it.user, group: it.group, asset_ids: Some(it.assets.clone()) };
        // «overlap» e «asset_busy» del controllo standard guardano gli orari ATTUALI degli interventi che la
        // proposta sposta: qui si controllano sulla proposta (sotto) e sulle prenotazioni esterne a essa
        let mut vs: Vec<Value> = check_order(st, tid, &plan).await?.into_iter().filter(|v| !["overlap", "asset_missing", "asset_busy"].contains(&v.code))
            .map(|v| json!({"code": v.code, "hard": v.hard, "message": v.message})).collect();
        for x in &it.assets { if let Some(asset) = p.assets.iter().find(|q| q.id == *x) { if asset.busy.iter().any(|(b0, b1)| *b0 < z && *b1 > a) {
            vs.push(json!({"code": "plan_asset_busy", "hard": true, "message": format!("Il mezzo «{}» è già prenotato in quel periodo da un intervento fuori dalla proposta.", asset.name)})); } } }
        let mut seen = HashSet::new();
        for x in extra.remove(&i).unwrap_or_default() { if seen.insert(x["code"].as_str().unwrap_or("").to_string()) { vs.push(x); } }
        if vs.iter().any(|v| v["hard"].as_bool() == Some(true)) { hard += 1; } else if !vs.is_empty() { warn += 1; }
        it.violations = json!(vs);
    }
    // spostamenti: proposta contro piano attuale (stessi interventi pianificati con squadra nel periodo)
    let team_depot = |g: Uuid| p.teams.iter().find(|t| t.id == g).and_then(|t| t.depot).or(p.depot);
    let loc_of = |id: Uuid| p.orders.iter().find(|o| o.id == id).and_then(|o| o.loc);
    let mut prop: HashMap<(Uuid, NaiveDate), Vec<(DateTime<Utc>, Option<(f64, f64)>)>> = HashMap::new();
    let mut cur: HashMap<(Uuid, NaiveDate), Vec<(DateTime<Utc>, Option<(f64, f64)>)>> = HashMap::new();
    for it in items.iter() { if let (Some(g), Some(a)) = (it.group, it.start) { prop.entry((g, a.with_timezone(&p.tz).date_naive())).or_default().push((a, loc_of(it.order))); } }
    for o in &p.orders { if let (Some(g), Some(a)) = (o.prev_group, o.pref) { if a >= p.from && a < p.to {
        cur.entry((g, a.with_timezone(&p.tz).date_naive())).or_default().push((a, o.loc));
        if o.fixed { prop.entry((g, a.with_timezone(&p.tz).date_naive())).or_default().push((a, o.loc)); } } } }
    let sum = |m: &HashMap<(Uuid, NaiveDate), Vec<(DateTime<Utc>, Option<(f64, f64)>)>>| m.iter().fold((0i64, 0.0f64), |acc, ((g, _), v)| { let (t, k) = day_travel(p, team_depot(*g), v); (acc.0 + t, acc.1 + k) });
    let (pt, pk) = sum(&prop); let (ct, ck) = sum(&cur);
    // straordinario: proposta (con gli interventi fissi) e piano attuale
    let mut po: HashMap<(Uuid, NaiveDate), Vec<(DateTime<Utc>, DateTime<Utc>, Option<(f64, f64)>)>> = HashMap::new();
    let mut co: HashMap<(Uuid, NaiveDate), Vec<(DateTime<Utc>, DateTime<Utc>, Option<(f64, f64)>)>> = HashMap::new();
    for it in items.iter() { if let (Some(g), Some(a), Some(z)) = (it.group, it.start, it.end) { po.entry((g, a.with_timezone(&p.tz).date_naive())).or_default().push((a, z, loc_of(it.order))); } }
    for o in &p.orders { if let (Some(g), Some(a)) = (o.prev_group, o.pref) { if a >= p.from && a < p.to {
        let z = o.prev_end.filter(|z| *z > a).unwrap_or(a + Duration::minutes(o.dur));
        co.entry((g, a.with_timezone(&p.tz).date_naive())).or_default().push((a, z, o.loc));
        if o.fixed { po.entry((g, a.with_timezone(&p.tz).date_naive())).or_default().push((a, z, o.loc)); } } } }
    let ot = |m: &HashMap<(Uuid, NaiveDate), Vec<(DateTime<Utc>, DateTime<Utc>, Option<(f64, f64)>)>>| m.iter().map(|((g, _), v)| day_overtime(p, team_depot(*g), v)).sum::<i64>();
    let (pov, cov) = (ot(&po), ot(&co));
    let planned = items.iter().filter(|i| i.start.is_some()).count();
    let moved = items.iter().filter(|i| i.start.is_some() && (i.previous["start"].as_str().map(|s| parse_ts(s).ok() != i.start).unwrap_or(true) || i.previous["group_id"].as_str().map(|g| Some(g.to_string()) != i.group.map(|x| x.to_string())).unwrap_or(true))).count();
    let weather_avoided = p.s.weather_avoid;
    Ok(json!({"contract": CONTRACT, "orders": items.len(), "planned": planned, "unplanned": items.len() - planned, "changed": moved,
        "fixed": p.orders.iter().filter(|o| o.fixed).count(), "hard": hard, "warnings": warn,
        "travel_minutes": pt, "travel_km": (pk * 10.0).round() / 10.0, "current_travel_minutes": ct, "current_travel_km": (ck * 10.0).round() / 10.0,
        "overtime_minutes": pov, "current_overtime_minutes": cov,
        "teams": p.teams.len(), "days": p.days.iter().map(|d| d.to_string()).collect::<Vec<_>>(), "weather_avoided": weather_avoided,
        "travel_model": {"kmh": p.s.kmh, "detour_factor": p.s.detour, "setup_minutes": p.s.setup}}))
}

#[allow(clippy::too_many_arguments)]
async fn save_run(st: &AppState, tid: Uuid, by: Uuid, source: &str, engine: &str, from: DateTime<Utc>, to: DateTime<Utc>, params: Value, summary: Value, items: &[Item]) -> Result<Uuid, (StatusCode, String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // una sola bozza alla volta per lo stesso periodo: le precedenti dello stesso motore si scartano
    sqlx::query("UPDATE app.planning_runs SET status='scartato' WHERE status='bozza' AND source=$1 AND period_from=$2 AND period_to=$3").bind(source).bind(from).bind(to).execute(&mut *tx).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.planning_runs(tenant_id,source,engine,period_from,period_to,params,summary,created_by) VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(tid).bind(source).bind(engine).bind(from).bind(to).bind(&params).bind(&summary).bind(by).fetch_one(&mut *tx).await.map_err(ise)?;
    for it in items {
        sqlx::query("INSERT INTO app.planning_run_items(run_id,tenant_id,work_order_id,order_version,group_id,user_id,starts_at,ends_at,asset_ids,seq,travel_minutes,travel_km,previous,violations,reason)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12::numeric,$13,$14,$15)")
            .bind(id).bind(tid).bind(it.order).bind(it.version).bind(it.group).bind(it.user).bind(it.start).bind(it.end).bind(&it.assets).bind(it.seq)
            .bind(it.travel_min).bind(it.travel_km.map(|k| (k * 10.0).round() / 10.0)).bind(&it.previous).bind(&it.violations).bind(&it.reason)
            .execute(&mut *tx).await.map_err(|e| crate::api::conflict_or(e, "voce della proposta non valida"))?;
    }
    tx.commit().await.map_err(ise)?;
    Ok(id)
}

pub(crate) async fn run_detail(st: &AppState, tid: Uuid, id: Uuid) -> Result<Value, (StatusCode, String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = sqlx::query("SELECT r.*, u.email AS by_email FROM app.planning_runs r LEFT JOIN app.users u ON u.id=r.created_by WHERE r.id=$1").bind(id)
        .fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "proposta inesistente"))?;
    let items = sqlx::query("SELECT i.*, i.travel_km::float8 AS travel_km_f, o.title, o.version AS cur_version, s.name AS site, g.name AS team,
            (SELECT array_agg(a.name ORDER BY a.name) FROM app.assets a WHERE a.id = ANY(i.asset_ids)) AS asset_names,
            pg.name AS prev_team
         FROM app.planning_run_items i JOIN app.work_orders o ON o.id=i.work_order_id LEFT JOIN app.sites s ON s.id=o.site_id
         LEFT JOIN app.user_groups g ON g.id=i.group_id LEFT JOIN app.user_groups pg ON pg.id=(i.previous->>'group_id')::uuid
         WHERE i.run_id=$1 ORDER BY g.name NULLS LAST, i.starts_at").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(json!({"id": id, "source": r.get::<String,_>("source"), "engine": r.get::<String,_>("engine"), "status": r.get::<String,_>("status"),
        "from": r.get::<DateTime<Utc>,_>("period_from"), "to": r.get::<DateTime<Utc>,_>("period_to"), "params": r.get::<Value,_>("params"),
        "summary": r.get::<Value,_>("summary"), "created_at": r.get::<DateTime<Utc>,_>("created_at"), "created_by": r.try_get::<Option<String>,_>("by_email").ok().flatten(),
        "applied_at": r.try_get::<Option<DateTime<Utc>>,_>("applied_at").ok().flatten(),
        "items": items.iter().map(|i| json!({"order_id": i.get::<Uuid,_>("work_order_id"), "title": i.get::<String,_>("title"), "site": i.try_get::<Option<String>,_>("site").ok().flatten(),
            "group_id": i.try_get::<Option<Uuid>,_>("group_id").ok().flatten(), "team": i.try_get::<Option<String>,_>("team").ok().flatten(),
            "start": i.try_get::<Option<DateTime<Utc>>,_>("starts_at").ok().flatten(), "end": i.try_get::<Option<DateTime<Utc>>,_>("ends_at").ok().flatten(),
            "assets": i.try_get::<Option<Vec<String>>,_>("asset_names").ok().flatten().unwrap_or_default(), "seq": i.try_get::<Option<i32>,_>("seq").ok().flatten(),
            "travel_minutes": i.try_get::<Option<i32>,_>("travel_minutes").ok().flatten(), "travel_km": i.try_get::<Option<f64>,_>("travel_km_f").ok().flatten(),
            "previous": i.get::<Value,_>("previous"), "prev_team": i.try_get::<Option<String>,_>("prev_team").ok().flatten(),
            "violations": i.get::<Value,_>("violations"), "reason": i.try_get::<Option<String>,_>("reason").ok().flatten(),
            "stale": i.get::<i32,_>("cur_version") != i.get::<i32,_>("order_version"),
            "applied": i.get::<bool,_>("applied"), "apply_error": i.try_get::<Option<String>,_>("apply_error").ok().flatten()})).collect::<Vec<_>>()}))
}

pub async fn planning_runs(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_any_cap(&s, &["plan", "orders.view_all"])?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT r.id,r.source,r.engine,r.status,r.period_from,r.period_to,r.summary,r.created_at,u.email FROM app.planning_runs r
         LEFT JOIN app.users u ON u.id=r.created_by ORDER BY r.created_at DESC LIMIT 30").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({"id": r.get::<Uuid,_>("id"), "source": r.get::<String,_>("source"), "engine": r.get::<String,_>("engine"),
        "status": r.get::<String,_>("status"), "from": r.get::<DateTime<Utc>,_>("period_from"), "to": r.get::<DateTime<Utc>,_>("period_to"),
        "summary": r.get::<Value,_>("summary"), "created_at": r.get::<DateTime<Utc>,_>("created_at"), "created_by": r.try_get::<Option<String>,_>("email").ok().flatten()})).collect::<Vec<_>>())))
}
pub async fn planning_run_get(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_any_cap(&s, &["plan", "orders.view_all"])?;
    run_detail(&st, tid, id).await.map(Json)
}
pub async fn planning_run_discard(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.planning_runs SET status='scartato' WHERE id=$1 AND status='bozza'").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::CONFLICT, "la proposta non è una bozza")); }
    audit(&st, tid, &s.email, "planning_discard", json!({"id": id})).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct ApplyReq { #[serde(default)] pub order_ids: Option<Vec<Uuid>>, #[serde(default)] pub force: Option<bool> }
/// POST /api/planning/runs/:id/apply — applica la bozza (tutta o le voci scelte). Ogni voce si
/// ricontrolla adesso: intervento cambiato dopo la proposta o vincolo duro → non applicata, col motivo.
pub async fn planning_apply(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ApplyReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let force = b.force.unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM app.planning_runs WHERE id=$1 FOR UPDATE").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    match status.as_deref() { None => return Err(err(StatusCode::NOT_FOUND, "proposta inesistente")), Some("bozza") | Some("applicato") => {}, _ => return Err(err(StatusCode::CONFLICT, "proposta scartata")) }
    let items = sqlx::query("SELECT i.work_order_id, i.order_version, i.group_id, i.user_id, i.starts_at, i.ends_at, i.asset_ids, o.version, o.site_id, o.activity_type_id,
            o.assigned_user, o.state FROM app.planning_run_items i JOIN app.work_orders o ON o.id=i.work_order_id
         WHERE i.run_id=$1 AND i.starts_at IS NOT NULL AND NOT i.applied AND ($2::uuid[] IS NULL OR i.work_order_id = ANY($2)) ORDER BY i.starts_at")
        .bind(id).bind(&b.order_ids).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // 1) controllo di ogni voce con lo stato attuale
    let mut ok_items = vec![]; let mut errors: Vec<(Uuid, String)> = vec![];
    for r in &items {
        let oid: Uuid = r.get("work_order_id");
        if r.get::<i32,_>("version") != r.get::<i32,_>("order_version") { errors.push((oid, "L'intervento è stato modificato dopo la proposta.".into())); continue; }
        if ["completato", "annullato", "in_corso"].contains(&r.get::<String,_>("state").as_str()) { errors.push((oid, "L'intervento non è più da pianificare.".into())); continue; }
        let plan = OrderPlan { order_id: Some(oid), site_id: r.try_get("site_id").ok().flatten(), activity_type_id: r.try_get("activity_type_id").ok().flatten(),
            start: r.get("starts_at"), end: r.get("ends_at"), user: r.try_get("user_id").ok().flatten(), group: r.try_get("group_id").ok().flatten(),
            asset_ids: Some(r.get::<Vec<Uuid>,_>("asset_ids")) };
        let vs = check_order(&st, tid, &plan).await?;
        // le prenotazioni dei mezzi si rifanno tutte insieme: un conflitto vero lo ferma il vincolo del database
        let hard: Vec<String> = vs.iter().filter(|v| v.hard && v.code != "asset_busy").map(|v| v.message.clone()).collect();
        if !hard.is_empty() && !force { errors.push((oid, hard.join(" "))); continue; }
        ok_items.push(r);
    }
    // 2) applicazione in una transazione: prima si liberano i mezzi di tutte le voci (si scambiano
    //    di orario tra loro), poi ogni voce con un punto di ripristino; i mezzi di una voce fallita
    //    tornano come prima.
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ids: Vec<Uuid> = ok_items.iter().map(|r| r.get("work_order_id")).collect();
    let old = sqlx::query("SELECT work_order_id, asset_id, starts_at, ends_at FROM app.asset_bookings WHERE work_order_id = ANY($1)").bind(&ids).fetch_all(&mut *tx).await.map_err(ise)?;
    let mezzi_on = module_active(&st, tid, "mezzi").await;
    if mezzi_on { sqlx::query("DELETE FROM app.asset_bookings WHERE work_order_id = ANY($1)").bind(&ids).execute(&mut *tx).await.map_err(ise)?; }
    let mut applied = 0;
    for r in &ok_items {
        let oid: Uuid = r.get("work_order_id");
        let (a, z): (DateTime<Utc>, DateTime<Utc>) = (r.get("starts_at"), r.get("ends_at"));
        let group: Option<Uuid> = r.try_get("group_id").ok().flatten();
        let assets: Vec<Uuid> = r.get("asset_ids");
        let mut sp = sqlx::Acquire::begin(&mut *tx).await.map_err(ise)?;
        let res: Result<(), (StatusCode, String)> = async {
            // la persona assegnata resta solo se fa parte della nuova squadra
            let keep_user: bool = match (group, r.try_get::<Option<Uuid>,_>("assigned_user").ok().flatten()) {
                (Some(g), Some(u)) => sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.user_group_members WHERE group_id=$1 AND user_id=$2)").bind(g).bind(u).fetch_one(&mut *sp).await.map_err(ise)?,
                _ => true };
            let n = sqlx::query("UPDATE app.work_orders SET scheduled_start=$2, scheduled_end=$3, assigned_group=COALESCE($4,assigned_group),
                    assigned_user=CASE WHEN $5 THEN assigned_user ELSE NULL END,
                    state=CASE WHEN state='bozza' THEN 'assegnato' ELSE state END, version=version+1, updated_at=now()
                 WHERE id=$1 AND version=$6").bind(oid).bind(a).bind(z).bind(group).bind(keep_user).bind(r.get::<i32,_>("order_version"))
                .execute(&mut *sp).await.map_err(ise)?.rows_affected();
            if n == 0 { return Err(err(StatusCode::CONFLICT, "L'intervento è stato modificato dopo la proposta.")); }
            if mezzi_on {
                if !assets.is_empty() { book_order_assets(&mut sp, tid, oid, &assets, a, z, Some(s.user_id)).await?; }
                else {
                    for o in old.iter().filter(|o| o.get::<Uuid,_>("work_order_id") == oid) {
                        sqlx::query("INSERT INTO app.asset_bookings(tenant_id,asset_id,work_order_id,starts_at,ends_at,created_by) VALUES($1,$2,$3,$4,$5,$6)")
                            .bind(tid).bind(o.get::<Uuid,_>("asset_id")).bind(oid).bind(a).bind(z).bind(s.user_id).execute(&mut *sp).await.map_err(ise)?;
                    }
                    move_order_bookings(&mut sp, oid, a, z).await?;
                }
            }
            Ok(())
        }.await;
        match res {
            Ok(()) => { sp.commit().await.map_err(ise)?; applied += 1;
                sqlx::query("UPDATE app.planning_run_items SET applied=true, apply_error=NULL WHERE run_id=$1 AND work_order_id=$2").bind(id).bind(oid).execute(&mut *tx).await.map_err(ise)?; }
            Err((_, m)) => { let _ = sp.rollback().await;
                for o in old.iter().filter(|o| o.get::<Uuid,_>("work_order_id") == oid) { // mezzi come prima
                    let mut sp2 = sqlx::Acquire::begin(&mut *tx).await.map_err(ise)?;
                    let ok = sqlx::query("INSERT INTO app.asset_bookings(tenant_id,asset_id,work_order_id,starts_at,ends_at,created_by) VALUES($1,$2,$3,$4,$5,$6)")
                        .bind(tid).bind(o.get::<Uuid,_>("asset_id")).bind(oid).bind(o.get::<DateTime<Utc>,_>("starts_at")).bind(o.get::<DateTime<Utc>,_>("ends_at")).bind(s.user_id)
                        .execute(&mut *sp2).await.is_ok();
                    if ok { sp2.commit().await.map_err(ise)?; } else { let _ = sp2.rollback().await; }
                }
                errors.push((oid, m)); }
        }
    }
    for (oid, m) in &errors {
        sqlx::query("UPDATE app.planning_run_items SET apply_error=$3 WHERE run_id=$1 AND work_order_id=$2").bind(id).bind(oid).bind(m).execute(&mut *tx).await.map_err(ise)?;
    }
    if applied > 0 { sqlx::query("UPDATE app.planning_runs SET status='applicato', applied_by=$2, applied_at=now() WHERE id=$1").bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?; }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "planning_apply", json!({"id": id, "applied": applied, "errors": errors.len(), "forced": force})).await;
    Ok(Json(json!({"applied": applied, "errors": errors.iter().map(|(o, m)| json!({"order_id": o, "error": m})).collect::<Vec<_>>()})))
}

// --- proposta di un motore esterno (Youco) sullo stesso contratto -----------------------------
#[derive(Deserialize)]
pub struct ExtItem { pub order_id: Uuid, #[serde(default)] pub group_id: Option<Uuid>, #[serde(default)] pub user_id: Option<Uuid>,
    #[serde(default)] pub start: Option<String>, #[serde(default)] pub end: Option<String>, #[serde(default)] pub asset_ids: Option<Vec<Uuid>>,
    #[serde(default)] pub seq: Option<i32>, #[serde(default)] pub reason: Option<String> }
#[derive(Deserialize)]
pub struct ExtReq { pub from: String, pub to: String, pub engine: String, pub items: Vec<ExtItem> }
/// POST /api/planning/runs/external — un motore esterno consegna la sua proposta: YW la valida con i
/// propri vincoli e la tiene come bozza da rivedere e applicare, come quella del motore di base.
pub async fn planning_external(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ExtReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let (from, to) = period(&b.from, &b.to)?;
    let engine = b.engine.trim(); if engine.is_empty() || engine.chars().count() > 60 { return Err(err(StatusCode::BAD_REQUEST, "nome del motore: da 1 a 60 caratteri")); }
    if b.items.is_empty() || b.items.len() > 3000 { return Err(err(StatusCode::BAD_REQUEST, "da 1 a 3000 voci")); }
    let p = load_problem(&st, tid, from, to, true, None).await?;
    let mut seen = HashSet::new();
    let mut items = vec![];
    for (i, x) in b.items.iter().enumerate() {
        if !seen.insert(x.order_id) { return Err(err(StatusCode::BAD_REQUEST, &format!("voce {}: intervento ripetuto", i + 1))); }
        let Some(o) = p.orders.iter().find(|o| o.id == x.order_id) else { return Err(err(StatusCode::BAD_REQUEST, &format!("voce {}: intervento non da pianificare nel periodo", i + 1))) };
        let (a, z) = (x.start.as_deref().map(parse_ts).transpose()?, x.end.as_deref().map(parse_ts).transpose()?);
        if a.is_some() != z.is_some() || matches!((a, z), (Some(a), Some(z)) if z <= a) { return Err(err(StatusCode::BAD_REQUEST, &format!("voce {}: inizio e fine insieme, fine dopo l'inizio", i + 1))); }
        if a.is_some() && x.group_id.is_none() && x.user_id.is_none() { return Err(err(StatusCode::BAD_REQUEST, &format!("voce {}: squadra o persona richiesta", i + 1))); }
        items.push(Item { order: o.id, version: o.version, group: x.group_id, user: x.user_id, start: a, end: z, assets: x.asset_ids.clone().unwrap_or_default(), seq: x.seq,
            travel_min: None, travel_km: None, reason: if a.is_none() { Some(x.reason.clone().unwrap_or_else(|| "Non pianificato dal motore esterno.".into())) } else { None },
            previous: json!({"start": o.pref, "end": o.prev_end, "group_id": o.prev_group, "user_id": o.prev_user}), violations: json!([]) });
    }
    let summary = summarize(&st, tid, &p, &mut items, None).await?;
    let run = save_run(&st, tid, s.user_id, "esterno", engine, from, to, json!({"via_key": s.via_key.is_some()}), summary, &items).await?;
    audit(&st, tid, &s.email, "planning_external", json!({"id": run, "engine": engine, "items": items.len()})).await;
    run_detail(&st, tid, run).await.map(Json)
}

// --- prova sullo storico ---------------------------------------------------------------------
/// POST /api/planning/backtest — ripianifica gli interventi completati di un periodo passato come se
/// fosse il suo primo giorno e confronta con il piano fatto a mano: vincoli rigidi (anche del piano a
/// mano), spostamenti, straordinario. Sola lettura: non salva nulla.
pub async fn planning_backtest(State(st): State<AppState>, h: HeaderMap, Json(b): Json<RunReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_any_cap(&s, &["plan", "orders.view_all"])?;
    let (from, to) = period(&b.from, &b.to)?;
    if to > st.clock.now() { return Err(err(StatusCode::BAD_REQUEST, "la prova sullo storico riguarda solo periodi passati")); }
    let p = load_problem_ex(&st, tid, from, to, true, b.group_ids.as_deref(), true).await?;
    let mut e = Engine::new(&p);
    let unplanned = e.solve(std::time::Duration::from_secs(20));
    let mut items: Vec<Item> = vec![];
    for ((ti, di), r) in &e.routes {
        let seq: Vec<usize> = r.iter().map(|x| x.o).collect();
        let Ok(ev) = e.eval_route(*ti, *di, &seq, &e.assets_of(*ti, *di)) else { continue };
        for (k, stp) in ev.stops.iter().enumerate() { let o = &p.orders[stp.o];
            items.push(Item { order: o.id, version: o.version, group: Some(p.teams[*ti].id), user: None, start: Some(stp.start), end: Some(stp.end), assets: stp.assets.clone(),
                seq: Some(k as i32 + 1), travel_min: Some(ev.legs[k].0 as i32), travel_km: Some(ev.legs[k].1), reason: None, previous: json!({}), violations: json!([]) }); }
    }
    for (oi, code) in &unplanned { let o = &p.orders[*oi]; items.push(Item { order: o.id, version: o.version, group: None, user: None, start: None, end: None, assets: vec![],
        seq: None, travel_min: None, travel_km: None, reason: Some(reason_text(&p, code)), previous: json!({}), violations: json!([]) }); }
    let mut summary = summarize(&st, tid, &p, &mut items, Some(&e)).await?;
    // vincoli rigidi del piano fatto a mano (stessi controlli)
    let mut cur_hard = 0;
    for o in &p.orders { if let (Some(g), Some(a)) = (o.prev_group, o.pref) {
        let plan = OrderPlan { order_id: Some(o.id), site_id: o.site, activity_type_id: o.at, start: Some(a), end: o.prev_end, user: None, group: Some(g), asset_ids: Some(o.fixed_assets.clone()) };
        if check_order(&st, tid, &plan).await?.iter().any(|v| v.hard && v.code != "asset_busy") { cur_hard += 1; } } }
    summary["current_hard"] = json!(cur_hard);
    summary["unplanned_reasons"] = json!(items.iter().filter_map(|i| i.reason.clone()).collect::<Vec<_>>());
    Ok(Json(summary))
}

// --- contratto dati per motori esterni --------------------------------------------------------
#[derive(Deserialize)]
pub struct ProblemQ { pub from: String, pub to: String, #[serde(default)] pub replan: Option<bool> }
/// GET /api/planning/problem — il problema del periodo nel contratto `yw-planning/1`.
pub async fn planning_problem(State(st): State<AppState>, h: HeaderMap, Query(q): Query<ProblemQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let (from, to) = period(&q.from, &q.to)?;
    let p = load_problem(&st, tid, from, to, q.replan.unwrap_or(false), None).await?;
    let ds = |d: &NaiveDate| d.to_string();
    let wbad = |o: &Ord| -> Vec<String> {
        if !o.wlim.any() { return vec![]; }
        let Some(sl) = o.site.and_then(|s| p.site_zone.get(&s)).and_then(|z| p.weather.get(z)) else { return vec![] };
        p.days.iter().filter(|d| weather_issue(sl, &o.wlim, local(&p, **d, p.s.day_start), local(&p, **d, p.s.day_end)).is_some()).map(ds).collect() };
    Ok(Json(json!({
        "contract": CONTRACT, "timezone": p.tz.name(), "from": from, "to": to, "now": p.now,
        "settings": {"day_start": p.s.day_start.format("%H:%M").to_string(), "day_end": p.s.day_end.format("%H:%M").to_string(), "workdays": mask_str(&p.s.workdays),
            "max_overtime_minutes": p.s.overtime, "weather_policy": if p.s.weather_avoid { "evita" } else { "ignora" },
            "travel": {"model": "linea d'aria × fattore / velocità + sistemazione", "kmh": p.s.kmh, "detour_factor": p.s.detour, "setup_minutes": p.s.setup},
            "depot": p.depot.map(|(a, b)| json!({"lat": a, "lon": b}))},
        "days": p.days.iter().map(ds).collect::<Vec<_>>(),
        "teams": p.teams.iter().map(|t| json!({"id": t.id, "name": t.name, "depot": t.depot.map(|(a, b)| json!({"lat": a, "lon": b})), "members": t.members})).collect::<Vec<_>>(),
        "people": p.teams.iter().flat_map(|t| t.members.iter()).collect::<HashSet<_>>().into_iter().map(|u| json!({"id": u,
            "qualifications": p.quals.get(u).map(|v| v.iter().map(|(q, e)| json!({"id": q, "name": p.qual_names.get(q), "expires_at": e})).collect::<Vec<_>>()).unwrap_or_default(),
            "absences": p.absences.get(u).map(|v| v.iter().map(|(a, z)| json!({"from": a, "to": z})).collect::<Vec<_>>()).unwrap_or_default()})).collect::<Vec<_>>(),
        "orders": p.orders.iter().map(|o| json!({"id": o.id, "version": o.version, "title": o.title, "site_id": o.site, "site": o.site_name,
            "location": o.loc.map(|(a, b)| json!({"lat": a, "lon": b})), "access_windows": o.windows, "activity_type_id": o.at, "duration_minutes": o.dur,
            "crew_size": o.crew, "qualifications": o.quals.iter().map(|(q, n)| json!({"id": q, "name": n})).collect::<Vec<_>>(),
            "asset_kinds": o.kinds.iter().map(|(k, n)| json!({"kind": k, "qty": n})).collect::<Vec<_>>(), "priority": o.priority,
            "preferred_start": o.pref, "current_end": o.prev_end, "current_group_id": o.prev_group, "fixed": o.fixed, "fixed_asset_ids": o.fixed_assets,
            "weather_bad_days": wbad(o)})).collect::<Vec<_>>(),
        "assets": p.assets.iter().map(|a| json!({"id": a.id, "name": a.name, "kind": a.kind, "required_qualification_id": a.qual,
            "busy": a.busy.iter().map(|(x, y)| json!({"from": x, "to": y})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    })))
}

// --- impostazioni e blocco ---------------------------------------------------------------------
pub async fn planning_settings(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_any_cap(&s, &["plan", "settings.manage"])?;
    let x = load_settings(&st, tid).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let teams = sqlx::query("SELECT g.id,g.name,g.depot_site_id,s.name AS depot FROM app.user_groups g LEFT JOIN app.sites s ON s.id=g.depot_site_id WHERE g.kind='squadra' ORDER BY g.name")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"depot_site_id": x.depot, "day_start": x.day_start.format("%H:%M").to_string(), "day_end": x.day_end.format("%H:%M").to_string(),
        "workdays": mask_str(&x.workdays), "travel_kmh": x.kmh, "detour_factor": x.detour, "setup_minutes": x.setup, "max_overtime_minutes": x.overtime,
        "weather_policy": if x.weather_avoid { "evita" } else { "ignora" },
        "teams": teams.iter().map(|t| json!({"id": t.get::<Uuid,_>("id"), "name": t.get::<String,_>("name"), "depot_site_id": t.try_get::<Option<Uuid>,_>("depot_site_id").ok().flatten(),
            "depot": t.try_get::<Option<String>,_>("depot").ok().flatten()})).collect::<Vec<_>>()})))
}
#[derive(Deserialize)]
pub struct SettingsReq { #[serde(default)] pub depot_site_id: Option<Uuid>, pub day_start: String, pub day_end: String, pub workdays: String,
    pub travel_kmh: i32, pub detour_factor: f64, pub setup_minutes: i32, pub max_overtime_minutes: i32, pub weather_policy: String,
    #[serde(default)] pub team_depots: Option<Vec<TeamDepot>> }
#[derive(Deserialize)]
pub struct TeamDepot { pub group_id: Uuid, #[serde(default)] pub depot_site_id: Option<Uuid> }
pub async fn put_planning_settings(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SettingsReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_any_cap(&s, &["plan", "settings.manage"])?;
    let (Some(a), Some(z)) = (hhmm(&b.day_start), hhmm(&b.day_end)) else { return Err(err(StatusCode::BAD_REQUEST, "orari nel formato HH:MM")) };
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "la giornata deve finire dopo l'inizio")); }
    if b.workdays.len() != 7 || !b.workdays.chars().all(|c| c == '0' || c == '1') || !b.workdays.contains('1') { return Err(err(StatusCode::BAD_REQUEST, "giorni lavorativi: 7 cifre 0/1, almeno un giorno")); }
    if !(5..=120).contains(&b.travel_kmh) || !(1.0..=3.0).contains(&b.detour_factor) || !(0..=60).contains(&b.setup_minutes) || !(0..=240).contains(&b.max_overtime_minutes) {
        return Err(err(StatusCode::BAD_REQUEST, "velocità 5–120 km/h, fattore di percorso 1–3, sistemazione 0–60 min, straordinario 0–240 min")); }
    if !["evita", "ignora"].contains(&b.weather_policy.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "meteo: evita o ignora")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.planning_settings(tenant_id,depot_site_id,day_start,day_end,workdays,travel_kmh,detour_factor,setup_minutes,max_overtime_minutes,weather_policy)
            VALUES($1,$2,$3,$4,$5,$6,$7::numeric,$8,$9,$10)
         ON CONFLICT (tenant_id) DO UPDATE SET depot_site_id=EXCLUDED.depot_site_id, day_start=EXCLUDED.day_start, day_end=EXCLUDED.day_end, workdays=EXCLUDED.workdays,
            travel_kmh=EXCLUDED.travel_kmh, detour_factor=EXCLUDED.detour_factor, setup_minutes=EXCLUDED.setup_minutes, max_overtime_minutes=EXCLUDED.max_overtime_minutes,
            weather_policy=EXCLUDED.weather_policy, updated_at=now()")
        .bind(tid).bind(b.depot_site_id).bind(a).bind(z).bind(&b.workdays).bind(b.travel_kmh).bind(b.detour_factor).bind(b.setup_minutes).bind(b.max_overtime_minutes).bind(&b.weather_policy)
        .execute(&mut *tx).await.map_err(|e| crate::api::conflict_or(e, "deposito inesistente"))?;
    for t in b.team_depots.as_deref().unwrap_or(&[]) {
        sqlx::query("UPDATE app.user_groups SET depot_site_id=$2 WHERE id=$1").bind(t.group_id).bind(t.depot_site_id).execute(&mut *tx).await
            .map_err(|e| crate::api::conflict_or(e, "deposito inesistente"))?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "planning_settings", json!({})).await;
    Ok(Json(json!({"ok": true})))
}
#[derive(Deserialize)]
pub struct LockReq { pub locked: bool }
/// POST /api/orders/:id/plan-lock — l'intervento resta dov'è: la pianificazione automatica non lo sposta.
pub async fn order_plan_lock(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<LockReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "pianificazione").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.work_orders SET plan_locked=$2 WHERE id=$1").bind(id).bind(b.locked).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "intervento inesistente")); }
    Ok(Json(json!({"ok": true, "locked": b.locked})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn travel_model() {
        let s = Settings::default();
        // Duomo → Sesto San Giovanni ≈ 7,9 km in linea d'aria → ×1,3 ≈ 10,3 km → 25 km/h ≈ 25 min + 5
        let (m, km) = travel_min(&s, (45.4642, 9.1900), (45.5340, 9.2300));
        assert!((9.5..11.0).contains(&km), "{km}"); assert!((28..=32).contains(&m), "{m}");
        assert_eq!(travel_min(&s, (45.0, 9.0), (45.0, 9.0)), (0, 0.0));
    }
    #[test] fn masks() { assert_eq!(mask_str(&wd_mask("1111100")), "1111100"); }
}
