//! Identità/utenti — servizio CORE (trasversale): anagrafica utenti del tenant (CRUD) e ACL
//! sedi-visibili per utente (operator_sites). Estratto da api.rs (Fase B). Le fondamenta di
//! login/sessione (tenant_login, /api/me, impersonazione, session_of) restano in api.rs perche'
//! legate all'autenticazione; qui vivono solo gli handler di gestione anagrafica.

use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, require_grantable, user_caps, effective_caps, ise, err, audit, USER_ROLES, is_e164};
use crate::{db, auth};

pub async fn list_users(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,email,role,phone FROM app.users ORDER BY role,email").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "email": r.get::<String,_>("email"), "role": r.get::<String,_>("role"),
        "phone": r.try_get::<String,_>("phone").ok() })).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct UserSitesReq { pub site_ids: Vec<Uuid> }
pub async fn get_user_sites(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = sqlx::query_scalar::<_, Uuid>("SELECT site_id FROM app.operator_sites WHERE user_id=$1")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"site_ids": rows})))
}
pub async fn set_user_sites(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<UserSitesReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.operator_sites WHERE user_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    for sid in &b.site_ids {
        sqlx::query("INSERT INTO app.operator_sites(tenant_id,user_id,site_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(tid).bind(id).bind(sid).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "set_user_sites", json!({"user":id,"n":b.site_ids.len()})).await;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct NewUser { pub email: String, pub role: String, pub password: String }
pub async fn create_user(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewUser>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if !USER_ROLES.contains(&b.role.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo non valido")); }
    // TA-1: il ruolo assegnato non può avere permessi che chi crea non ha
    require_grantable(&s, &effective_caps(&st, tid, &b.role).await)?;
    if b.password.len() < 4 { return Err(err(StatusCode::BAD_REQUEST, "password troppo corta")); }
    if b.email.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "email richiesta")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(b.email.trim()).bind(auth::hash_password(&b.password)).bind(&b.role)
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "email gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "email gia' presente"))?.get::<Uuid,_>("id");
    audit(&st, tid, &s.email, "create_user", json!({"email":b.email,"role":b.role})).await;
    Ok(Json(json!({"id":id})))
}

#[derive(Deserialize)]
pub struct UpdateUser { pub role: Option<String>, pub password: Option<String>, pub phone: Option<String> }
pub async fn update_user(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<UpdateUser>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let (cur_role, cur_caps) = user_caps(&st, tid, id).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    // TA-1: chi ha più permessi di te (es. un amministratore) non si modifica: né ruolo, né password, né telefono
    if id != s.user_id { require_grantable(&s, &cur_caps)?; }
    if let Some(r) = &b.role {
        if !USER_ROLES.contains(&r.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo non valido")); }
        // TA-1: il nuovo ruolo non può dare permessi che chi modifica non ha (niente auto-promozione)
        require_grantable(&s, &effective_caps(&st, tid, r).await)?;
        // TA-3: mai lasciare il tenant senza amministratore (per sé o per altri)
        if cur_role == "tenant_admin" && r != "tenant_admin" { ensure_other_admin(&st, tid, id).await?; }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if let Some(r) = &b.role {
        sqlx::query("UPDATE app.users SET role=$2 WHERE id=$1").bind(id).bind(r).execute(&mut *tx).await.map_err(ise)?;
    }
    if let Some(p) = &b.password {
        if p.len() < 4 { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, "password troppo corta")); }
        sqlx::query("UPDATE app.users SET pass_hash=$2 WHERE id=$1").bind(id).bind(auth::hash_password(p)).execute(&mut *tx).await.map_err(ise)?;
    }
    if let Some(ph) = &b.phone {
        let ph = ph.trim();
        if ph.is_empty() {
            sqlx::query("UPDATE app.users SET phone=NULL WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
        } else {
            if !is_e164(ph) { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, "telefono: formato E.164 richiesto (es. +393331234567)")); }
            sqlx::query("UPDATE app.users SET phone=$2 WHERE id=$1").bind(id).bind(ph).execute(&mut *tx).await.map_err(ise)?;
        }
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "update_user", json!({"id":id,"role":b.role})).await;
    Ok(Json(json!({"ok":true})))
}

pub async fn delete_user(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if id == s.user_id { return Err(err(StatusCode::CONFLICT, "non puoi eliminare te stesso")); }
    let (cur_role, cur_caps) = user_caps(&st, tid, id).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    require_grantable(&s, &cur_caps)?;                                   // TA-1
    if cur_role == "tenant_admin" { ensure_other_admin(&st, tid, id).await?; } // TA-3
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.users WHERE id=$1").bind(id).execute(&mut *tx).await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => err(StatusCode::CONFLICT, "utente referenziato (ordini/consuntivi): cambia ruolo invece di eliminarlo"),
            _ => ise(e),
        })?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "utente inesistente")); }
    audit(&st, tid, &s.email, "delete_user", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}

/// TA-3: deve restare almeno un altro amministratore attivo (non anonimizzato) oltre a `except`.
async fn ensure_other_admin(st: &AppState, tid: Uuid, except: Uuid) -> Result<(), (StatusCode, String)> {
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM app.users WHERE tenant_id=$1 AND role='tenant_admin' AND anonymized_at IS NULL AND id<>$2")
        .bind(tid).bind(except).fetch_one(&st.owner_pool).await.map_err(ise)?;
    if others < 1 { return Err(err(StatusCode::CONFLICT, "è l'unico amministratore del tenant: nominane un altro prima")); }
    Ok(())
}
