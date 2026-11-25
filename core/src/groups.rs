//! Gruppi di persone condivisi — servizio CORE (trasversale): primitiva "insieme di persone"
//! riusabile da qualunque modulo (campo, allarmi, ruoli custom). Estratto da api.rs (Fase B).
//! L'import dei membri in un gruppo di allarme (bridge escalation) resta in api.rs. Vedi
//! docs/gruppi-condivisi.md e docs/architettura-moduli.md (core.groups).

use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{conflict_or, R, require_tenant, require_cap, ise, err, audit, MemberReq};
use crate::db;

// ---- Gruppi di persone CONDIVISI (primitiva comune, riusabile da tutti i moduli) ----
pub async fn list_user_groups(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let groups = sqlx::query("SELECT id,name,kind,labels FROM app.user_groups ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    let mems = sqlx::query("SELECT m.group_id,m.user_id,u.email FROM app.user_group_members m JOIN app.users u ON u.id=m.user_id ORDER BY u.email")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = groups.iter().map(|g| {
        let gid: Uuid = g.get("id");
        let members: Vec<Value> = mems.iter().filter(|m| m.get::<Uuid,_>("group_id")==gid)
            .map(|m| json!({"user_id":m.get::<Uuid,_>("user_id"),"email":m.get::<String,_>("email")})).collect();
        json!({"id":gid,"name":g.get::<String,_>("name"),"kind":g.get::<String,_>("kind"),
            "labels":g.try_get::<Value,_>("labels").unwrap_or(json!({})),"members":members})
    }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct UserGroupReq { pub name: String, #[serde(default)] pub kind: Option<String>, #[serde(default)] pub labels: Option<Value> }
pub async fn create_user_group(State(st): State<AppState>, h: HeaderMap, Json(b): Json<UserGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.user_groups(tenant_id,name,kind,labels) VALUES($1,$2,COALESCE($3,'squadra'),COALESCE($4,'{}'::jsonb)) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.kind.as_deref()).bind(b.labels.as_ref())
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "gruppo già esistente (nome)"))?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_user_group", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn update_user_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<UserGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.user_groups SET name=$2, kind=COALESCE($3,kind), labels=COALESCE($4,labels) WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(b.kind.as_deref()).bind(b.labels.as_ref())
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    audit(&st, tid, &s.email, "update_user_group", json!({"id":id})).await; // TA-13
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_user_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.user_groups WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    audit(&st, tid, &s.email, "delete_user_group", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn user_group_member_add(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // verifica appartenenza al tenant del gruppo e dell'utente (RLS già scopa, ma espliciti i 404)
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.user_groups WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"gruppo inesistente")); }
    sqlx::query("INSERT INTO app.user_group_members(tenant_id,group_id,user_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(id).bind(b.user_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "user_group_member_add", json!({"group":id,"user":b.user_id})).await; // TA-13
    Ok(Json(json!({"ok":true})))
}
pub async fn user_group_member_del(State(st): State<AppState>, h: HeaderMap, Path((id,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_group_members WHERE group_id=$1 AND user_id=$2").bind(id).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "user_group_member_del", json!({"group":id,"user":uid})).await; // TA-13
    Ok(Json(json!({"ok":true})))
}
