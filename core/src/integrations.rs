//! Fase 2 «Integrazioni» (piano Idealverde, mig 0166). Un gestionale (ERP), un software turni o un
//! agente (Youco via MCP) parlano con YouWorkforce attraverso le API, con una CHIAVE DI INTEGRAZIONE:
//! - la chiave agisce per conto di chi la crea, con un sottoinsieme dei suoi permessi (se la persona
//!   perde un permesso, lo perde anche la chiave); si mostra una volta sola, in DB c'è l'impronta;
//! - CODICE ESTERNO (`external_ref`): clienti, sedi, commesse e interventi si inseriscono o
//!   aggiornano per codice dell'ERP, senza doppioni (idempotente: ripetere la stessa chiamata non
//!   crea nulla di nuovo);
//! - ELENCO DELLE MODIFICHE da una data: interventi e consuntivi cambiati, per riportarli all'ERP;
//! - IMPORTAZIONE CSV con le stesse regole, riga per riga, con il rapporto degli scarti.
//! Gli interventi passano dal controllo dei vincoli (constraints.rs) come quelli creati in console.
//! YW non chiama sistemi esterni: sono loro a chiamare YW.

use axum::{extract::{State, Path, Query}, http::{HeaderMap, StatusCode}, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;
use crate::state::{AppState, Session};
use crate::api::{R, require_tenant, require_cap, require_any_cap, require_module, module_active, ise, err, audit, parse_ts, conflict_or, has_cap};
use crate::db;

/// Permessi che una chiave può avere (mai `admin`): quelli che servono a un gestionale.
pub const KEY_SCOPES: &[(&str, &str)] = &[
    ("plan", "Pianificare: clienti, commesse, interventi"),
    ("orders.view_all", "Leggere tutti gli interventi"),
    ("timesheets.view_all", "Leggere tutti i consuntivi"),
    ("approve", "Approvare i consuntivi"),
    ("costs.view", "Vedere costi e prezzi"),
    ("sites.manage", "Sedi e mezzi"),
    ("users.manage", "Persone e gruppi"),
    ("config.view", "Leggere la configurazione"),
];

fn sha(k: &str) -> String { hex::encode(Sha256::digest(k.as_bytes())) }
fn ct_eq(a: &str, b: &str) -> bool { a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 }

/// Sessione da `Authorization: Bearer ywk_<prefisso>_<segreto>`. None = chiave sconosciuta,
/// revocata, scaduta, persona eliminata o azienda sospesa.
pub(crate) async fn session_from_key(st: &AppState, key: &str) -> Option<Session> {
    let mut parts = key.splitn(3, '_');
    let (Some("ywk"), Some(prefix), Some(_)) = (parts.next(), parts.next(), parts.next()) else { return None };
    // il prefisso è univoco su tutto il sistema: la chiave identifica da sola azienda e persona
    let r = sqlx::query("SELECT k.id,k.tenant_id,k.name,k.key_hash,k.scopes,k.last_used_at,u.id AS uid,u.email,u.role
         FROM app.api_keys k JOIN app.users u ON u.id=k.owner_user_id AND u.tenant_id=k.tenant_id JOIN app.tenants t ON t.id=k.tenant_id
         WHERE k.prefix=$1 AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at > now())
           AND u.anonymized_at IS NULL AND t.status IS DISTINCT FROM 'sospeso'")
        .bind(prefix).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    if !ct_eq(&sha(key), &r.get::<String,_>("key_hash")) { return None; }
    let (tid, uid, role): (Uuid, Uuid, String) = (r.get("tenant_id"), r.get("uid"), r.get("role"));
    let owner_caps = crate::api::effective_caps_user(st, tid, &role, uid).await;
    let scopes: Vec<String> = r.get("scopes");
    let caps: Vec<String> = scopes.into_iter().filter(|c| owner_caps.iter().any(|o| o == c || o == "admin")).collect();
    let kid: Uuid = r.get("id");
    let stale = r.try_get::<Option<DateTime<Utc>>,_>("last_used_at").ok().flatten().map(|t| Utc::now() - t > chrono::Duration::minutes(1)).unwrap_or(true);
    if stale { let _ = sqlx::query("UPDATE app.api_keys SET last_used_at=now() WHERE id=$1 AND tenant_id=$2").bind(kid).bind(tid).execute(&st.owner_pool).await; }
    let name: String = r.get("name");
    Some(Session { user_id: uid, tenant_id: Some(tid), email: format!("{} · chiave «{name}»", r.get::<String,_>("email")),
        role, is_super: false, impersonator: None, caps, via_key: Some(name) })
}

fn require_person(s: &Session) -> Result<(), (StatusCode, String)> {
    if s.via_key.is_some() { return Err(err(StatusCode::FORBIDDEN, "le chiavi di integrazione si gestiscono solo dalla console")); }
    Ok(())
}

// =============================================================================================
// Chiavi
// =============================================================================================
pub async fn list_keys(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "settings.manage")?; require_person(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let rows = sqlx::query("SELECT k.id,k.name,k.prefix,k.scopes,k.created_at,k.last_used_at,k.expires_at,k.revoked_at,u.email
         FROM app.api_keys k LEFT JOIN app.users u ON u.id=k.owner_user_id ORDER BY k.revoked_at NULLS FIRST, k.created_at DESC")
        .fetch_all(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let now = st.clock.now();
    Ok(Json(json!({"scopes": KEY_SCOPES.iter().map(|(c, l)| json!({"code": c, "label": l})).collect::<Vec<_>>(),
        "keys": rows.iter().map(|r| {
            let exp: Option<DateTime<Utc>> = r.try_get("expires_at").ok().flatten();
            let rev: Option<DateTime<Utc>> = r.try_get("revoked_at").ok().flatten();
            json!({"id": r.get::<Uuid,_>("id"), "name": r.get::<String,_>("name"), "prefix": r.get::<String,_>("prefix"),
                "scopes": r.get::<Vec<String>,_>("scopes"), "owner": r.try_get::<String,_>("email").ok(),
                "created_at": r.get::<DateTime<Utc>,_>("created_at"), "last_used_at": r.try_get::<Option<DateTime<Utc>>,_>("last_used_at").ok().flatten(),
                "expires_at": exp, "revoked_at": rev,
                "status": if rev.is_some() { "revocata" } else if exp.map(|e| e <= now).unwrap_or(false) { "scaduta" } else { "attiva" }})
        }).collect::<Vec<_>>()})))
}

#[derive(Deserialize)]
pub struct KeyReq { pub name: String, pub scopes: Vec<String>, #[serde(default)] pub expires_days: Option<i64> }
pub async fn create_key(State(st): State<AppState>, h: HeaderMap, Json(b): Json<KeyReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "settings.manage")?; require_person(&s)?;
    let tid = s.tenant_id.unwrap();
    let name = b.name.trim();
    if name.chars().count() < 2 || name.chars().count() > 80 { return Err(err(StatusCode::BAD_REQUEST, "nome della chiave: da 2 a 80 caratteri")); }
    if b.scopes.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "scegli almeno un permesso")); }
    for sc in &b.scopes {
        if !KEY_SCOPES.iter().any(|(c, _)| c == sc) { return Err(err(StatusCode::BAD_REQUEST, &format!("permesso non ammesso per una chiave: {sc}"))); }
        // non si dà a una chiave ciò che non si ha
        if !has_cap(&s, sc) { return Err(err(StatusCode::FORBIDDEN, &format!("non hai il permesso «{sc}»: non puoi darlo a una chiave"))); }
    }
    let exp = match b.expires_days { None | Some(0) => None, Some(d) if (1..=730).contains(&d) => Some(st.clock.now() + chrono::Duration::days(d)),
        Some(_) => return Err(err(StatusCode::BAD_REQUEST, "scadenza tra 1 e 730 giorni (0 = mai)")) };
    let prefix: String = crate::auth::new_token()[..10].to_string();
    let key = format!("ywk_{prefix}_{}", crate::auth::new_token());
    let mut scopes = b.scopes.clone(); scopes.sort(); scopes.dedup();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let id: Uuid = sqlx::query_scalar("INSERT INTO app.api_keys(tenant_id,owner_user_id,name,prefix,key_hash,scopes,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING id")
        .bind(tid).bind(s.user_id).bind(name).bind(&prefix).bind(sha(&key)).bind(&scopes).bind(exp)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "api_key_create", json!({"id": id, "name": name, "scopes": scopes})).await;
    Ok(Json(json!({"id": id, "key": key, "prefix": prefix, "note": "Copia la chiave adesso: non verrà più mostrata."})))
}
pub async fn revoke_key(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "settings.manage")?; require_person(&s)?;
    let tid = s.tenant_id.unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.api_keys SET revoked_at=COALESCE(revoked_at,now()) WHERE id=$1").bind(id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n == 0 { return Err(err(StatusCode::NOT_FOUND, "chiave inesistente")); }
    audit(&st, tid, &s.email, "api_key_revoke", json!({"id": id})).await;
    Ok(Json(json!({"ok": true})))
}

// =============================================================================================
// Inserimento o aggiornamento per codice esterno
// =============================================================================================
fn valid_ref(r: &str) -> Result<String, (StatusCode, String)> {
    let t = r.trim();
    if t.is_empty() || t.chars().count() > 120 { return Err(err(StatusCode::BAD_REQUEST, "codice esterno: da 1 a 120 caratteri")); }
    Ok(t.to_string())
}
/// id dal codice esterno (tabelle ammesse, mai dal nome passato dall'esterno).
async fn by_ref(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, table: &str, r: &Option<String>, what: &str) -> Result<Option<Uuid>, (StatusCode, String)> {
    let Some(r) = r.as_deref().map(str::trim).filter(|x| !x.is_empty()) else { return Ok(None) };
    let t = match table { "clients" | "sites" | "commesse" | "work_orders" | "activity_types" | "user_groups" | "assets" | "users" => table, _ => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "tabella")) };
    let id: Option<Uuid> = sqlx::query_scalar(&format!("SELECT id FROM app.{t} WHERE external_ref=$1")).bind(r).fetch_optional(&mut **tx).await.map_err(ise)?;
    id.map(Some).ok_or_else(|| err(StatusCode::BAD_REQUEST, &format!("{what} con codice «{r}» inesistente")))
}
/// Tipo di attività o squadra: per codice esterno o, se non c'è, per nome esatto.
async fn by_ref_or_name(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, table: &str, v: &Option<String>, what: &str) -> Result<Option<Uuid>, (StatusCode, String)> {
    let Some(x) = v.as_deref().map(str::trim).filter(|x| !x.is_empty()) else { return Ok(None) };
    let t = match table { "activity_types" | "user_groups" => table, _ => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "tabella")) };
    let id: Option<Uuid> = sqlx::query_scalar(&format!("SELECT id FROM app.{t} WHERE external_ref=$1 OR (external_ref IS NULL AND name=$1) ORDER BY external_ref NULLS LAST LIMIT 1"))
        .bind(x).fetch_optional(&mut **tx).await.map_err(ise)?;
    id.map(Some).ok_or_else(|| err(StatusCode::BAD_REQUEST, &format!("{what} «{x}» inesistente")))
}

#[derive(Deserialize, Default)]
pub struct ClientUp { pub name: String, #[serde(default)] pub vat: Option<String>, #[serde(default)] pub address: Option<String>,
    #[serde(default)] pub phone: Option<String>, #[serde(default)] pub email: Option<String>, #[serde(default)] pub notes: Option<String>,
    #[serde(default)] pub pec: Option<String>, #[serde(default)] pub sdi: Option<String>, #[serde(default)] pub payment_terms: Option<String> }
pub(crate) async fn upsert_client(st: &AppState, s: &Session, r: &str, b: &ClientUp) -> Result<(Uuid, bool), (StatusCode, String)> {
    let tid = s.tenant_id.unwrap();
    require_module(st, tid, "interventi").await?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome del cliente richiesto")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.clients(tenant_id,external_ref,name,vat,address,phone,email,notes,pec,sdi,payment_terms,created_by)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
         ON CONFLICT (tenant_id, external_ref) WHERE external_ref IS NOT NULL DO UPDATE SET name=EXCLUDED.name,
            vat=COALESCE(EXCLUDED.vat,clients.vat), address=COALESCE(EXCLUDED.address,clients.address), phone=COALESCE(EXCLUDED.phone,clients.phone),
            email=COALESCE(EXCLUDED.email,clients.email), notes=COALESCE(EXCLUDED.notes,clients.notes), pec=COALESCE(EXCLUDED.pec,clients.pec),
            sdi=COALESCE(EXCLUDED.sdi,clients.sdi), payment_terms=COALESCE(EXCLUDED.payment_terms,clients.payment_terms), updated_at=now()
         RETURNING id, (xmax = 0) AS created")
        .bind(tid).bind(r).bind(b.name.trim()).bind(&b.vat).bind(&b.address).bind(&b.phone).bind(&b.email).bind(&b.notes)
        .bind(&b.pec).bind(&b.sdi).bind(&b.payment_terms).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(|e| conflict_or(e, "dati del cliente non validi"))?;
    tx.commit().await.map_err(ise)?;
    Ok((row.get("id"), row.get("created")))
}

#[derive(Deserialize, Default)]
pub struct SiteUp { pub name: String, #[serde(default)] pub address: Option<String>, #[serde(default)] pub lat: Option<f64>, #[serde(default)] pub lon: Option<f64>,
    #[serde(default)] pub access_notes: Option<String>, #[serde(default)] pub access_windows: Option<Value> }
pub(crate) async fn upsert_site(st: &AppState, s: &Session, r: &str, b: &SiteUp) -> Result<(Uuid, bool), (StatusCode, String)> {
    let tid = s.tenant_id.unwrap();
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome della sede richiesto")); }
    let win = b.access_windows.as_ref().map(crate::constraints::parse_windows).transpose()?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.sites(tenant_id,external_ref,name,address,lat,lon,access_notes,access_windows) VALUES($1,$2,$3,$4,$5,$6,$7,COALESCE($8,'[]'::jsonb))
         ON CONFLICT (tenant_id, external_ref) WHERE external_ref IS NOT NULL DO UPDATE SET name=EXCLUDED.name,
            address=COALESCE(EXCLUDED.address,sites.address), lat=COALESCE(EXCLUDED.lat,sites.lat), lon=COALESCE(EXCLUDED.lon,sites.lon),
            access_notes=COALESCE(EXCLUDED.access_notes,sites.access_notes), access_windows=COALESCE($8,sites.access_windows)
         RETURNING id, (xmax = 0) AS created")
        .bind(tid).bind(r).bind(b.name.trim()).bind(&b.address).bind(b.lat).bind(b.lon).bind(&b.access_notes).bind(win)
        .fetch_one(&mut *tx).await.map_err(|e| conflict_or(e, "dati della sede non validi"))?;
    tx.commit().await.map_err(ise)?;
    Ok((row.get("id"), row.get("created")))
}

#[derive(Deserialize, Default)]
pub struct CommessaUp { pub name: String, #[serde(default)] pub code: Option<String>, #[serde(default)] pub site_ref: Option<String>,
    #[serde(default)] pub client_ref: Option<String>, #[serde(default)] pub start_date: Option<String>, #[serde(default)] pub end_date: Option<String>,
    #[serde(default)] pub budget_amount: Option<f64>, #[serde(default)] pub status: Option<String>, #[serde(default)] pub note: Option<String> }
pub(crate) async fn upsert_commessa(st: &AppState, s: &Session, r: &str, b: &CommessaUp) -> Result<(Uuid, bool), (StatusCode, String)> {
    let tid = s.tenant_id.unwrap();
    require_module(st, tid, "cantieri").await?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome della commessa richiesto")); }
    if let Some(x) = b.status.as_deref() { if !["pianificato","in_corso","sospeso","chiuso"].contains(&x) {
        return Err(err(StatusCode::BAD_REQUEST, "stato della commessa: pianificato, in_corso, sospeso o chiuso")); } }
    let d = |v: &Option<String>, n: &str| -> Result<Option<chrono::NaiveDate>, (StatusCode, String)> {
        v.as_deref().filter(|x| !x.is_empty()).map(|x| chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").map_err(|_| err(StatusCode::BAD_REQUEST, &format!("{n}: data AAAA-MM-GG")))).transpose() };
    let (sd, ed) = (d(&b.start_date, "inizio")?, d(&b.end_date, "fine")?);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let site = by_ref(&mut tx, "sites", &b.site_ref, "sede").await?;
    let client = by_ref(&mut tx, "clients", &b.client_ref, "cliente").await?;
    let code = b.code.clone().filter(|c| !c.trim().is_empty()).unwrap_or_else(|| r.to_string());
    let row = sqlx::query("INSERT INTO app.commesse(tenant_id,external_ref,code,name,site_id,client_id,start_date,end_date,budget_amount,status,note,created_by)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,COALESCE($10,'pianificato'),$11,$12)
         ON CONFLICT (tenant_id, external_ref) WHERE external_ref IS NOT NULL DO UPDATE SET name=EXCLUDED.name, code=EXCLUDED.code,
            site_id=COALESCE(EXCLUDED.site_id,commesse.site_id), client_id=COALESCE(EXCLUDED.client_id,commesse.client_id),
            start_date=COALESCE(EXCLUDED.start_date,commesse.start_date), end_date=COALESCE(EXCLUDED.end_date,commesse.end_date),
            budget_amount=COALESCE(EXCLUDED.budget_amount,commesse.budget_amount), status=COALESCE($10,commesse.status),
            note=COALESCE(EXCLUDED.note,commesse.note), updated_at=now()
         RETURNING id, (xmax = 0) AS created")
        .bind(tid).bind(r).bind(code.trim()).bind(b.name.trim()).bind(site).bind(client).bind(sd).bind(ed).bind(b.budget_amount)
        .bind(&b.status).bind(&b.note).bind(s.user_id)
        .fetch_one(&mut *tx).await.map_err(|e| conflict_or(e, "commessa non valida (codice già usato da un'altra commessa?)"))?;
    tx.commit().await.map_err(ise)?;
    Ok((row.get("id"), row.get("created")))
}

#[derive(Deserialize, Default)]
pub struct OrderUp { pub title: String, #[serde(default)] pub site_ref: Option<String>, #[serde(default)] pub client_ref: Option<String>,
    /// tipo di attività e squadra: codice esterno o nome
    #[serde(default)] pub activity_type: Option<String>, #[serde(default)] pub team: Option<String>,
    #[serde(default)] pub assignee_email: Option<String>, #[serde(default)] pub quantity: Option<f64>,
    #[serde(default)] pub scheduled_start: Option<String>, #[serde(default)] pub scheduled_end: Option<String>,
    #[serde(default)] pub priority: Option<i32>, #[serde(default)] pub instructions: Option<String> }
/// Intervento per codice esterno. Nuovo: come «Nuovo intervento» in console; esistente: aggiornato
/// nei campi passati, mai se già completato o annullato. Vincoli controllati (duri = 409 salvo forza).
pub(crate) async fn upsert_order(st: &AppState, s: &Session, r: &str, b: &OrderUp, forced: bool) -> Result<(Uuid, bool, Value), (StatusCode, String)> {
    let tid = s.tenant_id.unwrap();
    require_module(st, tid, "interventi").await?;
    if b.title.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "titolo dell'intervento richiesto")); }
    if b.quantity.map(|q| q < 0.0).unwrap_or(false) { return Err(err(StatusCode::BAD_REQUEST, "quantità non negativa")); }
    let ss = b.scheduled_start.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?;
    let mut se = b.scheduled_end.as_deref().filter(|x| !x.is_empty()).map(parse_ts).transpose()?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let site = by_ref(&mut tx, "sites", &b.site_ref, "sede").await?;
    let client = by_ref(&mut tx, "clients", &b.client_ref, "cliente").await?;
    let at = by_ref_or_name(&mut tx, "activity_types", &b.activity_type, "tipo di attività").await?;
    let group = by_ref_or_name(&mut tx, "user_groups", &b.team, "squadra").await?;
    let user: Option<Uuid> = match b.assignee_email.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        None => None,
        Some(e) => Some(sqlx::query_scalar("SELECT id FROM app.users WHERE lower(email)=lower($1) AND anonymized_at IS NULL").bind(e)
            .fetch_optional(&mut *tx).await.map_err(ise)?.ok_or_else(|| err(StatusCode::BAD_REQUEST, &format!("persona «{e}» inesistente")))?),
    };
    let cur = sqlx::query("SELECT id,state,version,site_id,client_id,activity_type_id,quantity::float8 AS qty,scheduled_start,scheduled_end,assigned_user,assigned_group
         FROM app.work_orders WHERE external_ref=$1").bind(r).fetch_optional(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    if let Some(c) = &cur { let stt: String = c.get("state");
        if stt == "completato" || stt == "annullato" { return Err(err(StatusCode::CONFLICT, &format!("intervento «{r}» {stt}: non si modifica"))); } }
    let g = |col: &str| cur.as_ref().and_then(|c| c.try_get::<Option<Uuid>,_>(col).ok().flatten());
    let at2 = at.or(g("activity_type_id"));
    let qty = b.quantity.or(cur.as_ref().and_then(|c| c.try_get::<Option<f64>,_>("qty").ok().flatten()));
    let est = crate::constraints::activity_estimate(st, tid, at2, qty).await?;
    let start = ss.or(cur.as_ref().and_then(|c| c.try_get::<Option<DateTime<Utc>>,_>("scheduled_start").ok().flatten()));
    if se.is_none() { se = cur.as_ref().and_then(|c| c.try_get::<Option<DateTime<Utc>>,_>("scheduled_end").ok().flatten()); }
    if (se.is_none() || ss.is_some() && b.scheduled_end.is_none() && b.quantity.is_some()) && est.is_some() && start.is_some() {
        se = Some(start.unwrap() + chrono::Duration::minutes(est.unwrap() as i64)); }
    if let (Some(a), Some(z)) = (start, se) { if z <= a { return Err(err(StatusCode::BAD_REQUEST, "la fine deve essere dopo l'inizio")); } }
    let plan = crate::constraints::OrderPlan { order_id: cur.as_ref().map(|c| c.get("id")), site_id: site.or(g("site_id")), activity_type_id: at2,
        start, end: se, user: user.or(g("assigned_user")), group: group.or(g("assigned_group")), asset_ids: None };
    let vs = crate::constraints::check_order(st, tid, &plan).await?;
    crate::constraints::enforce(&vs, forced)?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let (id, created) = if let Some(c) = cur {
        let id: Uuid = c.get("id");
        sqlx::query("UPDATE app.work_orders SET title=$2, site_id=COALESCE($3,site_id), client_id=COALESCE($4,client_id),
                activity_type_id=COALESCE($5,activity_type_id), quantity=COALESCE($6::numeric,quantity), estimated_minutes=COALESCE($7,estimated_minutes),
                scheduled_start=COALESCE($8,scheduled_start), scheduled_end=COALESCE($9,scheduled_end),
                assigned_user=COALESCE($10,assigned_user), assigned_group=COALESCE($11,assigned_group), priority=COALESCE($12,priority),
                instructions=COALESCE($13,instructions),
                state=CASE WHEN state='bozza' AND ($10 IS NOT NULL OR $11 IS NOT NULL) THEN 'assegnato' ELSE state END,
                version=version+1, updated_at=now() WHERE id=$1")
            .bind(id).bind(b.title.trim()).bind(site).bind(client).bind(at).bind(b.quantity).bind(est).bind(ss).bind(se)
            .bind(user).bind(group).bind(b.priority.map(|p| p.clamp(1, 3))).bind(&b.instructions)
            .execute(&mut *tx).await.map_err(|e| conflict_or(e, "dati dell'intervento non validi"))?;
        if let (Some(a), Some(z)) = (start, se) { crate::constraints::move_order_bookings(&mut tx, id, a, z).await?; }
        (id, false)
    } else {
        let state = if user.is_some() || group.is_some() { "assegnato" } else { "bozza" };
        let id: Uuid = sqlx::query_scalar("INSERT INTO app.work_orders(tenant_id,external_ref,title,site_id,client_id,activity_type_id,quantity,estimated_minutes,
                scheduled_start,scheduled_end,assigned_user,assigned_group,priority,instructions,state,created_by,checklist)
             VALUES($1,$2,$3,$4,$5,$6,$7::numeric,$8,$9,$10,$11,$12,COALESCE($13,2),$14,$15,$16,'[]'::jsonb) RETURNING id")
            .bind(tid).bind(r).bind(b.title.trim()).bind(site).bind(client).bind(at).bind(b.quantity).bind(est).bind(ss).bind(se)
            .bind(user).bind(group).bind(b.priority.map(|p| p.clamp(1, 3))).bind(&b.instructions).bind(state).bind(s.user_id)
            .fetch_one(&mut *tx).await.map_err(|e| conflict_or(e, "dati dell'intervento non validi"))?;
        (id, true)
    };
    tx.commit().await.map_err(ise)?;
    Ok((id, created, crate::constraints::vjson(&vs)))
}

pub async fn put_client(State(st): State<AppState>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<ClientUp>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "plan")?; require_module(&st, s.tenant_id.unwrap(), "interventi").await?;
    let r = valid_ref(&r)?; let (id, created) = upsert_client(&st, &s, &r, &b).await?;
    audit(&st, s.tenant_id.unwrap(), &s.email, "integration_client", json!({"ref": r, "created": created})).await;
    Ok(Json(json!({"id": id, "ref": r, "created": created})))
}
pub async fn put_site(State(st): State<AppState>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<SiteUp>) -> R {
    let s = require_tenant(&st, &h).await?; require_any_cap(&s, &["sites.manage", "users.manage"])?;
    let r = valid_ref(&r)?; let (id, created) = upsert_site(&st, &s, &r, &b).await?;
    audit(&st, s.tenant_id.unwrap(), &s.email, "integration_site", json!({"ref": r, "created": created})).await;
    Ok(Json(json!({"id": id, "ref": r, "created": created})))
}
pub async fn put_commessa(State(st): State<AppState>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<CommessaUp>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "plan")?;
    let r = valid_ref(&r)?; let (id, created) = upsert_commessa(&st, &s, &r, &b).await?;
    audit(&st, s.tenant_id.unwrap(), &s.email, "integration_commessa", json!({"ref": r, "created": created})).await;
    Ok(Json(json!({"id": id, "ref": r, "created": created})))
}
pub async fn put_order(State(st): State<AppState>, h: HeaderMap, Path(r): Path<String>, Json(b): Json<OrderUp>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "plan")?;
    let r = valid_ref(&r)?; let (id, created, w) = upsert_order(&st, &s, &r, &b, crate::constraints::force(&h)).await?;
    audit(&st, s.tenant_id.unwrap(), &s.email, "integration_order", json!({"ref": r, "created": created})).await;
    Ok(Json(json!({"id": id, "ref": r, "created": created, "warnings": w})))
}

// =============================================================================================
// Elenco delle modifiche (verso l'ERP)
// =============================================================================================
#[derive(Deserialize)]
pub struct ChangesQ { pub since: String, #[serde(default)] pub limit: Option<i64> }
/// GET /api/integration/changes?since=… — interventi e consuntivi cambiati dopo `since`, fino a
/// `until` (da usare come prossimo `since`). Al massimo `limit` per tipo (1..1000, default 500): se
/// `more` è vero si richiama subito con il nuovo `since`.
pub async fn changes(State(st): State<AppState>, h: HeaderMap, Query(q): Query<ChangesQ>) -> R {
    let s = require_tenant(&st, &h).await?; require_any_cap(&s, &["orders.view_all", "plan"])?;
    let tid = s.tenant_id.unwrap();
    let since = parse_ts(&q.since)?;
    let limit = q.limit.unwrap_or(500).clamp(1, 1000);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // l'istante di lettura del DB: nessuna modifica cade tra due chiamate consecutive
    let until: DateTime<Utc> = sqlx::query_scalar("SELECT now()").fetch_one(&mut *tx).await.map_err(ise)?;
    let orders = if module_active(&st, tid, "interventi").await {
        sqlx::query("SELECT o.id,o.external_ref,o.title,o.state,o.outcome,o.outcome_kind,o.scheduled_start,o.scheduled_end,o.arrived_at,o.work_started_at,
                o.work_minutes,o.signed_by,o.updated_at,s.external_ref AS site_ref,c.external_ref AS client_ref,at.name AS activity_type,g.name AS team,u.email AS assignee
             FROM app.work_orders o LEFT JOIN app.sites s ON s.id=o.site_id LEFT JOIN app.clients c ON c.id=o.client_id
             LEFT JOIN app.activity_types at ON at.id=o.activity_type_id LEFT JOIN app.user_groups g ON g.id=o.assigned_group LEFT JOIN app.users u ON u.id=o.assigned_user
             WHERE o.updated_at > $1 AND o.updated_at <= $2 ORDER BY o.updated_at LIMIT $3")
            .bind(since).bind(until).bind(limit).fetch_all(&mut *tx).await.map_err(ise)?
    } else { vec![] };
    let sheets = if module_active(&st, tid, "consuntivi").await && (has_cap(&s, "timesheets.view_all") || has_cap(&s, "approve")) {
        sqlx::query("SELECT t.id,t.state,t.updated_at,t.work_order_id,o.external_ref AS order_ref,u.email,
                (SELECT COALESCE(sum(l.minutes),0) FROM app.timesheet_lines l WHERE l.timesheet_id=t.id)::int AS minutes
             FROM app.timesheets t LEFT JOIN app.work_orders o ON o.id=t.work_order_id LEFT JOIN app.users u ON u.id=t.user_id
             WHERE t.updated_at > $1 AND t.updated_at <= $2 ORDER BY t.updated_at LIMIT $3")
            .bind(since).bind(until).bind(limit).fetch_all(&mut *tx).await.map_err(ise)?
    } else { vec![] };
    tx.commit().await.map_err(ise)?;
    let more = orders.len() as i64 == limit || sheets.len() as i64 == limit;
    // con troppe modifiche il prossimo `since` è l'ultima restituita (nessuna si perde)
    let next = if more { orders.iter().map(|r| r.get::<DateTime<Utc>,_>("updated_at")).chain(sheets.iter().map(|r| r.get::<DateTime<Utc>,_>("updated_at"))).min().unwrap_or(until) } else { until };
    let ts = |r: &sqlx::postgres::PgRow, c: &str| r.try_get::<Option<DateTime<Utc>>,_>(c).ok().flatten();
    Ok(Json(json!({"since": since, "until": next, "more": more,
        "orders": orders.iter().map(|r| json!({"id": r.get::<Uuid,_>("id"), "ref": r.try_get::<Option<String>,_>("external_ref").ok().flatten(),
            "title": r.get::<String,_>("title"), "state": r.get::<String,_>("state"), "outcome": r.try_get::<Option<String>,_>("outcome").ok().flatten(),
            "outcome_kind": r.try_get::<Option<String>,_>("outcome_kind").ok().flatten(),
            "scheduled_start": ts(r, "scheduled_start"), "scheduled_end": ts(r, "scheduled_end"), "arrived_at": ts(r, "arrived_at"),
            "work_started_at": ts(r, "work_started_at"), "work_minutes": r.try_get::<Option<i32>,_>("work_minutes").ok().flatten(),
            "signed_by": r.try_get::<Option<String>,_>("signed_by").ok().flatten(), "updated_at": r.get::<DateTime<Utc>,_>("updated_at"),
            "site_ref": r.try_get::<Option<String>,_>("site_ref").ok().flatten(), "client_ref": r.try_get::<Option<String>,_>("client_ref").ok().flatten(),
            "activity_type": r.try_get::<Option<String>,_>("activity_type").ok().flatten(), "team": r.try_get::<Option<String>,_>("team").ok().flatten(),
            "assignee": r.try_get::<Option<String>,_>("assignee").ok().flatten()})).collect::<Vec<_>>(),
        "timesheets": sheets.iter().map(|r| json!({"id": r.get::<Uuid,_>("id"), "state": r.get::<String,_>("state"),
            "order_id": r.try_get::<Option<Uuid>,_>("work_order_id").ok().flatten(), "order_ref": r.try_get::<Option<String>,_>("order_ref").ok().flatten(),
            "person": r.try_get::<Option<String>,_>("email").ok().flatten(), "minutes": r.try_get::<i32,_>("minutes").unwrap_or(0),
            "updated_at": r.get::<DateTime<Utc>,_>("updated_at")})).collect::<Vec<_>>() })))
}

// =============================================================================================
// Importazione CSV
// =============================================================================================
/// CSV con intestazione; separatore `;` o `,` (dedotto dalla prima riga); campi tra virgolette.
pub(crate) fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let text = text.trim_start_matches('\u{feff}');
    let sep = if text.lines().next().map(|l| l.matches(';').count() > l.matches(',').count()).unwrap_or(false) { ';' } else { ',' };
    let mut rows = vec![]; let mut row = vec![]; let mut cur = String::new(); let mut q = false;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' if q && it.peek() == Some(&'"') => { cur.push('"'); it.next(); }
            '"' => q = !q,
            c if c == sep && !q => { row.push(cur.trim().to_string()); cur.clear(); }
            '\r' if !q => {}
            '\n' if !q => { row.push(cur.trim().to_string()); cur.clear(); if row.iter().any(|x| !x.is_empty()) { rows.push(std::mem::take(&mut row)); } else { row.clear(); } }
            c => cur.push(c),
        }
    }
    row.push(cur.trim().to_string()); if row.iter().any(|x| !x.is_empty()) { rows.push(row); }
    rows
}

/// POST /api/integration/import/:entity (clients | sites | commesse | orders), corpo CSV.
/// Colonna obbligatoria `ref` (codice esterno); le altre hanno i nomi dei campi dell'API.
/// Ogni riga si inserisce o aggiorna da sola: gli errori non fermano le altre righe.
pub async fn import_csv(State(st): State<AppState>, h: HeaderMap, Path(entity): Path<String>, body: String) -> R {
    let s = require_tenant(&st, &h).await?;
    match entity.as_str() {
        "sites" => require_any_cap(&s, &["sites.manage", "users.manage"])?,
        "clients" | "commesse" | "orders" => require_cap(&s, "plan")?,
        _ => return Err(err(StatusCode::NOT_FOUND, "si importano: clients, sites, commesse, orders")),
    }
    let rows = parse_csv(&body);
    if rows.len() < 2 { return Err(err(StatusCode::BAD_REQUEST, "il file deve avere l'intestazione e almeno una riga")); }
    if rows.len() > 5001 { return Err(err(StatusCode::BAD_REQUEST, "al massimo 5000 righe per importazione")); }
    let head: Vec<String> = rows[0].iter().map(|x| x.trim().to_lowercase()).collect();
    let ri = head.iter().position(|x| x == "ref").ok_or_else(|| err(StatusCode::BAD_REQUEST, "manca la colonna «ref» (codice esterno)"))?;
    let forced = crate::constraints::force(&h);
    let (mut created, mut updated) = (0, 0); let mut errors: Vec<Value> = vec![]; let mut warnings = 0;
    for (n, row) in rows.iter().enumerate().skip(1) {
        let get = |k: &str| head.iter().position(|x| x == k).and_then(|i| row.get(i)).map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
        let num = |k: &str| get(k).and_then(|x| x.replace(',', ".").parse::<f64>().ok());
        let r = match row.get(ri).map(|x| x.trim()).filter(|x| !x.is_empty()) { Some(x) => x.to_string(), None => { errors.push(json!({"row": n + 1, "error": "codice «ref» vuoto"})); continue } };
        let res: Result<bool, (StatusCode, String)> = match entity.as_str() {
            "clients" => upsert_client(&st, &s, &r, &ClientUp { name: get("name").unwrap_or_default(), vat: get("vat"), address: get("address"), phone: get("phone"),
                email: get("email"), notes: get("notes"), pec: get("pec"), sdi: get("sdi"), payment_terms: get("payment_terms") }).await.map(|x| x.1),
            "sites" => upsert_site(&st, &s, &r, &SiteUp { name: get("name").unwrap_or_default(), address: get("address"), lat: num("lat"), lon: num("lon"),
                access_notes: get("access_notes"), access_windows: None }).await.map(|x| x.1),
            "commesse" => upsert_commessa(&st, &s, &r, &CommessaUp { name: get("name").unwrap_or_default(), code: get("code"), site_ref: get("site_ref"),
                client_ref: get("client_ref"), start_date: get("start_date"), end_date: get("end_date"), budget_amount: num("budget_amount"),
                status: get("status"), note: get("note") }).await.map(|x| x.1),
            _ => upsert_order(&st, &s, &r, &OrderUp { title: get("title").unwrap_or_default(), site_ref: get("site_ref"), client_ref: get("client_ref"),
                activity_type: get("activity_type"), team: get("team"), assignee_email: get("assignee_email"), quantity: num("quantity"),
                scheduled_start: get("scheduled_start"), scheduled_end: get("scheduled_end"), priority: num("priority").map(|p| p as i32),
                instructions: get("instructions") }, forced).await.map(|x| { if x.2.as_array().map(|a| !a.is_empty()).unwrap_or(false) { warnings += 1; } x.1 }),
        };
        match res { Ok(true) => created += 1, Ok(false) => updated += 1,
            Err((_, m)) => errors.push(json!({"row": n + 1, "ref": r, "error": serde_json::from_str::<Value>(&m).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(m)})) }
    }
    audit(&st, s.tenant_id.unwrap(), &s.email, "integration_import", json!({"entity": entity, "created": created, "updated": updated, "errors": errors.len()})).await;
    Ok(Json(json!({"entity": entity, "rows": rows.len() - 1, "created": created, "updated": updated, "with_warnings": warnings, "errors": errors})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn csv_semicolon_quotes() {
        let r = parse_csv("ref;name;note\nC1;\"Rossi; Bianchi\";\"dice \"\"ciao\"\"\"\n\nC2;Verdi;\n");
        assert_eq!(r.len(), 3);
        assert_eq!(r[1], vec!["C1", "Rossi; Bianchi", "dice \"ciao\""]);
        assert_eq!(r[2][0], "C2");
    }
    #[test] fn csv_comma_bom() {
        let r = parse_csv("\u{feff}ref,name\r\nA,Uno\r\nB,Due");
        assert_eq!(r, vec![vec!["ref","name"], vec!["A","Uno"], vec!["B","Due"]].iter().map(|v| v.iter().map(|x| x.to_string()).collect::<Vec<_>>()).collect::<Vec<_>>());
    }
    #[test] fn hash_compare() {
        assert!(ct_eq(&sha("ywk_a_b"), &sha("ywk_a_b")));
        assert!(!ct_eq(&sha("ywk_a_b"), &sha("ywk_a_c")));
    }
}
