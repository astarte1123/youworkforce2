//! Fase 1 «Lavoro sul campo a scala reale» (primo caso reale: Idealverde, 60+ giardinieri in
//! squadre). Migrazione 0165. Qui:
//! - catalogo dei TIPI DI ATTIVITÀ (durata standard, persone, qualifiche e mezzi richiesti);
//! - MEZZI E ATTREZZATURE (modulo `mezzi`): stato, scadenze, prenotazioni senza sovrapposizioni;
//! - SCADENZIARIO delle qualifiche;
//! - CONTROLLO DEI VINCOLI di un intervento (finestra oraria della sede, qualifiche, persone
//!   disponibili, assenze, mezzi, sovrapposizioni). Lo stesso controllo vale per la pianificazione
//!   a mano, per i suggerimenti e per qualunque pianificatore esterno: YW resta l'unico arbitro dei
//!   vincoli rigidi. Le violazioni usano il contratto di `scheduling.rs` (dura = blocca salvo
//!   «Forza», avviso = si procede consapevoli), con un messaggio in chiaro.

use axum::{extract::{State, Path, Query}, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Datelike, Utc, TimeZone as _};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, require_any_cap, require_module, module_active, is_planner, ise, err, audit, parse_ts, parse_tz};
use crate::scheduling::Violation;
use crate::db;

pub const UNITS: &[(&str, &str)] = &[("intervento", "a intervento"), ("m2", "m²"), ("ml", "metri lineari"), ("n", "numero"), ("ore", "ore")];
pub const DEADLINE_KINDS: &[&str] = &["revisione", "tagliando", "assicurazione", "verifica", "altro"];

fn v(code: &'static str, hard: bool, message: String) -> Violation { Violation { code, hard, message } }
fn hhmm(s: &str) -> Option<chrono::NaiveTime> { chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M").ok() }
pub(crate) fn force(h: &HeaderMap) -> bool { h.get("x-force").map(|v| v == "1" || v == "true").unwrap_or(false) }

/// Durata stimata: base + minuti per unità × quantità (mai meno di 1 minuto).
pub(crate) fn estimate_minutes(base: i32, per_unit: f64, qty: Option<f64>) -> i32 {
    (base as f64 + per_unit * qty.unwrap_or(0.0)).round().max(1.0).min(10080.0) as i32
}

// =============================================================================================
// Tipi di attività
// =============================================================================================
#[derive(Deserialize)]
pub struct ActivityAssetReq { pub kind: String, #[serde(default)] pub qty: Option<i32> }
#[derive(Deserialize)]
pub struct ActivityTypeReq {
    pub name: String, #[serde(default)] pub unit: Option<String>, #[serde(default)] pub base_minutes: Option<i32>,
    #[serde(default)] pub minutes_per_unit: Option<f64>, #[serde(default)] pub crew_size: Option<i32>,
    #[serde(default)] pub active: Option<bool>, #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub qualification_ids: Option<Vec<Uuid>>, #[serde(default)] pub assets: Option<Vec<ActivityAssetReq>> }

fn validate_activity(b: &ActivityTypeReq) -> Result<(), (StatusCode, String)> {
    let n = b.name.trim();
    if n.chars().count() < 2 || n.chars().count() > 120 { return Err(err(StatusCode::BAD_REQUEST, "nome del tipo di attività: da 2 a 120 caratteri")); }
    if let Some(u) = b.unit.as_deref() { if !UNITS.iter().any(|(c, _)| *c == u) {
        return Err(err(StatusCode::BAD_REQUEST, "unità non valida: intervento, m2, ml, n o ore")); } }
    if let Some(m) = b.base_minutes { if !(0..=1440).contains(&m) { return Err(err(StatusCode::BAD_REQUEST, "durata base tra 0 e 1440 minuti")); } }
    if let Some(m) = b.minutes_per_unit { if !(0.0..=600.0).contains(&m) { return Err(err(StatusCode::BAD_REQUEST, "minuti per unità tra 0 e 600")); } }
    if let Some(c) = b.crew_size { if !(1..=50).contains(&c) { return Err(err(StatusCode::BAD_REQUEST, "persone necessarie tra 1 e 50")); } }
    for a in b.assets.as_deref().unwrap_or(&[]) {
        let k = a.kind.trim();
        if k.is_empty() || k.chars().count() > 60 { return Err(err(StatusCode::BAD_REQUEST, "tipo di mezzo richiesto non valido")); }
        if let Some(q) = a.qty { if !(1..=20).contains(&q) { return Err(err(StatusCode::BAD_REQUEST, "quantità di mezzi tra 1 e 20")); } }
    }
    Ok(())
}

pub async fn list_activity_types(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,name,unit,base_minutes,minutes_per_unit::float8 AS mpu,crew_size,active,note FROM app.activity_types ORDER BY active DESC, name")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    let quals = sqlx::query("SELECT aq.activity_type_id,q.id,q.name FROM app.activity_type_qualifications aq JOIN app.qualifications q ON q.id=aq.qualification_id ORDER BY q.name")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    let assets = sqlx::query("SELECT activity_type_id,asset_kind,qty FROM app.activity_type_assets ORDER BY asset_kind").fetch_all(&mut *tx).await.map_err(ise)?;
    // durata reale media (minuti lavorati negli interventi completati) accanto a quella standard
    let real = sqlx::query("SELECT activity_type_id, count(*)::int AS n, round(avg(work_minutes))::int AS avg_min
        FROM app.work_orders WHERE activity_type_id IS NOT NULL AND state='completato' AND work_minutes>0 GROUP BY activity_type_id")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let real_m: HashMap<Uuid, (i32, i32)> = real.iter().map(|r| (r.get("activity_type_id"), (r.get("n"), r.try_get("avg_min").unwrap_or(0)))).collect();
    let out: Vec<Value> = rows.iter().map(|r| {
        let id: Uuid = r.get("id");
        let q: Vec<Value> = quals.iter().filter(|x| x.get::<Uuid,_>("activity_type_id") == id)
            .map(|x| json!({"id": x.get::<Uuid,_>("id"), "name": x.get::<String,_>("name")})).collect();
        let a: Vec<Value> = assets.iter().filter(|x| x.get::<Uuid,_>("activity_type_id") == id)
            .map(|x| json!({"kind": x.get::<String,_>("asset_kind"), "qty": x.get::<i32,_>("qty")})).collect();
        let (n, avg) = real_m.get(&id).cloned().unwrap_or((0, 0));
        json!({"id": id, "name": r.get::<String,_>("name"), "unit": r.get::<String,_>("unit"),
            "base_minutes": r.get::<i32,_>("base_minutes"), "minutes_per_unit": r.get::<f64,_>("mpu"),
            "crew_size": r.get::<i32,_>("crew_size"), "active": r.get::<bool,_>("active"),
            "note": r.try_get::<String,_>("note").ok(), "qualifications": q, "assets": a,
            "done_count": n, "real_avg_minutes": if n > 0 { Some(avg) } else { None }})
    }).collect();
    Ok(Json(json!({"units": UNITS.iter().map(|(c, l)| json!({"code": c, "label": l})).collect::<Vec<_>>(), "items": out})))
}

async fn save_activity_links(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, id: Uuid, b: &ActivityTypeReq) -> Result<(), (StatusCode, String)> {
    if let Some(qs) = &b.qualification_ids {
        sqlx::query("DELETE FROM app.activity_type_qualifications WHERE activity_type_id=$1").bind(id).execute(&mut **tx).await.map_err(ise)?;
        for q in qs {
            sqlx::query("INSERT INTO app.activity_type_qualifications(tenant_id,activity_type_id,qualification_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(tid).bind(id).bind(q).execute(&mut **tx).await
                .map_err(|e| crate::api::conflict_or(e, "qualifica inesistente"))?;
        }
    }
    if let Some(as_) = &b.assets {
        sqlx::query("DELETE FROM app.activity_type_assets WHERE activity_type_id=$1").bind(id).execute(&mut **tx).await.map_err(ise)?;
        for a in as_ {
            sqlx::query("INSERT INTO app.activity_type_assets(tenant_id,activity_type_id,asset_kind,qty) VALUES($1,$2,$3,$4)
                         ON CONFLICT (activity_type_id,asset_kind) DO UPDATE SET qty=EXCLUDED.qty")
                .bind(tid).bind(id).bind(a.kind.trim().to_lowercase()).bind(a.qty.unwrap_or(1)).execute(&mut **tx).await.map_err(ise)?;
        }
    }
    Ok(())
}

pub async fn create_activity_type(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ActivityTypeReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    validate_activity(&b)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.activity_types(tenant_id,name,unit,base_minutes,minutes_per_unit,crew_size,active,note)
            VALUES($1,$2,COALESCE($3,'intervento'),COALESCE($4,60),COALESCE($5,0)::numeric,COALESCE($6,1),COALESCE($7,true),$8) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.unit).bind(b.base_minutes).bind(b.minutes_per_unit).bind(b.crew_size).bind(b.active).bind(&b.note)
        .fetch_one(&mut *tx).await.map_err(|e| crate::api::conflict_or(e, "esiste già un tipo di attività con questo nome"))?;
    save_activity_links(&mut tx, tid, id, &b).await?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "activity_type_create", json!({"id": id, "name": b.name.trim()})).await;
    Ok(Json(json!({"id": id})))
}

pub async fn update_activity_type(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ActivityTypeReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    validate_activity(&b)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.activity_types SET name=$2, unit=COALESCE($3,unit), base_minutes=COALESCE($4,base_minutes),
            minutes_per_unit=COALESCE($5::numeric,minutes_per_unit), crew_size=COALESCE($6,crew_size), active=COALESCE($7,active),
            note=COALESCE($8,note) WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(&b.unit).bind(b.base_minutes).bind(b.minutes_per_unit).bind(b.crew_size).bind(b.active).bind(&b.note)
        .execute(&mut *tx).await.map_err(|e| crate::api::conflict_or(e, "esiste già un tipo di attività con questo nome"))?.rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "tipo di attività inesistente")); }
    save_activity_links(&mut tx, tid, id, &b).await?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "activity_type_update", json!({"id": id})).await;
    Ok(Json(json!({"ok": true})))
}

pub async fn delete_activity_type(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // usato da interventi o piani: non si elimina (si disattiva), così lo storico resta leggibile
    let used: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM app.work_orders WHERE activity_type_id=$1)+(SELECT count(*) FROM app.maintenance_plans WHERE activity_type_id=$1)")
        .bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    if used > 0 { return Err(err(StatusCode::CONFLICT, &format!("tipo di attività usato da {used} interventi o piani: disattivalo invece di eliminarlo"))); }
    let n = sqlx::query("DELETE FROM app.activity_types WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "tipo di attività inesistente")); }
    audit(&st, tid, &s.email, "activity_type_delete", json!({"id": id})).await;
    Ok(Json(json!({"ok": true})))
}

// =============================================================================================
// Finestre orarie della sede
// =============================================================================================
/// `[{"days":"1111100","from":"08:00","to":"17:00"}]`: valida e normalizza (vuoto = sempre).
pub(crate) fn parse_windows(v: &Value) -> Result<Value, (StatusCode, String)> {
    let arr = v.as_array().ok_or_else(|| err(StatusCode::BAD_REQUEST, "finestre orarie: elenco di {days, from, to}"))?;
    if arr.len() > 14 { return Err(err(StatusCode::BAD_REQUEST, "al massimo 14 finestre orarie per sede")); }
    let mut out = Vec::new();
    for w in arr {
        let days = w.get("days").and_then(|x| x.as_str()).unwrap_or("");
        let from = w.get("from").and_then(|x| x.as_str()).unwrap_or("");
        let to = w.get("to").and_then(|x| x.as_str()).unwrap_or("");
        if days.len() != 7 || !days.chars().all(|c| c == '0' || c == '1') || !days.contains('1') {
            return Err(err(StatusCode::BAD_REQUEST, "giorni della finestra: 7 cifre 0/1 da lunedì a domenica, almeno un giorno"));
        }
        let (Some(f), Some(t)) = (hhmm(from), hhmm(to)) else {
            return Err(err(StatusCode::BAD_REQUEST, "orari della finestra nel formato HH:MM")); };
        if t <= f { return Err(err(StatusCode::BAD_REQUEST, "la finestra deve finire dopo l'inizio (stesso giorno)")); }
        out.push(json!({"days": days, "from": from, "to": to}));
    }
    Ok(json!(out))
}
fn windows_label(ws: &Value) -> String {
    const D: [&str; 7] = ["lun", "mar", "mer", "gio", "ven", "sab", "dom"];
    ws.as_array().map(|a| a.iter().map(|w| {
        let days = w["days"].as_str().unwrap_or("");
        let ds: Vec<&str> = days.chars().enumerate().filter(|(_, c)| *c == '1').map(|(i, _)| D[i]).collect();
        let dl = if days == "1111100" { "lun–ven".to_string() } else if days == "1111111" { "tutti i giorni".to_string() } else { ds.join(", ") };
        format!("{dl} {}–{}", w["from"].as_str().unwrap_or(""), w["to"].as_str().unwrap_or(""))
    }).collect::<Vec<_>>().join("; ")).unwrap_or_default()
}
/// L'intervallo [start,end) sta dentro una finestra della sede (ora locale della sede)?
pub(crate) fn within_windows(ws: &Value, tz: chrono_tz::Tz, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    let arr = match ws.as_array() { Some(a) if !a.is_empty() => a, _ => return true };
    let (ls, le) = (start.with_timezone(&tz), end.with_timezone(&tz));
    if ls.date_naive() != le.date_naive() && !(le.time() == chrono::NaiveTime::MIN && le.date_naive() == ls.date_naive().succ_opt().unwrap_or(ls.date_naive())) { return false; }
    let wd = ls.weekday().num_days_from_monday() as usize;
    arr.iter().any(|w| {
        let days = w["days"].as_str().unwrap_or("");
        let (Some(f), Some(t)) = (hhmm(w["from"].as_str().unwrap_or("")), hhmm(w["to"].as_str().unwrap_or(""))) else { return false };
        days.chars().nth(wd) == Some('1') && ls.time() >= f && (le.time() <= t && le.date_naive() == ls.date_naive())
    })
}

// =============================================================================================
// Controllo dei vincoli di un intervento
// =============================================================================================
/// Un intervento come sarebbe dopo la modifica (pianificazione a mano, suggerimento o proposta esterna).
#[derive(Default, Clone)]
pub struct OrderPlan {
    pub order_id: Option<Uuid>, pub site_id: Option<Uuid>, pub activity_type_id: Option<Uuid>,
    pub start: Option<DateTime<Utc>>, pub end: Option<DateTime<Utc>>,
    pub user: Option<Uuid>, pub group: Option<Uuid>,
    /// mezzi prenotati (o da prenotare) per l'intervento; None = quelli già prenotati
    pub asset_ids: Option<Vec<Uuid>>,
}

fn dm(t: DateTime<Utc>, tz: chrono_tz::Tz) -> String { t.with_timezone(&tz).format("%d/%m %H:%M").to_string() }

/// Controlla i vincoli dell'intervento. Dure: fuori finestra della sede, nessuno con la qualifica
/// richiesta (dall'attività o dal mezzo), assegnatario assente, mezzo fermo o dismesso. Avvisi:
/// squadra sotto il numero di persone richiesto, membri assenti, mezzi richiesti non prenotati,
/// scadenza del mezzo superata, sovrapposizione con altri interventi della stessa squadra/persona.
pub(crate) async fn check_order(st: &AppState, tid: Uuid, p: &OrderPlan) -> Result<Vec<Violation>, (StatusCode, String)> {
    let mut out: Vec<Violation> = Vec::new();
    let tz = crate::field::tenant_tz(st, tid).await;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let when = p.start.unwrap_or_else(|| st.clock.now());
    let (ws, we) = match (p.start, p.end) { (Some(a), Some(b)) => (a, b), (Some(a), None) => (a, a + chrono::Duration::hours(1)), _ => (when, when) };

    // --- finestra oraria della sede (dura) ---
    let mut site_tz = tz;
    if let (Some(sid), Some(a), Some(b)) = (p.site_id, p.start, p.end) {
        if let Some(r) = sqlx::query("SELECT name,access_windows,timezone FROM app.sites WHERE id=$1").bind(sid).fetch_optional(&mut *tx).await.map_err(ise)? {
            let w: Value = r.try_get("access_windows").unwrap_or(json!([]));
            if let Some(z) = r.try_get::<Option<String>,_>("timezone").ok().flatten().and_then(|z| parse_tz(&z)) { site_tz = z; }
            if !within_windows(&w, site_tz, a, b) {
                out.push(v("site_window", true, format!("Fuori dall'orario della sede «{}»: {}.", r.get::<String,_>("name"), windows_label(&w))));
            }
        }
    }

    // --- persone coinvolte ---
    let people: Vec<(Uuid, String)> = if let Some(u) = p.user {
        sqlx::query("SELECT id,email FROM app.users WHERE id=$1").bind(u).fetch_all(&mut *tx).await.map_err(ise)?
    } else if let Some(g) = p.group {
        sqlx::query("SELECT u.id,u.email FROM app.user_group_members m JOIN app.users u ON u.id=m.user_id WHERE m.group_id=$1 AND u.anonymized_at IS NULL ORDER BY u.email")
            .bind(g).fetch_all(&mut *tx).await.map_err(ise)?
    } else { vec![] }.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email"))).collect();
    let group_name: Option<String> = match p.group { Some(g) => sqlx::query_scalar("SELECT name FROM app.user_groups WHERE id=$1").bind(g).fetch_optional(&mut *tx).await.map_err(ise)?, None => None };
    let who = group_name.clone().map(|g| format!("«{g}»")).unwrap_or_else(|| people.first().map(|x| x.1.clone()).unwrap_or_else(|| "l'assegnatario".into()));
    let ids: Vec<Uuid> = people.iter().map(|x| x.0).collect();

    // --- assenze ---
    let absent: Vec<Uuid> = if !ids.is_empty() && p.start.is_some() {
        sqlx::query_scalar("SELECT DISTINCT user_id FROM app.worker_absences WHERE user_id = ANY($1) AND starts_at < $3 AND ends_at > $2")
            .bind(&ids).bind(ws).bind(we).fetch_all(&mut *tx).await.map_err(ise)?
    } else { vec![] };
    if p.user.is_some() && !absent.is_empty() {
        out.push(v("absence", true, format!("{who} è assente in quel periodo.")));
    } else if p.group.is_some() && !absent.is_empty() {
        let names: Vec<String> = people.iter().filter(|x| absent.contains(&x.0)).map(|x| x.1.split('@').next().unwrap_or("").to_string()).collect();
        out.push(v("absence_members", false, format!("Assenti in {who}: {}.", names.join(", "))));
    }
    let available: Vec<Uuid> = ids.iter().filter(|u| !absent.contains(u)).cloned().collect();

    // qualifica valida a quella data tra le persone disponibili
    let quals_on = module_active(st, tid, "qualifiche").await;
    async fn holders(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, q: Uuid, people: &[Uuid], at: DateTime<Utc>) -> Result<i64, (StatusCode, String)> {
        sqlx::query_scalar("SELECT count(DISTINCT user_id) FROM app.user_qualifications WHERE qualification_id=$1 AND user_id = ANY($2) AND (expires_at IS NULL OR expires_at > $3)")
            .bind(q).bind(people).bind(at).fetch_one(&mut **tx).await.map_err(ise)
    }

    // --- tipo di attività: qualifiche, persone necessarie, mezzi richiesti ---
    let mut need_kinds: Vec<(String, i32)> = vec![];
    if let Some(at) = p.activity_type_id {
        if let Some(r) = sqlx::query("SELECT name,crew_size FROM app.activity_types WHERE id=$1").bind(at).fetch_optional(&mut *tx).await.map_err(ise)? {
            let crew: i32 = r.get("crew_size");
            if p.group.is_some() && (available.len() as i32) < crew {
                out.push(v("crew", false, format!("«{}» richiede {crew} persone: in {who} ne sono disponibili {}.", r.get::<String,_>("name"), available.len())));
            }
            if quals_on && !available.is_empty() {
                let qs = sqlx::query("SELECT q.id,q.name FROM app.activity_type_qualifications aq JOIN app.qualifications q ON q.id=aq.qualification_id WHERE aq.activity_type_id=$1 ORDER BY q.name")
                    .bind(at).fetch_all(&mut *tx).await.map_err(ise)?;
                for q in &qs {
                    if holders(&mut tx, q.get("id"), &available, when).await? == 0 {
                        let (qn, day) = (q.get::<String,_>("name"), dm(when, tz).split(' ').next().unwrap_or("").to_string());
                        out.push(v("qualification", true, if p.group.is_some() { format!("Nessuno in {who} ha «{qn}» valida il {day}.") } else { format!("{who} non ha «{qn}» valida il {day}.") }));
                    }
                }
            }
            need_kinds = sqlx::query("SELECT asset_kind,qty FROM app.activity_type_assets WHERE activity_type_id=$1").bind(at)
                .fetch_all(&mut *tx).await.map_err(ise)?.iter().map(|x| (x.get::<String,_>("asset_kind"), x.get::<i32,_>("qty"))).collect();
        }
    }

    // --- mezzi ---
    if module_active(st, tid, "mezzi").await {
        let asset_ids: Vec<Uuid> = match &p.asset_ids {
            Some(a) => a.clone(),
            None => match p.order_id { Some(o) => sqlx::query_scalar("SELECT asset_id FROM app.asset_bookings WHERE work_order_id=$1").bind(o).fetch_all(&mut *tx).await.map_err(ise)?, None => vec![] },
        };
        let assets = sqlx::query("SELECT a.id,a.name,lower(a.kind) AS kind,a.status,a.required_qualification_id,q.name AS qname,
                (SELECT min(d.due_date) FROM app.asset_deadlines d WHERE d.asset_id=a.id AND d.done_at IS NULL) AS next_due,
                (SELECT d.kind FROM app.asset_deadlines d WHERE d.asset_id=a.id AND d.done_at IS NULL ORDER BY d.due_date LIMIT 1) AS next_kind
             FROM app.assets a LEFT JOIN app.qualifications q ON q.id=a.required_qualification_id WHERE a.id = ANY($1)")
            .bind(&asset_ids).fetch_all(&mut *tx).await.map_err(ise)?;
        for a in &assets {
            let name: String = a.get("name");
            let status: String = a.get("status");
            if status != "disponibile" { out.push(v("asset_down", true, format!("Il mezzo «{name}» è {status}."))); }
            if let (Some(due), Some(k)) = (a.try_get::<Option<chrono::NaiveDate>,_>("next_due").ok().flatten(), a.try_get::<Option<String>,_>("next_kind").ok().flatten()) {
                if due < when.with_timezone(&tz).date_naive() {
                    out.push(v("asset_deadline", false, format!("Il mezzo «{name}» ha {k} scaduta il {}.", due.format("%d/%m/%Y"))));
                }
            }
            if quals_on && !available.is_empty() {
                if let Some(q) = a.try_get::<Option<Uuid>,_>("required_qualification_id").ok().flatten() {
                    if holders(&mut tx, q, &available, when).await? == 0 {
                        let qn = a.try_get::<String,_>("qname").unwrap_or_default();
                        out.push(v("asset_qualification", true, if p.group.is_some() { format!("Il mezzo «{name}» richiede «{qn}»: nessuno in {who} ce l'ha valida.") }
                            else { format!("Il mezzo «{name}» richiede «{qn}»: {who} non ce l'ha valida.") }));
                    }
                }
            }
            // sovrapposizione con prenotazioni di altri interventi (il vincolo del DB la impedisce in scrittura)
            if p.start.is_some() {
                let clash: Option<String> = sqlx::query_scalar("SELECT COALESCE(o.title,'prenotazione') FROM app.asset_bookings b LEFT JOIN app.work_orders o ON o.id=b.work_order_id
                        WHERE b.asset_id=$1 AND tstzrange(b.starts_at,b.ends_at) && tstzrange($2,$3) AND ($4::uuid IS NULL OR b.work_order_id IS DISTINCT FROM $4) LIMIT 1")
                    .bind(a.get::<Uuid,_>("id")).bind(ws).bind(we).bind(p.order_id).fetch_optional(&mut *tx).await.map_err(ise)?;
                if let Some(c) = clash { out.push(v("asset_busy", true, format!("Il mezzo «{name}» è già prenotato in quel periodo («{c}»)."))); }
            }
        }
        let mut missing: Vec<String> = vec![];
        for (k, q) in &need_kinds {
            let have = assets.iter().filter(|a| a.get::<String,_>("kind") == *k).count() as i32;
            if have < *q { missing.push(format!("{k} ×{}", q - have)); }
        }
        if !missing.is_empty() { out.push(v("asset_missing", false, format!("Mezzi richiesti non prenotati: {}.", missing.join(", ")))); }
    }

    // --- sovrapposizione con altri interventi della stessa persona o squadra (avviso) ---
    if let (Some(a), Some(b)) = (p.start, p.end) {
        let other: Option<(String, DateTime<Utc>)> = sqlx::query_as("SELECT title, scheduled_start FROM app.work_orders
                WHERE ($1::uuid IS NULL OR id<>$1) AND state NOT IN ('completato','annullato') AND scheduled_start < $3
                  AND COALESCE(scheduled_end, scheduled_start + interval '1 hour') > $2
                  AND (($4::uuid IS NOT NULL AND assigned_user=$4) OR ($5::uuid IS NOT NULL AND assigned_group=$5)) ORDER BY scheduled_start LIMIT 1")
            .bind(p.order_id).bind(a).bind(b).bind(p.user).bind(p.group).fetch_optional(&mut *tx).await.map_err(ise)?;
        if let Some((t, s0)) = other { out.push(v("overlap", false, format!("Si sovrappone a «{t}» delle {} per {who}.", dm(s0, tz).split(' ').nth(1).unwrap_or("")))); }
    }
    tx.commit().await.map_err(ise)?;
    Ok(out)
}

/// Esito del controllo per le API: errore 409 con i motivi se c'è un vincolo duro e non si forza.
pub(crate) fn enforce(vs: &[Violation], forced: bool) -> Result<(), (StatusCode, String)> {
    if crate::scheduling::has_hard(vs) && !forced {
        let msg = vs.iter().filter(|x| x.hard).map(|x| x.message.clone()).collect::<Vec<_>>().join(" ");
        return Err(err(StatusCode::CONFLICT, &format!("Conflitto: {msg} (usa Forza per procedere)")));
    }
    Ok(())
}
pub(crate) fn vjson(vs: &[Violation]) -> Value { crate::field::violations_json(vs) }

/// Prenota i mezzi per l'intervento nel suo orario (sostituisce le prenotazioni precedenti
/// dell'intervento). Sovrapposizione → 409 con il nome del mezzo.
pub(crate) async fn book_order_assets(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, order: Uuid, assets: &[Uuid],
    start: DateTime<Utc>, end: DateTime<Utc>, by: Option<Uuid>) -> Result<(), (StatusCode, String)> {
    sqlx::query("DELETE FROM app.asset_bookings WHERE work_order_id=$1").bind(order).execute(&mut **tx).await.map_err(ise)?;
    for a in assets {
        sqlx::query("INSERT INTO app.asset_bookings(tenant_id,asset_id,work_order_id,starts_at,ends_at,created_by) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(tid).bind(a).bind(order).bind(start).bind(end).bind(by).execute(&mut **tx).await
            .map_err(|e| booking_err(e))?;
    }
    Ok(())
}
/// Sposta le prenotazioni dell'intervento al nuovo orario.
pub(crate) async fn move_order_bookings(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, order: Uuid, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<(), (StatusCode, String)> {
    sqlx::query("UPDATE app.asset_bookings SET starts_at=$2, ends_at=$3 WHERE work_order_id=$1").bind(order).bind(start).bind(end)
        .execute(&mut **tx).await.map_err(|e| booking_err(e))?;
    Ok(())
}
fn booking_err(e: sqlx::Error) -> (StatusCode, String) {
    match e.as_database_error().and_then(|d| d.code()) {
        Some(c) if c == "23P01" => err(StatusCode::CONFLICT, "il mezzo è già prenotato in quel periodo"),
        Some(c) if c == "23503" => err(StatusCode::BAD_REQUEST, "mezzo inesistente"),
        _ => crate::api::conflict_or(e, "prenotazione non valida"),
    }
}

#[derive(Deserialize)]
pub struct CheckReq { #[serde(default)] pub order_id: Option<Uuid>, #[serde(default)] pub site_id: Option<Uuid>,
    #[serde(default)] pub activity_type_id: Option<Uuid>, #[serde(default)] pub quantity: Option<f64>,
    #[serde(default)] pub scheduled_start: Option<String>, #[serde(default)] pub scheduled_end: Option<String>,
    #[serde(default)] pub assigned_user: Option<Uuid>, #[serde(default)] pub assigned_group: Option<Uuid>,
    #[serde(default)] pub asset_ids: Option<Vec<Uuid>> }
/// POST /api/orders/check — anteprima dei vincoli senza salvare (modulo intervento, agenda,
/// pianificatori esterni). Restituisce anche la durata stimata dal tipo di attività.
pub async fn order_check(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CheckReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let start = b.scheduled_start.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?;
    let mut end = b.scheduled_end.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?;
    let est = activity_estimate(&st, tid, b.activity_type_id, b.quantity).await?;
    if end.is_none() { if let (Some(a), Some(m)) = (start, est) { end = Some(a + chrono::Duration::minutes(m as i64)); } }
    let p = OrderPlan { order_id: b.order_id, site_id: b.site_id, activity_type_id: b.activity_type_id, start, end,
        user: b.assigned_user, group: b.assigned_group, asset_ids: b.asset_ids.clone() };
    let vs = check_order(&st, tid, &p).await?;
    let hist = match b.activity_type_id { Some(at) if module_active(&st, tid, "analisi").await =>
        Some(crate::analytics::history_estimate(&st, tid, at, b.site_id, b.assigned_group, b.quantity).await?), _ => None };
    Ok(Json(json!({"violations": vjson(&vs), "hard": crate::scheduling::has_hard(&vs), "estimated_minutes": est,
        "scheduled_end": end.map(|e| e.to_rfc3339()), "history_estimate": hist})))
}
/// Durata stimata dal tipo di attività e dalla quantità (None senza tipo).
pub(crate) async fn activity_estimate(st: &AppState, tid: Uuid, at: Option<Uuid>, qty: Option<f64>) -> Result<Option<i32>, (StatusCode, String)> {
    let Some(at) = at else { return Ok(None) };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = sqlx::query("SELECT base_minutes, minutes_per_unit::float8 AS mpu FROM app.activity_types WHERE id=$1").bind(at).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let r = r.ok_or_else(|| err(StatusCode::BAD_REQUEST, "tipo di attività inesistente"))?;
    Ok(Some(estimate_minutes(r.get("base_minutes"), r.get("mpu"), qty)))
}

#[derive(Deserialize)]
pub struct RangeQ { #[serde(default)] pub from: Option<String>, #[serde(default)] pub to: Option<String>,
    #[serde(default)] pub asset_id: Option<Uuid>, #[serde(default)] pub days: Option<i64> }
/// GET /api/orders/checks?from&to — vincoli degli interventi aperti nel periodo (segnali in agenda).
pub async fn order_checks(State(st): State<AppState>, h: HeaderMap, Query(q): Query<RangeQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let from = q.from.as_deref().map(parse_ts).transpose()?.ok_or_else(|| err(StatusCode::BAD_REQUEST, "from richiesto"))?;
    let to = q.to.as_deref().map(parse_ts).transpose()?.ok_or_else(|| err(StatusCode::BAD_REQUEST, "to richiesto"))?;
    if to <= from || to - from > chrono::Duration::days(31) { return Err(err(StatusCode::BAD_REQUEST, "periodo tra 1 minuto e 31 giorni")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,site_id,activity_type_id,scheduled_start,scheduled_end,assigned_user,assigned_group FROM app.work_orders
            WHERE state NOT IN ('completato','annullato') AND scheduled_start >= $1 AND scheduled_start < $2 ORDER BY scheduled_start LIMIT 1500")
        .bind(from).bind(to).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut out = serde_json::Map::new();
    for r in &rows {
        let p = OrderPlan { order_id: Some(r.get("id")), site_id: r.try_get("site_id").ok(), activity_type_id: r.try_get("activity_type_id").ok(),
            start: r.try_get("scheduled_start").ok(), end: r.try_get("scheduled_end").ok(),
            user: r.try_get("assigned_user").ok(), group: r.try_get("assigned_group").ok(), asset_ids: None };
        let vs = check_order(&st, tid, &p).await?;
        if !vs.is_empty() { out.insert(r.get::<Uuid,_>("id").to_string(), vjson(&vs)); }
    }
    Ok(Json(Value::Object(out)))
}

// =============================================================================================
// Mezzi: stato, scadenze, prenotazioni (modulo `mezzi`)
// =============================================================================================
pub async fn list_bookings(State(st): State<AppState>, h: HeaderMap, Query(q): Query<RangeQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "mezzi").await?;
    let from = q.from.as_deref().map(parse_ts).transpose()?.unwrap_or_else(|| st.clock.now() - chrono::Duration::days(1));
    let to = q.to.as_deref().map(parse_ts).transpose()?.unwrap_or_else(|| from + chrono::Duration::days(14));
    if to <= from || to - from > chrono::Duration::days(93) { return Err(err(StatusCode::BAD_REQUEST, "periodo fino a 93 giorni")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT b.id,b.asset_id,a.name AS asset,a.kind,b.work_order_id,o.title AS order_title,b.group_id,g.name AS group_name,
            b.user_id,u.email,b.starts_at,b.ends_at,b.note
         FROM app.asset_bookings b JOIN app.assets a ON a.id=b.asset_id LEFT JOIN app.work_orders o ON o.id=b.work_order_id
         LEFT JOIN app.user_groups g ON g.id=COALESCE(b.group_id,o.assigned_group) LEFT JOIN app.users u ON u.id=COALESCE(b.user_id,o.assigned_user)
         WHERE b.starts_at < $2 AND b.ends_at > $1 AND ($3::uuid IS NULL OR b.asset_id=$3) ORDER BY b.starts_at")
        .bind(from).bind(to).bind(q.asset_id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "asset_id": r.get::<Uuid,_>("asset_id"), "asset": r.get::<String,_>("asset"), "kind": r.get::<String,_>("kind"),
        "work_order_id": r.try_get::<Uuid,_>("work_order_id").ok(), "order_title": r.try_get::<String,_>("order_title").ok(),
        "group_id": r.try_get::<Uuid,_>("group_id").ok(), "group_name": r.try_get::<String,_>("group_name").ok(),
        "user_id": r.try_get::<Uuid,_>("user_id").ok(), "user": r.try_get::<String,_>("email").ok(),
        "starts_at": r.get::<DateTime<Utc>,_>("starts_at"), "ends_at": r.get::<DateTime<Utc>,_>("ends_at"), "note": r.try_get::<String,_>("note").ok()
    })).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
pub struct BookingReq { pub asset_id: Uuid, #[serde(default)] pub work_order_id: Option<Uuid>, #[serde(default)] pub group_id: Option<Uuid>,
    #[serde(default)] pub user_id: Option<Uuid>, #[serde(default)] pub starts_at: Option<String>, #[serde(default)] pub ends_at: Option<String>,
    #[serde(default)] pub note: Option<String> }
/// Prenota un mezzo: per un intervento (orario dell'intervento se non indicato), una squadra o una persona.
pub async fn create_booking(State(st): State<AppState>, h: HeaderMap, Json(b): Json<BookingReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "mezzi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let (mut a, mut z) = (b.starts_at.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?, b.ends_at.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?);
    if let Some(o) = b.work_order_id {
        let r = sqlx::query("SELECT scheduled_start,scheduled_end FROM app.work_orders WHERE id=$1").bind(o).fetch_optional(&mut *tx).await.map_err(ise)?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "intervento inesistente"))?;
        if a.is_none() { a = r.try_get("scheduled_start").ok(); }
        if z.is_none() { z = r.try_get("scheduled_end").ok(); }
    }
    let (Some(a), Some(z)) = (a, z) else { return Err(err(StatusCode::BAD_REQUEST, "indica inizio e fine (o un intervento con orario)")) };
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "la fine deve essere dopo l'inizio")); }
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM app.assets WHERE id=$1").bind(b.asset_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    match status.as_deref() {
        None => return Err(err(StatusCode::NOT_FOUND, "mezzo inesistente")),
        Some("disponibile") => {}
        Some(x) => if !force(&h) { return Err(err(StatusCode::CONFLICT, &format!("Conflitto: il mezzo è {x} (usa Forza per procedere)"))) },
    }
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.asset_bookings(tenant_id,asset_id,work_order_id,group_id,user_id,starts_at,ends_at,note,created_by)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(b.asset_id).bind(b.work_order_id).bind(b.group_id).bind(b.user_id).bind(a).bind(z).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(booking_err)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "asset_booking_create", json!({"id": id, "asset": b.asset_id, "order": b.work_order_id})).await;
    Ok(Json(json!({"id": id})))
}
pub async fn delete_booking(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "mezzi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.asset_bookings WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "prenotazione inesistente")); }
    audit(&st, tid, &s.email, "asset_booking_delete", json!({"id": id})).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct DeadlineReq { pub kind: String, pub due_date: String, #[serde(default)] pub note: Option<String> }
pub async fn create_deadline(State(st): State<AppState>, h: HeaderMap, Path(asset): Path<Uuid>, Json(b): Json<DeadlineReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "mezzi").await?; require_any_cap(&s, &["sites.manage", "plan"])?;
    if !DEADLINE_KINDS.contains(&b.kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo di scadenza: revisione, tagliando, assicurazione, verifica o altro")); }
    let due = chrono::NaiveDate::parse_from_str(b.due_date.trim(), "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "data della scadenza AAAA-MM-GG"))?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.asset_deadlines(tenant_id,asset_id,kind,due_date,note) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(asset).bind(&b.kind).bind(due).bind(&b.note).fetch_one(&mut *tx).await
        .map_err(|e| crate::api::conflict_or(e, "mezzo inesistente"))?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "asset_deadline_create", json!({"id": id, "asset": asset, "kind": b.kind})).await;
    Ok(Json(json!({"id": id})))
}
#[derive(Deserialize)]
pub struct DeadlineDone { #[serde(default)] pub done: Option<bool> }
pub async fn update_deadline(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<DeadlineDone>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "mezzi").await?; require_any_cap(&s, &["sites.manage", "plan"])?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.asset_deadlines SET done_at=CASE WHEN $2 THEN now() ELSE NULL END WHERE id=$1")
        .bind(id).bind(b.done.unwrap_or(true)).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "scadenza inesistente")); }
    Ok(Json(json!({"ok": true})))
}
pub async fn delete_deadline(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "mezzi").await?; require_any_cap(&s, &["sites.manage", "plan"])?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.asset_deadlines WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "scadenza inesistente")); }
    Ok(Json(json!({"ok": true})))
}

/// GET /api/deadlines?days=60 — SCADENZIARIO: qualifiche delle persone (modulo qualifiche) e
/// scadenze dei mezzi (modulo mezzi) entro N giorni, comprese quelle già scadute.
pub async fn list_deadlines(State(st): State<AppState>, h: HeaderMap, Query(q): Query<RangeQ>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_cap(&s, &["plan", "users.manage", "sites.manage"])?;
    let (quals, mezzi) = (module_active(&st, tid, "qualifiche").await, module_active(&st, tid, "mezzi").await);
    if !quals && !mezzi { return Err(err(StatusCode::FORBIDDEN, "modulo qualifiche o mezzi non abilitato")); }
    let days = q.days.unwrap_or(60).clamp(1, 365);
    let tz = crate::field::tenant_tz(&st, tid).await;
    let today = st.clock.now().with_timezone(&tz).date_naive();
    let limit = today + chrono::Duration::days(days);
    let mut out: Vec<Value> = vec![];
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if quals {
        let rows = sqlx::query("SELECT uq.user_id,u.email,q.id AS qid,q.name,uq.expires_at FROM app.user_qualifications uq
                JOIN app.users u ON u.id=uq.user_id JOIN app.qualifications q ON q.id=uq.qualification_id
                WHERE uq.expires_at IS NOT NULL AND uq.expires_at < $1 AND u.anonymized_at IS NULL ORDER BY uq.expires_at")
            .bind(tz.from_local_datetime(&limit.and_hms_opt(23, 59, 59).unwrap()).earliest().map(|x| x.with_timezone(&Utc)).unwrap_or_else(Utc::now))
            .fetch_all(&mut *tx).await.map_err(ise)?;
        for r in &rows {
            let d = r.get::<DateTime<Utc>,_>("expires_at").with_timezone(&tz).date_naive();
            out.push(json!({"kind": "qualifica", "what": r.get::<String,_>("name"), "who": r.get::<String,_>("email"), "user_id": r.get::<Uuid,_>("user_id"),
                "qualification_id": r.get::<Uuid,_>("qid"), "due_date": d.to_string(), "days_left": (d - today).num_days()}));
        }
    }
    if mezzi {
        let rows = sqlx::query("SELECT d.id,d.kind,d.due_date,d.note,a.id AS asset_id,a.name FROM app.asset_deadlines d JOIN app.assets a ON a.id=d.asset_id
                WHERE d.done_at IS NULL AND d.due_date <= $1 ORDER BY d.due_date").bind(limit).fetch_all(&mut *tx).await.map_err(ise)?;
        for r in &rows {
            let d: chrono::NaiveDate = r.get("due_date");
            out.push(json!({"kind": "mezzo", "id": r.get::<Uuid,_>("id"), "what": r.get::<String,_>("kind"), "who": r.get::<String,_>("name"),
                "asset_id": r.get::<Uuid,_>("asset_id"), "note": r.try_get::<String,_>("note").ok(), "due_date": d.to_string(), "days_left": (d - today).num_days()}));
        }
    }
    tx.commit().await.map_err(ise)?;
    out.sort_by_key(|x| x["days_left"].as_i64().unwrap_or(0));
    Ok(Json(json!({"days": days, "items": out})))
}

/// GET /api/assets/:id/deadlines
pub async fn asset_deadlines(State(st): State<AppState>, h: HeaderMap, Path(asset): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "mezzi").await?;
    if !is_planner(&s) { require_any_cap(&s, &["sites.manage", "plan"])?; }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,kind,due_date,note,done_at FROM app.asset_deadlines WHERE asset_id=$1 ORDER BY done_at NULLS FIRST, due_date")
        .bind(asset).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({"id": r.get::<Uuid,_>("id"), "kind": r.get::<String,_>("kind"),
        "due_date": r.get::<chrono::NaiveDate,_>("due_date").to_string(), "note": r.try_get::<String,_>("note").ok(),
        "done_at": r.try_get::<DateTime<Utc>,_>("done_at").ok()})).collect::<Vec<_>>())))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(s: &str) -> DateTime<Utc> { DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc) }
    #[test] fn estimate_adds_units() {
        assert_eq!(estimate_minutes(30, 0.05, Some(2000.0)), 130);
        assert_eq!(estimate_minutes(60, 0.0, None), 60);
        assert_eq!(estimate_minutes(0, 0.0, None), 1);
    }
    #[test] fn windows_check_local_time() {
        let w = json!([{"days":"1111100","from":"08:00","to":"17:00"}]);
        let rome = chrono_tz::Europe::Rome;
        // lunedì 30/11/2026 09:00–11:00 ora di Roma = 08:00–10:00 UTC
        assert!(within_windows(&w, rome, t("2026-11-30T08:00:00Z"), t("2026-11-30T10:00:00Z")));
        // finisce alle 18:00 locali: fuori
        assert!(!within_windows(&w, rome, t("2026-11-30T15:00:00Z"), t("2026-11-30T17:00:00Z")));
        // sabato: fuori
        assert!(!within_windows(&w, rome, t("2026-12-05T08:00:00Z"), t("2026-12-05T09:00:00Z")));
        // nessuna finestra = sempre
        assert!(within_windows(&json!([]), rome, t("2026-12-05T02:00:00Z"), t("2026-12-05T03:00:00Z")));
    }
    #[test] fn windows_validation() {
        assert!(parse_windows(&json!([{"days":"1111100","from":"08:00","to":"17:00"}])).is_ok());
        assert!(parse_windows(&json!([{"days":"0000000","from":"08:00","to":"17:00"}])).is_err());
        assert!(parse_windows(&json!([{"days":"1111100","from":"17:00","to":"08:00"}])).is_err());
        assert!(parse_windows(&json!([{"days":"11111","from":"08:00","to":"17:00"}])).is_err());
    }
}
