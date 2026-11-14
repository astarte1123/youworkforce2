//! Permessi e ruoli (IAM) — servizio CORE (trasversale): matrice capability per-ruolo (override
//! per-tenant) e ruoli personalizzati assegnabili a utenti/gruppi. Estratto da api.rs (Fase B).
//! Le fondamenta (CAPABILITIES, USER_ROLES, default_role_caps, effective_caps*) restano in api.rs
//! perche' usate da login/sessione; qui vivono solo gli handler HTTP. Vedi docs/capability-layer.md.

use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, ise, err, audit, CAPABILITIES, USER_ROLES, default_role_caps, effective_caps};
use crate::db;

// ---- Fase 2: gestione capability per ruolo (override per-tenant) ----
pub async fn list_capabilities(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let catalog: Vec<Value> = CAPABILITIES.iter().map(|(c,d)| json!({"code":c,"desc":d})).collect();
    // matrice effettiva ruolo x capability + flag "override" (diverso dal default)
    let mut matrix = serde_json::Map::new();
    for role in USER_ROLES {
        let eff = effective_caps(&st, tid, role).await;
        let def: Vec<&str> = default_role_caps(role);
        let row: Vec<Value> = CAPABILITIES.iter().map(|(c,_)| {
            let has = eff.iter().any(|x| x==c);
            let is_def = def.contains(c);
            json!({"cap":c,"granted":has,"default":is_def,"override":has!=is_def})
        }).collect();
        matrix.insert(role.to_string(), json!(row));
    }
    Ok(Json(json!({"catalog":catalog,"roles":USER_ROLES,"matrix":matrix})))
}
#[derive(Deserialize)] pub struct CapReq { pub role: String, pub capability: String, pub granted: bool }
pub async fn set_capability(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CapReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if !USER_ROLES.contains(&b.role.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo sconosciuto")); }
    if !CAPABILITIES.iter().any(|(c,_)| *c==b.capability) { return Err(err(StatusCode::BAD_REQUEST, "capability sconosciuta")); }
    // guardia anti-lockout: non togliere 'admin' a tenant_admin
    if b.role=="tenant_admin" && b.capability=="admin" && !b.granted {
        return Err(err(StatusCode::BAD_REQUEST, "non si puo' togliere 'admin' a tenant_admin"));
    }
    // se granted coincide col default -> rimuovi l'override (torna al default)
    let is_def = default_role_caps(&b.role).contains(&b.capability.as_str());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if b.granted == is_def {
        sqlx::query("DELETE FROM app.role_capabilities WHERE tenant_id=$1 AND role=$2 AND capability=$3")
            .bind(tid).bind(&b.role).bind(&b.capability).execute(&mut *tx).await.map_err(ise)?;
    } else {
        sqlx::query("INSERT INTO app.role_capabilities(tenant_id,role,capability,granted) VALUES($1,$2,$3,$4)
                     ON CONFLICT (tenant_id,role,capability) DO UPDATE SET granted=EXCLUDED.granted, updated_at=now()")
            .bind(tid).bind(&b.role).bind(&b.capability).bind(b.granted).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "set_capability", json!({"role":b.role,"capability":b.capability,"granted":b.granted})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn reset_capabilities(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.role_capabilities WHERE tenant_id=$1").bind(tid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "reset_capabilities", json!({})).await;
    Ok(Json(json!({"ok":true})))
}

// ---- Fase 3: ruoli personalizzati per tenant (bundle di capability, assegnabili) ----
pub async fn list_custom_roles(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let roles = sqlx::query("SELECT id,name,description FROM app.custom_roles ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    let caps = sqlx::query("SELECT role_id,capability FROM app.custom_role_capabilities").fetch_all(&mut *tx).await.map_err(ise)?;
    let ur = sqlx::query("SELECT r.role_id,u.id as user_id,u.email FROM app.user_custom_roles r JOIN app.users u ON u.id=r.user_id ORDER BY u.email").fetch_all(&mut *tx).await.map_err(ise)?;
    let gr = sqlx::query("SELECT r.role_id,g.id as group_id,g.name FROM app.user_group_custom_roles r JOIN app.user_groups g ON g.id=r.group_id ORDER BY g.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = roles.iter().map(|r| {
        let rid: Uuid = r.get("id");
        let caps: Vec<String> = caps.iter().filter(|c| c.get::<Uuid,_>("role_id")==rid).map(|c| c.get::<String,_>("capability")).collect();
        let users: Vec<Value> = ur.iter().filter(|x| x.get::<Uuid,_>("role_id")==rid).map(|x| json!({"user_id":x.get::<Uuid,_>("user_id"),"email":x.get::<String,_>("email")})).collect();
        let groups: Vec<Value> = gr.iter().filter(|x| x.get::<Uuid,_>("role_id")==rid).map(|x| json!({"group_id":x.get::<Uuid,_>("group_id"),"name":x.get::<String,_>("name")})).collect();
        json!({"id":rid,"name":r.get::<String,_>("name"),"description":r.try_get::<String,_>("description").ok(),"capabilities":caps,"users":users,"groups":groups})
    }).collect();
    let catalog: Vec<Value> = CAPABILITIES.iter().map(|(c,d)| json!({"code":c,"desc":d})).collect();
    Ok(Json(json!({"roles":out,"catalog":catalog})))
}
#[derive(Deserialize)] pub struct CustomRoleReq { pub name: String, #[serde(default)] pub description: Option<String>, #[serde(default)] pub capabilities: Vec<String> }
fn validate_caps(caps: &[String]) -> Result<(), (StatusCode,String)> {
    for c in caps { if !CAPABILITIES.iter().any(|(k,_)| k==c) { return Err(err(StatusCode::BAD_REQUEST, "capability sconosciuta")); } }
    Ok(())
}
async fn set_role_caps(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, rid: Uuid, caps: &[String]) -> Result<(), (StatusCode,String)> {
    sqlx::query("DELETE FROM app.custom_role_capabilities WHERE role_id=$1").bind(rid).execute(&mut **tx).await.map_err(ise)?;
    for c in caps {
        sqlx::query("INSERT INTO app.custom_role_capabilities(tenant_id,role_id,capability) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(tid).bind(rid).bind(c).execute(&mut **tx).await.map_err(ise)?;
    }
    Ok(())
}
pub async fn create_custom_role(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CustomRoleReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    validate_caps(&b.capabilities)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.custom_roles(tenant_id,name,description) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.description.as_deref())
        .fetch_optional(&mut *tx).await.map_err(|_| err(StatusCode::CONFLICT, "ruolo già esistente (nome)"))?;
    let rid = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    set_role_caps(&mut tx, tid, rid, &b.capabilities).await?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_custom_role", json!({"name":b.name,"caps":b.capabilities})).await;
    Ok(Json(json!({"id":rid})))
}
pub async fn update_custom_role(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<CustomRoleReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    validate_caps(&b.capabilities)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.custom_roles SET name=$2, description=$3 WHERE id=$1")
        .bind(rid).bind(b.name.trim()).bind(b.description.as_deref()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "ruolo inesistente")); }
    set_role_caps(&mut tx, tid, rid, &b.capabilities).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_custom_role(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.custom_roles WHERE id=$1").bind(rid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "ruolo inesistente")); }
    audit(&st, tid, &s.email, "delete_custom_role", json!({"id":rid})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct AssignUserReq { pub user_id: Uuid }
pub async fn custom_role_assign_user(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<AssignUserReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.custom_roles WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"ruolo inesistente")); }
    sqlx::query("INSERT INTO app.user_custom_roles(tenant_id,user_id,role_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(b.user_id).bind(rid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn custom_role_unassign_user(State(st): State<AppState>, h: HeaderMap, Path((rid,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_custom_roles WHERE role_id=$1 AND user_id=$2").bind(rid).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct AssignGroupReq { pub group_id: Uuid }
pub async fn custom_role_assign_group(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<AssignGroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.custom_roles WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"ruolo inesistente")); }
    sqlx::query("INSERT INTO app.user_group_custom_roles(tenant_id,group_id,role_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(b.group_id).bind(rid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn custom_role_unassign_group(State(st): State<AppState>, h: HeaderMap, Path((rid,gid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_group_custom_roles WHERE role_id=$1 AND group_id=$2").bind(rid).bind(gid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
