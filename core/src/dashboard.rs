//! Dashboard unica configurabile — servizio CORE (trasversale): aggrega i widget dei moduli attivi.
//! Primo modulo-feature estratto da api.rs verso lo strato core (vedi docs/architettura-moduli.md).
//! Il catalogo widget qui è la single source of truth dei widget disponibili; il filtro autoritativo
//! (modulo attivo + ruolo) vive in `dashboard_layout_get`. L'organizzazione (abilita/ordina/dimensiona)
//! è del tenant_admin (capability dashboard.manage); l'attivazione dei moduli resta del superadmin.

use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, has_cap, ise, audit};
use crate::db;

/// Catalogo widget: (key, moduli-any-of, ruoli-any-of, size-default). Un widget è candidato se un
/// suo modulo è attivo (lista vuota = sempre, es. core) e il ruolo combacia (lista vuota = tutti).
/// L'ordine qui è il default quando il tenant non ha un layout salvato.
pub const DASHBOARD_WIDGETS: &[(&str, &[&str], &[&str], &str)] = &[
    ("incidents",  &["loneworker","traccar","iot","thingsboard"], &["operatore","tenant_admin","auditor","responsabile"], "half"),
    ("checkins",   &["loneworker"], &["operatore","tenant_admin","auditor"], "half"),
    ("orders",     &["interventi"], &["pianificatore","tenant_admin","lavoratore","auditor"], "half"),
    ("timesheets", &["consuntivi"], &["lavoratore","responsabile","tenant_admin","auditor"], "half"),
    ("rounds",     &["giri"], &["pianificatore","tenant_admin","lavoratore","auditor"], "half"),
    ("shifts",     &["workforce"], &["pianificatore","tenant_admin","lavoratore","reperibile","auditor"], "half"),
    ("role",       &[], &[], "half"),
];

/// Layout effettivo per l'utente: catalogo filtrato per moduli attivi + ruolo, merge col layout
/// salvato (enabled/position/size), ordinato per position. `can_edit` = capability dashboard.manage.
pub async fn dashboard_layout_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let act: std::collections::HashSet<String> = sqlx::query_scalar::<_,String>(
        "SELECT module_code FROM app.tenant_modules WHERE tenant_id=$1 AND state='attivo'")
        .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default().into_iter().collect();
    let saved = sqlx::query("SELECT widget_key,enabled,position,size FROM app.dashboard_layouts WHERE tenant_id=$1")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut smap: std::collections::HashMap<String,(bool,i32,String)> = std::collections::HashMap::new();
    for r in &saved { smap.insert(r.get::<String,_>("widget_key"), (r.get("enabled"), r.get("position"), r.get("size"))); }
    let mut out: Vec<Value> = Vec::new();
    for (i,(key,mods,roles,size)) in DASHBOARD_WIDGETS.iter().enumerate() {
        let mod_ok = mods.is_empty() || mods.iter().any(|m| act.contains(*m));
        let role_ok = roles.is_empty() || roles.contains(&s.role.as_str());
        if !mod_ok || !role_ok { continue; }
        let (enabled, position, sz) = match smap.get(*key) {
            Some((e,p,s2)) => (*e, *p, s2.clone()),
            None => (true, i as i32, size.to_string()),
        };
        out.push(json!({"key":key,"enabled":enabled,"position":position,"size":sz}));
    }
    out.sort_by_key(|w| w.get("position").and_then(|v| v.as_i64()).unwrap_or(0));
    Ok(Json(json!({"widgets":out, "can_edit": has_cap(&s,"dashboard.manage")})))
}

fn dtrue() -> bool { true }
#[derive(Deserialize)]
pub struct WidgetLayout { pub widget_key: String, #[serde(default="dtrue")] pub enabled: bool,
    #[serde(default)] pub position: i32, #[serde(default)] pub size: Option<String> }
#[derive(Deserialize)]
pub struct DashboardLayoutReq { pub widgets: Vec<WidgetLayout> }

/// Salva il layout dashboard del tenant. Solo capability dashboard.manage (tenant_admin di default).
/// Le chiavi sconosciute al catalogo sono ignorate; size validata (half|full).
pub async fn dashboard_layout_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<DashboardLayoutReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s,"dashboard.manage")?;
    let tid = s.tenant_id.unwrap();
    let valid: std::collections::HashSet<&str> = DASHBOARD_WIDGETS.iter().map(|w| w.0).collect();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    for w in &b.widgets {
        if !valid.contains(w.widget_key.as_str()) { continue; }
        let size = match w.size.as_deref() { Some("full") => "full", _ => "half" };
        sqlx::query("INSERT INTO app.dashboard_layouts(tenant_id,widget_key,enabled,position,size) VALUES($1,$2,$3,$4,$5)
            ON CONFLICT (tenant_id,widget_key) DO UPDATE SET enabled=$3,position=$4,size=$5,updated_at=now()")
            .bind(tid).bind(&w.widget_key).bind(w.enabled).bind(w.position).bind(size).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "dashboard_layout_set", json!({"n":b.widgets.len()})).await;
    Ok(Json(json!({"ok":true})))
}
