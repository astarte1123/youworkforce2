//! Lavoro sul campo: clienti, interventi, listino, consuntivi, registro ore, ronde, forza lavoro e turni, cantieri
//! Estratto da api.rs (split, nessun cambio di comportamento): i gestori restano
//! raggiungibili come `api::nome` grazie al re-export in api.rs.
#![allow(unused_imports)]
use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, Response, sse::{Sse, Event as SseEvent}}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::convert::Infallible;
use uuid::Uuid;
use futures::stream::Stream;
use crate::state::{AppState, Session, TenantEvent};
use crate::{auth, db};
use crate::api::*;

// ===== Assistenza A1: anagrafica Cliente + referenti =====
#[derive(Deserialize)]
pub struct ClientReq { pub name: String, #[serde(default)] pub vat: Option<String>, #[serde(default)] pub address: Option<String>,
    #[serde(default)] pub phone: Option<String>, #[serde(default)] pub email: Option<String>, #[serde(default)] pub notes: Option<String>,
    #[serde(default)] pub pec: Option<String>, #[serde(default)] pub sdi: Option<String>, #[serde(default)] pub payment_terms: Option<String> }
pub async fn list_clients(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT c.id,c.name,c.vat,c.address,c.phone,c.email,c.notes,c.active,
                (SELECT count(*) FROM app.work_orders o WHERE o.client_id=c.id)::int AS orders,
                (SELECT count(*) FROM app.client_contacts k WHERE k.client_id=c.id)::int AS contacts
         FROM app.clients c ORDER BY c.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"vat":r.try_get::<String,_>("vat").ok(),
        "address":r.try_get::<String,_>("address").ok(),"phone":r.try_get::<String,_>("phone").ok(),
        "email":r.try_get::<String,_>("email").ok(),"notes":r.try_get::<String,_>("notes").ok(),
        "active":r.get::<bool,_>("active"),"orders":r.get::<i32,_>("orders"),"contacts":r.get::<i32,_>("contacts") })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_client(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ClientReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.clients(tenant_id,name,vat,address,phone,email,notes,pec,sdi,payment_terms,created_by) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.vat).bind(&b.address).bind(&b.phone).bind(&b.email).bind(&b.notes).bind(&b.pec).bind(&b.sdi).bind(&b.payment_terms).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "client_create", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn get_client(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let c = sqlx::query("SELECT id,name,vat,address,phone,email,notes,active,pec,sdi,payment_terms FROM app.clients WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let c = match c { Some(r)=>r, None=>{ return Err(err(StatusCode::NOT_FOUND, "cliente non trovato")); } };
    let ks = sqlx::query("SELECT id,name,role,phone,email FROM app.client_contacts WHERE client_id=$1 ORDER BY name").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let orders = sqlx::query("SELECT o.id,o.title,o.state,o.scheduled_start FROM app.work_orders o WHERE o.client_id=$1 ORDER BY o.scheduled_start NULLS LAST, o.updated_at DESC LIMIT 50").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let kout: Vec<Value> = ks.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),
        "role":r.try_get::<String,_>("role").ok(),"phone":r.try_get::<String,_>("phone").ok(),"email":r.try_get::<String,_>("email").ok()})).collect();
    let oout: Vec<Value> = orders.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),
        "state":r.get::<String,_>("state"),"scheduled_start":r.try_get::<chrono::DateTime<chrono::Utc>,_>("scheduled_start").ok()})).collect();
    Ok(Json(json!({"id":c.get::<Uuid,_>("id"),"name":c.get::<String,_>("name"),"vat":c.try_get::<String,_>("vat").ok(),
        "address":c.try_get::<String,_>("address").ok(),"phone":c.try_get::<String,_>("phone").ok(),"email":c.try_get::<String,_>("email").ok(),
        "notes":c.try_get::<String,_>("notes").ok(),"active":c.get::<bool,_>("active"),
        "pec":c.try_get::<String,_>("pec").ok(),"sdi":c.try_get::<String,_>("sdi").ok(),"payment_terms":c.try_get::<String,_>("payment_terms").ok(),
        "contacts":kout,"orders":oout})))
}
pub async fn update_client(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ClientReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.clients SET name=COALESCE($2,name),vat=$3,address=$4,phone=$5,email=$6,notes=$7,pec=$8,sdi=$9,payment_terms=$10 WHERE id=$1")
        .bind(id).bind(if b.name.trim().is_empty(){None}else{Some(b.name.trim())}).bind(&b.vat).bind(&b.address).bind(&b.phone).bind(&b.email).bind(&b.notes).bind(&b.pec).bind(&b.sdi).bind(&b.payment_terms)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "cliente non trovato")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_client(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.clients WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "client_delete", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct ContactReq { pub name: String, #[serde(default)] pub role: Option<String>, #[serde(default)] pub phone: Option<String>, #[serde(default)] pub email: Option<String> }
pub async fn create_contact(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<ContactReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.client_contacts(tenant_id,client_id,name,role,phone,email) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(cid).bind(b.name.trim()).bind(&b.role).bind(&b.phone).bind(&b.email).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_contact(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.client_contacts WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ===== Interventi A2: listino prezzi + valorizzazione della visita =====
#[derive(Deserialize)]
pub struct PriceItemReq { pub name: String, #[serde(default)] pub code: Option<String>, #[serde(default)] pub category: Option<String>,
    #[serde(default)] pub unit: Option<String>, #[serde(default)] pub unit_price: Option<f64>, #[serde(default)] pub active: Option<bool>,
    // CA-6: qualifica (tariffe di manodopera per qualifica) e validità; "" = togli
    #[serde(default)] pub qualification_id: Option<String>, #[serde(default)] pub valid_from: Option<String>, #[serde(default)] pub valid_to: Option<String> }
/// CA-6: euro -> centesimi (arrotondati), mai negativi.
pub(crate) fn eur_to_cents(v: Option<f64>) -> Result<Option<i64>, (StatusCode,String)> {
    match v { None => Ok(None), Some(x) if x < 0.0 || !x.is_finite() => Err(err(StatusCode::BAD_REQUEST, "prezzo non valido")), Some(x) => Ok(Some((x * 100.0).round() as i64)) }
}
/// Campo opzionale del listino: None = non toccare, Some(None) = togli, Some(Some(v)) = imposta.
pub(crate) fn opt_field<T>(v: &Option<String>, parse: impl Fn(&str) -> Option<T>, what: &str) -> Result<Option<Option<T>>, (StatusCode,String)> {
    match v.as_deref().map(str::trim) {
        None => Ok(None), Some("") => Ok(Some(None)),
        Some(x) => parse(x).map(|p| Some(Some(p))).ok_or_else(|| err(StatusCode::BAD_REQUEST, &format!("{what} non valido"))),
    }
}
/// Il listino serve agli interventi e ai consuntivi (tariffa delle righe): basta uno dei due moduli.
pub(crate) const PRICE_MODS: &[&str] = &["interventi", "consuntivi"];
pub async fn list_price_items(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, PRICE_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT i.id,i.code,i.name,i.category,i.unit,i.price_cents,i.active,i.qualification_id,q.name AS qualification,i.valid_from,i.valid_to
            FROM app.price_list_items i LEFT JOIN app.qualifications q ON q.id=i.qualification_id ORDER BY i.active DESC,i.category,i.name,q.name NULLS FIRST").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| { let c: i64 = r.get("price_cents"); json!({
        "id":r.get::<Uuid,_>("id"),"code":r.try_get::<String,_>("code").ok(),"name":r.get::<String,_>("name"),
        "category":r.get::<String,_>("category"),"unit":r.get::<String,_>("unit"),
        "price_cents":c, "unit_price": c as f64 / 100.0, "active":r.get::<bool,_>("active"),
        "qualification_id":r.try_get::<Option<Uuid>,_>("qualification_id").ok().flatten(),
        "qualification":r.try_get::<Option<String>,_>("qualification").ok().flatten(),
        "valid_from":r.try_get::<Option<chrono::NaiveDate>,_>("valid_from").ok().flatten(),
        "valid_to":r.try_get::<Option<chrono::NaiveDate>,_>("valid_to").ok().flatten() }) }).collect();
    Ok(Json(json!(out)))
}
pub async fn create_price_item(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PriceItemReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, PRICE_MODS).await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let cents = eur_to_cents(b.unit_price)?;
    let q = opt_field(&b.qualification_id, |x| Uuid::parse_str(x).ok(), "qualifica")?.flatten();
    let vf = opt_field(&b.valid_from, |x| chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").ok(), "inizio validità")?.flatten();
    let vt = opt_field(&b.valid_to, |x| chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").ok(), "fine validità")?.flatten();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.price_list_items(tenant_id,code,name,category,unit,price_cents,qualification_id,valid_from,valid_to)
         VALUES($1,$2,$3,COALESCE($4,'altro'),COALESCE($5,'pezzo'),COALESCE($6,0),$7,$8,$9) RETURNING id")
        .bind(tid).bind(&b.code).bind(b.name.trim()).bind(&b.category).bind(&b.unit).bind(cents).bind(q).bind(vf).bind(vt)
        .fetch_one(&mut *tx).await.map_err(|e| conflict_or(e, "periodo di validità non valido"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_price_item(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PriceItemReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, PRICE_MODS).await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let cents = eur_to_cents(b.unit_price)?;
    let q = opt_field(&b.qualification_id, |x| Uuid::parse_str(x).ok(), "qualifica")?;
    let vf = opt_field(&b.valid_from, |x| chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").ok(), "inizio validità")?;
    let vt = opt_field(&b.valid_to, |x| chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").ok(), "fine validità")?;
    let n = sqlx::query("UPDATE app.price_list_items SET name=COALESCE($2,name),code=$3,category=COALESCE($4,category),unit=COALESCE($5,unit),
            price_cents=COALESCE($6,price_cents),active=COALESCE($7,active),
            qualification_id=CASE WHEN $8 THEN $9 ELSE qualification_id END,
            valid_from=CASE WHEN $10 THEN $11 ELSE valid_from END, valid_to=CASE WHEN $12 THEN $13 ELSE valid_to END WHERE id=$1")
        .bind(id).bind(if b.name.trim().is_empty(){None}else{Some(b.name.trim())}).bind(&b.code).bind(&b.category).bind(&b.unit).bind(cents).bind(b.active)
        .bind(q.is_some()).bind(q.flatten()).bind(vf.is_some()).bind(vf.flatten()).bind(vt.is_some()).bind(vt.flatten())
        .execute(&mut *tx).await.map_err(|e| conflict_or(e, "periodo di validità non valido"))?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "voce non trovata")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_price_item(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, PRICE_MODS).await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.price_list_items WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Chi può valorizzare una visita: pianificatore o l'assegnatario dell'ordine.
pub(crate) async fn order_line_allowed(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, s: &Session, wo: Uuid) -> Result<bool, (StatusCode,String)> {
    if is_planner(s) { return Ok(true); }
    let asg: Option<Uuid> = sqlx::query_scalar("SELECT assigned_user FROM app.work_orders WHERE id=$1").bind(wo).fetch_optional(&mut **tx).await.map_err(ise)?.flatten();
    Ok(asg == Some(s.user_id))
}
pub async fn list_order_lines(State(st): State<AppState>, h: HeaderMap, Path(wo): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_view_allowed(&st, &s, wo).await?; // CA-10
    let costs = can_see_costs(&s); // CA-11: il tecnico vede i materiali, non i prezzi
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,item_id,description,category,unit,qty::float8 AS qty,unit_price::float8 AS unit_price,(qty*unit_price)::float8 AS total FROM app.work_order_lines WHERE work_order_id=$1 ORDER BY created_at").bind(wo).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let mut tot=0.0;
    let out: Vec<Value> = rows.iter().map(|r| { let t=r.get::<f64,_>("total"); tot+=t; json!({
        "id":r.get::<Uuid,_>("id"),"item_id":r.try_get::<Uuid,_>("item_id").ok(),"description":r.get::<String,_>("description"),
        "category":r.get::<String,_>("category"),"unit":r.get::<String,_>("unit"),
        "qty":r.get::<f64,_>("qty"),
        "unit_price": if costs { Some(r.get::<f64,_>("unit_price")) } else { None },
        "total": if costs { Some(t) } else { None } }) }).collect();
    Ok(Json(json!({"lines":out,"total": if costs { Some((tot*100.0).round()/100.0) } else { None }, "prices_hidden": !costs})))
}
#[derive(Deserialize)]
pub struct OrderLineReq { pub description: String, #[serde(default)] pub item_id: Option<Uuid>, #[serde(default)] pub category: Option<String>,
    #[serde(default)] pub unit: Option<String>, #[serde(default)] pub qty: Option<f64>, #[serde(default)] pub unit_price: Option<f64> }
pub async fn add_order_line(State(st): State<AppState>, h: HeaderMap, Path(wo): Path<Uuid>, Json(b): Json<OrderLineReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    if b.description.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "descrizione richiesta")); }
    if let Some(q) = b.qty { if q <= 0.0 { return Err(err(StatusCode::BAD_REQUEST, "quantità maggiore di zero")); } }
    if let Some(p) = b.unit_price { if p < 0.0 { return Err(err(StatusCode::BAD_REQUEST, "prezzo negativo")); } }
    order_open(&st, tid, wo).await?; // CA-13
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if !order_line_allowed(&mut tx, &s, wo).await? { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore o assegnatario")); }
    let id: Uuid = sqlx::query_scalar(
        // CA-6: prezzo in centesimi COPIATO dalla voce di listino (resta storico se il listino cambia)
        "INSERT INTO app.work_order_lines(tenant_id,work_order_id,item_id,description,category,unit,qty,unit_price_cents,created_by)
         VALUES($1,$2,$3,$4,COALESCE($5,'altro'),COALESCE($6,'pezzo'),COALESCE($7::numeric,1),
                COALESCE($8,(SELECT price_cents FROM app.price_list_items WHERE id=$3 AND tenant_id=$1),0),$9) RETURNING id")
        // CA-11: chi non vede i costi non fissa il prezzo: vale quello del listino (o 0, da valorizzare in ufficio)
        .bind(tid).bind(wo).bind(b.item_id).bind(b.description.trim()).bind(&b.category).bind(&b.unit).bind(b.qty)
        .bind(if can_see_costs(&s) { eur_to_cents(b.unit_price)? } else { None }).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_order_line(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // consenti a pianificatore o assegnatario dell'ordine a cui appartiene la riga
    let wo: Option<Uuid> = sqlx::query_scalar("SELECT work_order_id FROM app.work_order_lines WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let wo = match wo { Some(x)=>x, None=>{ return Err(err(StatusCode::NOT_FOUND,"riga non trovata")); } };
    order_open(&st, tid, wo).await?; // CA-13
    if !order_line_allowed(&mut tx, &s, wo).await? { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore o assegnatario")); }
    sqlx::query("DELETE FROM app.work_order_lines WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ===== Interventi A3: dispatch — suggerimento tecnico per un intervento =====
// Grounded sui fatti del tenant: esclude gli assenti nel giorno dell'intervento, ordina per
// carico (meno interventi quel giorno, poi meno aperti). youworker.ai opzionale per il riassunto.
pub async fn suggest_order_tech(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    use chrono::TimeZone as _;
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let ord = sqlx::query("SELECT scheduled_start,title FROM app.work_orders WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let ord = match ord { Some(r)=>r, None=>{ return Err(err(StatusCode::NOT_FOUND,"intervento non trovato")); } };
    let sched: Option<chrono::DateTime<chrono::Utc>> = ord.try_get("scheduled_start").ok();
    let day = sched.map(|d| d.date_naive()).unwrap_or_else(|| st.clock.now().date_naive());
    let day_start = chrono::Utc.from_utc_datetime(&day.and_hms_opt(0,0,0).unwrap());
    let day_end = day_start + chrono::Duration::days(1);
    let workers = sqlx::query("SELECT id,email FROM app.users WHERE role IN ('lavoratore','operatore') ORDER BY email").fetch_all(&mut *tx).await.map_err(ise)?;
    let loads = sqlx::query("SELECT assigned_user, count(*) FILTER (WHERE state IN ('assegnato','in_corso'))::int AS open_load, count(*) FILTER (WHERE scheduled_start::date=$1)::int AS day_load FROM app.work_orders WHERE assigned_user IS NOT NULL GROUP BY assigned_user")
        .bind(day).fetch_all(&mut *tx).await.map_err(ise)?;
    let absent = sqlx::query("SELECT DISTINCT user_id FROM app.worker_absences WHERE starts_at < $2 AND ends_at > $1")
        .bind(day_start).bind(day_end).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    use std::collections::{HashMap,HashSet};
    let mut open_m: HashMap<Uuid,i32>=HashMap::new(); let mut day_m: HashMap<Uuid,i32>=HashMap::new();
    for r in &loads { let u:Uuid=r.get("assigned_user"); open_m.insert(u, r.get::<i32,_>("open_load")); day_m.insert(u, r.get::<i32,_>("day_load")); }
    let absent_set: HashSet<Uuid> = absent.iter().map(|r| r.get::<Uuid,_>("user_id")).collect();
    let mut cand: Vec<(Uuid,String,i32,i32,bool)> = workers.iter().map(|r| {
        let u:Uuid=r.get("id"); (u, r.get::<String,_>("email"), *day_m.get(&u).unwrap_or(&0), *open_m.get(&u).unwrap_or(&0), absent_set.contains(&u))
    }).collect();
    cand.sort_by(|a,b| a.4.cmp(&b.4).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)).then(a.1.cmp(&b.1)));
    let out: Vec<Value> = cand.iter().map(|(u,em,dl,ol,ab)| json!({
        "user_id":u,"email":em,"day_load":dl,"open_load":ol,"absent":ab,
        "reason": if *ab {format!("assente il {day}")} else {format!("{dl} interventi il {day} · {ol} aperti")} })).collect();
    let det = {
        let avail: Vec<&(Uuid,String,i32,i32,bool)> = cand.iter().filter(|c| !c.4).collect();
        match avail.first() {
            Some(top) => format!("Consiglio: {} (carico più basso: {} il {}, {} aperti). {} tecnici disponibili.", top.1, top.2, day, top.3, avail.len()),
            None => "Nessun tecnico disponibile quel giorno (tutti assenti).".to_string(),
        }
    };
    let mut summary = det.clone();
    if module_active(&st, tid, "assistant").await && ai_feature_on(&st, tid, "assistant").await {
        if let Some(c) = ai_conn(&st, tid).await {
            let sys = "Assistente dispatch YouWorkforce. In UNA frase consiglia il tecnico migliore usando SOLO i fatti (carico, assenze). Italiano, conciso.";
            let ctx = format!("Intervento: {}. Candidati: {}", ord.get::<String,_>("title"), serde_json::to_string(&out).unwrap_or_default());
            if let Ok(ans) = ai_chat(&st, &c, sys, &ctx).await { if !ans.trim().is_empty() { summary = ans; } }
        }
    }
    Ok(Json(json!({"day":day.to_string(),"candidates":out,"summary":summary,"provider": if summary==det {"deterministico"} else {"youworker.ai"}})))
}
// ===== Interventi A4: contratti/SLA + piani di manutenzione ricorrente =====
#[derive(Deserialize)]
pub struct ContractReq { pub name: String, #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub sla_hours: Option<i32>,
    #[serde(default)] pub hours_bundle: Option<f64>, #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String>,
    #[serde(default)] pub active: Option<bool>, #[serde(default)] pub note: Option<String> }
pub async fn list_contracts(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT c.id,c.name,c.sla_hours,c.hours_bundle::float8 AS hours_bundle,c.start_date,c.end_date,c.active,c.note,c.client_id,cl.name AS client_name FROM app.service_contracts c LEFT JOIN app.clients cl ON cl.id=c.client_id ORDER BY c.active DESC,c.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"sla_hours":r.try_get::<i32,_>("sla_hours").ok(),
        "hours_bundle":r.try_get::<f64,_>("hours_bundle").ok(),
        "start_date":r.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":r.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "active":r.get::<bool,_>("active"),"note":r.try_get::<String,_>("note").ok(),
        "client_id":r.try_get::<Uuid,_>("client_id").ok(),"client_name":r.try_get::<String,_>("client_name").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_contract(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ContractReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.service_contracts(tenant_id,client_id,name,sla_hours,hours_bundle,start_date,end_date,note,created_by) VALUES($1,$2,$3,$4,$5::numeric,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(b.client_id).bind(b.name.trim()).bind(b.sla_hours).bind(b.hours_bundle)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_contract(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ContractReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.service_contracts SET name=COALESCE($2,name),client_id=$3,sla_hours=$4,hours_bundle=$5::numeric,start_date=$6,end_date=$7,active=COALESCE($8,active),note=$9 WHERE id=$1")
        .bind(id).bind(if b.name.trim().is_empty(){None}else{Some(b.name.trim())}).bind(b.client_id).bind(b.sla_hours).bind(b.hours_bundle)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(b.active).bind(&b.note)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "contratto non trovato")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_contract(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.service_contracts WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct PlanReq { pub title: String, #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub site_id: Option<Uuid>,
    #[serde(default)] pub contract_id: Option<Uuid>, #[serde(default)] pub every_days: Option<i32>, #[serde(default)] pub next_date: Option<String>,
    #[serde(default)] pub active: Option<bool>,
    #[serde(default)] pub auto_generate: Option<bool>, #[serde(default)] pub lead_days: Option<i32> }
pub async fn list_plans(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT p.id,p.title,p.every_days,p.next_date,p.active,p.auto_generate,p.lead_days,p.last_order,p.last_generated_at,p.client_id,cl.name AS client_name,p.site_id,si.name AS site FROM app.maintenance_plans p LEFT JOIN app.clients cl ON cl.id=p.client_id LEFT JOIN app.sites si ON si.id=p.site_id ORDER BY p.active DESC,p.next_date").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),"every_days":r.get::<i32,_>("every_days"),
        "next_date":r.get::<chrono::NaiveDate,_>("next_date").to_string(),"active":r.get::<bool,_>("active"),
        "auto_generate":r.get::<bool,_>("auto_generate"),"lead_days":r.get::<i32,_>("lead_days"),
        "last_order":r.try_get::<Option<Uuid>,_>("last_order").ok().flatten(),
        "last_generated_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("last_generated_at").ok().flatten(),
        "client_id":r.try_get::<Uuid,_>("client_id").ok(),"client_name":r.try_get::<String,_>("client_name").ok(),
        "site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_plan(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PlanReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.title.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "titolo richiesto")); }
    let ev = b.every_days.unwrap_or(30).max(1);
    let nd = parse_date_opt(&b.next_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let lead = b.lead_days.unwrap_or(7);
    if !(0..=90).contains(&lead) { return Err(err(StatusCode::BAD_REQUEST, "anticipo da 0 a 90 giorni")); }
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.maintenance_plans(tenant_id,client_id,site_id,contract_id,title,every_days,next_date,auto_generate,lead_days) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(b.client_id).bind(b.site_id).bind(b.contract_id).bind(b.title.trim()).bind(ev).bind(nd).bind(b.auto_generate.unwrap_or(true)).bind(lead)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_plan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PlanReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    if b.title.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "titolo richiesto")); }
    let ev = b.every_days.unwrap_or(30).max(1);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if b.lead_days.map(|l| !(0..=90).contains(&l)).unwrap_or(false) { return Err(err(StatusCode::BAD_REQUEST, "anticipo da 0 a 90 giorni")); }
    let n = sqlx::query("UPDATE app.maintenance_plans SET title=$2,client_id=$3,site_id=$4,contract_id=$5,every_days=$6,next_date=COALESCE($7,next_date),active=COALESCE($8,active),
            auto_generate=COALESCE($9,auto_generate),lead_days=COALESCE($10,lead_days) WHERE id=$1")
        .bind(id).bind(b.title.trim()).bind(b.client_id).bind(b.site_id).bind(b.contract_id).bind(ev).bind(parse_date_opt(&b.next_date)).bind(b.active)
        .bind(b.auto_generate).bind(b.lead_days)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "piano non trovato")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_plan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.maintenance_plans WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// CA-20: crea l'intervento della prossima scadenza di un piano e avanza la data. Usata da «Genera»
/// e dal worker. L'ora dell'appuntamento è le 9:00 nel fuso dell'azienda (CA-16), non in UTC.
/// Se il piano è rimasto indietro (scadenze già passate) crea UN solo intervento e porta la prossima
/// data alla prima scadenza futura, invece di sommergere di interventi arretrati.
pub(crate) async fn plan_generate_one(tx: &mut sqlx::PgConnection, tid: Uuid, plan_id: Uuid, actor: Option<Uuid>,
    today: chrono::NaiveDate, tz: chrono_tz::Tz) -> Result<Option<(Uuid, chrono::NaiveDate)>, (StatusCode, String)> {
    use chrono::TimeZone as _;
    let p = sqlx::query("SELECT title,client_id,site_id,contract_id,every_days,next_date FROM app.maintenance_plans WHERE id=$1 AND tenant_id=$2 FOR UPDATE")
        .bind(plan_id).bind(tid).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some(p) = p else { return Ok(None) };
    let nd: chrono::NaiveDate = p.get("next_date");
    let start = tz.from_local_datetime(&nd.and_hms_opt(9,0,0).unwrap()).earliest()
        .map(|x| x.with_timezone(&chrono::Utc)).unwrap_or_else(|| chrono::Utc.from_utc_datetime(&nd.and_hms_opt(9,0,0).unwrap()));
    let oid: Uuid = sqlx::query_scalar(
        "INSERT INTO app.work_orders(tenant_id,site_id,title,state,created_by,scheduled_start,priority,client_id,contract_id)
         VALUES($1,$2,$3,'bozza',$4,$5,2,$6,$7) RETURNING id")
        .bind(tid).bind(p.try_get::<Uuid,_>("site_id").ok()).bind(format!("{} (manutenzione)", p.get::<String,_>("title"))).bind(actor)
        .bind(start).bind(p.try_get::<Uuid,_>("client_id").ok()).bind(p.try_get::<Uuid,_>("contract_id").ok())
        .fetch_one(&mut *tx).await.map_err(ise)?;
    let ev = (p.get::<i32,_>("every_days").max(1)) as i64;
    let mut next = nd + chrono::Duration::days(ev);
    while next < today { next += chrono::Duration::days(ev); }
    sqlx::query("UPDATE app.maintenance_plans SET next_date=$2,last_order=$3,last_generated_at=now() WHERE id=$1").bind(plan_id).bind(next).bind(oid).execute(&mut *tx).await.map_err(ise)?;
    Ok(Some((oid, next)))
}
/// Mezzanotte locale di un giorno nel fuso dato, in UTC (al cambio d'ora vale la prima occorrenza).
pub(crate) fn local_midnight_utc(tz: chrono_tz::Tz, d: chrono::NaiveDate) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone as _;
    tz.from_local_datetime(&d.and_hms_opt(0,0,0).unwrap()).earliest().map(|x| x.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| d.and_hms_opt(0,0,0).unwrap().and_utc())
}
/// Fuso del percorso di ronda: quello della sua sede, altrimenti dell'azienda.
pub(crate) async fn route_tz(st: &AppState, tid: Uuid, route_id: Uuid) -> chrono_tz::Tz {
    sqlx::query_scalar::<_,String>("SELECT COALESCE(si.timezone,t.timezone) FROM app.patrol_routes r JOIN app.tenants t ON t.id=r.tenant_id
            LEFT JOIN app.sites si ON si.id=r.site_id WHERE r.id=$1 AND r.tenant_id=$2").bind(route_id).bind(tid)
        .fetch_optional(&st.owner_pool).await.ok().flatten().and_then(|z| parse_tz(&z)).unwrap_or(chrono_tz::Europe::Rome)
}
pub(crate) async fn tenant_tz(st: &AppState, tid: Uuid) -> chrono_tz::Tz {
    sqlx::query_scalar::<_,String>("SELECT timezone FROM app.tenants WHERE id=$1").bind(tid)
        .fetch_optional(&st.owner_pool).await.ok().flatten().and_then(|z| parse_tz(&z)).unwrap_or(chrono_tz::Europe::Rome)
}
/// Genera l'appuntamento dalla ricorrenza e avanza la prossima data.
pub async fn generate_plan_order(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let tz = tenant_tz(&st, tid).await;
    let today = st.clock.now().with_timezone(&tz).date_naive();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let r = plan_generate_one(&mut tx, tid, id, Some(s.user_id), today, tz).await?;
    tx.commit().await.map_err(ise)?;
    match r {
        Some((oid, next)) => Ok(Json(json!({"id":oid,"next_date":next.to_string()}))),
        None => Err(err(StatusCode::NOT_FOUND,"piano non trovato")),
    }
}
/// CA-20 (decisione PO): le manutenzioni si generano da sole. Ogni piano attivo con generazione
/// automatica crea il suo intervento quando mancano `lead_days` giorni alla scadenza. Worker,
/// all'incirca ogni 10 minuti; più istanze non generano due volte (FOR UPDATE SKIP LOCKED + la
/// data che avanza nella stessa transazione).
pub async fn maintenance_autogen(st: &AppState) {
    let tenants: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT t.id FROM app.tenants t JOIN app.tenant_modules m ON m.tenant_id=t.id AND m.module_code='interventi' AND m.state='attivo'
          WHERE t.status='attivo' AND EXISTS (SELECT 1 FROM app.maintenance_plans p WHERE p.tenant_id=t.id AND p.active AND p.auto_generate)")
        .fetch_all(&st.owner_pool).await { Ok(v) => v, Err(e) => { tracing::warn!("maintenance autogen: {e}"); return; } };
    for tid in tenants {
        let tz = tenant_tz(st, tid).await;
        let today = st.clock.now().with_timezone(&tz).date_naive();
        let Ok(mut tx) = db::begin_tenant(&st.rt_pool, tid).await else { continue };
        let due: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM app.maintenance_plans WHERE tenant_id=$1 AND active AND auto_generate AND next_date - lead_days <= $2
              ORDER BY next_date LIMIT 50 FOR UPDATE SKIP LOCKED").bind(tid).bind(today)
            .fetch_all(&mut *tx).await.unwrap_or_default();
        let mut made = 0;
        for pid in due {
            match plan_generate_one(&mut tx, tid, pid, None, today, tz).await {
                Ok(Some((oid, _))) => { made += 1; tracing::info!("manutenzione generata: piano {pid} -> intervento {oid}"); }
                Ok(None) => {}
                Err(e) => { tracing::warn!("manutenzione piano {pid}: {:?}", e); }
            }
        }
        if tx.commit().await.is_ok() && made > 0 {
            crate::api::audit(st, tid, "sistema", "maintenance_autogen", json!({"interventi": made})).await;
        }
    }
}
// ===== Interventi A5: richieste/ticket =====
#[derive(Deserialize)]
pub struct RequestReq { pub title: String, #[serde(default)] pub description: Option<String>, #[serde(default)] pub client_id: Option<Uuid>,
    #[serde(default)] pub priority: Option<i32>, #[serde(default)] pub requester: Option<String>, #[serde(default)] pub status: Option<String> }
pub async fn list_requests(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT r.id,r.title,r.description,r.priority,r.status,r.requester,r.created_at,r.client_id,cl.name AS client_name,r.order_id FROM app.service_requests r LEFT JOIN app.clients cl ON cl.id=r.client_id ORDER BY (r.status IN ('convertita','chiusa')), r.created_at DESC").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"title":r.get::<String,_>("title"),"description":r.try_get::<String,_>("description").ok(),
        "priority":r.get::<i32,_>("priority"),"status":r.get::<String,_>("status"),"requester":r.try_get::<String,_>("requester").ok(),
        "created_at":r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
        "client_id":r.try_get::<Uuid,_>("client_id").ok(),"client_name":r.try_get::<String,_>("client_name").ok(),
        "order_id":r.try_get::<Uuid,_>("order_id").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_request(State(st): State<AppState>, h: HeaderMap, Json(b): Json<RequestReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?;
    if b.title.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "titolo richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.service_requests(tenant_id,client_id,title,description,priority,requester,created_by) VALUES($1,$2,$3,$4,COALESCE($5,2),$6,$7) RETURNING id")
        .bind(tid).bind(b.client_id).bind(b.title.trim()).bind(&b.description).bind(b.priority).bind(&b.requester).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn update_request(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<RequestReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.service_requests SET title=COALESCE($2,title),description=$3,priority=COALESCE($4,priority),status=COALESCE($5,status),requester=$6,client_id=$7 WHERE id=$1")
        .bind(id).bind(if b.title.trim().is_empty(){None}else{Some(b.title.trim())}).bind(&b.description).bind(b.priority).bind(&b.status).bind(&b.requester).bind(b.client_id)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "richiesta non trovata")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_request(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.service_requests WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Converte la richiesta in un intervento (bozza) e la segna "convertita".
pub async fn convert_request(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "interventi").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // CA-13: presa atomica (due conversioni contemporanee non creano due ordini); una richiesta
    // chiusa o già convertita non si converte
    let r = sqlx::query("UPDATE app.service_requests SET status='convertita' WHERE id=$1 AND status NOT IN ('convertita','chiusa')
                         RETURNING title,description,client_id,priority").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let r = match r { Some(x)=>x, None=>{
        let st0: Option<String> = sqlx::query_scalar("SELECT status FROM app.service_requests WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
        return Err(match st0.as_deref() {
            None => err(StatusCode::NOT_FOUND,"richiesta non trovata"),
            Some("chiusa") => err(StatusCode::CONFLICT,"richiesta chiusa: non si converte"),
            _ => err(StatusCode::CONFLICT,"richiesta già convertita"),
        }); } };
    let oid: Uuid = sqlx::query_scalar(
        "INSERT INTO app.work_orders(tenant_id,title,instructions,state,created_by,priority,client_id) VALUES($1,$2,$3,'bozza',$4,$5,$6) RETURNING id")
        .bind(tid).bind(r.get::<String,_>("title")).bind(r.try_get::<String,_>("description").ok()).bind(s.user_id).bind(r.get::<i32,_>("priority")).bind(r.try_get::<Uuid,_>("client_id").ok())
        .fetch_one(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.service_requests SET order_id=$2 WHERE id=$1").bind(id).bind(oid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "request_convert", json!({"request_id":id,"order_id":oid})).await;
    Ok(Json(json!({"id":oid})))
}

#[derive(Deserialize)]
pub struct OrderAction { pub version: i32, pub user_id: Option<Uuid>, #[serde(default)] pub group_id: Option<Uuid>, pub outcome: Option<String>, pub reason: Option<String> }

/// Valore (controllato dall'utente) per il campo SET della transizione: sempre BINDATO, mai interpolato.
pub(crate) enum SetVal { Txt(String), Id(Uuid) }
pub(crate) async fn order_transition(st: &AppState, s: &Session, iid: Uuid, b: &OrderAction,
    to: &str, set_col: Option<&str>, set_val: Option<SetVal>, from_states: &[&str]) -> R {
    let tid = s.tenant_id.unwrap();
    // CA-21: nessuna transizione d'ordine con il modulo spento (anche assegna/annulla)
    if !module_active(st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // version/state check dentro l'UPDATE (concorrenza ottimistica). `to`/`from_states` sono
    // costanti interne; il valore utente (outcome/reason/assigned_user) è bindato come $3.
    let states = from_states.iter().map(|x| format!("'{x}'")).collect::<Vec<_>>().join(",");
    let extra = set_col.map(|c| format!("{c}=$3, ")).unwrap_or_default();
    let sql = format!("UPDATE app.work_orders SET {extra}state='{to}', version=version+1, updated_at=now()
         WHERE id=$1 AND version=$2 AND state IN ({states})");
    let mut q = sqlx::query(&sql).bind(iid).bind(b.version);
    if let Some(v) = set_val { q = match v { SetVal::Txt(t) => q.bind(t), SetVal::Id(u) => q.bind(u) }; }
    let n = q.execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n == 0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT, "versione superata o stato non valido")); }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":to})))
}

pub async fn order_assign(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let uid = b.user_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "user_id richiesto"))?;
    order_transition(&st, &s, id, &b, "assegnato",
        Some("assigned_user"), Some(SetVal::Id(uid)), &["bozza","assegnato"]).await
}

/// Tappa 3: assegna l'ordine a un GRUPPO di persone condiviso. I membri del gruppo lo vedono
/// nei propri ordini e possono eseguirlo (ACL per gruppo in order_edit_allowed).
pub async fn order_assign_group(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), &["interventi"]).await?; // AR-1: controllo del modulo dichiarato
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let gid = b.group_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "group_id richiesto"))?;
    let tid = s.tenant_id.unwrap();
    // il gruppo deve esistere nel tenant (RLS su user_groups)
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.user_groups WHERE id=$1 AND tenant_id=$2").bind(gid).bind(tid)
        .fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente"));
    }
    order_transition(&st, &s, id, &b, "assegnato",
        Some("assigned_group"), Some(SetVal::Id(gid)), &["bozza","assegnato"]).await
}
pub async fn order_start(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    // CA-7: avvia l'assegnatario, un membro del gruppo assegnato o il pianificatore (stessa regola ovunque)
    let tid = order_edit_allowed(&st, &s, id).await?;
    let r = order_transition(&st, &s, id, &b, "in_corso", None, None, &["assegnato"]).await?;
    // avvio lavoro = timestamp dedicato (distinto dall'arrivo)
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let _ = sqlx::query("UPDATE app.work_orders SET work_started_at=COALESCE(work_started_at,now()) WHERE id=$1").bind(id).execute(&mut *tx).await;
    let _ = tx.commit().await;
    Ok(r)
}
pub async fn order_complete(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    // CA-1: prima chiunque nel tenant poteva completare qualunque ordine. Ora solo chi lo esegue
    // (assegnatario o membro del gruppo assegnato) o il pianificatore.
    let tid = order_edit_allowed(&st, &s, id).await?;
    let outcome = b.outcome.clone().unwrap_or_default();
    let r = order_transition(&st, &s, id, &b, "completato", Some("outcome"), Some(SetVal::Txt(outcome)), &["in_corso"]).await?;
    audit(&st, tid, &s.email, "order_complete", json!({"order":id})).await;
    Ok(r)
}
pub async fn order_cancel(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), &["interventi"]).await?; // AR-1: controllo del modulo dichiarato
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let reason = b.reason.clone().unwrap_or_default();
    order_transition(&st, &s, id, &b, "annullato", Some("cancel_reason"), Some(SetVal::Txt(reason)), &["bozza","assegnato","in_corso"]).await
}

// ---- Intervento end-to-end: report, allegati (foto/doc), firma ----
/// Chi può operare sull'esecuzione di un ordine: il pianificatore/admin oppure l'assegnatario.
/// CA-13: un intervento completato o annullato non si modifica più (rapporto, firma, materiali,
/// allegati, pianificazione): è un documento chiuso.
pub(crate) async fn order_open(st: &AppState, tid: Uuid, oid: Uuid) -> Result<(),(StatusCode,String)> {
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM app.work_orders WHERE id=$1 AND tenant_id=$2")
        .bind(oid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match state.as_deref() {
        None => Err(err(StatusCode::NOT_FOUND, "ordine inesistente")),
        Some("completato") | Some("annullato") => Err(err(StatusCode::CONFLICT, "intervento chiuso: non si modifica più")),
        _ => Ok(()),
    }
}
/// CA-10: lettura di un ordine = chi può lavorarci (order_edit_allowed) oppure chi ha
/// `orders.view_all` e l'ordine è in una delle sue sedi.
pub(crate) async fn order_view_allowed(st: &AppState, s: &Session, oid: Uuid) -> Result<Uuid,(StatusCode,String)> {
    if let Ok(t) = order_edit_allowed(st, s, oid).await { return Ok(t); }
    let tid = s.tenant_id.unwrap();
    if !module_active(st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    if has_cap(s, "orders.view_all") {
        let site: Option<Uuid> = sqlx::query_scalar("SELECT site_id FROM app.work_orders WHERE id=$1 AND tenant_id=$2")
            .bind(oid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "ordine inesistente"))?;
        if site_allowed(&allowed_sites(st, s).await, site) { return Ok(tid); }
    }
    Err(err(StatusCode::FORBIDDEN, "non autorizzato su questo ordine"))
}
pub(crate) async fn order_edit_allowed(st: &AppState, s: &Session, oid: Uuid) -> Result<Uuid,(StatusCode,String)> {
    let tid = s.tenant_id.unwrap();
    if !module_active(st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    let row = sqlx::query("SELECT assigned_user, assigned_group, site_id FROM app.work_orders WHERE id=$1 AND tenant_id=$2")
        .bind(oid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "ordine inesistente"))?;
    let au: Option<Uuid> = row.try_get("assigned_user").ok().flatten();
    let ag: Option<Uuid> = row.try_get("assigned_group").ok().flatten();
    let site: Option<Uuid> = row.try_get("site_id").ok().flatten();
    if au == Some(s.user_id) { return Ok(tid); }
    // un pianificatore ristretto per sede vede/modifica solo gli ordini delle sue sedi (come l'elenco)
    if is_planner(&s) && site_allowed(&allowed_sites(st, s).await, site) { return Ok(tid); }
    // ACL per gruppo: membro del gruppo di persone assegnato
    if let Some(g) = ag { if user_in_group(st, s.user_id, g).await { return Ok(tid); } }
    Err(err(StatusCode::FORBIDDEN, "non autorizzato su questo ordine"))
}
#[derive(Deserialize)] pub struct OrderReport {
    pub report: Option<String>, pub signed_by: Option<String>,
    #[serde(default)] pub checklist: Option<Value>, #[serde(default)] pub materials: Option<Value>,
    #[serde(default)] pub work_minutes: Option<i32>, #[serde(default)] pub outcome_kind: Option<String> }
/// Salvataggio esecuzione intervento (bozza/rapporto): report, firmatario, checklist, materiali,
/// minuti lavorati, tipo esito. Tutto opzionale e idempotente (COALESCE).
pub async fn order_report_set(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<OrderReport>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_edit_allowed(&st, &s, id).await?;
    order_open(&st, tid, id).await?; // CA-13: rapporto e firma di un intervento chiuso non cambiano
    let signed_by = b.signed_by.as_deref().map(|x| x.trim()).filter(|x| !x.is_empty());
    let signed_at = signed_by.map(|_| st.clock.now());
    let ok = b.outcome_kind.as_deref().filter(|x| matches!(*x, "completato"|"parziale"|"cliente_assente"|"accesso_impedito"|"ritorno"));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.work_orders SET
            report=COALESCE($2,report), signed_by=COALESCE($3,signed_by), signed_at=COALESCE($4,signed_at),
            checklist=COALESCE($5,checklist), materials=COALESCE($6,materials),
            work_minutes=COALESCE($7,work_minutes), outcome_kind=COALESCE($8,outcome_kind), updated_at=now()
         WHERE id=$1")
        .bind(id).bind(b.report.as_deref()).bind(signed_by).bind(signed_at)
        .bind(b.checklist.as_ref()).bind(b.materials.as_ref()).bind(b.work_minutes).bind(ok)
        .execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_report_set", json!({"order":id,"signed":signed_by.is_some()})).await;
    Ok(Json(json!({"ok":true,"signed_at":signed_at.map(|t| t.to_rfc3339())})))
}
/// Registra l'ARRIVO sul posto (distinto dall'avvio lavoro). Assegnatario o pianificatore.
pub async fn order_arrive(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_edit_allowed(&st, &s, id).await?;
    let now = st.clock.now();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.work_orders SET arrived_at=COALESCE(arrived_at,$2), updated_at=now() WHERE id=$1 AND state IN ('assegnato','in_corso')")
        .bind(id).bind(now).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_arrive", json!({"order":id})).await;
    Ok(Json(json!({"ok":true,"arrived_at":now.to_rfc3339()})))
}
/// Upload allegato: body = bytes del file; metadati in query (kind, filename, caption); ct dall'header.
pub async fn order_attachment_add(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>,
    body: axum::body::Bytes) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_edit_allowed(&st, &s, id).await?;
    order_open(&st, tid, id).await?; // CA-13
    if body.len() < 4 { return Err(err(StatusCode::BAD_REQUEST, "file vuoto")); }
    if body.len() > 10_000_000 { return Err(err(StatusCode::BAD_REQUEST, "file troppo grande (max 10MB)")); }
    let kind = match q.get("kind").map(|s| s.as_str()) { Some("doc") => "doc", Some("signature") => "signature", _ => "photo" };
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
    let filename = q.get("filename").map(|x| x.trim().to_string());
    let caption = q.get("caption").map(|x| x.trim().to_string());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.work_order_attachments(tenant_id,work_order_id,kind,filename,content_type,caption,data,created_by)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(tid).bind(id).bind(kind).bind(&filename).bind(&ct).bind(&caption).bind(body.as_ref()).bind(&s.email)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_attachment_add", json!({"order":id,"kind":kind,"bytes":body.len()})).await;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id"), "kind": kind})))
}
pub async fn order_attachments_list(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = order_view_allowed(&st, &s, id).await?;
    let rows = sqlx::query("SELECT id,kind,filename,content_type,caption,created_by,created_at FROM app.work_order_attachments WHERE tenant_id=$1 AND work_order_id=$2 ORDER BY created_at")
        .bind(tid).bind(id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "kind": r.get::<String,_>("kind"),
        "filename": r.try_get::<String,_>("filename").ok(), "content_type": r.get::<String,_>("content_type"),
        "caption": r.try_get::<String,_>("caption").ok(), "created_by": r.try_get::<String,_>("created_by").ok(),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at").to_rfc3339() })).collect();
    Ok(Json(json!(out)))
}
pub async fn order_attachment_get(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>)
    -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let row = sqlx::query("SELECT data,content_type,work_order_id FROM app.work_order_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    // CA-1: foto, documenti e firme solo a chi può operare sull'ordine (come l'elenco allegati)
    order_edit_allowed(&st, &s, row.get::<Uuid,_>("work_order_id")).await?;
    let data: Vec<u8> = row.get("data"); let ct: String = row.get("content_type");
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "private, max-age=3600".to_string())], data).into_response())
}
pub async fn order_attachment_del(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    // il pianificatore/admin o chi l'ha caricato
    let owner: Option<String> = sqlx::query_scalar("SELECT created_by FROM app.work_order_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    if !is_planner(&s) && owner.as_deref() != Some(s.email.as_str()) { return Err(err(StatusCode::FORBIDDEN, "non autorizzato")); }
    sqlx::query("DELETE FROM app.work_order_attachments WHERE id=$1 AND tenant_id=$2").bind(aid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "order_attachment_del", json!({"id":aid})).await;
    Ok(Json(json!({"ok":true})))
}

// ---- consuntivi ----
#[derive(Deserialize)]
pub struct NewTimesheet { pub work_order_id: Option<Uuid>, pub note: Option<String> }
pub async fn create_timesheet(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewTimesheet>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "consuntivi").await { return Err(err(StatusCode::FORBIDDEN, "modulo consuntivi non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.timesheets(tenant_id,work_order_id,user_id,note) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(b.work_order_id).bind(s.user_id).bind(&b.note).fetch_one(&mut *tx).await.map_err(ise)?;
    let id: Uuid = row.get("id");
    // Storico completo: anche la creazione lascia traccia (Slice 1).
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'create',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"work_order_id":b.work_order_id})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id,"state":"bozza","version":1})))
}

/// Riassunto leggibile dei materiali dell'intervento, per la nota del consuntivo generato.
pub(crate) fn materials_note(m: &Value) -> Option<String> {
    let arr = m.as_array()?;
    let items: Vec<String> = arr.iter().filter_map(|x| {
        let d = x.get("descr").and_then(|v| v.as_str())?;
        let q = x.get("qta").and_then(|v| v.as_f64()).map(|q| format!(" {q}")).unwrap_or_default();
        let u = x.get("unita").and_then(|v| v.as_str()).map(|u| format!(" {u}")).unwrap_or_default();
        Some(format!("{d}{q}{u}"))
    }).collect();
    if items.is_empty() { None } else { Some(format!("Materiali intervento: {}", items.join("; "))) }
}

/// Ponte intervento -> consuntivo (Slice 2): genera (o riusa) una BOZZA di consuntivo per
/// l'assegnatario dell'ordine, prefillata con una riga "labor" dai minuti lavorati dell'intervento
/// (tariffa da valorizzare finché non c'è il listino). Idempotente: ri-eseguire non duplica la riga
/// (vincolo univoco per ordine+utente+labor). I materiali finiscono nella nota. Un solo inserimento:
/// il tecnico non ricompila ore/materiali a mano.
pub async fn generate_timesheet_from_order(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    if !module_active(&st, tid, "interventi").await { return Err(err(StatusCode::FORBIDDEN, "modulo interventi non abilitato")); }
    if !module_active(&st, tid, "consuntivi").await { return Err(err(StatusCode::FORBIDDEN, "modulo consuntivi non abilitato")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let o = sqlx::query("SELECT title,state,assigned_user,work_minutes,work_started_at,arrived_at,materials FROM app.work_orders WHERE id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "ordine non trovato"))?;
    let owner: Uuid = o.try_get::<Uuid,_>("assigned_user").map_err(|_| err(StatusCode::BAD_REQUEST, "ordine non assegnato a una persona"))?;
    // Autorizzazione: l'assegnatario stesso o un pianificatore.
    if owner != s.user_id && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo l'assegnatario o il pianificatore")); }
    // CA-18: si consuntiva un intervento finito, non uno in corso
    if o.get::<String,_>("state") != "completato" {
        return Err(err(StatusCode::CONFLICT, "l'intervento non è ancora completato: il consuntivo si genera a lavoro finito"));
    }
    let wm: i32 = o.try_get::<i32,_>("work_minutes").ok().filter(|m| *m > 0)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "nessuna esecuzione da consuntivare (minuti lavorati mancanti)"))?;
    let title: String = o.get("title");
    let started: Option<chrono::DateTime<chrono::Utc>> = o.try_get("work_started_at").ok();
    let arrived: Option<chrono::DateTime<chrono::Utc>> = o.try_get("arrived_at").ok();
    let wd = started.or(arrived).map(|t| t.date_naive()).unwrap_or_else(|| st.clock.now().date_naive());
    let materials: Value = o.try_get("materials").unwrap_or(json!([]));
    let labor_rate = ts_line_rate(&st, &s, None, owner, wd).await?; // CA-6: qualifica di chi ha lavorato
    let def_cur = tool_config(&st, tid, "consuntivi", "rates").await
        .get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
    // Riusa una bozza aperta per (ordine, assegnatario) oppure creane una.
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM app.timesheets WHERE work_order_id=$1 AND user_id=$2 AND state IN ('bozza','da_correggere') ORDER BY created_at DESC LIMIT 1")
        .bind(id).bind(owner).fetch_optional(&mut *tx).await.map_err(ise)?;
    let (ts_id, created) = match existing {
        Some(t) => (t, false),
        None => {
            let note = materials_note(&materials);
            let r = sqlx::query("INSERT INTO app.timesheets(tenant_id,work_order_id,user_id,note) VALUES($1,$2,$3,$4) RETURNING id")
                .bind(tid).bind(id).bind(owner).bind(note).fetch_one(&mut *tx).await.map_err(ise)?;
            let tsid: Uuid = r.get("id");
            sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'create',$3)")
                .bind(tid).bind(tsid).bind(json!({"by":s.email,"from_order":id})).execute(&mut *tx).await.map_err(ise)?;
            (tsid, true)
        }
    };
    // Riga labor idempotente dall'ordine (tariffa NULL = da valorizzare; listino = Slice 3).
    let ins = sqlx::query(
        "INSERT INTO app.timesheet_lines(tenant_id,timesheet_id,activity,work_date,minutes,rate_cents,currency,source_order_id,source_user_id,source_kind)
         VALUES($1,$2,$3,$4,$5,$9,$6,$7,$8,'labor')
         ON CONFLICT (tenant_id,source_order_id,source_user_id,source_kind) WHERE source_order_id IS NOT NULL DO NOTHING
         RETURNING id")
        // CA-14: tariffa dal listino (manodopera a ore); senza listino resta da valorizzare in revisione
        .bind(tid).bind(ts_id).bind(&title).bind(wd).bind(wm).bind(&def_cur).bind(id).bind(owner).bind(labor_rate)
        .fetch_optional(&mut *tx).await.map_err(ise)?;
    let line_added = ins.is_some();
    // CA-18: riga già presente (consuntivo generato prima di una correzione dei minuti): se il
    // consuntivo è ancora modificabile, minuti e data si riallineano all'intervento.
    let mut line_updated = false;
    if !line_added {
        let n = sqlx::query("UPDATE app.timesheet_lines l SET minutes=$4, work_date=$5
             FROM app.timesheets t WHERE t.id=l.timesheet_id AND t.state IN ('bozza','da_correggere')
               AND l.tenant_id=$1 AND l.source_order_id=$2 AND l.source_user_id=$3 AND l.source_kind='labor'
               AND (l.minutes<>$4 OR l.work_date<>$5)")
            .bind(tid).bind(id).bind(owner).bind(wm).bind(wd).execute(&mut *tx).await.map_err(ise)?.rows_affected();
        if n > 0 {
            line_updated = true;
            sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1").bind(ts_id).execute(&mut *tx).await.map_err(ise)?;
            sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'edit_line',$3)")
                .bind(tid).bind(ts_id).bind(json!({"by":s.email,"source":"order","order_id":id,"minutes":wm})).execute(&mut *tx).await.map_err(ise)?;
        }
    }
    if line_added {
        sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1").bind(ts_id).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'add_line',$3)")
            .bind(tid).bind(ts_id).bind(json!({"by":s.email,"source":"order","order_id":id,"minutes":wm})).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "timesheet_from_order", json!({"order_id":id,"timesheet_id":ts_id,"created":created,"line_added":line_added,"line_updated":line_updated})).await;
    Ok(Json(json!({"timesheet_id":ts_id,"created":created,"line_added":line_added,"line_updated":line_updated})))
}

pub async fn list_timesheets(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "consuntivi").await?;
    // ACL di sede (coerente con incidenti): i ruoli ristretti (operatore/lavoratore/reperibile,
    // anche con view_all via ruolo custom) vedono SOLO i propri consuntivi + quelli di ordini
    // nelle loro sedi. I ruoli non ristretti (responsabile/auditor/admin) con view_all vedono tutto.
    let allowed = allowed_sites(&st, &s).await;
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = if has_cap(&s,"timesheets.view_all") {
        match &allowed {
            None => sqlx::query("SELECT t.id,t.state,t.version,t.created_at,u.email AS owner,o.title AS order_title
                FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
                ORDER BY t.updated_at DESC").fetch_all(&mut *tx).await,
            Some(set) => {
                let sites: Vec<Uuid> = set.iter().copied().collect();
                sqlx::query("SELECT t.id,t.state,t.version,t.created_at,u.email AS owner,o.title AS order_title
                    FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
                    WHERE t.user_id=$1 OR o.site_id = ANY($2) ORDER BY t.updated_at DESC")
                    .bind(s.user_id).bind(&sites).fetch_all(&mut *tx).await
            }
        }
    } else {
        sqlx::query("SELECT t.id,t.state,t.version,t.created_at,u.email AS owner,o.title AS order_title
            FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id
            WHERE t.user_id=$1 ORDER BY t.updated_at DESC").bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "state": r.get::<String,_>("state"), "version": r.get::<i32,_>("version"),
        "owner": r.get::<String,_>("owner"), "order_title": r.try_get::<String,_>("order_title").ok() })).collect();
    Ok(Json(json!(out)))
}

pub(crate) async fn timesheet_detail(st: &AppState, tid: Uuid, id: Uuid) -> Result<Value, (StatusCode,String)> {
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let head = sqlx::query("SELECT t.id,t.state,t.note,t.review_note,t.user_id,t.version,o.site_id,u.email AS owner,o.title AS order_title
        FROM app.timesheets t JOIN app.users u ON u.id=t.user_id LEFT JOIN app.work_orders o ON o.id=t.work_order_id WHERE t.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "non trovato"))?;
    let lines = sqlx::query("SELECT id,activity,work_date,minutes,rate_cents,currency FROM app.timesheet_lines WHERE timesheet_id=$1 ORDER BY work_date")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let exps = sqlx::query("SELECT id,description,amount_cents,currency,has_receipt FROM app.expenses WHERE timesheet_id=$1")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    // CA-8: storico (invio, correzioni richieste con la nota, approvazione, modifiche)
    let evs = sqlx::query("SELECT kind,detail,at FROM app.timesheet_events WHERE timesheet_id=$1 ORDER BY at")
        .bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let history: Vec<Value> = evs.iter().map(|r| json!({"kind": r.get::<String,_>("kind"),
        "detail": r.try_get::<Value,_>("detail").unwrap_or(json!(null)),
        "at": r.get::<chrono::DateTime<chrono::Utc>,_>("at").to_rfc3339()})).collect();
    // Per-tenant billing policy (consuntivi/rates): round billed minutes up, default currency.
    let rates = tool_config(st, tid, "consuntivi", "rates").await;
    let round = rates.get("round_minutes").and_then(|v| v.as_i64()).unwrap_or(15).max(1);
    let currency = rates.get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
    let mut minutes = 0i64; let mut billed = 0i64; let mut labor = 0i64; let mut missing_rate = false;
    // CA-14: i totali non sommano valute diverse: il totale è nella valuta del tenant, le altre a parte
    let mut by_cur: std::collections::BTreeMap<String,(i64,i64)> = std::collections::BTreeMap::new();
    let jl: Vec<Value> = lines.iter().map(|r| {
        let m = r.get::<i32,_>("minutes") as i64; minutes += m;
        let bm = if m > 0 { ((m + round - 1) / round) * round } else { 0 }; billed += bm;
        let rate: Option<i64> = r.try_get("rate_cents").ok();
        let lc: String = r.get("currency");
        // arrotondamento al centesimo (half-up) per riga: evita il troncamento della divisione intera
        match rate { Some(rc) => { let a = (bm * rc + 30) / 60; by_cur.entry(lc.clone()).or_default().0 += a; if lc == currency { labor += a; } }, None => missing_rate = true }
        json!({"id":r.get::<Uuid,_>("id"),"activity":r.get::<String,_>("activity"),
            "work_date":r.get::<chrono::NaiveDate,_>("work_date").to_string(),
            "minutes":m,"billed_minutes":bm,"rate_cents":rate,"currency":r.get::<String,_>("currency")})
    }).collect();
    let mut exp_total = 0i64;
    let je: Vec<Value> = exps.iter().map(|r| {
        let a = r.get::<i64,_>("amount_cents"); let ec: String = r.get("currency");
        by_cur.entry(ec.clone()).or_default().1 += a; if ec == currency { exp_total += a; }
        json!({"id":r.get::<Uuid,_>("id"),"description":r.get::<String,_>("description"),
            "amount_cents":a,"currency":r.get::<String,_>("currency"),"has_receipt":r.get::<bool,_>("has_receipt")})
    }).collect();
    Ok(json!({
        "id": head.get::<Uuid,_>("id"), "state": head.get::<String,_>("state"), "version": head.get::<i32,_>("version"),
        "owner": head.get::<String,_>("owner"), "owner_id": head.get::<Uuid,_>("user_id"),
        "site_id": head.try_get::<Uuid,_>("site_id").ok(),
        "order_title": head.try_get::<String,_>("order_title").ok(), "note": head.try_get::<String,_>("note").ok(),
        "review_note": head.try_get::<String,_>("review_note").ok(), "history": history,
        "lines": jl, "expenses": je, "currency": currency, "round_minutes": round,
        "total": {"minutes":minutes,"billed_minutes":billed,"labor_cents":labor,"expense_cents":exp_total,"total_cents":labor+exp_total,"missing_rate":missing_rate,
            "mixed_currency": by_cur.keys().any(|c| *c != currency),
            "by_currency": by_cur.iter().map(|(c,(l,e))| (c.clone(), json!({"labor_cents":l,"expense_cents":e,"total_cents":l+e}))).collect::<serde_json::Map<String,Value>>()}
    }))
}

/// ACL di lettura di un consuntivo: proprietario sempre; altrimenti serve `timesheets.view_all`
/// e, per i ruoli ristretti per sede, il consuntivo deve appartenere a una sede consentita.
pub(crate) async fn guard_timesheet_access(st: &AppState, s: &Session, d: &Value) -> Result<(),(StatusCode,String)> {
    let owner_id = d["owner_id"].as_str().and_then(|x| Uuid::parse_str(x).ok());
    if owner_id == Some(s.user_id) { return Ok(()); }
    if !has_cap(s,"timesheets.view_all") { return Err(err(StatusCode::FORBIDDEN, "non autorizzato")); }
    if let Some(set) = allowed_sites(st, s).await {
        let site = d["site_id"].as_str().and_then(|x| Uuid::parse_str(x).ok());
        match site { Some(si) if set.contains(&si) => {}, _ => return Err(err(StatusCode::FORBIDDEN, "consuntivo fuori dalle tue sedi")) }
    }
    Ok(())
}

/// CA-9: chi approva/rimanda deve avere il modulo attivo e, se ristretto per sede, il consuntivo
/// deve essere di una sua sede (un consuntivo senza sede non è approvabile da un ruolo ristretto).
pub(crate) async fn guard_timesheet_review(st: &AppState, s: &Session, id: Uuid) -> Result<(),(StatusCode,String)> {
    let tid = s.tenant_id.unwrap();
    if !module_active(st, tid, "consuntivi").await { return Err(err(StatusCode::FORBIDDEN, "modulo consuntivi non abilitato")); }
    if let Some(set) = allowed_sites(st, s).await {
        let site: Option<Uuid> = sqlx::query_scalar("SELECT o.site_id FROM app.timesheets t LEFT JOIN app.work_orders o ON o.id=t.work_order_id WHERE t.id=$1 AND t.tenant_id=$2")
            .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "non trovato"))?;
        match site { Some(si) if set.contains(&si) => {}, _ => return Err(err(StatusCode::FORBIDDEN, "consuntivo fuori dalle tue sedi")) }
    }
    Ok(())
}
pub async fn get_timesheet(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "consuntivi").await?; // CA-9
    let d = timesheet_detail(&st, s.tenant_id.unwrap(), id).await?;
    guard_timesheet_access(&st, &s, &d).await?;
    Ok(Json(d))
}

#[derive(Deserialize)]
pub struct NewLine { pub activity: String, pub work_date: String, pub minutes: i32, pub rate_cents: Option<i64>, pub currency: Option<String> }
pub async fn add_line(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<NewLine>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let wd = chrono::NaiveDate::parse_from_str(&b.work_date, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "data non valida"))?;
    let (act, minutes, cur) = ts_line_validate(&st, tid, &b.activity, b.minutes, b.currency.as_deref()).await?;
    let rate = ts_line_rate(&st, &s, b.rate_cents, s.user_id, wd).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_lines(tenant_id,timesheet_id,activity,work_date,minutes,rate_cents,currency) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(tid).bind(id).bind(&act).bind(wd).bind(minutes).bind(rate)
        .bind(&cur).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'add_line',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"activity":b.activity,"minutes":b.minutes})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct NewExpense { pub description: String, pub amount_cents: i64, pub currency: Option<String>, pub has_receipt: Option<bool> }
pub async fn add_expense(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<NewExpense>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let (desc, amount, cur) = ts_expense_validate(&st, tid, &b.description, b.amount_cents, b.currency.as_deref()).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.expenses(tenant_id,timesheet_id,description,amount_cents,currency,has_receipt) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(tid).bind(id).bind(&desc).bind(amount)
        .bind(&cur).bind(b.has_receipt.unwrap_or(false)).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'add_expense',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"description":b.description,"amount_cents":b.amount_cents})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---- CA-8 / CA-14: correzione di un consuntivo, valori validati, tariffa dal listino ----
pub(crate) async fn ts_currency(st: &AppState, tid: Uuid, c: Option<&str>) -> Result<String,(StatusCode,String)> {
    let def = tool_config(st, tid, "consuntivi", "rates").await.get("currency").and_then(|v| v.as_str()).unwrap_or("EUR").to_string();
    let c = c.map(|x| x.trim().to_ascii_uppercase()).filter(|x| !x.is_empty()).unwrap_or(def);
    if c.len() == 3 && c.chars().all(|ch| ch.is_ascii_uppercase()) { Ok(c) } else { Err(err(StatusCode::BAD_REQUEST, "valuta non valida (es. EUR)")) }
}
pub(crate) async fn ts_line_validate(st: &AppState, tid: Uuid, activity: &str, minutes: i32, cur: Option<&str>) -> Result<(String,i32,String),(StatusCode,String)> {
    let a = activity.trim(); if a.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "attività richiesta")); }
    if !(1..=1440).contains(&minutes) { return Err(err(StatusCode::BAD_REQUEST, "minuti tra 1 e 1440 (24 ore)")); }
    Ok((a.to_string(), minutes, ts_currency(st, tid, cur).await?))
}
pub(crate) async fn ts_expense_validate(st: &AppState, tid: Uuid, desc: &str, amount: i64, cur: Option<&str>) -> Result<(String,i64,String),(StatusCode,String)> {
    let d = desc.trim(); if d.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "descrizione richiesta")); }
    if amount <= 0 { return Err(err(StatusCode::BAD_REQUEST, "importo della spesa maggiore di zero")); }
    Ok((d.to_string(), amount, ts_currency(st, tid, cur).await?))
}
/// CA-14: chi consuntiva non sceglie la propria tariffa. La tariffa indicata vale solo per chi
/// approva, pianifica o vede i costi; per gli altri si usa la tariffa oraria di manodopera del
/// listino (prima voce attiva «manodopera» a ore), altrimenti resta da valorizzare (NULL).
// ---------------- CA-4: registro unico delle ore (vista app.work_hours, mig 0158) ----------------
/// Ore lavorate per persona e giorno da tutte le fonti (intervento, ronda, presenza, turno), con le
/// sovrapposizioni: una registrazione coperta da una fonte più specifica conta solo per la parte
/// non coperta (`effective_min`). In più: turni pianificati confrontati con il lavoro registrato.
/// Chi approva, pianifica o vede tutti i consuntivi vede tutti; gli altri solo sé stessi.
pub async fn work_hours_report(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, &["consuntivi","workforce","cantieri","giri","interventi"]).await?;
    let day = |k: &str| q.get(k).and_then(|v| chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").ok());
    // giorni nell'ORA LOCALE dell'azienda (prima UTC: un turno 00:30 ora italiana finiva nel giorno prima)
    let tz = tenant_tz(&st, tid).await;
    let today = st.clock.now().with_timezone(&tz).date_naive();
    let to = day("to").unwrap_or(today);
    let from = day("from").unwrap_or(to - chrono::Duration::days(6));
    if from > to || (to - from).num_days() > 62 { return Err(err(StatusCode::BAD_REQUEST, "periodo non valido (al massimo 62 giorni)")); }
    let all = is_approver(&s) || is_planner(&s) || has_cap(&s, "timesheets.view_all");
    let who: Option<Uuid> = if all { q.get("user_id").and_then(|v| Uuid::parse_str(v).ok()) } else { Some(s.user_id) };
    let (t0, t1) = (local_midnight_utc(tz, from), local_midnight_utc(tz, to + chrono::Duration::days(1)));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT w.kind,w.prio,w.source_id,w.user_id,u.email,w.started_at,w.ended_at,w.billable,w.work_order_id,w.commessa_id,w.label
            FROM app.work_hours w JOIN app.users u ON u.id=w.user_id
           WHERE w.started_at < $2 AND w.ended_at > $1 AND ($3::uuid IS NULL OR w.user_id=$3)
           ORDER BY u.email, w.started_at")
        .bind(t0).bind(t1).bind(who).fetch_all(&mut *tx).await.map_err(ise)?;
    let plans = sqlx::query("SELECT p.id,p.user_id,u.email,p.starts_at,p.ends_at,
              (SELECT min(w.started_at) FROM app.work_hours w WHERE w.user_id=p.user_id AND w.started_at<p.ends_at AND w.ended_at>p.starts_at) AS first_start,
              (SELECT COALESCE(sum(EXTRACT(EPOCH FROM (LEAST(p.ends_at,w.ended_at)-GREATEST(p.starts_at,w.started_at)))/60.0),0)::float8
                 FROM app.work_hours w WHERE w.user_id=p.user_id AND w.kind='turno' AND w.started_at<p.ends_at AND w.ended_at>p.starts_at) AS worked
            FROM app.shift_plans p JOIN app.users u ON u.id=p.user_id
           WHERE p.state<>'annullato' AND p.starts_at < $2 AND p.ends_at > $1 AND p.ends_at <= $4 AND ($3::uuid IS NULL OR p.user_id=$3)
           ORDER BY p.starts_at")
        .bind(t0).bind(t1).bind(who).bind(st.clock.now()).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // sovrapposizioni: per ogni registrazione, l'UNIONE degli intervalli delle fonti più specifiche
    // della stessa persona (così un intervento dentro una presenza dentro un turno non si sottrae due volte)
    let iv: Vec<(Uuid, i32, Dt, Dt)> = rows.iter().map(|r| (r.get("user_id"), r.get("prio"), r.get("started_at"), r.get("ended_at"))).collect();
    let covered = |i: usize| -> f64 {
        let (u, p, a, b) = iv[i];
        let mut segs: Vec<(Dt, Dt)> = iv.iter().filter(|(u2, p2, a2, b2)| *u2 == u && *p2 < p && *a2 < b && *b2 > a)
            .map(|(_, _, a2, b2)| ((*a2).max(a), (*b2).min(b))).collect();
        segs.sort();
        let (mut tot, mut cur): (i64, Option<(Dt, Dt)>) = (0, None);
        for (x, y) in segs { match cur { Some((cx, cy)) if x <= cy => cur = Some((cx, cy.max(y))), Some((cx, cy)) => { tot += (cy - cx).num_seconds(); cur = Some((x, y)); } None => cur = Some((x, y)) } }
        if let Some((cx, cy)) = cur { tot += (cy - cx).num_seconds(); }
        tot as f64 / 60.0
    };
    let mut by_day: std::collections::BTreeMap<(String, chrono::NaiveDate), (f64, f64, f64)> = Default::default();
    let entries: Vec<Value> = rows.iter().enumerate().map(|(i, r)| {
        let minutes: f64 = (iv[i].3 - iv[i].2).num_seconds() as f64 / 60.0;
        let overlap: f64 = covered(i);
        let eff = (minutes - overlap).max(0.0);
        let started: Dt = r.get("started_at");
        let email: String = r.get("email"); let billable: bool = r.get("billable");
        let e = by_day.entry((email.clone(), started.with_timezone(&tz).date_naive())).or_default();
        e.0 += eff; if billable { e.1 += eff; } e.2 += overlap.min(minutes);
        json!({"kind": r.get::<String,_>("kind"), "source_id": r.get::<Uuid,_>("source_id"), "user_id": r.get::<Uuid,_>("user_id"),
               "email": email, "started_at": started.to_rfc3339(), "ended_at": r.get::<Dt,_>("ended_at").to_rfc3339(),
               "minutes": minutes.round() as i64, "effective_min": eff.round() as i64, "overlap_min": overlap.min(minutes).round() as i64,
               "billable": billable, "label": r.try_get::<Option<String>,_>("label").ok().flatten(),
               "work_order_id": r.try_get::<Option<Uuid>,_>("work_order_id").ok().flatten(), "commessa_id": r.try_get::<Option<Uuid>,_>("commessa_id").ok().flatten()})
    }).collect();
    let days: Vec<Value> = by_day.iter().map(|((email, d), (eff, bill, ov))| json!({"email": email, "day": d,
        "effective_min": eff.round() as i64, "billable_min": bill.round() as i64, "overlap_min": ov.round() as i64})).collect();
    let plan_out: Vec<Value> = plans.iter().map(|r| {
        let starts: Dt = r.get("starts_at"); let ends: Dt = r.get("ends_at");
        let first: Option<Dt> = r.try_get("first_start").ok().flatten();
        let worked: f64 = r.try_get::<f64,_>("worked").unwrap_or(0.0);
        let planned = (ends - starts).num_minutes();
        json!({"id": r.get::<Uuid,_>("id"), "email": r.get::<String,_>("email"), "starts_at": starts.to_rfc3339(), "ends_at": ends.to_rfc3339(),
               "planned_min": planned, "worked_min": worked.round() as i64,
               "late_min": first.map(|f| (f - starts).num_minutes().max(0)),
               "missing": first.is_none()})
    }).collect();
    Ok(Json(json!({"from": from, "to": to, "timezone": tz.name(), "all": all, "entries": entries, "days": days, "plans": plan_out,
        "rules": "intervento > ronda > presenza > turno; ronde e turni non fatturabili"})))
}

/// CA-6 (PO 25/11 sera): la tariffa viene dal listino unico per QUALIFICA della persona, valida in
/// quel giorno (app.yw_labor_rate_cents: prima la qualifica, poi la voce per tutti).
pub(crate) async fn ts_line_rate(st: &AppState, s: &Session, asked: Option<i64>, worker: Uuid, day: chrono::NaiveDate) -> Result<Option<i64>,(StatusCode,String)> {
    if let Some(r) = asked { if r < 0 { return Err(err(StatusCode::BAD_REQUEST, "tariffa negativa")); } }
    if asked.is_some() && (is_approver(s) || can_see_costs(s)) { return Ok(asked); }
    Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT app.yw_labor_rate_cents($1,$2,$3)")
        .bind(s.tenant_id.unwrap()).bind(worker).bind(day).fetch_one(&st.owner_pool).await.map_err(ise)?)
}
pub(crate) async fn ts_touch(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tid: Uuid, id: Uuid, kind: &str, detail: Value) -> Result<(),(StatusCode,String)> {
    sqlx::query("UPDATE app.timesheets SET version=version+1,updated_at=now() WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut **tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,$3,$4)")
        .bind(tid).bind(id).bind(kind).bind(detail).execute(&mut **tx).await.map_err(ise)?;
    Ok(())
}
pub async fn update_line(State(st): State<AppState>, h: HeaderMap, Path((id, lid)): Path<(Uuid, Uuid)>, Json(b): Json<NewLine>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let wd = chrono::NaiveDate::parse_from_str(&b.work_date, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "data non valida"))?;
    let (act, minutes, cur) = ts_line_validate(&st, tid, &b.activity, b.minutes, b.currency.as_deref()).await?;
    let can_rate = is_approver(&s) || can_see_costs(&s);
    if let Some(r) = b.rate_cents { if r < 0 { return Err(err(StatusCode::BAD_REQUEST, "tariffa negativa")); } }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // la tariffa cambia solo per chi può fissarla E solo se la manda: modificare ore o attività senza
    // indicare la tariffa non la cancella (prima la console la azzerava a chi approva o vede i costi)
    let n = sqlx::query("UPDATE app.timesheet_lines SET activity=$3,work_date=$4,minutes=$5,currency=$6,
                         rate_cents=CASE WHEN $7 AND $8::bigint IS NOT NULL THEN $8 ELSE rate_cents END WHERE id=$1 AND timesheet_id=$2")
        .bind(lid).bind(id).bind(&act).bind(wd).bind(minutes).bind(&cur).bind(can_rate).bind(b.rate_cents)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n == 0 { let _ = tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "riga inesistente")); }
    ts_touch(&mut tx, tid, id, "edit_line", json!({"by":s.email,"line_id":lid,"activity":act,"minutes":minutes})).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_line(State(st): State<AppState>, h: HeaderMap, Path((id, lid)): Path<(Uuid, Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("DELETE FROM app.timesheet_lines WHERE id=$1 AND timesheet_id=$2 RETURNING activity,minutes")
        .bind(lid).bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some(row) = row else { let _ = tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "riga inesistente")); };
    ts_touch(&mut tx, tid, id, "delete_line", json!({"by":s.email,"activity":row.get::<String,_>("activity"),"minutes":row.get::<i32,_>("minutes")})).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn update_expense(State(st): State<AppState>, h: HeaderMap, Path((id, eid)): Path<(Uuid, Uuid)>, Json(b): Json<NewExpense>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let (desc, amount, cur) = ts_expense_validate(&st, tid, &b.description, b.amount_cents, b.currency.as_deref()).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.expenses SET description=$3,amount_cents=$4,currency=$5,has_receipt=COALESCE($6,has_receipt) WHERE id=$1 AND timesheet_id=$2")
        .bind(eid).bind(id).bind(&desc).bind(amount).bind(&cur).bind(b.has_receipt).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n == 0 { let _ = tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "spesa inesistente")); }
    ts_touch(&mut tx, tid, id, "edit_expense", json!({"by":s.email,"expense_id":eid,"description":desc,"amount_cents":amount})).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_expense(State(st): State<AppState>, h: HeaderMap, Path((id, eid)): Path<(Uuid, Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    guard_owner_editable(&st, tid, id, s.user_id).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("DELETE FROM app.expenses WHERE id=$1 AND timesheet_id=$2 RETURNING description,amount_cents")
        .bind(eid).bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some(row) = row else { let _ = tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "spesa inesistente")); };
    ts_touch(&mut tx, tid, id, "delete_expense", json!({"by":s.email,"description":row.get::<String,_>("description"),"amount_cents":row.get::<i64,_>("amount_cents")})).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct LineRateReq { pub rate_cents: i64 }
/// CA-14: chi approva valorizza le righe senza tariffa durante la revisione (separazione dei compiti).
pub async fn set_line_rate(State(st): State<AppState>, h: HeaderMap, Path((id, lid)): Path<(Uuid, Uuid)>, Json(b): Json<LineRateReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), &["consuntivi"]).await?; // AR-1: controllo del modulo dichiarato
    if !is_approver(&s) { return Err(err(StatusCode::FORBIDDEN, "solo chi approva")); }
    let tid = s.tenant_id.unwrap();
    guard_timesheet_review(&st, &s, id).await?;
    if b.rate_cents < 0 { return Err(err(StatusCode::BAD_REQUEST, "tariffa negativa")); }
    let allow_self = tool_config(&st, tid, "consuntivi", "approval").await
        .get("allow_self_approve").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("SELECT user_id,state FROM app.timesheets WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "non trovato"))?;
    if row.get::<Uuid,_>("user_id") == s.user_id && !allow_self { let _ = tx.rollback().await; return Err(err(StatusCode::FORBIDDEN, "non puoi valorizzare il tuo consuntivo (separazione dei compiti)")); }
    if row.get::<String,_>("state") != "in_revisione" { let _ = tx.rollback().await; return Err(err(StatusCode::CONFLICT, "la tariffa si fissa durante la revisione")); }
    let n = sqlx::query("UPDATE app.timesheet_lines SET rate_cents=$3 WHERE id=$1 AND timesheet_id=$2").bind(lid).bind(id).bind(b.rate_cents)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n == 0 { let _ = tx.rollback().await; return Err(err(StatusCode::NOT_FOUND, "riga inesistente")); }
    ts_touch(&mut tx, tid, id, "set_rate", json!({"by":s.email,"line_id":lid,"rate_cents":b.rate_cents})).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub(crate) async fn guard_owner_editable(st: &AppState, tid: Uuid, id: Uuid, uid: Uuid) -> Result<(),(StatusCode,String)> {
    if !module_active(st, tid, "consuntivi").await { return Err(err(StatusCode::FORBIDDEN, "modulo consuntivi non abilitato")); } // CA-9
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("SELECT user_id,state FROM app.timesheets WHERE id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let _ = tx.commit().await;
    let row = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "non trovato"))?;
    if row.get::<Uuid,_>("user_id") != uid { return Err(err(StatusCode::FORBIDDEN, "non tuo")); }
    let stt: String = row.get("state");
    if !matches!(stt.as_str(), "bozza"|"da_correggere") { return Err(err(StatusCode::CONFLICT, "non modificabile in questo stato")); }
    Ok(())
}

pub async fn submit_timesheet(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "consuntivi").await?; // CA-9
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.timesheets SET state='in_revisione',version=version+1,updated_at=now() WHERE id=$1 AND user_id=$2 AND state IN ('bozza','da_correggere')")
        .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"non inviabile")); }
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'submit',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":"in_revisione"})))
}

pub async fn approve_timesheet(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), &["consuntivi"]).await?; // AR-1: controllo del modulo dichiarato
    if !is_approver(&s) { return Err(err(StatusCode::FORBIDDEN, "solo responsabile")); }
    let tid = s.tenant_id.unwrap();
    guard_timesheet_review(&st, &s, id).await?; // CA-9: modulo + sedi dell'approvatore
    // Segregation of duties: di default NON si approva il proprio consuntivo. Un tenant può
    // consentirlo esplicitamente (consuntivi.approval.allow_self_approve=true) per realtà piccole.
    let allow_self = tool_config(&st, tid, "consuntivi", "approval").await
        .get("allow_self_approve").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("SELECT user_id,state FROM app.timesheets WHERE id=$1").bind(id)
        .fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"non trovato"))?;
    if row.get::<Uuid,_>("user_id") == s.user_id && !allow_self {
        let _=tx.rollback().await; return Err(err(StatusCode::FORBIDDEN,"non puoi approvare il tuo consuntivo (separazione dei compiti)"));
    }
    // only from in_revisione -> blocks double approval
    let n = sqlx::query("UPDATE app.timesheets SET state='approvato',approved_by=$2,version=version+1,updated_at=now() WHERE id=$1 AND state='in_revisione'")
        .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"non in revisione (doppia approvazione bloccata)")); }
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'approve',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":"approvato"})))
}

#[derive(Deserialize)]
pub struct ReqChanges { pub note: String }
pub async fn request_changes(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ReqChanges>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), &["consuntivi"]).await?; // AR-1: controllo del modulo dichiarato
    if !is_approver(&s) { return Err(err(StatusCode::FORBIDDEN, "solo responsabile")); }
    let tid = s.tenant_id.unwrap();
    guard_timesheet_review(&st, &s, id).await?; // CA-9
    let allow_self = tool_config(&st, tid, "consuntivi", "approval").await
        .get("allow_self_approve").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let owner = sqlx::query_scalar::<_,Uuid>("SELECT user_id FROM app.timesheets WHERE id=$1").bind(id)
        .fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"non trovato"))?;
    if owner == s.user_id && !allow_self {
        let _=tx.rollback().await; return Err(err(StatusCode::FORBIDDEN,"non puoi revisionare il tuo consuntivo (separazione dei compiti)"));
    }
    let n = sqlx::query("UPDATE app.timesheets SET state='da_correggere',review_note=$2,version=version+1,updated_at=now() WHERE id=$1 AND state='in_revisione'")
        .bind(id).bind(&b.note).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { let _=tx.rollback().await; return Err(err(StatusCode::CONFLICT,"non in revisione")); }
    sqlx::query("INSERT INTO app.timesheet_events(tenant_id,timesheet_id,kind,detail) VALUES($1,$2,'request_changes',$3)")
        .bind(tid).bind(id).bind(json!({"by":s.email,"note":b.note})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"state":"da_correggere"})))
}

pub async fn export_timesheet_csv(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Response, (StatusCode,String)> {
    let s = require_tenant(&st, &h).await?;
    require_module(&st, s.tenant_id.unwrap(), "consuntivi").await?; // CA-9
    let d = timesheet_detail(&st, s.tenant_id.unwrap(), id).await?;
    guard_timesheet_access(&st, &s, &d).await?;
    let cur = d["currency"].as_str().unwrap_or("EUR").to_string();
    let mut csv = String::from("tipo,descrizione,data,minuti,minuti_fatturati,tariffa_cent_ora,importo_cent,valuta,giustificativo\n");
    for l in d["lines"].as_array().unwrap_or(&vec![]) {
        let m = l["minutes"].as_i64().unwrap_or(0);
        let bm = l["billed_minutes"].as_i64().unwrap_or(m);
        let rate = l["rate_cents"].as_i64();
        let amt = rate.map(|r| (bm*r+30)/60); // half-up, coerente con il calcolo del dettaglio
        csv.push_str(&format!("riga,{},{},{},{},{},{},{},\n",
            csv_esc(l["activity"].as_str().unwrap_or("")), l["work_date"].as_str().unwrap_or(""), m, bm,
            rate.map(|x|x.to_string()).unwrap_or_default(), amt.map(|x|x.to_string()).unwrap_or_default(),
            l["currency"].as_str().unwrap_or(&cur)));
    }
    for e in d["expenses"].as_array().unwrap_or(&vec![]) {
        csv.push_str(&format!("spesa,{},,,,,{},{},{}\n",
            csv_esc(e["description"].as_str().unwrap_or("")), e["amount_cents"].as_i64().unwrap_or(0),
            e["currency"].as_str().unwrap_or(&cur), e["has_receipt"].as_bool().unwrap_or(false)));
    }
    let t = &d["total"];
    csv.push_str(&format!("TOTALE,,,{},{},,{},{},\n", t["minutes"].as_i64().unwrap_or(0), t["billed_minutes"].as_i64().unwrap_or(0), t["total_cents"].as_i64().unwrap_or(0), cur));
    let mut resp = csv.into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, "text/csv; charset=utf-8".parse().unwrap());
    resp.headers_mut().insert(header::CONTENT_DISPOSITION, format!("attachment; filename=consuntivo-{}.csv", id).parse().unwrap());
    Ok(resp)
}
pub(crate) fn csv_esc(s: &str) -> String { if s.contains(',')||s.contains('"') { format!("\"{}\"", s.replace('"',"\"\"")) } else { s.to_string() } }


// ============================ Campo Slice 2 — Ronde e punzonature (giri) ============================
pub(crate) const PATROL_KINDS: &[&str] = &["qr","nfc","beacon","gps"];

// ---- Configurazione (pianificatore) ----
pub async fn list_patrol_routes(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // ACL per gruppo: il pianificatore vede tutti i giri; gli altri vedono i giri senza gruppo
    // (aperti, comportamento storico) + quelli assegnati a un gruppo di cui sono membri.
    let base = "SELECT r.id,r.name,r.site_id,r.window_minutes,r.active,r.assigned_group,r.mode,r.grace_min,r.manual_code_allowed,s.name as site_name,g.name as group_name \
        FROM app.patrol_routes r LEFT JOIN app.sites s ON s.id=r.site_id LEFT JOIN app.user_groups g ON g.id=r.assigned_group";
    let routes = if is_planner(&s) {
        sqlx::query(&format!("{base} ORDER BY r.name")).fetch_all(&mut *tx).await
    } else {
        sqlx::query(&format!("{base} WHERE r.assigned_group IS NULL OR r.assigned_group IN (SELECT group_id FROM app.user_group_members WHERE user_id=$1) ORDER BY r.name")).bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    let cps = sqlx::query("SELECT id,route_id,name,position,kind,code,lat,lon,radius_m,expected_min,max_min,dwell_max_min FROM app.patrol_checkpoints ORDER BY position,name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // CA-2: il codice atteso resta a chi configura; alla guardia non arriva mai (altrimenti la
    // punzonatura si potrebbe fare senza essere sul posto).
    let show_code = is_planner(&s);
    let out: Vec<Value> = routes.iter().map(|r| {
        let rid: Uuid = r.get("id");
        let checkpoints: Vec<Value> = cps.iter().filter(|c| c.get::<Uuid,_>("route_id")==rid).map(|c| json!({
            "id":c.get::<Uuid,_>("id"),"name":c.get::<String,_>("name"),"position":c.get::<i32,_>("position"),
            "kind":c.get::<String,_>("kind"),"code":if show_code { c.try_get::<String,_>("code").ok() } else { None },
            "lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),"radius_m":c.get::<i32,_>("radius_m"),
            "expected_min":c.try_get::<i32,_>("expected_min").ok(),"max_min":c.try_get::<i32,_>("max_min").ok(),"dwell_max_min":c.try_get::<i32,_>("dwell_max_min").ok()})).collect();
        json!({"id":rid,"name":r.get::<String,_>("name"),"site_id":r.try_get::<Uuid,_>("site_id").ok(),
            "site_name":r.try_get::<String,_>("site_name").ok(),"window_minutes":r.try_get::<i32,_>("window_minutes").ok(),
            "active":r.get::<bool,_>("active"),"assigned_group":r.try_get::<Uuid,_>("assigned_group").ok(),
            "mode":r.try_get::<String,_>("mode").ok(),"grace_min":r.try_get::<i32,_>("grace_min").ok(),
            "group_name":r.try_get::<String,_>("group_name").ok(),
            "manual_code_allowed":r.try_get::<bool,_>("manual_code_allowed").unwrap_or(true),"checkpoints":checkpoints})
    }).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct PatrolRouteReq { pub name: String, #[serde(default)] pub site_id: Option<Uuid>, #[serde(default)] pub window_minutes: Option<i32>, #[serde(default)] pub active: Option<bool>, #[serde(default)] pub assigned_group: Option<Uuid>, #[serde(default)] pub clear_group: Option<bool>, #[serde(default)] pub mode: Option<String>, #[serde(default)] pub grace_min: Option<i32>, #[serde(default)] pub manual_code_allowed: Option<bool> }
pub async fn create_patrol_route(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PatrolRouteReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.patrol_routes(tenant_id,name,site_id,window_minutes,assigned_group,mode,grace_min,manual_code_allowed) VALUES($1,$2,$3,$4,$5,COALESCE($6,'piedi'),COALESCE($7,0),COALESCE($8,true)) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(b.site_id).bind(b.window_minutes).bind(b.assigned_group).bind(&b.mode).bind(b.grace_min).bind(b.manual_code_allowed)
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "esiste già un percorso di ronda con questo nome"))?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_patrol_route", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn update_patrol_route(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PatrolRouteReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome del percorso richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let clear_g = b.clear_group.unwrap_or(false);
    let n = sqlx::query("UPDATE app.patrol_routes SET name=$2, site_id=$3, window_minutes=$4, active=COALESCE($5,active), assigned_group=CASE WHEN $6 THEN NULL ELSE COALESCE($7,assigned_group) END, mode=COALESCE($8,mode), grace_min=COALESCE($9,grace_min), manual_code_allowed=COALESCE($10,manual_code_allowed) WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(b.site_id).bind(b.window_minutes).bind(b.active).bind(clear_g).bind(b.assigned_group).bind(&b.mode).bind(b.grace_min).bind(b.manual_code_allowed).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "percorso di ronda inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_patrol_route(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.patrol_routes WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "percorso di ronda inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct CheckpointReq { pub name: String, pub kind: String, #[serde(default)] pub code: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>, #[serde(default)] pub radius_m: Option<i32>, #[serde(default)] pub position: Option<i32>,
    #[serde(default)] pub expected_min: Option<i32>, #[serde(default)] pub max_min: Option<i32>, #[serde(default)] pub dwell_max_min: Option<i32> }
pub async fn create_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<CheckpointReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if !PATROL_KINDS.contains(&b.kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo checkpoint sconosciuto (qr/nfc/beacon/gps)")); }
    if b.kind=="gps" && (b.lat.is_none()||b.lon.is_none()) { return Err(err(StatusCode::BAD_REQUEST, "checkpoint GPS richiede lat/lon")); }
    let given = b.code.as_deref().unwrap_or("").trim().to_string();
    if b.kind=="beacon" && given.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "il checkpoint beacon richiede l'identificativo del beacon")); }
    // CA-2: QR/NFC senza codice indicato -> codice casuale non indovinabile generato dal sistema.
    let generated = (b.kind=="qr"||b.kind=="nfc") && given.is_empty();
    let code: Option<String> = if b.kind=="gps" { None } else if generated { Some(patrol_code()) } else { Some(given.clone()) };
    let weak = !generated && b.kind!="gps" && b.kind!="beacon" && given.chars().count() < 10;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.patrol_routes WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"percorso di ronda inesistente")); }
    let pos = match b.position { Some(p) => p, None => {
        let m: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(position),0)::int FROM app.patrol_checkpoints WHERE route_id=$1").bind(rid).fetch_one(&mut *tx).await.map_err(ise)?;
        m + 1 } };
    let row = sqlx::query("INSERT INTO app.patrol_checkpoints(tenant_id,route_id,name,position,kind,code,lat,lon,radius_m,expected_min,max_min,dwell_max_min) VALUES($1,$2,$3,$4,$5,$6,$7,$8,COALESCE($9,50),$10,$11,$12) RETURNING id")
        .bind(tid).bind(rid).bind(b.name.trim()).bind(pos).bind(&b.kind).bind(code.as_deref()).bind(b.lat).bind(b.lon).bind(b.radius_m).bind(b.expected_min).bind(b.max_min).bind(b.dwell_max_min)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    let id = row.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    let warning = if weak { Some("Codice corto o prevedibile: chi lo conosce può punzonare senza essere sul posto. Lascia il campo vuoto per farlo generare dal sistema.") } else { None };
    Ok(Json(json!({"id":id,"code":code,"generated":generated,"warning":warning})))
}
pub async fn update_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<CheckpointReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if !PATROL_KINDS.contains(&b.kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo checkpoint sconosciuto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.patrol_checkpoints SET name=$2, kind=$3, code=COALESCE(NULLIF(btrim($4),''),code), lat=$5, lon=$6, radius_m=COALESCE($7,radius_m), position=COALESCE($8,position), expected_min=$9, max_min=$10, dwell_max_min=$11 WHERE id=$1")
        .bind(id).bind(b.name.trim()).bind(&b.kind).bind(b.code.as_deref()).bind(b.lat).bind(b.lon).bind(b.radius_m).bind(b.position).bind(b.expected_min).bind(b.max_min).bind(b.dwell_max_min).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "checkpoint inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.patrol_checkpoints WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "checkpoint inesistente")); }
    Ok(Json(json!({"ok":true})))
}
/// Checkpoint riusabili fra giri: copia la definizione di un checkpoint esistente (stesso tenant)
/// in un altro giro, in coda. Niente schema nuovo: il punto fisico si definisce una volta e si
/// riusa copiandolo dove serve.
#[derive(Deserialize)] pub struct ImportCheckpointReq { pub source_checkpoint_id: Uuid }
pub async fn import_checkpoint(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>, Json(b): Json<ImportCheckpointReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.patrol_routes WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND,"percorso di ronda inesistente")); }
    let src = sqlx::query("SELECT name,kind,code,lat,lon,radius_m FROM app.patrol_checkpoints WHERE id=$1")
        .bind(b.source_checkpoint_id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"checkpoint sorgente inesistente"))?;
    let pos: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(position),0)::int FROM app.patrol_checkpoints WHERE route_id=$1").bind(rid).fetch_one(&mut *tx).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.patrol_checkpoints(tenant_id,route_id,name,position,kind,code,lat,lon,radius_m) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(rid).bind(src.get::<String,_>("name")).bind(pos+1).bind(src.get::<String,_>("kind"))
        .bind(src.try_get::<String,_>("code").ok()).bind(src.try_get::<f64,_>("lat").ok()).bind(src.try_get::<f64,_>("lon").ok())
        .bind(src.get::<i32,_>("radius_m")).fetch_one(&mut *tx).await.map_err(ise)?;
    let id = row.get::<Uuid,_>("id");
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}

// ---- Esecuzione (qualunque utente del tenant col modulo attivo) ----
/// Codice casuale per un checkpoint QR/NFC (CA-2): 16 caratteri da un alfabeto senza simboli
/// ambigui, ~80 bit: non si indovina e non si ricava dal nome del punto.
pub(crate) fn patrol_code() -> String {
    use rand::Rng;
    const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut r = rand::thread_rng();
    let body: String = (0..16).map(|_| A[r.gen_range(0..A.len())] as char).collect();
    format!("YW-{body}")
}
/// `show_code`: il codice atteso del checkpoint solo a chi configura (CA-2).
pub(crate) async fn round_view(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, rid: Uuid, show_code: bool) -> Result<Value, (StatusCode,String)> {
    let r = sqlx::query("SELECT ro.id,ro.route_id,ro.user_id,ro.started_at,ro.ended_at,ro.status,rt.name as route_name,rt.window_minutes,rt.manual_code_allowed,u.email FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id LEFT JOIN app.users u ON u.id=ro.user_id WHERE ro.id=$1")
        .bind(rid).fetch_optional(&mut **tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"ronda inesistente"))?;
    let route_id: Uuid = r.get("route_id");
    let cps = sqlx::query("SELECT id,name,position,kind,code,lat,lon,radius_m,expected_min,max_min,dwell_max_min FROM app.patrol_checkpoints WHERE route_id=$1 ORDER BY position,name").bind(route_id).fetch_all(&mut **tx).await.map_err(ise)?;
    let punches = sqlx::query("SELECT checkpoint_id,method,entry,punched_at,ok,note,attempts,last_fail_at,last_fail_reason,distance_m FROM app.patrol_punches WHERE round_id=$1").bind(rid).fetch_all(&mut **tx).await.map_err(ise)?;
    let cpv: Vec<Value> = cps.iter().map(|c| {
        let cid: Uuid = c.get("id");
        let p = punches.iter().find(|p| p.get::<Uuid,_>("checkpoint_id")==cid);
        json!({"id":cid,"name":c.get::<String,_>("name"),"position":c.get::<i32,_>("position"),"kind":c.get::<String,_>("kind"),
            "code":if show_code { c.try_get::<String,_>("code").ok() } else { None },"lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),"radius_m":c.get::<i32,_>("radius_m"),
            "expected_min":c.try_get::<i32,_>("expected_min").ok(),"max_min":c.try_get::<i32,_>("max_min").ok(),"dwell_max_min":c.try_get::<i32,_>("dwell_max_min").ok(),
            "punched": p.map(|p| json!({"method":p.get::<String,_>("method"),"entry":p.try_get::<String,_>("entry").ok(),
                "at":p.get::<chrono::DateTime<chrono::Utc>,_>("punched_at").to_rfc3339(),"ok":p.get::<bool,_>("ok"),"note":p.try_get::<String,_>("note").ok(),
                "attempts":p.try_get::<i32,_>("attempts").unwrap_or(1),
                "last_fail_at":p.try_get::<chrono::DateTime<chrono::Utc>,_>("last_fail_at").ok().map(|d| d.to_rfc3339()),
                "last_fail_reason":p.try_get::<String,_>("last_fail_reason").ok(),"distance_m":p.try_get::<i32,_>("distance_m").ok()}))})
    }).collect();
    let anoms = sqlx::query("SELECT kind,detail,checkpoint_id,created_at FROM app.patrol_anomalies WHERE round_id=$1 ORDER BY created_at").bind(rid).fetch_all(&mut **tx).await.map_err(ise)?;
    let anom_v: Vec<Value> = anoms.iter().map(|a| json!({"kind":a.get::<String,_>("kind"),"detail":a.try_get::<String,_>("detail").ok(),
        "checkpoint_id":a.try_get::<Uuid,_>("checkpoint_id").ok()})).collect();
    let total = cps.len();
    let done = cpv.iter().filter(|c| c["punched"].get("ok").and_then(|v| v.as_bool())==Some(true)).count();
    Ok(json!({"id":rid,"route_id":route_id,"route_name":r.get::<String,_>("route_name"),"user_id":r.get::<Uuid,_>("user_id"),
        "user_email":r.try_get::<String,_>("email").ok(),
        "manual_code_allowed":r.try_get::<bool,_>("manual_code_allowed").unwrap_or(true),
        "started_at":r.get::<chrono::DateTime<chrono::Utc>,_>("started_at").to_rfc3339(),
        "ended_at":r.try_get::<chrono::DateTime<chrono::Utc>,_>("ended_at").ok().map(|d| d.to_rfc3339()),
        "status":r.get::<String,_>("status"),"window_minutes":r.try_get::<i32,_>("window_minutes").ok(),
        "checkpoints":cpv,"total":total,"done":done,"anomalies":anom_v}))
}
pub async fn start_round(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rr = sqlx::query("SELECT active, assigned_group FROM app.patrol_routes WHERE id=$1").bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"percorso di ronda inesistente"))?;
    if !rr.get::<bool,_>("active") { return Err(err(StatusCode::BAD_REQUEST,"percorso di ronda non attivo")); }
    // ACL per gruppo: se il giro è assegnato a un gruppo, solo i membri (o il pianificatore) avviano ronde
    if let Some(g) = rr.try_get::<Option<Uuid>,_>("assigned_group").ok().flatten() {
        if !is_planner(&s) && !user_in_group(&st, s.user_id, g).await {
            return Err(err(StatusCode::FORBIDDEN, "percorso di ronda riservato a un altro gruppo"));
        }
    }
    // CA-17: una sola ronda in corso per giro (anche con indice unico parziale, mig 0141)
    // started_at = now() SQL come le punzonature (ritardi e finestra si misurano sullo stesso orologio)
    let row = sqlx::query("INSERT INTO app.patrol_rounds(tenant_id,route_id,user_id)
            SELECT $1,$2,$3 WHERE NOT EXISTS (SELECT 1 FROM app.patrol_rounds WHERE route_id=$2 AND status='in_corso') RETURNING id")
        .bind(tid).bind(rid).bind(s.user_id).fetch_optional(&mut *tx).await
        .map_err(|e| if e.to_string().contains("patrol_rounds_one_open") { err(StatusCode::CONFLICT, "ronda già in corso su questo percorso") } else { ise(e) })?;
    let Some(row) = row else {
        let who = sqlx::query("SELECT u.email, ro.started_at FROM app.patrol_rounds ro JOIN app.users u ON u.id=ro.user_id WHERE ro.route_id=$1 AND ro.status='in_corso' LIMIT 1")
            .bind(rid).fetch_optional(&mut *tx).await.map_err(ise)?;
        let tz = route_tz(&st, tid, rid).await;
        let msg = match who { Some(w) => format!("ronda già in corso su questo percorso: avviata da {} alle {}", w.get::<String,_>("email"),
                w.get::<chrono::DateTime<chrono::Utc>,_>("started_at").with_timezone(&tz).format("%H:%M")), None => "ronda già in corso su questo percorso".into() };
        return Err(err(StatusCode::CONFLICT, &msg));
    };
    let round_id = row.get::<Uuid,_>("id");
    let v = round_view(&mut tx, round_id, false).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(v))
}
pub async fn list_rounds(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // pianificatore vede tutte; altri vedono le proprie
    let rows = if is_planner(&s) {
        sqlx::query("SELECT ro.id,ro.route_id,ro.user_id,ro.started_at,ro.ended_at,ro.status,rt.name as route_name,u.email FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id JOIN app.users u ON u.id=ro.user_id ORDER BY ro.started_at DESC LIMIT 200").fetch_all(&mut *tx).await.map_err(ise)?
    } else {
        sqlx::query("SELECT ro.id,ro.route_id,ro.user_id,ro.started_at,ro.ended_at,ro.status,rt.name as route_name,u.email FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id JOIN app.users u ON u.id=ro.user_id WHERE ro.user_id=$1 ORDER BY ro.started_at DESC LIMIT 200").bind(s.user_id).fetch_all(&mut *tx).await.map_err(ise)?
    };
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"route_id":r.get::<Uuid,_>("route_id"),
        "route_name":r.get::<String,_>("route_name"),"email":r.get::<String,_>("email"),
        "started_at":r.get::<chrono::DateTime<chrono::Utc>,_>("started_at").to_rfc3339(),
        "ended_at":r.try_get::<chrono::DateTime<chrono::Utc>,_>("ended_at").ok().map(|d| d.to_rfc3339()),
        "status":r.get::<String,_>("status")})).collect();
    Ok(Json(json!(out)))
}
pub async fn get_round(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let v = round_view(&mut tx, id, is_planner(&s)).await?;
    if !is_planner(&s) && v["user_id"].as_str() != Some(&s.user_id.to_string()) { return Err(err(StatusCode::FORBIDDEN, "non è la tua ronda")); }
    tx.commit().await.map_err(ise)?;
    Ok(Json(v))
}
/// `entry`: come l'app ha ottenuto il codice — "scansione" (fotocamera), "nfc" (tag avvicinato),
/// "manuale" (digitato). Se manca vale "manuale": il server non può dare per verificato ciò che
/// non sa (CA-2).
#[derive(Deserialize)] pub struct PunchReq { pub checkpoint_id: Uuid, #[serde(default)] pub code: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>, #[serde(default)] pub note: Option<String>, #[serde(default)] pub entry: Option<String> }
pub async fn punch(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<PunchReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let round = sqlx::query("SELECT ro.user_id,ro.route_id,ro.status,rt.manual_code_allowed FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id WHERE ro.id=$1").bind(id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND,"ronda inesistente"))?;
    // CA-2: punzona SOLO chi esegue la ronda. Il pianificatore la vede e la chiude, ma non può
    // attestare il passaggio di un'altra persona.
    if round.get::<Uuid,_>("user_id") != s.user_id { return Err(err(StatusCode::FORBIDDEN, "solo chi esegue la ronda può punzonare")); }
    if round.get::<String,_>("status") != "in_corso" { return Err(err(StatusCode::BAD_REQUEST, "ronda già chiusa")); }
    let route_id: Uuid = round.get("route_id");
    let manual_allowed: bool = round.try_get::<bool,_>("manual_code_allowed").unwrap_or(true);
    let cp = sqlx::query("SELECT kind,code,lat,lon,radius_m FROM app.patrol_checkpoints WHERE id=$1 AND route_id=$2")
        .bind(b.checkpoint_id).bind(route_id).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::BAD_REQUEST,"il checkpoint non appartiene a questo percorso"))?;
    let kind: String = cp.get("kind");
    // CA-2: come è stato ottenuto il codice. Assente o sconosciuto = manuale.
    let entry: &str = match (kind.as_str(), b.entry.as_deref()) {
        ("gps", _) => "gps",
        ("qr", Some("scansione")) => "scansione",
        ("nfc", Some("nfc")) => "nfc",
        ("beacon", Some("scansione"|"beacon")) => "beacon",
        _ => "manuale",
    };
    // distanza dal punto, se il checkpoint ha coordinate e l'app ha mandato la posizione: prova in più
    let distance_m: Option<i32> = match (cp.try_get::<f64,_>("lat").ok(), cp.try_get::<f64,_>("lon").ok(), b.lat, b.lon) {
        (Some(clat),Some(clon),Some(ulat),Some(ulon)) => Some(haversine_m(clat,clon,ulat,ulon).round() as i32),
        _ => None };
    // verifica per tipo
    let (ok, method, reason) = match kind.as_str() {
        "qr"|"nfc"|"beacon" => {
            let exp = cp.try_get::<String,_>("code").ok().unwrap_or_default();
            let got = b.code.clone().unwrap_or_default();
            let got = got.trim();
            let method = if entry == "manuale" { format!("{kind}_manuale") } else { kind.clone() };
            if entry == "manuale" && !manual_allowed {
                (false, method, "inserimento manuale non ammesso su questo percorso: scansiona il codice o avvicina il tag")
            } else {
                (!exp.is_empty() && exp==got, method, if !exp.is_empty() && exp==got {""} else {"codice non corrispondente"})
            }
        }
        "gps" => {
            match (cp.try_get::<f64,_>("lat").ok(), cp.try_get::<f64,_>("lon").ok(), b.lat, b.lon) {
                (Some(clat),Some(clon),Some(ulat),Some(ulon)) => {
                    let d = haversine_m(clat,clon,ulat,ulon);
                    let rad = cp.get::<i32,_>("radius_m") as f64;
                    (d<=rad, "gps".to_string(), if d<=rad {""} else {"fuori raggio"})
                }
                _ => (false, "gps".to_string(), "posizione mancante")
            }
        }
        _ => (false, "manual".to_string(), "tipo ignoto")
    };
    // CA-3: un tentativo fallito NON declassa una punzonatura già riuscita; ogni tentativo conta
    // (attempts) e l'ultimo fallimento resta registrato con il motivo.
    sqlx::query("INSERT INTO app.patrol_punches(tenant_id,round_id,checkpoint_id,user_id,method,lat,lon,ok,note,entry,distance_m,last_fail_at,last_fail_reason)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,CASE WHEN $8 THEN NULL ELSE now() END,CASE WHEN $8 THEN NULL ELSE $12 END)
                 ON CONFLICT (round_id,checkpoint_id) DO UPDATE SET
                   attempts   = patrol_punches.attempts + 1,
                   method     = CASE WHEN patrol_punches.ok THEN patrol_punches.method ELSE EXCLUDED.method END,
                   entry      = CASE WHEN patrol_punches.ok THEN patrol_punches.entry ELSE EXCLUDED.entry END,
                   lat        = CASE WHEN patrol_punches.ok THEN patrol_punches.lat ELSE EXCLUDED.lat END,
                   lon        = CASE WHEN patrol_punches.ok THEN patrol_punches.lon ELSE EXCLUDED.lon END,
                   distance_m = CASE WHEN patrol_punches.ok THEN patrol_punches.distance_m ELSE EXCLUDED.distance_m END,
                   note       = CASE WHEN patrol_punches.ok THEN patrol_punches.note ELSE EXCLUDED.note END,
                   punched_at = CASE WHEN patrol_punches.ok THEN patrol_punches.punched_at ELSE now() END,
                   ok         = patrol_punches.ok OR EXCLUDED.ok,
                   last_fail_at     = CASE WHEN EXCLUDED.ok THEN patrol_punches.last_fail_at ELSE now() END,
                   last_fail_reason = CASE WHEN EXCLUDED.ok THEN patrol_punches.last_fail_reason ELSE EXCLUDED.last_fail_reason END")
        .bind(tid).bind(id).bind(b.checkpoint_id).bind(s.user_id).bind(&method).bind(b.lat).bind(b.lon).bind(ok).bind(b.note.as_deref())
        .bind(entry).bind(distance_m).bind(reason)
        .execute(&mut *tx).await.map_err(ise)?;
    let v = round_view(&mut tx, id, false).await?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":ok,"reason":reason,"round":v})))
}
pub async fn close_round(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let round = sqlx::query("SELECT ro.user_id,ro.route_id,ro.started_at,ro.status,rt.window_minutes,rt.grace_min FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id WHERE ro.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND,"ronda inesistente"))?;
    if round.get::<Uuid,_>("user_id") != s.user_id && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "non è la tua ronda")); }
    if round.get::<String,_>("status") != "in_corso" { return Err(err(StatusCode::BAD_REQUEST, "ronda già chiusa")); }
    let route_id: Uuid = round.get("route_id");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM app.patrol_checkpoints WHERE route_id=$1").bind(route_id).fetch_one(&mut *tx).await.map_err(ise)?;
    let done: i64 = sqlx::query_scalar("SELECT count(*) FROM app.patrol_punches WHERE round_id=$1 AND ok=true").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    // finestra: se impostata, controlla che la chiusura sia entro i minuti dall'avvio
    let mut in_window = true;
    if let Ok(win) = round.try_get::<i32,_>("window_minutes") {
        let started: chrono::DateTime<chrono::Utc> = round.get("started_at");
        // wall-clock: started_at e' il DEFAULT now() SQL (wall), quindi l'elapsed si misura sul wall
        // per restare coerente con come e' stato scritto. NON usare st.clock.now() qui.
        let elapsed = (chrono::Utc::now() - started).num_minutes();
        in_window = elapsed <= win as i64;
    }
    let status = if total>0 && done==total && in_window { "completato" } else { "incompleto" };
    sqlx::query("UPDATE app.patrol_rounds SET ended_at=$2, status=$3 WHERE id=$1")
        .bind(id).bind(chrono::Utc::now()).bind(status).execute(&mut *tx).await.map_err(ise)?;
    // ---- V1 vigilanza: rileva anomalie (tappa saltata, tratta in ritardo, giro non completato) ----
    let grace: i32 = round.try_get::<i32,_>("grace_min").unwrap_or(0);
    let cps = sqlx::query("SELECT c.id,c.name,c.position,c.max_min,c.radius_m, p.punched_at, p.ok, p.entry, p.distance_m FROM app.patrol_checkpoints c
                 LEFT JOIN app.patrol_punches p ON p.checkpoint_id=c.id AND p.round_id=$1
                 WHERE c.route_id=$2 ORDER BY c.position,c.name").bind(id).bind(route_id).fetch_all(&mut *tx).await.map_err(ise)?;
    // pulizia anomalie precedenti dello stesso round (idempotenza se richiuso)
    sqlx::query("DELETE FROM app.patrol_anomalies WHERE round_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    let mut anomalies: Vec<(Option<Uuid>,&str,String)> = Vec::new();
    let mut prev_time: Option<chrono::DateTime<chrono::Utc>> = None;
    for c in &cps {
        let cid: Uuid = c.get("id");
        let name: String = c.get("name");
        let okp = c.try_get::<bool,_>("ok").ok();
        let at = c.try_get::<chrono::DateTime<chrono::Utc>,_>("punched_at").ok();
        if okp != Some(true) {
            anomalies.push((Some(cid), "saltato", format!("Tappa non punzonata: {name}")));
        } else {
            // CA-2: le punzonature non verificate dal dispositivo restano valide ma vanno guardate
            if c.try_get::<String,_>("entry").ok().as_deref() == Some("manuale") {
                anomalies.push((Some(cid), "inserimento_manuale", format!("«{name}»: codice digitato a mano, non scansionato")));
            }
            if let Ok(d) = c.try_get::<i32,_>("distance_m") {
                let rad: i32 = c.get("radius_m");
                if d > rad { anomalies.push((Some(cid), "posizione_distante", format!("«{name}»: punzonato a {d} m dal punto (tolleranza {rad} m)"))); }
            }
            if let (Some(mx), Some(pt), Some(cur)) = (c.try_get::<i32,_>("max_min").ok(), prev_time, at) {
                let mins = (cur - pt).num_minutes();
                if mins > (mx as i64 + grace as i64) {
                    anomalies.push((Some(cid), "in_ritardo", format!("Tratta verso «{name}»: {mins} min (max {mx}+{grace})")));
                }
            }
            prev_time = at;
        }
    }
    if status=="incompleto" {
        anomalies.push((None, "non_completato", if in_window { format!("Ronda incompleta: {done}/{total} tappe") } else { "Ronda oltre la durata massima".to_string() }));
    }
    for (cid, kind, detail) in &anomalies {
        sqlx::query("INSERT INTO app.patrol_anomalies(tenant_id,round_id,checkpoint_id,kind,detail) VALUES($1,$2,$3,$4,$5)")
            .bind(tid).bind(id).bind(cid).bind(kind).bind(detail).execute(&mut *tx).await.map_err(ise)?;
    }
    let anom_count = anomalies.len();
    let v = round_view(&mut tx, id, is_planner(&s)).await?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "close_round", json!({"round":id,"status":status,"done":done,"total":total,"anomalies":anom_count})).await;
    Ok(Json(json!({"status":status,"in_window":in_window,"anomalies":anom_count,"round":v})))
}
// ===== Vigilanza V2: ronde pianificate/ricorrenti + cruscotto =====
#[derive(Deserialize)] pub struct ScheduleReq { pub route_id: Uuid, #[serde(default)] pub days_mask: Option<String>, #[serde(default)] pub from_time: Option<String>, #[serde(default)] pub to_time: Option<String>, #[serde(default)] pub every_minutes: Option<i32> }
pub(crate) fn parse_time(s:&Option<String>, def:&str)->chrono::NaiveTime {
    let v=s.as_deref().unwrap_or(def);
    chrono::NaiveTime::parse_from_str(v,"%H:%M").or_else(|_| chrono::NaiveTime::parse_from_str(v,"%H:%M:%S")).unwrap_or_else(|_| chrono::NaiveTime::from_hms_opt(22,0,0).unwrap())
}
pub async fn list_patrol_schedules(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT sc.id,sc.route_id,rt.name AS route_name,sc.days_mask,sc.from_time,sc.to_time,sc.every_minutes,sc.active FROM app.patrol_schedules sc JOIN app.patrol_routes rt ON rt.id=sc.route_id ORDER BY rt.name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"route_id":r.get::<Uuid,_>("route_id"),"route_name":r.get::<String,_>("route_name"),
        "days_mask":r.get::<String,_>("days_mask"),
        "from_time":r.get::<chrono::NaiveTime,_>("from_time").format("%H:%M").to_string(),
        "to_time":r.get::<chrono::NaiveTime,_>("to_time").format("%H:%M").to_string(),
        "every_minutes":r.get::<i32,_>("every_minutes"),"active":r.get::<bool,_>("active") })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_patrol_schedule(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ScheduleReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?; if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN,"solo pianificatore/admin")); }
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".to_string());
    if dm.len()!=7 || !dm.chars().all(|c| c=='0'||c=='1') { return Err(err(StatusCode::BAD_REQUEST,"days_mask 7 cifre 0/1")); }
    let ev = b.every_minutes.unwrap_or(60).max(5);
    let (ft,tt)=(parse_time(&b.from_time,"22:00"), parse_time(&b.to_time,"06:00"));
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.patrol_schedules(tenant_id,route_id,days_mask,from_time,to_time,every_minutes) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.route_id).bind(&dm).bind(ft).bind(tt).bind(ev).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_patrol_schedule(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?; if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN,"solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.patrol_schedules WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// CA-16: fuso orario valido (nome IANA, es. Europe/Rome).
pub(crate) fn parse_tz(name: &str) -> Option<chrono_tz::Tz> { name.trim().parse::<chrono_tz::Tz>().ok() }

/// Orari previsti di una pianificazione per il giorno LOCALE `day`, nel fuso `tz` (CA-16).
/// `days_mask` vale per il giorno di inizio; una fascia che finisce prima di iniziare (22:00→06:00)
/// prosegue nel giorno dopo. Orari inesistenti (passaggio all'ora legale) sono saltati.
pub(crate) fn patrol_slots_for_day(dm: &str, ft: chrono::NaiveTime, tt: chrono::NaiveTime, every: i32,
    tz: chrono_tz::Tz, day: chrono::NaiveDate) -> Vec<chrono::DateTime<chrono::Utc>> {
    use chrono::{Datelike, Duration, TimeZone as _};
    let widx = day.weekday().num_days_from_monday() as usize;
    if dm.chars().nth(widx) != Some('1') { return vec![]; }
    let step = Duration::minutes(every.max(5) as i64);
    let start = day.and_time(ft);
    let mut end = day.and_time(tt);
    if end <= start { end = end + Duration::days(1); }
    let mut out = Vec::new();
    let mut t = start;
    while t <= end {
        if let Some(u) = tz.from_local_datetime(&t).earliest() { out.push(u.with_timezone(&chrono::Utc)); }
        t = t + step;
    }
    out
}
/// Una ronda copre lo slot se è stata avviata da metà intervallo prima fino alla scadenza.
pub(crate) fn patrol_slot_covered(rounds: &[(Uuid, chrono::DateTime<chrono::Utc>)], rid: Uuid, slot: chrono::DateTime<chrono::Utc>, step: chrono::Duration) -> bool {
    rounds.iter().any(|(r,t)| *r==rid && *t >= slot - step/2 && *t < slot + step)
}

/// Cruscotto vigilanza: ronde in corso (live), ronde dovute vs svolte oggi (giorno e orari nel fuso
/// della sede del giro, o del tenant), anomalie di giornata. Gli slot mancati riportano l'incidente.
pub async fn patrol_dashboard(State(st): State<AppState>, h: HeaderMap) -> R {
    use chrono::Duration;
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "giri").await?;
    // CA-11: il cruscotto mostra dove sono le guardie: solo a chi supervisiona
    if !is_supervisor(&s) { return Err(err(StatusCode::FORBIDDEN, "cruscotto vigilanza riservato a chi supervisiona")); }
    let now = st.clock.now(); // orologio controllabile (coerente col resto; TEST_CLOCK)
    let ttz: chrono_tz::Tz = sqlx::query_scalar::<_,String>("SELECT timezone FROM app.tenants WHERE id=$1").bind(tid)
        .fetch_optional(&st.owner_pool).await.ok().flatten().and_then(|z| parse_tz(&z)).unwrap_or(chrono_tz::Europe::Rome);
    let local_midnight = {
        use chrono::TimeZone as _;
        let d = now.with_timezone(&ttz).date_naive();
        ttz.from_local_datetime(&d.and_hms_opt(0,0,0).unwrap()).earliest().map(|x| x.with_timezone(&chrono::Utc)).unwrap_or(now - Duration::hours(24))
    };
    let win_start = now - Duration::hours(36); // include la notte precedente
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let live = sqlx::query("SELECT ro.id,ro.route_id,rt.name AS route_name,u.email,ro.started_at,
            (SELECT count(*) FROM app.patrol_checkpoints c WHERE c.route_id=ro.route_id)::int AS total,
            (SELECT count(*) FROM app.patrol_punches p WHERE p.round_id=ro.id AND p.ok)::int AS done,
            (SELECT d.last_lat FROM app.devices d WHERE d.assigned_user=ro.user_id AND d.last_lat IS NOT NULL ORDER BY d.last_pos_at DESC NULLS LAST LIMIT 1) AS dev_lat,
            (SELECT d.last_lon FROM app.devices d WHERE d.assigned_user=ro.user_id AND d.last_lon IS NOT NULL ORDER BY d.last_pos_at DESC NULLS LAST LIMIT 1) AS dev_lon
         FROM app.patrol_rounds ro JOIN app.patrol_routes rt ON rt.id=ro.route_id JOIN app.users u ON u.id=ro.user_id
         WHERE ro.status='in_corso' ORDER BY ro.started_at").fetch_all(&mut *tx).await.map_err(ise)?;
    let rounds = sqlx::query("SELECT route_id,started_at FROM app.patrol_rounds WHERE started_at >= $1").bind(win_start).fetch_all(&mut *tx).await.map_err(ise)?;
    let anom_today: i64 = sqlx::query_scalar("SELECT count(*) FROM app.patrol_anomalies WHERE created_at >= $1").bind(local_midnight).fetch_one(&mut *tx).await.map_err(ise)?;
    let missed = sqlx::query("SELECT schedule_id,slot_at,incident_id FROM app.patrol_missed WHERE slot_at >= $1").bind(win_start).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let scheds = sqlx::query("SELECT sc.id,sc.route_id,rt.name AS route_name,sc.days_mask,sc.from_time,sc.to_time,sc.every_minutes,
            COALESCE(si.timezone,t.timezone) AS tz
          FROM app.patrol_schedules sc JOIN app.patrol_routes rt ON rt.id=sc.route_id JOIN app.tenants t ON t.id=sc.tenant_id
          LEFT JOIN app.sites si ON si.id=rt.site_id WHERE sc.active AND sc.tenant_id=$1")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let round_times: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = rounds.iter().map(|r| (r.get("route_id"), r.get("started_at"))).collect();
    let missed_inc: std::collections::HashMap<(Uuid, i64), Option<Uuid>> = missed.iter()
        .map(|m| ((m.get::<Uuid,_>("schedule_id"), m.get::<chrono::DateTime<chrono::Utc>,_>("slot_at").timestamp()), m.try_get::<Uuid,_>("incident_id").ok())).collect();
    let mut slots: Vec<Value> = Vec::new();
    let (mut done_n, mut miss_n, mut up_n) = (0i64,0i64,0i64);
    for sc in &scheds {
        let tz = sc.try_get::<String,_>("tz").ok().and_then(|z| parse_tz(&z)).unwrap_or(ttz);
        let sid: Uuid = sc.get("id");
        let dm: String = sc.get("days_mask");
        let ft: chrono::NaiveTime = sc.get("from_time"); let tt: chrono::NaiveTime = sc.get("to_time");
        let ev: i32 = sc.get("every_minutes");
        let rid: Uuid = sc.get("route_id"); let rname: String = sc.get("route_name");
        let step = Duration::minutes(ev.max(5) as i64);
        let today = now.with_timezone(&tz).date_naive();
        for slot in patrol_slots_for_day(&dm, ft, tt, ev, tz, today) {
            let covered = patrol_slot_covered(&round_times, rid, slot, step);
            let status = if covered { done_n+=1; "svolta" } else if slot + step < now { miss_n+=1; "mancante" } else { up_n+=1; "attesa" };
            let inc = missed_inc.get(&(sid, slot.timestamp())).cloned().flatten();
            slots.push(json!({"route_id":rid,"route_name":rname,"at":slot.to_rfc3339(),
                "local_time":slot.with_timezone(&tz).format("%H:%M").to_string(),"status":status,"incident_id":inc}));
        }
    }
    let live_v: Vec<Value> = live.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"route_name":r.get::<String,_>("route_name"),"email":r.get::<String,_>("email"),
        "started_at":r.get::<chrono::DateTime<chrono::Utc>,_>("started_at").to_rfc3339(),
        "done":r.get::<i32,_>("done"),"total":r.get::<i32,_>("total"),
        "dev_lat":r.try_get::<f64,_>("dev_lat").ok(),"dev_lon":r.try_get::<f64,_>("dev_lon").ok() })).collect();
    Ok(Json(json!({"live":live_v,"slots":slots,"timezone":ttz.name(),
        "summary":{"svolte":done_n,"mancanti":miss_n,"attese":up_n,"anomalie_oggi":anom_today}})))
}

/// CA-12: ronda pianificata non svolta entro la scadenza -> incidente «Ronda mancata» (escalation
/// normale, non salva-vita). Gira nel worker. Una sola apertura per slot (presa atomica su
/// patrol_missed). Guarda solo le scadenze delle ultime 6 ore e mai slot precedenti alla creazione
/// della pianificazione (niente raffica di allarmi storici quando si crea o riattiva un piano).
pub async fn patrol_missed_watchdog(st: &AppState) {
    use chrono::Duration;
    let now = st.clock.now();
    let rows = sqlx::query("SELECT sc.id, sc.tenant_id, sc.route_id, rt.name AS route_name, si.name AS site_name,
            sc.days_mask, sc.from_time, sc.to_time, sc.every_minutes, sc.created_at, COALESCE(si.timezone,t.timezone) AS tz
          FROM app.patrol_schedules sc
          JOIN app.patrol_routes rt ON rt.id=sc.route_id AND rt.active
          JOIN app.tenants t ON t.id=sc.tenant_id AND t.status<>'sospeso'
          LEFT JOIN app.sites si ON si.id=rt.site_id
         WHERE sc.active AND EXISTS (SELECT 1 FROM app.tenant_modules tm
                 WHERE tm.tenant_id=sc.tenant_id AND tm.module_code='giri' AND tm.state='attivo')")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for r in rows {
        let sid: Uuid = r.get("id"); let tid: Uuid = r.get("tenant_id"); let rid: Uuid = r.get("route_id");
        let tz = r.try_get::<String,_>("tz").ok().and_then(|z| parse_tz(&z)).unwrap_or(chrono_tz::Europe::Rome);
        let dm: String = r.get("days_mask");
        let (ft, tt): (chrono::NaiveTime, chrono::NaiveTime) = (r.get("from_time"), r.get("to_time"));
        let ev: i32 = r.get("every_minutes");
        let created: chrono::DateTime<chrono::Utc> = r.get("created_at");
        let step = Duration::minutes(ev.max(5) as i64);
        let today = now.with_timezone(&tz).date_naive();
        let mut slots = patrol_slots_for_day(&dm, ft, tt, ev, tz, today - Duration::days(1));
        slots.extend(patrol_slots_for_day(&dm, ft, tt, ev, tz, today));
        for slot in slots {
            let deadline = slot + step;
            if deadline > now || deadline < now - Duration::hours(6) || slot < created { continue; }
            let covered: i64 = sqlx::query_scalar("SELECT count(*) FROM app.patrol_rounds WHERE tenant_id=$1 AND route_id=$2 AND started_at >= $3 AND started_at < $4")
                .bind(tid).bind(rid).bind(slot - step/2).bind(deadline).fetch_one(&st.owner_pool).await.unwrap_or(1);
            if covered > 0 { continue; }
            let claimed: Option<Uuid> = sqlx::query_scalar("INSERT INTO app.patrol_missed(tenant_id,schedule_id,route_id,slot_at,deadline_at) VALUES($1,$2,$3,$4,$5)
                    ON CONFLICT (schedule_id,slot_at) DO NOTHING RETURNING id")
                .bind(tid).bind(sid).bind(rid).bind(slot).bind(deadline).fetch_optional(&st.owner_pool).await.ok().flatten();
            let Some(mid) = claimed else { continue; };
            let rname: String = r.get("route_name");
            let key = format!("ronda-mancata-{sid}-{}", slot.timestamp());
            let res = persist_ingest(st, tid, Normalized{ device_id: None,
                source: "giri".into(), source_event: key.clone(), idem: key.clone(), etype: "ronda_mancata".into(),
                device_external_id: None, site_name: r.try_get::<String,_>("site_name").ok(),
                lat: None, lon: None, accuracy_m: None, correlation: key,
                payload: json!({"giro": rname, "previsto": slot.with_timezone(&tz).format("%d/%m/%Y %H:%M").to_string(),
                    "scadenza": deadline.with_timezone(&tz).format("%H:%M").to_string(), "fuso": tz.name()}),
                indoor: false, beacon_uid: None, zone: None }).await;
            if let Ok(Json(v)) = res {
                if let Some(iid) = v.get("incident_id").and_then(|x| x.as_str()).and_then(|x| Uuid::parse_str(x).ok()) {
                    let _ = sqlx::query("UPDATE app.patrol_missed SET incident_id=$2 WHERE id=$1").bind(mid).bind(iid).execute(&st.owner_pool).await;
                }
            }
        }
    }
}

/// CA-16: fuso orario del tenant (orari delle ronde pianificate; la sede può avere il proprio).
pub async fn tenant_timezone_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    let tz: String = sqlx::query_scalar("SELECT timezone FROM app.tenants WHERE id=$1").bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"timezone": tz})))
}
#[derive(Deserialize)] pub struct TimezoneReq { pub timezone: String }
pub async fn tenant_timezone_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<TimezoneReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "settings.manage")?;
    let tid = s.tenant_id.unwrap();
    let tz = parse_tz(&b.timezone).ok_or_else(|| err(StatusCode::BAD_REQUEST, "fuso orario sconosciuto (es. Europe/Rome)"))?;
    sqlx::query("UPDATE app.tenants SET timezone=$2 WHERE id=$1").bind(tid).bind(tz.name()).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &s.email, "tenant_timezone_set", json!({"timezone": tz.name()})).await;
    Ok(Json(json!({"ok":true,"timezone":tz.name()})))
}

// ============================ Fase 2 — Forza lavoro + turni ============================
pub(crate) fn parse_ts(s: &str) -> Result<chrono::DateTime<chrono::Utc>, (StatusCode, String)> {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|_| err(StatusCode::BAD_REQUEST, "data/ora non valida (usa ISO 8601)"))
}
pub(crate) async fn need_workforce(st: &AppState, tid: Uuid) -> Result<(), (StatusCode, String)> {
    if !module_active(st, tid, "workforce").await { return Err(err(StatusCode::FORBIDDEN, "modulo workforce non abilitato")); }
    Ok(())
}

// ---------------- Squadre ----------------
pub async fn list_teams(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    // CA-21: le squadre servono a turni E cantieri (prima solo workforce: in un tenant solo
    // cantieri l'elenco era vietato mentre crearle era permesso a chiunque)
    require_any_module(&st, tid, crate::modules::feature_modules("teams")).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // CA-5: le squadre SONO i gruppi di persone (alias compatibile di /api/user-groups, leggibile da
    // chi pianifica): tutti i gruppi, con il tipo («squadra», «reparto», …)
    let teams = sqlx::query("SELECT id,name,kind FROM app.user_groups ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    let mem = sqlx::query("SELECT m.group_id AS team_id,m.user_id,u.email FROM app.user_group_members m JOIN app.users u ON u.id=m.user_id")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = teams.iter().map(|t| {
        let id: Uuid = t.get("id");
        let ms: Vec<Value> = mem.iter().filter(|m| m.get::<Uuid,_>("team_id")==id)
            .map(|m| json!({"user_id":m.get::<Uuid,_>("user_id"),"email":m.get::<String,_>("email")})).collect();
        json!({"id":id,"name":t.get::<String,_>("name"),"kind":t.get::<String,_>("kind"),"members":ms})
    }).collect();
    Ok(Json(json!(out)))
}
pub async fn create_team(State(st): State<AppState>, h: HeaderMap, Json(b): Json<GroupReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("teams")).await?; // CA-21
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.user_groups(tenant_id,name,kind) VALUES($1,$2,'squadra') RETURNING id")
        .bind(tid).bind(b.name.trim()).fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "esiste già un gruppo con questo nome"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.ok_or_else(|| err(StatusCode::CONFLICT,"gia' presente"))?.get::<Uuid,_>("id")})))
}
pub async fn delete_team(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("teams")).await?; // CA-21
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.user_groups WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "squadra inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn add_team_member(State(st): State<AppState>, h: HeaderMap, Path(tid_p): Path<Uuid>, Json(b): Json<MemberReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("teams")).await?; // CA-21
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.user_group_members(tenant_id,group_id,user_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(tid_p).bind(b.user_id).execute(&mut *tx).await.map_err(|e| conflict_or(e, "gruppo o persona non validi"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn remove_team_member(State(st): State<AppState>, h: HeaderMap, Path((team,uid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("teams")).await?; // CA-21
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_group_members WHERE group_id=$1 AND user_id=$2").bind(team).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Turni pianificati ----------------
#[derive(Deserialize)]
pub struct ShiftPlanReq { pub user_id: Uuid, pub site_id: Option<Uuid>, pub starts_at: String, pub ends_at: String, pub note: Option<String>, #[serde(default)] pub required_qualification: Option<Uuid>, #[serde(default)] pub work_order_id: Option<Uuid>, #[serde(default)] pub commessa_id: Option<Uuid> }

/// Regole di pianificazione del tenant (default di prodotto; preset "standard").
pub(crate) async fn scheduling_rules(st: &AppState, tid: Uuid) -> crate::scheduling::Rules {
    let c = tool_config(st, tid, "workforce", "rules").await;
    let d = crate::scheduling::Rules::default();
    crate::scheduling::Rules {
        min_rest_hours: c.get("min_rest_hours").and_then(|v| v.as_i64()).unwrap_or(d.min_rest_hours),
        max_hours_week: c.get("max_hours_week").and_then(|v| v.as_f64()).unwrap_or(d.max_hours_week),
        max_consec_days: c.get("max_consec_days").and_then(|v| v.as_i64()).unwrap_or(d.max_consec_days),
    }
}
/// Gli altri turni della persona in una finestra ampia attorno al candidato (per i vincoli).
/// `exclude` esclude un turno (usato quando si SPOSTA un turno: non deve confliggere con se stesso).
pub(crate) async fn shift_intervals(st: &AppState, tid: Uuid, uid: Uuid,
    a: chrono::DateTime<chrono::Utc>, z: chrono::DateTime<chrono::Utc>, exclude: Option<Uuid>) -> Vec<crate::scheduling::Interval> {
    let from = a - chrono::Duration::days(9); let to = z + chrono::Duration::days(9);
    sqlx::query("SELECT starts_at,ends_at FROM app.shift_plans WHERE tenant_id=$1 AND user_id=$2 AND ends_at>=$3 AND starts_at<=$4 AND ($5::uuid IS NULL OR id<>$5)")
        .bind(tid).bind(uid).bind(from).bind(to).bind(exclude).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| crate::scheduling::Interval{ start: r.get("starts_at"), end: r.get("ends_at") }).collect()
}
/// Le assenze della persona che toccano la finestra del candidato (vincolo duro).
pub(crate) async fn absence_intervals(st: &AppState, tid: Uuid, uid: Uuid,
    a: chrono::DateTime<chrono::Utc>, z: chrono::DateTime<chrono::Utc>) -> Vec<crate::scheduling::Interval> {
    sqlx::query("SELECT starts_at,ends_at FROM app.worker_absences WHERE tenant_id=$1 AND user_id=$2 AND ends_at>=$3 AND starts_at<=$4")
        .bind(tid).bind(uid).bind(a).bind(z).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| crate::scheduling::Interval{ start: r.get("starts_at"), end: r.get("ends_at") }).collect()
}
/// Se il turno richiede una qualifica, verifica che la persona la possieda valida (non scaduta)
/// all'orario del turno. Nessun requisito o qualifica rimossa -> nessun vincolo.
pub(crate) async fn qual_check(st: &AppState, tid: Uuid, uid: Uuid, required: Option<Uuid>,
    at: chrono::DateTime<chrono::Utc>) -> Option<crate::scheduling::QualReq> {
    let rq = required?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM app.qualifications WHERE id=$1 AND tenant_id=$2")
        .bind(rq).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let name = name?;
    let held: bool = sqlx::query_scalar::<_,Option<i32>>(
        "SELECT 1 FROM app.user_qualifications WHERE tenant_id=$1 AND user_id=$2 AND qualification_id=$3 AND (expires_at IS NULL OR expires_at > $4)")
        .bind(tid).bind(uid).bind(rq).bind(at).fetch_optional(&st.owner_pool).await.ok().flatten().flatten().is_some();
    Some(crate::scheduling::QualReq{ name, satisfied: held })
}
pub(crate) async fn validate_shift_candidate(st: &AppState, tid: Uuid, uid: Uuid,
    a: chrono::DateTime<chrono::Utc>, z: chrono::DateTime<chrono::Utc>, required_qual: Option<Uuid>, exclude: Option<Uuid>) -> Vec<crate::scheduling::Violation> {
    let rules = scheduling_rules(st, tid).await;
    let ex = shift_intervals(st, tid, uid, a, z, exclude).await;
    let ab = absence_intervals(st, tid, uid, a, z).await;
    let q = qual_check(st, tid, uid, required_qual, a).await;
    crate::scheduling::validate(&crate::scheduling::Interval{start:a,end:z}, &ex, &ab, q.as_ref(), &rules)
}

// ---------------- Assenze / indisponibilità (W3a) ----------------
#[derive(Deserialize)]
pub struct AbsenceReq { pub user_id: Uuid, pub starts_at: String, pub ends_at: String, pub kind: Option<String>, pub note: Option<String> }
pub async fn list_absences(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // planner/admin: tutte; altri: solo le proprie.
    let rows = if is_planner(&s) {
        sqlx::query("SELECT a.id,a.user_id,a.starts_at,a.ends_at,a.kind,a.note,u.email FROM app.worker_absences a JOIN app.users u ON u.id=a.user_id ORDER BY a.starts_at DESC")
            .fetch_all(&mut *tx).await
    } else {
        sqlx::query("SELECT a.id,a.user_id,a.starts_at,a.ends_at,a.kind,a.note,u.email FROM app.worker_absences a JOIN app.users u ON u.id=a.user_id WHERE a.user_id=$1 ORDER BY a.starts_at DESC")
            .bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"user_id":r.get::<Uuid,_>("user_id"),"email":r.get::<String,_>("email"),
        "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
        "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at"),
        "kind":r.get::<String,_>("kind"),"note":r.try_get::<String,_>("note").ok() })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_absence(State(st): State<AppState>, h: HeaderMap, Json(b): Json<AbsenceReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let kind = b.kind.clone().unwrap_or_else(|| "assenza".into());
    if !["ferie","malattia","permesso","assenza"].contains(&kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo non valido")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.worker_absences(tenant_id,user_id,starts_at,ends_at,kind,note,created_by) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(b.user_id).bind(a).bind(z).bind(&kind).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "create_absence", json!({"user":b.user_id,"kind":kind})).await;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn update_absence(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<AbsenceReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let kind = b.kind.clone().unwrap_or_else(|| "assenza".into());
    if !["ferie","malattia","permesso","assenza"].contains(&kind.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "tipo non valido")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.worker_absences SET user_id=$2,starts_at=$3,ends_at=$4,kind=$5,note=$6 WHERE id=$1")
        .bind(id).bind(b.user_id).bind(a).bind(z).bind(&kind).bind(&b.note).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "assenza inesistente")); }
    audit(&st, tid, &s.email, "update_absence", json!({"user":b.user_id,"kind":kind})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_absence(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.worker_absences WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "assenza inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---------------- Qualifiche e competenze (W3b, modulo `qualifiche`) ----------------
#[derive(Deserialize)] pub struct QualBody { pub name: String, pub note: Option<String> }
pub async fn list_qualifications(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT id,name,note FROM app.qualifications ORDER BY name").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"note":r.try_get::<String,_>("note").ok()})).collect::<Vec<_>>())))
}
pub async fn create_qualification(State(st): State<AppState>, h: HeaderMap, Json(b): Json<QualBody>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.qualifications(tenant_id,name,note) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&b.note)
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "qualifica già esistente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT,"già esistente"))?.get::<Uuid,_>("id");
    audit(&st, tid, &s.email, "create_qualification", json!({"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_qualification(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.qualifications WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn list_user_qualifications(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT q.id,q.name,uq.expires_at FROM app.user_qualifications uq JOIN app.qualifications q ON q.id=uq.qualification_id WHERE uq.user_id=$1 ORDER BY q.name")
        .bind(uid).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),
        "expires_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("expires_at").ok()})).collect::<Vec<_>>())))
}
#[derive(Deserialize)] pub struct AssignQual { pub qualification_id: Uuid, pub expires_at: Option<String> }
pub async fn user_qualification_add(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<AssignQual>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let exp = match &b.expires_at { Some(e) if !e.trim().is_empty() => Some(parse_ts(e)?), _ => None };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.user_qualifications(tenant_id,user_id,qualification_id,expires_at) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id,user_id,qualification_id) DO UPDATE SET expires_at=EXCLUDED.expires_at")
        .bind(tid).bind(uid).bind(b.qualification_id).bind(exp).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn user_qualification_del(State(st): State<AppState>, h: HeaderMap, Path((uid,qid)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "qualifiche").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.user_qualifications WHERE user_id=$1 AND qualification_id=$2").bind(uid).bind(qid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---------------- Copertura minima (W3c, modulo workforce) ----------------
#[derive(Deserialize)]
pub struct CoverageBody { pub site_id: Option<Uuid>, pub days_mask: Option<String>, pub cov_start: String, pub cov_end: String,
    pub min_people: Option<i32>, pub required_qualification: Option<Uuid>, pub note: Option<String> }
pub async fn list_coverage(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT c.id,c.site_id,c.days_mask,c.cov_start,c.cov_end,c.min_people,c.required_qualification,c.note,
            s.name AS site, q.name AS qual
        FROM app.coverage_requirements c LEFT JOIN app.sites s ON s.id=c.site_id LEFT JOIN app.qualifications q ON q.id=c.required_qualification
        ORDER BY c.created_at").fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!(rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok(),
        "days_mask":r.get::<String,_>("days_mask"),"cov_start":r.get::<String,_>("cov_start"),"cov_end":r.get::<String,_>("cov_end"),
        "min_people":r.get::<i32,_>("min_people"),"required_qualification":r.try_get::<Uuid,_>("required_qualification").ok(),
        "qual":r.try_get::<String,_>("qual").ok(),"note":r.try_get::<String,_>("note").ok()})).collect::<Vec<_>>())))
}
pub async fn create_coverage(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CoverageBody>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (sh, eh) = (parse_hhmm(&b.cov_start), parse_hhmm(&b.cov_end));
    if sh.is_none() || eh.is_none() { return Err(err(StatusCode::BAD_REQUEST, "orari HH:MM richiesti")); }
    let (sh, eh) = (sh.unwrap(), eh.unwrap());
    if (eh.0*60+eh.1) <= (sh.0*60+sh.1) { return Err(err(StatusCode::BAD_REQUEST, "la fine deve essere dopo l'inizio (stessa giornata)")); }
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".into());
    if dm.len()!=7 || !dm.chars().all(|c| c=='0'||c=='1') { return Err(err(StatusCode::BAD_REQUEST, "giorni: 7 cifre 0/1 (lun..dom)")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.coverage_requirements(tenant_id,site_id,days_mask,cov_start,cov_end,min_people,required_qualification,note) VALUES($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
        .bind(tid).bind(b.site_id).bind(&dm).bind(&b.cov_start).bind(&b.cov_end).bind(b.min_people.unwrap_or(1).max(1)).bind(b.required_qualification).bind(&b.note)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":row.get::<Uuid,_>("id")})))
}
pub async fn update_coverage(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<CoverageBody>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (sh, eh) = (parse_hhmm(&b.cov_start), parse_hhmm(&b.cov_end));
    if sh.is_none() || eh.is_none() { return Err(err(StatusCode::BAD_REQUEST, "orari HH:MM richiesti")); }
    let (sh, eh) = (sh.unwrap(), eh.unwrap());
    if (eh.0*60+eh.1) <= (sh.0*60+sh.1) { return Err(err(StatusCode::BAD_REQUEST, "la fine deve essere dopo l'inizio (stessa giornata)")); }
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".into());
    if dm.len()!=7 || !dm.chars().all(|c| c=='0'||c=='1') { return Err(err(StatusCode::BAD_REQUEST, "giorni: 7 cifre 0/1 (lun..dom)")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.coverage_requirements SET site_id=$2,days_mask=$3,cov_start=$4,cov_end=$5,min_people=$6,required_qualification=$7,note=$8 WHERE id=$1")
        .bind(id).bind(b.site_id).bind(&dm).bind(&b.cov_start).bind(&b.cov_end).bind(b.min_people.unwrap_or(1).max(1)).bind(b.required_qualification).bind(&b.note)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND,"inesistente")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_coverage(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.coverage_requirements WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND,"inesistente")); }
    Ok(Json(json!({"ok":true})))
}
/// Controllo copertura: per ogni requisito e giorno applicabile nell'intervallo, conta le persone che
/// coprono l'INTERA fascia (con sede/qualifica giusta) e segnala i giorni sotto il minimo (i "buchi").
pub async fn coverage_check(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    use chrono::{Datelike, TimeZone};
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let now = st.clock.now();
    let pd = |k:&str, def: chrono::DateTime<chrono::Utc>| q.get(k).and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok().map(|t| t.with_timezone(&chrono::Utc))).unwrap_or(def);
    let from = pd("from", now);
    let mut to = pd("to", now + chrono::Duration::days(14));
    if (to - from).num_days() > 62 { to = from + chrono::Duration::days(62); } // cap
    // filtro facoltativo a un singolo requisito (?requirement=<id>)
    let only: Option<Uuid> = q.get("requirement").and_then(|v| Uuid::parse_str(v).ok());
    let reqs = sqlx::query("SELECT c.id,c.site_id,c.days_mask,c.cov_start,c.cov_end,c.min_people,c.required_qualification,s.name AS site
        FROM app.coverage_requirements c LEFT JOIN app.sites s ON s.id=c.site_id WHERE c.tenant_id=$1 AND ($2::uuid IS NULL OR c.id=$2)")
        .bind(tid).bind(only).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut gaps: Vec<Value> = Vec::new();
    for r in &reqs {
        let rid: Uuid = r.get("id");
        let site_id: Option<Uuid> = r.try_get("site_id").ok();
        let dm: String = r.get("days_mask");
        let (sh,sm) = match parse_hhmm(&r.get::<String,_>("cov_start")) { Some(x)=>x, None=>continue };
        let (eh,em) = match parse_hhmm(&r.get::<String,_>("cov_end")) { Some(x)=>x, None=>continue };
        let min_people: i32 = r.get("min_people");
        let qual: Option<Uuid> = r.try_get("required_qualification").ok();
        let site_name: Option<String> = r.try_get("site").ok();
        let mut d = from.date_naive();
        let last = to.date_naive();
        while d <= last {
            let widx = d.weekday().num_days_from_monday() as usize; // 0=lun
            if dm.chars().nth(widx) == Some('1') {
                let ws = chrono::Utc.from_utc_datetime(&d.and_hms_opt(sh,sm,0).unwrap());
                let we = chrono::Utc.from_utc_datetime(&d.and_hms_opt(eh,em,0).unwrap());
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(DISTINCT sp.user_id) FROM app.shift_plans sp
                     WHERE sp.tenant_id=$1 AND sp.starts_at<=$2 AND sp.ends_at>=$3
                       AND ($4::uuid IS NULL OR sp.site_id=$4)
                       AND ($5::uuid IS NULL OR EXISTS(SELECT 1 FROM app.user_qualifications uq
                              WHERE uq.tenant_id=$1 AND uq.user_id=sp.user_id AND uq.qualification_id=$5
                                AND (uq.expires_at IS NULL OR uq.expires_at>$2)))")
                    .bind(tid).bind(ws).bind(we).bind(site_id).bind(qual)
                    .fetch_one(&st.owner_pool).await.unwrap_or(0);
                if (count as i32) < min_people {
                    gaps.push(json!({"requirement_id":rid,"date":d.to_string(),"site":site_name,
                        "window":format!("{:02}:{:02}–{:02}:{:02}",sh,sm,eh,em),
                        "have":count,"need":min_people}));
                }
            }
            d = d.succ_opt().unwrap();
        }
    }
    Ok(Json(json!({"gaps":gaps,"requirements":reqs.len(),"from":from.to_rfc3339(),"to":to.to_rfc3339()})))
}
/// Equità (vincolo MORBIDO, informativo): distribuzione del carico per persona nell'intervallo —
/// ore, numero turni, weekend, notti. Aiuta a bilanciare; non blocca nulla.
pub async fn fairness_report(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let now = st.clock.now();
    let pd = |k:&str, def: chrono::DateTime<chrono::Utc>| q.get(k).and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok().map(|t| t.with_timezone(&chrono::Utc))).unwrap_or(def);
    let from = pd("from", now - chrono::Duration::days(30));
    let to   = pd("to", now + chrono::Duration::days(1));
    let rows = sqlx::query(
        "SELECT u.email,
            count(*) AS n,
            COALESCE(sum(EXTRACT(EPOCH FROM (sp.ends_at-sp.starts_at))/3600.0),0)::float8 AS hours,
            count(*) FILTER (WHERE EXTRACT(DOW FROM sp.starts_at) IN (0,6)) AS weekend,
            count(*) FILTER (WHERE EXTRACT(HOUR FROM sp.starts_at) >= 22 OR EXTRACT(HOUR FROM sp.starts_at) < 6) AS nights
         FROM app.shift_plans sp JOIN app.users u ON u.id=sp.user_id
         WHERE sp.tenant_id=$1 AND sp.starts_at>=$2 AND sp.starts_at<$3
         GROUP BY u.email ORDER BY hours DESC")
        .bind(tid).bind(from).bind(to).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let people: Vec<Value> = rows.iter().map(|r| json!({
        "email":r.get::<String,_>("email"),"shifts":r.get::<i64,_>("n"),
        "hours":(r.get::<f64,_>("hours")*10.0).round()/10.0,
        "weekend":r.get::<i64,_>("weekend"),"nights":r.get::<i64,_>("nights")})).collect();
    // scarto ore max-min (indice di squilibrio)
    let hrs: Vec<f64> = rows.iter().map(|r| r.get::<f64,_>("hours")).collect();
    let spread = match (hrs.iter().cloned().fold(f64::MIN, f64::max), hrs.iter().cloned().fold(f64::MAX, f64::min)) {
        (mx, mn) if !hrs.is_empty() => ((mx-mn)*10.0).round()/10.0, _ => 0.0 };
    Ok(Json(json!({"people":people,"spread_hours":spread,"from":from.to_rfc3339(),"to":to.to_rfc3339()})))
}

// ---------------- W4: AI turni — proposta sostituti (cambio turno) ----------------
// Deterministico e GROUNDED: usa il MOTORE regole per scartare chi ha conflitti/assenze o manca la
// competenza, e ordina per equità (meno ore). Se youworker.ai è attivo, aggiunge una frase di
// consiglio in linguaggio naturale (fallback: testo deterministico). L'AI propone, l'uomo assegna.
pub async fn shift_substitutes(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let sh = sqlx::query("SELECT user_id,site_id,starts_at,ends_at,required_qualification FROM app.shift_plans WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "turno inesistente"))?;
    let cur: Uuid = sh.get("user_id");
    let a: chrono::DateTime<chrono::Utc> = sh.get("starts_at");
    let z: chrono::DateTime<chrono::Utc> = sh.get("ends_at");
    let req_qual: Option<Uuid> = sh.try_get("required_qualification").ok();
    // bacino candidati: ruoli assegnabili del tenant
    let pool = sqlx::query("SELECT id,email FROM app.users WHERE tenant_id=$1 AND role IN ('lavoratore','operatore','reperibile','responsabile','pianificatore') AND id<>$2")
        .bind(tid).bind(cur).fetch_all(&st.owner_pool).await.unwrap_or_default();
    // carico (ore ultimi 30g) per equità
    let from30 = a - chrono::Duration::days(30);
    let load = sqlx::query("SELECT user_id, COALESCE(sum(EXTRACT(EPOCH FROM (ends_at-starts_at))/3600.0),0)::float8 AS hrs FROM app.shift_plans WHERE tenant_id=$1 AND starts_at>=$2 AND starts_at<$3 GROUP BY user_id")
        .bind(tid).bind(from30).bind(a).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let hmap: std::collections::HashMap<Uuid,f64> = load.iter().map(|r| (r.get::<Uuid,_>("user_id"), r.get::<f64,_>("hrs"))).collect();
    let mut cands: Vec<(Uuid,String,f64,Vec<crate::scheduling::Violation>)> = Vec::new();
    for u in &pool {
        let uid: Uuid = u.get("id");
        let vs = validate_shift_candidate(&st, tid, uid, a, z, req_qual, None).await;
        if crate::scheduling::has_hard(&vs) { continue; } // scarta chi ha conflitti duri
        cands.push((uid, u.get::<String,_>("email"), *hmap.get(&uid).unwrap_or(&0.0), vs));
    }
    // ordina: prima chi non ha nemmeno avvisi, poi meno ore (equità)
    cands.sort_by(|x,y| (x.3.len(), (x.2*10.0) as i64).cmp(&(y.3.len(), (y.2*10.0) as i64)));
    let candidates: Vec<Value> = cands.iter().take(10).map(|(uid,em,hrs,vs)| json!({
        "user_id":uid,"email":em,"hours_30d":(hrs*10.0).round()/10.0,
        "warnings": vs.iter().map(|v| v.message.clone()).collect::<Vec<_>>()})).collect();
    // sintesi: deterministica, poi (se AI attiva) riformulata
    let det = match cands.first() {
        Some((_,em,hrs,vs)) => format!("Consiglio: {} ({} ore negli ultimi 30g{}). {} candidati idonei.",
            em, (hrs*10.0).round()/10.0, if vs.is_empty() {", nessun avviso"} else {", con avvisi"}, cands.len()),
        None => "Nessun sostituto idoneo (tutti in conflitto, assenti o senza la competenza richiesta).".into(),
    };
    let mut summary = det.clone();
    if module_active(&st, tid, "assistant").await && ai_feature_on(&st, tid, "assistant").await {
        if let Some(c) = ai_conn(&st, tid).await {
            let sys = "Sei l'assistente di pianificazione turni di YouWorkforce. In UNA frase, consiglia il \
                sostituto migliore usando SOLO i fatti forniti (equità=meno ore, nessun avviso meglio). Italiano, conciso.";
            let ctx = format!("Candidati idonei (già filtrati dal motore regole): {}", serde_json::to_string(&candidates).unwrap_or_default());
            if let Ok(ans) = ai_chat(&st, &c, sys, &ctx).await { if !ans.trim().is_empty() { summary = ans; } }
        }
    }
    Ok(Json(json!({"shift_id":id,"candidates":candidates,"summary":summary,
        "provider": if summary==det {"deterministico"} else {"youworker.ai"}})))
}
pub(crate) fn violations_json(vs: &[crate::scheduling::Violation]) -> Value {
    json!(vs.iter().map(|v| json!({"code":v.code,"hard":v.hard,"message":v.message})).collect::<Vec<_>>())
}

/// Preflight: valida un turno candidato senza crearlo (riusabile dalla UI). Non modifica nulla.
pub async fn shift_plan_validate(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ShiftPlanReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("shift_plans")).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    let vs = validate_shift_candidate(&st, tid, b.user_id, a, z, b.required_qualification, None).await;
    Ok(Json(json!({"ok": !crate::scheduling::has_hard(&vs), "violations": violations_json(&vs)})))
}

pub async fn create_shift_plan(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ShiftPlanReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("shift_plans")).await?; // CA-21
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    // Motore regole: i vincoli DURI (es. sovrapposizione) bloccano; gli avvisi (riposo, ore
    // settimana) non bloccano ma tornano nella risposta. Con `force=1` si ignorano anche gli hard
    // (override consapevole del pianificatore), tranne la finestra non valida.
    let force = h.get("x-force").map(|v| v=="1"||v=="true").unwrap_or(false);
    let vs = validate_shift_candidate(&st, tid, b.user_id, a, z, b.required_qualification, None).await;
    if crate::scheduling::has_hard(&vs) && !force {
        let msg = vs.iter().filter(|v| v.hard).map(|v| v.message.clone()).collect::<Vec<_>>().join(" ");
        return Err(err(StatusCode::CONFLICT, &format!("Conflitto: {msg} (usa Forza per procedere)")));
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query(
        "INSERT INTO app.shift_plans(tenant_id,user_id,site_id,starts_at,ends_at,note,created_by,required_qualification,work_order_id,commessa_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id")
        .bind(tid).bind(b.user_id).bind(b.site_id).bind(a).bind(z).bind(&b.note).bind(s.user_id).bind(b.required_qualification).bind(b.work_order_id).bind(b.commessa_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let iid: Uuid = row.get("id");
    audit(&st, tid, &s.email, "create_shift_plan", json!({"user":b.user_id,"warnings":vs.len()})).await;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"shift_planned","id":iid})}).await;
    Ok(Json(json!({"id":iid, "warnings": violations_json(&vs)})))
}

#[derive(Deserialize)]
pub struct ShiftMove { pub starts_at: String, pub ends_at: String, #[serde(default)] pub user_id: Option<Uuid> }
/// Sposta/ridimensiona un turno (drag&drop dal calendario) o lo riassegna. Validato dal motore
/// (overlap/assenza/competenza = duri, salvo header x-force; riposo/ore = avvisi). Escludo il turno
/// stesso dai conflitti così trascinarlo di poco non confligge con se stesso.
pub async fn shift_plan_move(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ShiftMove>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("shift_plans")).await?; // CA-21
    let tid = s.tenant_id.unwrap();
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let (a,z) = (parse_ts(&b.starts_at)?, parse_ts(&b.ends_at)?);
    if z <= a { return Err(err(StatusCode::BAD_REQUEST, "fine deve essere dopo inizio")); }
    let cur = sqlx::query("SELECT user_id,required_qualification,state FROM app.shift_plans WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "turno inesistente"))?;
    let new_uid = b.user_id.unwrap_or_else(|| cur.get::<Uuid,_>("user_id"));
    let req_qual: Option<Uuid> = cur.try_get("required_qualification").ok();
    let force = h.get("x-force").map(|v| v=="1"||v=="true").unwrap_or(false);
    let vs = validate_shift_candidate(&st, tid, new_uid, a, z, req_qual, Some(id)).await;
    if crate::scheduling::has_hard(&vs) && !force {
        let msg = vs.iter().filter(|v| v.hard).map(|v| v.message.clone()).collect::<Vec<_>>().join(" ");
        return Err(err(StatusCode::CONFLICT, &format!("Conflitto: {msg} (usa Forza per procedere)")));
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.shift_plans SET starts_at=$2, ends_at=$3, user_id=$4 WHERE id=$1")
        .bind(id).bind(a).bind(z).bind(new_uid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "shift_plan_move", json!({"id":id,"user":new_uid})).await;
    publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"shift_planned","id":id})}).await;
    Ok(Json(json!({"ok":true, "warnings": violations_json(&vs)})))
}

#[derive(Deserialize)]
pub struct SuggestReq { pub from: String, pub to: String, #[serde(default)] pub days_mask: Option<String>,
    pub cov_start: String, pub cov_end: String, #[serde(default)] pub min_people: Option<i32>,
    #[serde(default)] pub candidates: Option<Vec<Uuid>> }
/// W4/W5 — SUGGERIMENTO AI di combinazioni turni: per ogni giorno applicabile propone chi assegnare
/// per coprire la fascia, scegliendo candidati VALIDI (motore: no conflitti/assenze/competenza) e
/// bilanciando il carico (equità: aggiorna le ore man mano). NON crea nulla: il pianificatore rivede
/// e applica. Se youworker.ai è attivo, aggiunge una frase di sintesi.
pub async fn shift_suggest(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SuggestReq>) -> R {
    use chrono::{Datelike, TimeZone};
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("shift_plans")).await?; // CA-21
    need_workforce(&st, tid).await?;
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin")); }
    let d0 = chrono::NaiveDate::parse_from_str(&b.from, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "from: data YYYY-MM-DD"))?;
    let mut d1 = chrono::NaiveDate::parse_from_str(&b.to, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, "to: data YYYY-MM-DD"))?;
    if (d1 - d0).num_days() > 62 { d1 = d0 + chrono::Duration::days(62); }
    let (sh, eh) = (parse_hhmm(&b.cov_start).ok_or_else(|| err(StatusCode::BAD_REQUEST, "cov_start HH:MM"))?,
                    parse_hhmm(&b.cov_end).ok_or_else(|| err(StatusCode::BAD_REQUEST, "cov_end HH:MM"))?);
    let dm = b.days_mask.clone().unwrap_or_else(|| "1111111".into());
    let need = b.min_people.unwrap_or(1).max(1);
    // pool candidati
    let pool: Vec<(Uuid,String)> = match &b.candidates {
        Some(ids) if !ids.is_empty() => sqlx::query("SELECT id,email FROM app.users WHERE tenant_id=$1 AND id=ANY($2)")
            .bind(tid).bind(ids).fetch_all(&st.owner_pool).await.unwrap_or_default(),
        _ => sqlx::query("SELECT id,email FROM app.users WHERE tenant_id=$1 AND role IN ('lavoratore','operatore','reperibile','responsabile','pianificatore')")
            .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default(),
    }.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email"))).collect();
    // carico iniziale (ultimi 30g prima di from) per equità, poi aggiornato man mano
    let from_dt = chrono::Utc.from_utc_datetime(&d0.and_hms_opt(0,0,0).unwrap());
    let mut hours: std::collections::HashMap<Uuid,f64> = sqlx::query(
        "SELECT user_id, COALESCE(sum(EXTRACT(EPOCH FROM (ends_at-starts_at))/3600.0),0)::float8 AS hrs FROM app.shift_plans WHERE tenant_id=$1 AND starts_at>=$2 AND starts_at<$3 GROUP BY user_id")
        .bind(tid).bind(from_dt - chrono::Duration::days(30)).bind(from_dt).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| (r.get::<Uuid,_>("user_id"), r.get::<f64,_>("hrs"))).collect();
    let shift_hours = ((eh.0*60+eh.1) as f64 - (sh.0*60+sh.1) as f64)/60.0;
    let mut proposals: Vec<Value> = Vec::new();
    let mut d = d0;
    while d <= d1 {
        let widx = d.weekday().num_days_from_monday() as usize;
        if dm.chars().nth(widx) == Some('1') {
            let ws = chrono::Utc.from_utc_datetime(&d.and_hms_opt(sh.0,sh.1,0).unwrap());
            let we = chrono::Utc.from_utc_datetime(&d.and_hms_opt(eh.0,eh.1,0).unwrap());
            // candidati validi per questo giorno, ordinati per meno ore (equità)
            let mut ok_c: Vec<(Uuid,String,f64)> = Vec::new();
            for (uid,em) in &pool {
                let vs = validate_shift_candidate(&st, tid, *uid, ws, we, None, None).await;
                if crate::scheduling::has_hard(&vs) { continue; }
                ok_c.push((*uid, em.clone(), *hours.get(uid).unwrap_or(&0.0)));
            }
            ok_c.sort_by(|a,b| ((a.2*10.0) as i64).cmp(&((b.2*10.0) as i64)));
            for (uid,em,_) in ok_c.into_iter().take(need as usize) {
                *hours.entry(uid).or_insert(0.0) += shift_hours; // bilancia i giorni successivi
                proposals.push(json!({"date":d.to_string(),"user_id":uid,"email":em,
                    "starts_at":ws.to_rfc3339(),"ends_at":we.to_rfc3339()}));
            }
        }
        d = d.succ_opt().unwrap();
    }
    let det = format!("{} assegnazioni proposte su {}–{} ({} persone/giorno), bilanciate per equità.",
        proposals.len(), b.from, b.to, need);
    let mut summary = det.clone();
    if module_active(&st, tid, "assistant").await && ai_feature_on(&st, tid, "assistant").await {
        if let Some(c) = ai_conn(&st, tid).await {
            let sys = "Assistente pianificazione turni YouWorkforce. In UNA frase riassumi la proposta usando SOLO i fatti. Italiano, conciso.";
            let ctx = format!("Proposte: {}", serde_json::to_string(&proposals).unwrap_or_default());
            if let Ok(ans) = ai_chat(&st, &c, sys, &ctx).await { if !ans.trim().is_empty() { summary = ans; } }
        }
    }
    Ok(Json(json!({"proposals":proposals,"summary":summary,"provider": if summary==det {"deterministico"} else {"youworker.ai"}})))
}

#[derive(Deserialize)] pub struct ColorReq { pub color: Option<String> }
/// Imposta il colore calendario di un utente (il pianificatore per chiunque, ognuno per sé).
pub async fn user_set_color(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<ColorReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    if uid != s.user_id && !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore/admin o te stesso")); }
    let c = match b.color.as_deref().map(|x| x.trim()) {
        None | Some("") => None,
        Some(x) => {
            let ok = x.starts_with('#') && x[1..].chars().all(|c| c.is_ascii_hexdigit()) && (x.len()==4 || x.len()==7);
            if !ok { return Err(err(StatusCode::BAD_REQUEST, "colore non valido (#rgb o #rrggbb)")); }
            Some(x.to_string())
        }
    };
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("UPDATE app.users SET cal_color=$2 WHERE id=$1").bind(uid).bind(&c).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"color":c})))
}

pub async fn list_shift_plans(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "workforce").await?;
    let from = q.get("from").and_then(|x| parse_ts(x).ok());
    let to = q.get("to").and_then(|x| parse_ts(x).ok());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // worker sees own; planner/admin/auditor see all
    let own = matches!(s.role.as_str(), "lavoratore"|"reperibile");
    let rows = sqlx::query(
        "SELECT p.id,p.user_id,p.site_id,p.starts_at,p.ends_at,p.note,p.state,u.email,si.name AS site,
                p.work_order_id, o.title AS order_title, p.commessa_id, cm.code AS commessa_code, cm.name AS commessa_name
         FROM app.shift_plans p JOIN app.users u ON u.id=p.user_id LEFT JOIN app.sites si ON si.id=p.site_id
         LEFT JOIN app.work_orders o ON o.id=p.work_order_id
         LEFT JOIN app.commesse cm ON cm.id=p.commessa_id
         WHERE ($1=false OR p.user_id=$2)
           AND ($3::timestamptz IS NULL OR p.ends_at>=$3)
           AND ($4::timestamptz IS NULL OR p.starts_at<=$4)
         ORDER BY p.starts_at")
        .bind(own).bind(s.user_id).bind(from).bind(to).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"user_id":r.get::<Uuid,_>("user_id"),"email":r.get::<String,_>("email"),
        "site_id":r.try_get::<Uuid,_>("site_id").ok(),"site":r.try_get::<String,_>("site").ok(),
        "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
        "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at"),
        "note":r.try_get::<String,_>("note").ok(),"state":r.get::<String,_>("state"),
        "work_order_id":r.try_get::<Uuid,_>("work_order_id").ok(),"order_title":r.try_get::<String,_>("order_title").ok(),
        "commessa_id":r.try_get::<Uuid,_>("commessa_id").ok(),"commessa_code":r.try_get::<String,_>("commessa_code").ok(),"commessa_name":r.try_get::<String,_>("commessa_name").ok() })).collect();
    Ok(Json(json!(out)))
}
// ===== Modulo Cantieri: commessa (progetto) -> lavorazioni (WBS) -> SAL; riuso turni =====
pub(crate) fn parse_date_opt(s: &Option<String>) -> Option<chrono::NaiveDate> {
    s.as_ref().and_then(|x| chrono::NaiveDate::parse_from_str(x.trim(), "%Y-%m-%d").ok())
}
#[derive(Deserialize)]
pub struct CommessaReq {
    pub code: String, pub name: String,
    #[serde(default)] pub site_id: Option<Uuid>, #[serde(default)] pub client_name: Option<String>,
    #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub cig: Option<String>, #[serde(default)] pub cup: Option<String>,
    #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String>,
    #[serde(default)] pub budget_amount: Option<f64>, #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub address: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>,
}
#[derive(Deserialize)]
pub struct CommessaPatch {
    #[serde(default)] pub name: Option<String>, #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub client_name: Option<String>, #[serde(default)] pub start_date: Option<String>,
    #[serde(default)] pub end_date: Option<String>, #[serde(default)] pub budget_amount: Option<f64>,
    #[serde(default)] pub note: Option<String>, #[serde(default)] pub labor_cost_hour: Option<f64>,
    #[serde(default)] pub client_id: Option<Uuid>, #[serde(default)] pub cig: Option<String>, #[serde(default)] pub cup: Option<String>,
    #[serde(default)] pub address: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>,
}
pub async fn list_commesse(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query(
        "SELECT c.id,c.code,c.name,c.status,COALESCE(cl.name,c.client_name) AS client_name,c.start_date,c.end_date,c.budget_amount::float8 AS budget,
                c.address AS address,c.lat AS lat,c.lon AS lon,
                si.name AS site,si.id AS site_id,si.lat AS site_lat,si.lon AS site_lon,
                COALESCE((SELECT CASE WHEN sum(weight)>0 THEN round(sum(weight*progress_pct)/sum(weight)) ELSE 0 END
                          FROM app.commessa_lavorazioni l WHERE l.commessa_id=c.id),0)::int AS progress,
                (SELECT count(*) FROM app.commessa_lavorazioni l WHERE l.commessa_id=c.id)::int AS lav_count,
                (SELECT count(*) FROM app.shift_plans p WHERE p.commessa_id=c.id AND p.state<>'annullato')::int AS shift_count
         FROM app.commesse c LEFT JOIN app.sites si ON si.id=c.site_id LEFT JOIN app.clients cl ON cl.id=c.client_id ORDER BY c.created_at DESC")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"code":r.get::<String,_>("code"),"name":r.get::<String,_>("name"),
        "status":r.get::<String,_>("status"),"client_name":r.try_get::<String,_>("client_name").ok(),
        "site":r.try_get::<String,_>("site").ok(),"site_id":r.try_get::<Uuid,_>("site_id").ok(),
        "site_lat":r.try_get::<f64,_>("site_lat").ok(),"site_lon":r.try_get::<f64,_>("site_lon").ok(),
        "address":r.try_get::<String,_>("address").ok(),
        "lat":r.try_get::<f64,_>("lat").ok(),"lon":r.try_get::<f64,_>("lon").ok(),
        "start_date":r.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":r.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "budget": if can_see_costs(&s) { r.try_get::<f64,_>("budget").ok() } else { None }, // CA-11
        "progress":r.get::<i32,_>("progress"),"lav_count":r.get::<i32,_>("lav_count"),"shift_count":r.get::<i32,_>("shift_count")
    })).collect();
    Ok(Json(json!(out)))
}
pub async fn create_commessa(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CommessaReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.code.trim().is_empty() || b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "codice e nome richiesti")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let dup: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.commesse WHERE code=$1").bind(b.code.trim()).fetch_optional(&mut *tx).await.map_err(ise)?;
    if dup.is_some() { return Err(err(StatusCode::CONFLICT, "codice commessa già esistente")); }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.commesse(tenant_id,code,name,site_id,client_name,start_date,end_date,budget_amount,status,note,created_by,client_id,cig,cup,address,lat,lon)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8::numeric,COALESCE($9,'pianificato'),$10,$11,$12,$13,$14,$15,$16,$17) RETURNING id")
        .bind(tid).bind(b.code.trim()).bind(b.name.trim()).bind(b.site_id).bind(&b.client_name)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(b.budget_amount)
        .bind(&b.status).bind(&b.note).bind(s.user_id).bind(b.client_id).bind(&b.cig).bind(&b.cup)
        .bind(&b.address).bind(b.lat).bind(b.lon)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_create", json!({"code":b.code,"name":b.name})).await;
    Ok(Json(json!({"id":id})))
}
pub async fn get_commessa(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let c = sqlx::query(
        "SELECT c.id,c.code,c.name,c.status,COALESCE(cl.name,c.client_name) AS client_name,c.client_id,c.cig,c.cup,
                c.start_date,c.end_date,c.budget_amount::float8 AS budget,
                c.labor_cost_hour::float8 AS labor_rate,
                c.note,c.site_id,si.name AS site,c.address AS address,c.lat AS lat,c.lon AS lon,si.lat AS site_lat,si.lon AS site_lon,
                COALESCE((SELECT CASE WHEN sum(weight)>0 THEN round(sum(weight*progress_pct)/sum(weight)) ELSE 0 END
                          FROM app.commessa_lavorazioni l WHERE l.commessa_id=c.id),0)::int AS progress
         FROM app.commesse c LEFT JOIN app.sites si ON si.id=c.site_id LEFT JOIN app.clients cl ON cl.id=c.client_id WHERE c.id=$1")
        .bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
    let c = match c { Some(r)=>r, None=>{ return Err(err(StatusCode::NOT_FOUND, "commessa non trovata")); } };
    let lav = sqlx::query("SELECT id,name,position,weight::float8 AS weight,progress_pct,status,note,start_date,end_date,depends_on,
        (end_date IS NOT NULL AND end_date < current_date AND progress_pct<100) AS late FROM app.commessa_lavorazioni WHERE commessa_id=$1 ORDER BY position,name").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let deprows = sqlx::query("SELECT d.lav_id,d.depends_on_id FROM app.commessa_lav_deps d JOIN app.commessa_lavorazioni l ON l.id=d.lav_id WHERE l.commessa_id=$1").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let sal = sqlx::query("SELECT s.id,s.sal_date,s.pct,s.amount::float8 AS amount,s.note,s.created_at,u.email AS author FROM app.commessa_sal s LEFT JOIN app.users u ON u.id=s.created_by WHERE s.commessa_id=$1 ORDER BY s.sal_date DESC,s.created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let shifts = sqlx::query("SELECT p.id,p.starts_at,p.ends_at,p.state,u.email FROM app.shift_plans p JOIN app.users u ON u.id=p.user_id WHERE p.commessa_id=$1 AND p.state<>'annullato' ORDER BY p.starts_at").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let teams = sqlx::query("SELECT t.id,t.name,(SELECT count(*) FROM app.user_group_members m WHERE m.group_id=t.id)::int AS members FROM app.commessa_groups ct JOIN app.user_groups t ON t.id=ct.group_id WHERE ct.commessa_id=$1 ORDER BY t.name").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let pres = sqlx::query("SELECT p.id,p.clock_in,p.clock_out,p.auto_closed,p.distance_m,p.outside_area,u.email FROM app.commessa_presenze p JOIN app.users u ON u.id=p.user_id WHERE p.commessa_id=$1 ORDER BY p.clock_in DESC LIMIT 50").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let my_open: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.commessa_presenze WHERE commessa_id=$1 AND user_id=$2 AND clock_out IS NULL").bind(id).bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    // C3 — costi: voci (materiali/noli/...) + ore di presenza chiuse valorizzate al costo orario
    let costi = sqlx::query("SELECT k.id,k.description,k.category,k.amount::float8 AS amount,k.cost_date,u.email AS author FROM app.commessa_costi k LEFT JOIN app.users u ON u.id=k.created_by WHERE k.commessa_id=$1 ORDER BY k.cost_date DESC,k.created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let labor_hours: f64 = sqlx::query_scalar("SELECT COALESCE(sum(EXTRACT(EPOCH FROM (clock_out-clock_in))/3600.0),0)::float8 FROM app.commessa_presenze WHERE commessa_id=$1 AND clock_out IS NOT NULL").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    // CA-6: ogni presenza valorizzata con la tariffa della persona (listino per qualifica, valida quel
    // giorno); se il listino non ce l'ha, col costo orario della commessa. In centesimi.
    let labor_cost_cents: i64 = sqlx::query_scalar("SELECT COALESCE(round(sum(EXTRACT(EPOCH FROM (p.clock_out-p.clock_in))/3600.0
            * COALESCE(app.yw_labor_rate_cents(p.tenant_id,p.user_id,(p.clock_in AT TIME ZONE COALESCE(tt.timezone,'Europe/Rome'))::date), round(COALESCE(c.labor_cost_hour,0)*100)))),0)::bigint
          FROM app.commessa_presenze p JOIN app.commesse c ON c.id=p.commessa_id JOIN app.tenants tt ON tt.id=p.tenant_id
         WHERE p.commessa_id=$1 AND p.clock_out IS NOT NULL AND NOT p.auto_closed")
        .bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    let materials_cost: f64 = sqlx::query_scalar("SELECT COALESCE(sum(amount),0)::float8 FROM app.commessa_costi WHERE commessa_id=$1").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
    // CA-20: un solo diario (app.diary_entries); qui le note legate al cantiere
    let diario = sqlx::query("SELECT g.id,g.diary_date,g.weather,g.present_count,g.body,u.email AS author FROM app.diary_entries g LEFT JOIN app.users u ON u.id=g.user_id WHERE g.commessa_id=$1 ORDER BY g.diary_date DESC,g.created_at DESC LIMIT 100").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let fatture = sqlx::query("SELECT f.id,f.number,f.amount::float8 AS amount,f.issue_date,f.status,f.note,f.sal_id FROM app.commessa_fatture f WHERE f.commessa_id=$1 ORDER BY f.issue_date DESC,f.created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    let atts = sqlx::query("SELECT id,entity_kind,kind,filename,content_type,caption,created_by,created_at FROM app.commessa_attachments WHERE commessa_id=$1 ORDER BY created_at DESC").bind(id).fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let att_out: Vec<Value> = atts.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"entity_kind":r.get::<String,_>("entity_kind"),"kind":r.get::<String,_>("kind"),
        "filename":r.try_get::<String,_>("filename").ok(),"content_type":r.get::<String,_>("content_type"),
        "caption":r.try_get::<String,_>("caption").ok(),"created_by":r.try_get::<String,_>("created_by").ok() })).collect();
    let diario_out: Vec<Value> = diario.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"diary_date":r.get::<chrono::NaiveDate,_>("diary_date").to_string(),
        "weather":r.try_get::<String,_>("weather").ok(),"present_count":r.try_get::<i32,_>("present_count").ok(),
        "body":r.get::<String,_>("body"),"author":r.try_get::<String,_>("author").ok() })).collect();
    let fatture_out: Vec<Value> = fatture.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"number":r.get::<String,_>("number"),"amount":r.get::<f64,_>("amount"),
        "issue_date":r.get::<chrono::NaiveDate,_>("issue_date").to_string(),"status":r.get::<String,_>("status"),
        "note":r.try_get::<String,_>("note").ok(),"sal_id":r.try_get::<Uuid,_>("sal_id").ok() })).collect();
    let labor_rate: f64 = c.try_get::<f64,_>("labor_rate").ok().unwrap_or(0.0);
    let labor_cost = labor_cost_cents as f64 / 100.0;
    let total_cost = ((labor_cost+materials_cost)*100.0).round()/100.0;
    let budget_v = c.try_get::<f64,_>("budget").ok();
    let variance = budget_v.map(|b| ((b-total_cost)*100.0).round()/100.0);
    let costi_out: Vec<Value> = costi.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"description":r.get::<String,_>("description"),"category":r.get::<String,_>("category"),
        "amount":r.get::<f64,_>("amount"),"cost_date":r.get::<chrono::NaiveDate,_>("cost_date").to_string(),
        "author":r.try_get::<String,_>("author").ok() })).collect();
    let team_out: Vec<Value> = teams.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),"members":r.get::<i32,_>("members")})).collect();
    let pres_out: Vec<Value> = pres.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"email":r.get::<String,_>("email"),
        "clock_in":r.get::<chrono::DateTime<chrono::Utc>,_>("clock_in"),
        "clock_out":r.try_get::<chrono::DateTime<chrono::Utc>,_>("clock_out").ok(),
        "auto_closed":r.try_get::<bool,_>("auto_closed").unwrap_or(false),
        "distance_m":r.try_get::<Option<f64>,_>("distance_m").ok().flatten().map(|d| d.round()),
        "outside_area":r.try_get::<Option<bool>,_>("outside_area").ok().flatten() })).collect();
    let mut deps_map: std::collections::HashMap<Uuid,Vec<Uuid>> = std::collections::HashMap::new();
    for r in &deprows { deps_map.entry(r.get::<Uuid,_>("lav_id")).or_default().push(r.get::<Uuid,_>("depends_on_id")); }
    let lav_out: Vec<Value> = lav.iter().map(|r| { let lid=r.get::<Uuid,_>("id"); json!({
        "id":lid,"name":r.get::<String,_>("name"),"position":r.get::<i32,_>("position"),
        "weight":r.get::<f64,_>("weight"),"progress_pct":r.get::<i32,_>("progress_pct"),
        "status":r.get::<String,_>("status"),"note":r.try_get::<String,_>("note").ok(),
        "start_date":r.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":r.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "depends_on":r.try_get::<Uuid,_>("depends_on").ok(),"deps":deps_map.get(&lid).cloned().unwrap_or_default(),
        "late":r.try_get::<bool,_>("late").ok().unwrap_or(false) }) }).collect();
    let sal_out: Vec<Value> = sal.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"sal_date":r.get::<chrono::NaiveDate,_>("sal_date").to_string(),
        "pct":r.get::<i32,_>("pct"),"amount":r.try_get::<f64,_>("amount").ok(),
        "note":r.try_get::<String,_>("note").ok(),"author":r.try_get::<String,_>("author").ok() })).collect();
    let sh_out: Vec<Value> = shifts.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"email":r.get::<String,_>("email"),"state":r.get::<String,_>("state"),
        "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
        "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at") })).collect();
    let mut out = json!({
        "id":c.get::<Uuid,_>("id"),"code":c.get::<String,_>("code"),"name":c.get::<String,_>("name"),
        "status":c.get::<String,_>("status"),"client_name":c.try_get::<String,_>("client_name").ok(),
        "site":c.try_get::<String,_>("site").ok(),"site_id":c.try_get::<Uuid,_>("site_id").ok(),
        "address":c.try_get::<String,_>("address").ok(),
        "lat":c.try_get::<f64,_>("lat").ok(),"lon":c.try_get::<f64,_>("lon").ok(),
        "site_lat":c.try_get::<f64,_>("site_lat").ok(),"site_lon":c.try_get::<f64,_>("site_lon").ok(),
        "start_date":c.try_get::<chrono::NaiveDate,_>("start_date").ok().map(|d|d.to_string()),
        "end_date":c.try_get::<chrono::NaiveDate,_>("end_date").ok().map(|d|d.to_string()),
        "budget":c.try_get::<f64,_>("budget").ok(),"note":c.try_get::<String,_>("note").ok(),
        "client_id":c.try_get::<Uuid,_>("client_id").ok(),"cig":c.try_get::<String,_>("cig").ok(),"cup":c.try_get::<String,_>("cup").ok(),
        "progress":c.get::<i32,_>("progress"),
        "lavorazioni":lav_out,"sal":sal_out,"shifts":sh_out,
        "teams":team_out,"presenze":pres_out,"my_open":my_open.is_some(),
        "labor_rate":labor_rate,"costi":costi_out,
        "cost":{"labor_hours":(labor_hours*100.0).round()/100.0,"labor_cost":labor_cost,"materials_cost":materials_cost,
                "total_cost":total_cost,"budget":budget_v,"variance":variance},
        "diario":diario_out,"fatture":fatture_out,"attachments":att_out
    });
    // CA-11: budget, costo orario, costi, SAL in euro e fatture solo a chi pianifica o ha costs.view
    if !can_see_costs(&s) {
        if let Some(o) = out.as_object_mut() {
            for k in ["budget","labor_rate","costi","cost","fatture"] { o.remove(k); }
            o.insert("costs_hidden".into(), json!(true));
            if let Some(Value::Array(sal)) = o.get_mut("sal") { for x in sal.iter_mut() { if let Some(m) = x.as_object_mut() { m.remove("amount"); } } }
        }
    }
    Ok(Json(out))
}
pub async fn update_commessa(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<CommessaPatch>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query(
        "UPDATE app.commesse SET name=COALESCE($2,name),status=COALESCE($3,status),client_name=COALESCE($4,client_name),
         start_date=COALESCE($5,start_date),end_date=COALESCE($6,end_date),budget_amount=COALESCE($7::numeric,budget_amount),
         note=COALESCE($8,note),labor_cost_hour=COALESCE($9::numeric,labor_cost_hour),
         client_id=COALESCE($10,client_id),cig=COALESCE($11,cig),cup=COALESCE($12,cup),
         address=COALESCE($13,address),lat=COALESCE($14,lat),lon=COALESCE($15,lon) WHERE id=$1")
        .bind(id).bind(&b.name).bind(&b.status).bind(&b.client_name)
        .bind(parse_date_opt(&b.start_date)).bind(parse_date_opt(&b.end_date)).bind(b.budget_amount).bind(&b.note).bind(b.labor_cost_hour)
        .bind(b.client_id).bind(&b.cig).bind(&b.cup)
        .bind(&b.address).bind(b.lat).bind(b.lon)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "commessa non trovata")); }
    audit(&st, tid, &s.email, "commessa_update", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_commessa(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commesse WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_delete", json!({"id":id})).await;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct LavorazioneReq { pub name: String, #[serde(default)] pub weight: Option<f64>, #[serde(default)] pub position: Option<i32>, #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String> }
pub async fn create_lavorazione(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<LavorazioneReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let (sd,ed)=(parse_date_opt(&b.start_date),parse_date_opt(&b.end_date));
    if let (Some(a),Some(z))=(sd,ed) { if z<a { return Err(err(StatusCode::BAD_REQUEST, "fine prima dell'inizio")); } }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let pos = match b.position { Some(p)=>p, None=> sqlx::query_scalar::<_,Option<i32>>("SELECT max(position) FROM app.commessa_lavorazioni WHERE commessa_id=$1").bind(cid).fetch_one(&mut *tx).await.map_err(ise)?.unwrap_or(0)+1 };
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.commessa_lavorazioni(tenant_id,commessa_id,name,position,weight,note,start_date,end_date) VALUES($1,$2,$3,$4,COALESCE($5::numeric,1),$6,$7,$8) RETURNING id")
        .bind(tid).bind(cid).bind(b.name.trim()).bind(pos).bind(b.weight).bind(&b.note).bind(sd).bind(ed)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)]
pub struct LavorazionePatch { #[serde(default)] pub name: Option<String>, #[serde(default)] pub weight: Option<f64>, #[serde(default)] pub progress_pct: Option<i32>, #[serde(default)] pub status: Option<String>, #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String>, #[serde(default)] pub depends_on: Option<Uuid>, #[serde(default)] pub clear_dep: Option<bool>,
    #[serde(default)] pub deps: Option<Vec<Uuid>> }
pub async fn update_lavorazione(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<LavorazionePatch>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if let Some(p)=b.progress_pct { if !(0..=100).contains(&p) { return Err(err(StatusCode::BAD_REQUEST, "avanzamento 0..100")); } }
    if b.depends_on==Some(id) { return Err(err(StatusCode::BAD_REQUEST, "una lavorazione non può dipendere da sé stessa")); }
    let (sd,ed)=(parse_date_opt(&b.start_date),parse_date_opt(&b.end_date));
    let clear_dep=b.clear_dep.unwrap_or(false);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // se status='fatto' porta a 100; date e dipendenza con COALESCE (clear_dep azzera la dipendenza)
    let n = sqlx::query(
        "UPDATE app.commessa_lavorazioni SET name=COALESCE($2,name),weight=COALESCE($3::numeric,weight),
         progress_pct=CASE WHEN $5='fatto' THEN 100 ELSE COALESCE($4,progress_pct) END,
         status=COALESCE($5,status),note=COALESCE($6,note),
         start_date=COALESCE($7,start_date),end_date=COALESCE($8,end_date),
         depends_on=CASE WHEN $9 THEN NULL ELSE COALESCE($10,depends_on) END
         WHERE id=$1")
        .bind(id).bind(&b.name).bind(b.weight).bind(b.progress_pct).bind(&b.status).bind(&b.note)
        .bind(sd).bind(ed).bind(clear_dep).bind(b.depends_on)
        .execute(&mut *tx).await.map_err(ise)?.rows_affected();
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "lavorazione non trovata")); }
    // dipendenze MULTIPLE: se passate, sostituiscono il set (no self, no 2-cicli)
    if let Some(deps)=&b.deps {
        let cm: Uuid = sqlx::query_scalar("SELECT commessa_id FROM app.commessa_lavorazioni WHERE id=$1").bind(id).fetch_one(&mut *tx).await.map_err(ise)?;
        sqlx::query("DELETE FROM app.commessa_lav_deps WHERE lav_id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
        for dep in deps {
            if *dep==id { continue; }
            // la dipendenza deve appartenere alla stessa commessa
            let ok: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.commessa_lavorazioni WHERE id=$1 AND commessa_id=$2").bind(dep).bind(cm).fetch_optional(&mut *tx).await.map_err(ise)?;
            if ok.is_none() { continue; }
            // evita ciclo diretto: dep non deve già dipendere da id
            let cyc: Option<(Uuid,Uuid)> = sqlx::query_as("SELECT lav_id,depends_on_id FROM app.commessa_lav_deps WHERE lav_id=$1 AND depends_on_id=$2").bind(dep).bind(id).fetch_optional(&mut *tx).await.map_err(ise)?;
            if cyc.is_some() { return Err(err(StatusCode::BAD_REQUEST, "dipendenza ciclica non ammessa")); }
            sqlx::query("INSERT INTO app.commessa_lav_deps(tenant_id,lav_id,depends_on_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(tid).bind(id).bind(dep).execute(&mut *tx).await.map_err(ise)?;
        }
        // mantieni la colonna singola allineata (prima dipendenza) per retrocompatibilità
        let first = deps.iter().find(|d| **d!=id).copied();
        sqlx::query("UPDATE app.commessa_lavorazioni SET depends_on=$2 WHERE id=$1").bind(id).bind(first).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_lavorazione(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_lavorazioni WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct SalReq { pub pct: i32, #[serde(default)] pub amount: Option<f64>, #[serde(default)] pub note: Option<String>, #[serde(default)] pub sal_date: Option<String> }
pub async fn create_sal(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<SalReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if !(0..=100).contains(&b.pct) { return Err(err(StatusCode::BAD_REQUEST, "pct 0..100")); }
    let d = parse_date_opt(&b.sal_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO app.commessa_sal(tenant_id,commessa_id,sal_date,pct,amount,note,created_by) VALUES($1,$2,$3,$4,$5::numeric,$6,$7) RETURNING id")
        .bind(tid).bind(cid).bind(d).bind(b.pct).bind(b.amount).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_sal", json!({"commessa":cid,"pct":b.pct})).await;
    Ok(Json(json!({"id":id})))
}
// ----- Cantieri C1: squadre assegnate + presenze (timbrature) -----
#[derive(Deserialize)] pub struct AssignTeamReq { #[serde(alias = "group_id")] pub team_id: Uuid }
pub async fn commessa_assign_team(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<AssignTeamReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.commessa_groups(tenant_id,commessa_id,group_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(tid).bind(cid).bind(b.team_id).execute(&mut *tx).await.map_err(|e| conflict_or(e, "gruppo non valido"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
pub async fn commessa_remove_team(State(st): State<AppState>, h: HeaderMap, Path((cid,team)): Path<(Uuid,Uuid)>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_groups WHERE commessa_id=$1 AND group_id=$2").bind(cid).bind(team).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct ClockReq { #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64> }
pub async fn commessa_clock_in(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<ClockReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let com = sqlx::query("SELECT c.id, s.lat, s.lon FROM app.commesse c LEFT JOIN app.sites s ON s.id=c.site_id WHERE c.id=$1")
        .bind(cid).fetch_optional(&mut *tx).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "commessa inesistente"))?;
    // CA-15: una sola presenza aperta per persona, su qualunque commessa
    let open = sqlx::query("SELECT p.commessa_id, c.name FROM app.commessa_presenze p JOIN app.commesse c ON c.id=p.commessa_id
            WHERE p.user_id=$1 AND p.clock_out IS NULL LIMIT 1").bind(s.user_id).fetch_optional(&mut *tx).await.map_err(ise)?;
    if let Some(o) = open {
        if o.get::<Uuid,_>("commessa_id") == cid { return Err(err(StatusCode::CONFLICT, "hai già una presenza aperta su questa commessa")); }
        return Err(err(StatusCode::CONFLICT, &format!("hai già una presenza aperta sulla commessa «{}»: timbra l'uscita lì prima di entrare qui", o.get::<String,_>("name"))));
    }
    // CA-15: se alla commessa sono assegnate squadre, timbra solo chi ne fa parte (o chi pianifica)
    let teams: i64 = sqlx::query_scalar("SELECT count(*) FROM app.commessa_groups WHERE commessa_id=$1").bind(cid).fetch_one(&mut *tx).await.map_err(ise)?;
    if teams > 0 && !is_planner(&s) {
        let member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.commessa_groups ct JOIN app.user_group_members m ON m.group_id=ct.group_id
                WHERE ct.commessa_id=$1 AND m.user_id=$2)").bind(cid).bind(s.user_id).fetch_one(&mut *tx).await.map_err(ise)?;
        if !member { return Err(err(StatusCode::FORBIDDEN, "non fai parte di una squadra assegnata a questa commessa")); }
    }
    // CA-15: distanza dalla sede. Oltre il raggio la presenza vale ma è «fuori area», da verificare;
    // senza posizione del telefono o senza coordinate della sede non è verificabile (NULL)
    let radius = std::env::var("YW_PRESENCE_RADIUS_M").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(500.0);
    let (slat, slon): (Option<f64>, Option<f64>) = (com.try_get("lat").ok().flatten(), com.try_get("lon").ok().flatten());
    let dist = match (b.lat, b.lon, slat, slon) {
        (Some(a), Some(o), Some(c), Some(d)) if (-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&o) => Some(haversine_m(a, o, c, d)),
        _ => None,
    };
    let outside = dist.map(|d| d > radius);
    // orologio unico (st.clock): coerente con la chiusura automatica delle presenze dimenticate (CA-17)
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.commessa_presenze(tenant_id,commessa_id,user_id,lat,lon,source,clock_in,distance_m,outside_area) VALUES($1,$2,$3,$4,$5,'manuale',$6,$7,$8) RETURNING id")
        .bind(tid).bind(cid).bind(s.user_id).bind(b.lat).bind(b.lon).bind(st.clock.now()).bind(dist).bind(outside)
        .fetch_one(&mut *tx).await.map_err(|e| conflict_or(e, "hai già una presenza aperta"))?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id,"distance_m":dist.map(|d| d.round()),"outside_area":outside,"radius_m":radius})))
}
pub async fn commessa_clock_out(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.commessa_presenze SET clock_out=GREATEST($3, clock_in) WHERE commessa_id=$1 AND user_id=$2 AND clock_out IS NULL")
        .bind(cid).bind(s.user_id).bind(st.clock.now()).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::BAD_REQUEST, "nessuna presenza aperta")); }
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri C3: costi di commessa -----
#[derive(Deserialize)]
pub struct CostoReq { pub description: String, #[serde(default)] pub category: Option<String>, pub amount: f64, #[serde(default)] pub cost_date: Option<String> }
pub async fn add_commessa_costo(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<CostoReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.description.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "descrizione richiesta")); }
    let d = parse_date_opt(&b.cost_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.commessa_costi(tenant_id,commessa_id,description,category,amount,cost_date,created_by) VALUES($1,$2,$3,COALESCE($4,'materiale'),$5::numeric,$6,$7) RETURNING id")
        .bind(tid).bind(cid).bind(b.description.trim()).bind(&b.category).bind(b.amount).bind(d).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_commessa_costo(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.commessa_costi WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri C4: giornale di cantiere -----
#[derive(Deserialize)]
pub struct DiarioReq { pub body: String, #[serde(default)] pub diary_date: Option<String>, #[serde(default)] pub weather: Option<String>, #[serde(default)] pub present_count: Option<i32> }
pub async fn add_commessa_diario(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<DiarioReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    if b.body.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "testo richiesto")); }
    let d = parse_date_opt(&b.diary_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    if b.present_count.map(|n| n < 0).unwrap_or(false) { return Err(err(StatusCode::BAD_REQUEST, "numero di presenti non valido")); }
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.diary_entries(tenant_id,user_id,commessa_id,diary_date,weather,present_count,body,source,idem_key) VALUES($1,$2,$3,$4,$5,$6,$7,'cantiere',$8) RETURNING id")
        .bind(tid).bind(s.user_id).bind(cid).bind(d).bind(&b.weather).bind(b.present_count).bind(b.body.trim()).bind(format!("cantiere:{}", Uuid::new_v4()))
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id":id})))
}
pub async fn delete_commessa_diario(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // CA-20: si eliminano da qui solo le note del giornale di cantiere (non quelle di intervento)
    sqlx::query("DELETE FROM app.diary_entries WHERE id=$1 AND commessa_id IS NOT NULL").bind(id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri C5: fatturazione da SAL -----
#[derive(Deserialize)]
pub struct FatturaReq { pub number: String, pub amount: f64, #[serde(default)] pub sal_id: Option<Uuid>, #[serde(default)] pub issue_date: Option<String>, #[serde(default)] pub note: Option<String> }
pub async fn add_commessa_fattura(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>, Json(b): Json<FatturaReq>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if b.number.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "numero richiesto")); }
    if b.amount <= 0.0 { return Err(err(StatusCode::BAD_REQUEST, "importo maggiore di zero")); }
    let d = parse_date_opt(&b.issue_date).unwrap_or_else(|| st.clock.now().date_naive());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // CA-13: numero di fattura unico nel tenant (anche vincolo DB, mig 0140)
    let dup: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.commessa_fatture WHERE tenant_id=$1 AND number=$2)")
        .bind(tid).bind(b.number.trim()).fetch_one(&mut *tx).await.map_err(ise)?;
    if dup { return Err(err(StatusCode::CONFLICT, "numero di fattura già usato")); }
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.commessa_fatture(tenant_id,commessa_id,sal_id,number,amount,issue_date,note,created_by) VALUES($1,$2,$3,$4,$5::numeric,$6,$7,$8) RETURNING id")
        .bind(tid).bind(cid).bind(b.sal_id).bind(b.number.trim()).bind(b.amount).bind(d).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_fattura", json!({"commessa":cid,"number":b.number,"amount":b.amount})).await;
    Ok(Json(json!({"id":id})))
}
#[derive(Deserialize)] pub struct FatturaPatch { pub status: String }
pub async fn update_commessa_fattura(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<FatturaPatch>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    if !["emessa","pagata","annullata"].contains(&b.status.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "stato non valido")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // CA-13: emessa -> pagata | annullata; pagata e annullata sono stati finali
    let cur: Option<(String,String)> = sqlx::query_as("SELECT status,number FROM app.commessa_fatture WHERE id=$1 AND tenant_id=$2 FOR UPDATE")
        .bind(id).bind(tid).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some((cur, number)) = cur else { return Err(err(StatusCode::NOT_FOUND, "fattura non trovata")); };
    if cur == b.status { return Ok(Json(json!({"ok":true,"unchanged":true}))); }
    if cur != "emessa" { return Err(err(StatusCode::CONFLICT, &format!("fattura {cur}: stato finale, non si cambia"))); }
    sqlx::query("UPDATE app.commessa_fatture SET status=$2 WHERE id=$1").bind(id).bind(&b.status).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_fattura_stato", json!({"fattura":id,"numero":number,"da":cur,"a":b.status})).await;
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_commessa_fattura(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?; require_cap(&s, "plan")?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // CA-13: si cancella solo una fattura emessa per errore (mai una pagata: si annulla); con audit
    let row: Option<(String,String,Option<f64>)> = sqlx::query_as("SELECT status,number,amount::float8 FROM app.commessa_fatture WHERE id=$1 AND tenant_id=$2")
        .bind(id).bind(tid).fetch_optional(&mut *tx).await.map_err(ise)?;
    let Some((status, number, amount)) = row else { return Err(err(StatusCode::NOT_FOUND, "fattura non trovata")); };
    if status == "pagata" { return Err(err(StatusCode::CONFLICT, "fattura pagata: non si cancella (va annullata)")); }
    sqlx::query("DELETE FROM app.commessa_fatture WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "commessa_fattura_delete", json!({"fattura":id,"numero":number,"stato":status,"importo":amount})).await;
    Ok(Json(json!({"ok":true})))
}
// ----- Cantieri R3: allegati di commessa (bytea, self-contained) -----
pub async fn commessa_attachment_add(State(st): State<AppState>, h: HeaderMap, Path(cid): Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>, body: axum::body::Bytes) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    if body.len() < 4 { return Err(err(StatusCode::BAD_REQUEST, "file vuoto")); }
    if body.len() > 10_000_000 { return Err(err(StatusCode::BAD_REQUEST, "file troppo grande (max 10MB)")); }
    let kind = match q.get("kind").map(|s| s.as_str()) { Some("doc") => "doc", _ => "photo" };
    let ek = q.get("entity_kind").map(|x| x.trim()).filter(|x| ["diario","fattura","sal","commessa"].contains(x)).unwrap_or("commessa");
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
    let filename = q.get("filename").map(|x| x.trim().to_string());
    let caption = q.get("caption").map(|x| x.trim().to_string());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // AR-1: fatture e SAL solo a chi vede i costi; foto e diario a chi pianifica o a chi lavora
    // sulla commessa (membro di un gruppo assegnato; se nessun gruppo è assegnato, chiunque del modulo)
    if matches!(ek, "fattura" | "sal") && !can_see_costs(&s) {
        return Err(err(StatusCode::FORBIDDEN, "fatture e SAL li allega chi gestisce i costi della commessa"));
    }
    if !is_planner(&s) {
        let ok: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM app.commessa_groups WHERE commessa_id=$1)
               OR EXISTS(SELECT 1 FROM app.commessa_groups g JOIN app.user_group_members m ON m.group_id=g.group_id WHERE g.commessa_id=$1 AND m.user_id=$2)")
            .bind(cid).bind(s.user_id).fetch_one(&mut *tx).await.map_err(ise)?;
        if !ok { return Err(err(StatusCode::FORBIDDEN, "non fai parte di un gruppo assegnato a questa commessa")); }
    }
    let row = sqlx::query("INSERT INTO app.commessa_attachments(tenant_id,commessa_id,entity_kind,kind,filename,content_type,caption,data,created_by) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id")
        .bind(tid).bind(cid).bind(ek).bind(kind).bind(&filename).bind(&ct).bind(&caption).bind(body.as_ref()).bind(&s.email)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"id": row.get::<Uuid,_>("id")})))
}
pub async fn commessa_attachment_get(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_any_module(&st, s.tenant_id.unwrap(), &["cantieri"]).await?; // AR-1: controllo del modulo dichiarato
    let row = sqlx::query("SELECT data,content_type FROM app.commessa_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    let data: Vec<u8> = row.get("data"); let ct: String = row.get("content_type");
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "private, max-age=3600".to_string())], data).into_response())
}
pub async fn commessa_attachment_del(State(st): State<AppState>, h: HeaderMap, Path(aid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "cantieri").await?;
    let owner: Option<String> = sqlx::query_scalar("SELECT created_by FROM app.commessa_attachments WHERE id=$1 AND tenant_id=$2")
        .bind(aid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "allegato inesistente"))?;
    if !is_planner(&s) && owner.as_deref() != Some(s.email.as_str()) { return Err(err(StatusCode::FORBIDDEN, "non autorizzato")); }
    sqlx::query("DELETE FROM app.commessa_attachments WHERE id=$1 AND tenant_id=$2").bind(aid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct ShiftPlanAction { pub action: String } // confirm|cancel
pub async fn shift_plan_action(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>, Json(b): Json<ShiftPlanAction>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("shift_plans")).await?; // CA-21
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = match b.action.as_str() {
        // il lavoratore conferma il PROPRIO turno; il pianificatore può confermare per conto di chiunque
        "confirm" => if is_planner(&s) {
            sqlx::query("UPDATE app.shift_plans SET state='confermato' WHERE id=$1 AND state='pianificato'")
                .bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected()
        } else {
            sqlx::query("UPDATE app.shift_plans SET state='confermato' WHERE id=$1 AND user_id=$2 AND state='pianificato'")
                .bind(id).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected()
        },
        // pianificatore/admin annulla
        "cancel" => { if !is_planner(&s) { let _=tx.rollback().await; return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
            sqlx::query("UPDATE app.shift_plans SET state='annullato' WHERE id=$1 AND state<>'annullato'")
                .bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected() }
        _ => { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, "azione non valida")); }
    };
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::CONFLICT, "turno non aggiornabile (stato o proprieta')")); }
    Ok(Json(json!({"ok":true})))
}
pub async fn delete_shift_plan(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("shift_plans")).await?; // CA-21
    if !is_planner(&s) { return Err(err(StatusCode::FORBIDDEN, "solo pianificatore")); }
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.shift_plans WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "turno inesistente")); }
    Ok(Json(json!({"ok":true})))
}

/// Hook di test (solo TEST_CLOCK): una passata della generazione automatica delle manutenzioni.
pub async fn test_maintenance_autogen(State(st): State<AppState>) -> R {
    if !st.clock.test_mode { return Err(err(StatusCode::FORBIDDEN,"test clock disabled")); }
    maintenance_autogen(&st).await;
    Ok(Json(json!({"ok":true})))
}
