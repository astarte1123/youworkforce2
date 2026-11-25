//! Cataloghi di prodotto nel DB, gestiti dal superadmin (decisioni PO 26/11/2026):
//! - SL-15 tipi di evento (`app.event_types`): UNICA fonte di nome leggibile, famiglia e spiegazione
//!   di ogni codice evento, servita al core (report, messaggi di escalation) e alla console.
//!   Se un tipo è critico o salva-vita NON sta qui: lo decide il prodotto (api.rs CRITICAL /
//!   LIFE_SAFETY) e il catalogo lo mostra soltanto, così nessuno può declassare un allarme.
//! - SA-10 pacchetti (`app.plans_catalog`): gruppi di moduli proposti alla creazione di un tenant.
//! Migrazione 0161.

use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use crate::state::AppState;
use crate::api::{R, require_tenant, require_super, ise, err, CRITICAL, LIFE_SAFETY};

pub const EVENT_CATEGORIES: &[(&str, &str)] = &[
    ("soccorso", "Soccorso al lavoratore"), ("connessione", "Dispositivo che non comunica"),
    ("area", "Aree e posizione"), ("dispositivo", "Stato del dispositivo"), ("veicolo", "Mezzi e veicoli"),
    ("sensori", "Sensori"), ("ronde", "Ronde"), ("altro", "Altro"),
];

#[derive(Clone)]
pub struct EventType { pub label: String, pub category: String, pub description: String, pub sort: i32 }
static EVENT_TYPES: OnceLock<RwLock<HashMap<String, EventType>>> = OnceLock::new();
fn cache() -> &'static RwLock<HashMap<String, EventType>> { EVENT_TYPES.get_or_init(|| RwLock::new(HashMap::new())) }

/// Carica (o ricarica) il catalogo dei tipi di evento in memoria. All'avvio e dopo ogni modifica.
pub async fn load_event_types(pool: &sqlx::PgPool) {
    match sqlx::query("SELECT code,label,category,description,sort FROM app.event_types").fetch_all(pool).await {
        Ok(rows) => {
            let m: HashMap<String, EventType> = rows.iter().map(|r| (r.get::<String,_>("code"), EventType {
                label: r.get("label"), category: r.get("category"), description: r.get("description"), sort: r.get("sort") })).collect();
            if let Ok(mut w) = cache().write() { *w = m; }
        }
        Err(e) => tracing::warn!("catalogo tipi di evento non caricato ({e}): uso i nomi predefiniti"),
    }
}
/// Nome leggibile dal catalogo (None se il codice non c'è o il catalogo non è caricato).
pub fn event_label(code: &str) -> Option<String> {
    cache().read().ok().and_then(|m| m.get(code).map(|e| e.label.clone()))
}
fn is_critical(code: &str) -> bool { CRITICAL.contains(&code) || code.ends_with("_critico") }

fn event_rows() -> Vec<Value> {
    let m = cache().read().map(|m| m.clone()).unwrap_or_default();
    let mut v: Vec<(&String, &EventType)> = m.iter().collect();
    v.sort_by(|a, b| a.1.sort.cmp(&b.1.sort).then(a.0.cmp(b.0)));
    v.into_iter().map(|(code, e)| json!({
        "code": code, "label": e.label, "category": e.category,
        "category_label": EVENT_CATEGORIES.iter().find(|c| c.0 == e.category).map(|c| c.1).unwrap_or("Altro"),
        "description": e.description, "critical": is_critical(code), "life_safety": LIFE_SAFETY.contains(&code.as_str()),
    })).collect()
}
fn categories() -> Vec<Value> { EVENT_CATEGORIES.iter().map(|(c, l)| json!({"code":c,"label":l})).collect() }

/// Console del tenant: nomi e famiglie dei tipi di evento (sola lettura, uguale per tutti).
pub async fn event_types_public(State(st): State<AppState>, h: HeaderMap) -> R {
    require_tenant(&st, &h).await?;
    Ok(Json(json!({"types": event_rows(), "categories": categories()})))
}
pub async fn super_event_types(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    Ok(Json(json!({"types": event_rows(), "categories": categories()})))
}
#[derive(Deserialize)] pub struct EventTypeUpd { pub label: String, pub category: String, #[serde(default)] pub description: String }
pub async fn super_event_type_set(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>, Json(b): Json<EventTypeUpd>) -> R {
    let s = require_super(&st, &h).await?;
    let label = b.label.trim();
    if !(2..=120).contains(&label.chars().count()) { return Err(err(StatusCode::BAD_REQUEST, "il nome deve avere da 2 a 120 caratteri")); }
    if !EVENT_CATEGORIES.iter().any(|c| c.0 == b.category) { return Err(err(StatusCode::BAD_REQUEST, "famiglia non valida")); }
    let desc = b.description.trim(); if desc.chars().count() > 400 { return Err(err(StatusCode::BAD_REQUEST, "spiegazione troppo lunga (max 400 caratteri)")); }
    let n = sqlx::query("UPDATE app.event_types SET label=$2,category=$3,description=$4,updated_at=now(),updated_by=$5 WHERE code=$1")
        .bind(&code).bind(label).bind(&b.category).bind(desc).bind(&s.email).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "tipo di evento non trovato")); }
    load_event_types(&st.owner_pool).await;
    tracing::info!(target: "audit", "superadmin {} ha modificato il tipo di evento {code}", s.email);
    Ok(Json(json!({"ok": true})))
}

// ---------------- SA-10: pacchetti ----------------
pub struct PlanRow { pub code: String, pub name: String, pub description: String, pub modules: Vec<String>, pub active: bool, pub builtin: bool }
pub async fn plans(st: &AppState, only_active: bool) -> Result<Vec<PlanRow>, (StatusCode, String)> {
    let rows = sqlx::query("SELECT code,name,description,modules,active,builtin FROM app.plans_catalog WHERE ($1=false OR active) ORDER BY sort,name")
        .bind(only_active).fetch_all(&st.owner_pool).await.map_err(ise)?;
    Ok(rows.iter().map(|r| PlanRow { code: r.get("code"), name: r.get("name"), description: r.get("description"),
        modules: r.get("modules"), active: r.get("active"), builtin: r.get("builtin") }).collect())
}
pub async fn super_plans_list(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let names: HashMap<String, String> = sqlx::query("SELECT code,name FROM app.modules_catalog").fetch_all(&st.owner_pool).await.map_err(ise)?
        .iter().map(|r| (r.get("code"), r.get("name"))).collect();
    Ok(Json(json!(plans(&st, false).await?.iter().map(|p| json!({
        "code":p.code,"name":p.name,"desc":p.description,"modules":p.modules,"active":p.active,"builtin":p.builtin,
        "module_names": p.modules.iter().map(|m| names.get(m).cloned().unwrap_or_else(|| m.clone())).collect::<Vec<_>>(),
    })).collect::<Vec<_>>())))
}
#[derive(Deserialize)] pub struct PlanUpsert { pub code: Option<String>, pub name: String, #[serde(default)] pub desc: String, pub modules: Vec<String>, #[serde(default = "yes")] pub active: bool }
fn yes() -> bool { true }
async fn check_plan(st: &AppState, b: &PlanUpsert) -> Result<Vec<String>, (StatusCode, String)> {
    if !(2..=80).contains(&b.name.trim().chars().count()) { return Err(err(StatusCode::BAD_REQUEST, "il nome deve avere da 2 a 80 caratteri")); }
    let mut mods: Vec<String> = b.modules.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
    mods.sort(); mods.dedup();
    if mods.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "scegli almeno un modulo")); }
    let known: Vec<String> = sqlx::query_scalar("SELECT code FROM app.modules_catalog WHERE code = ANY($1)").bind(&mods)
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    if let Some(bad) = mods.iter().find(|m| !known.contains(m)) { return Err(err(StatusCode::BAD_REQUEST, &format!("modulo sconosciuto: {bad}"))); }
    Ok(mods)
}
pub async fn super_plan_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PlanUpsert>) -> R {
    let s = require_super(&st, &h).await?;
    let mods = check_plan(&st, &b).await?;
    let code = crate::api::clean_slug(b.code.as_deref().unwrap_or(&b.name))?;
    sqlx::query("INSERT INTO app.plans_catalog(code,name,description,modules,active,updated_by) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(&code).bind(b.name.trim()).bind(b.desc.trim()).bind(&mods).bind(b.active).bind(&s.email)
        .execute(&st.owner_pool).await.map_err(|e| crate::api::conflict_or(e, "esiste già un pacchetto con questo codice"))?;
    Ok(Json(json!({"ok": true, "code": code})))
}
pub async fn super_plan_update(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>, Json(b): Json<PlanUpsert>) -> R {
    let s = require_super(&st, &h).await?;
    let mods = check_plan(&st, &b).await?;
    let n = sqlx::query("UPDATE app.plans_catalog SET name=$2,description=$3,modules=$4,active=$5,updated_at=now(),updated_by=$6 WHERE code=$1")
        .bind(&code).bind(b.name.trim()).bind(b.desc.trim()).bind(&mods).bind(b.active).bind(&s.email)
        .execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "pacchetto non trovato")); }
    Ok(Json(json!({"ok": true})))
}
pub async fn super_plan_delete(State(st): State<AppState>, h: HeaderMap, Path(code): Path<String>) -> R {
    require_super(&st, &h).await?;
    let builtin: Option<bool> = sqlx::query_scalar("SELECT builtin FROM app.plans_catalog WHERE code=$1").bind(&code)
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match builtin {
        None => Err(err(StatusCode::NOT_FOUND, "pacchetto non trovato")),
        Some(true) => Err(err(StatusCode::CONFLICT, "i pacchetti predefiniti non si eliminano: puoi disattivarli")),
        Some(false) => {
            sqlx::query("DELETE FROM app.plans_catalog WHERE code=$1").bind(&code).execute(&st.owner_pool).await.map_err(ise)?;
            Ok(Json(json!({"ok": true})))
        }
    }
}
/// Moduli del prodotto (per comporre i pacchetti).
pub async fn super_modules_catalog(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query("SELECT code,name,available FROM app.modules_catalog ORDER BY name").fetch_all(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({"code":r.get::<String,_>("code"),"name":r.get::<String,_>("name"),"available":r.get::<bool,_>("available")})).collect::<Vec<_>>())))
}
