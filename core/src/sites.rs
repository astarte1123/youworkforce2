//! Sedi e anagrafiche operative — servizio CORE (trasversale): sedi e asset/mezzi condivisi da
//! tutti i moduli. Estratto da api.rs (Fase B). Le squadre (teams) restano in api.rs: sono una
//! primitiva del modulo workforce (gated), non core. Vedi docs/architettura-moduli.md (core.sites).

use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, ise, err, audit, is_planner};
use crate::db;

// --- Sedi ---
// ---------------- Sedi ----------------
#[derive(Deserialize)]
pub struct SiteReq { pub name: String, #[serde(default)] pub address: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>, #[serde(default)] pub access_notes: Option<String>,
    /// CA-16: fuso della sede (IANA). Assente = invariato; "" = come il tenant.
    #[serde(default)] pub timezone: Option<String> }
/// Valida il fuso: None = non toccare, Some(None) = azzera (come il tenant), Some(Some(tz)) = imposta.
fn site_tz(b: &SiteReq) -> Result<Option<Option<String>>, (StatusCode, String)> {
    match b.timezone.as_deref().map(|x| x.trim()) {
        None => Ok(None),
        Some("") => Ok(Some(None)),
        Some(z) => crate::api::parse_tz(z).map(|t| Some(Some(t.name().to_string())))
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "fuso orario sconosciuto (es. Europe/Rome)")),
    }
}
pub async fn create_site(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SiteReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let tz = site_tz(&b)?.flatten();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.sites(tenant_id,name,address,lat,lon,access_notes,timezone) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.address.as_deref()).bind(b.lat).bind(b.lon).bind(b.access_notes.as_deref()).bind(tz)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_site", json!({"name":b.name})).await;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn update_site(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<SiteReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let tz = site_tz(&b)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // COALESCE: aggiorna solo i campi passati; address/access_notes vuoti ("") azzerano.
    let n = sqlx::query("UPDATE app.sites SET name=$2,
            address=COALESCE($3,address), lat=COALESCE($4,lat), lon=COALESCE($5,lon), access_notes=COALESCE($6,access_notes),
            timezone=CASE WHEN $7 THEN $8 ELSE timezone END
         WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(b.address.as_deref()).bind(b.lat).bind(b.lon).bind(b.access_notes.as_deref())
        .bind(tz.is_some()).bind(tz.clone().flatten())
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "sede inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_site(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.sites WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => err(StatusCode::CONFLICT, "sede referenziata: rimuovi prima i collegamenti"),
            _ => ise(e),
        })?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "sede inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// --- Asset ---
pub async fn list_assets(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT a.id,a.name,a.kind,a.identifier,a.site_id,si.name AS site,a.note
        FROM app.assets a LEFT JOIN app.sites si ON si.id=a.site_id ORDER BY a.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"kind":r.get::<String,_>("kind"),
        "identifier":r.try_get::<String,_>("identifier").ok(),"site_id":r.try_get::<Uuid,_>("site_id").ok(),
        "site":r.try_get::<String,_>("site").ok(),"note":r.try_get::<String,_>("note").ok() })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)]
pub struct AssetReq { pub name: String, pub kind: Option<String>, pub identifier: Option<String>, pub site_id: Option<Uuid>, pub note: Option<String> }
pub async fn create_asset(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AssetReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let kind = b.kind.clone().unwrap_or("attrezzatura".into());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.assets(tenant_id,name,kind,identifier,site_id,note) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&kind).bind(&b.identifier).bind(b.site_id).bind(&b.note)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "nome gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id")})))
}
pub async fn update_asset(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<AssetReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.assets SET name=$2,kind=COALESCE($3,kind),identifier=$4,site_id=$5,note=$6 WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(&b.kind).bind(&b.identifier).bind(b.site_id).bind(&b.note)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "asset inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_asset(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.assets WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => err(StatusCode::CONFLICT, "asset con dispositivi collegati"),
            _ => ise(e),
        })?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "asset inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// --- list ---
pub async fn list_sites(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,name,address,lat,lon,access_notes,timezone FROM app.sites ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // access_notes (indicazioni d'accesso) solo agli autorizzati (pianificatore/admin)
    let can_access = is_planner(&s);
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"), "name":r.get::<String,_>("name"),
        "address": r.try_get::<String,_>("address").ok(),
        "lat": r.try_get::<f64,_>("lat").ok(), "lon": r.try_get::<f64,_>("lon").ok(),
        "timezone": r.try_get::<String,_>("timezone").ok(),
        "access_notes": if can_access { r.try_get::<String,_>("access_notes").ok() } else { None }
    })).collect();
    Ok(Json(json!(out)))
}
