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
use crate::api::{conflict_or, R, require_tenant, require_cap, has_cap, require_grantable, user_caps, effective_caps, license_overage, ise, err, audit, USER_ROLES, is_e164, valid_email};
use crate::{db, auth};

/// TA-10 (25/11/2026): prima ogni utente (anche un lavoratore) leggeva email, ruolo e telefono di
/// tutti. Ora:
/// - telefono: solo a chi gestisce utenti o escalation, gestisce gli allarmi (deve poter
///   chiamare il lavoratore in emergenza) o pianifica (contattare un tecnico sul campo);
/// - elenco (email e ruolo): a chi coordina (pianifica, segue allarmi, approva, amministra…);
/// - chi non ha nessuna di queste capability vede solo sé stesso.
pub async fn list_users(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    // decisione PO 25/11/2026: anche chi pianifica vede i telefoni (contattare un tecnico sul campo)
    let phones = ["users.manage","escalation.manage","incidents.handle","plan"].iter().any(|c| has_cap(&s, c));
    let directory = phones || ["plan","incidents.view","approve","timesheets.view_all","orders.view_all","devices.admin",
        "devices.manage","iot.manage","audit.view","gdpr.manage","settings.manage","dashboard.manage","config.view"]
        .iter().any(|c| has_cap(&s, c));
    let mut tx = db::begin_tenant(&st.rt_pool, s.tenant_id.unwrap()).await.map_err(ise)?;
    let rows = if directory {
        sqlx::query("SELECT id,email,role,phone FROM app.users ORDER BY role,email").fetch_all(&mut *tx).await
    } else {
        sqlx::query("SELECT id,email,role,phone FROM app.users WHERE id=$1").bind(s.user_id).fetch_all(&mut *tx).await
    }.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let id: Uuid = r.get("id");
        json!({
        "id": id, "email": r.get::<String,_>("email"), "role": r.get::<String,_>("role"),
        "phone": if phones || id == s.user_id { r.try_get::<String,_>("phone").ok() } else { None } }) }).collect();
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
pub struct NewUser { pub email: String, pub role: String, #[serde(default)] pub password: Option<String> }
/// TA-8 (decisione PO 26/11/2026): password scelte dalle persone almeno 10 caratteri; quella
/// iniziale data via API (importazioni, test) almeno 8.
pub(crate) const MIN_OWN_PASSWORD: usize = 10;
/// Crea un invito monouso (7 giorni) per l'utente; gli inviti precedenti non usati decadono.
/// Ritorna il percorso del link (`/invite.html?slug=..&token=..`) e, se la consegna email è
/// configurata e c'è un indirizzo pubblico, prova a inviarlo per email.
pub(crate) async fn issue_invite(st: &AppState, tid: Uuid, uid: Uuid, email: &str) -> Result<Value, (StatusCode, String)> {
    let slug: String = sqlx::query_scalar("SELECT slug FROM app.tenants WHERE id=$1").bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    let tok = auth::new_token();
    let exp = st.clock.now() + chrono::Duration::days(7);
    sqlx::query("UPDATE app.user_invites SET expires_at=LEAST(expires_at, now()) WHERE tenant_id=$1 AND user_id=$2 AND used_at IS NULL")
        .bind(tid).bind(uid).execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.user_invites(token_hash,tenant_id,user_id,email,expires_at) VALUES($1,$2,$3,$4,$5)")
        .bind(crate::api::hash_token(&tok)).bind(tid).bind(uid).bind(email).bind(exp).execute(&st.owner_pool).await.map_err(ise)?;
    let path = format!("/invite.html?slug={}&token={}", slug, tok);
    let mut emailed = false;
    let relay = crate::delivery::chan("YW_ESC_SMTP_RELAY_URL").unwrap_or_default();
    let base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    if !relay.is_empty() && !base.trim().is_empty() {
        let link = format!("{}{}", base.trim_end_matches('/'), path);
        let body = json!({"email": {"to": email, "subject": "Invito a YouWorkforce",
            "body": format!("Sei stato invitato a YouWorkforce. Per scegliere la tua password e accedere apri questo link (vale 7 giorni, una sola volta):\n{link}")}});
        emailed = crate::delivery::deliver_email_relay(&st.http, &relay, &body).await.state == "live";
    }
    Ok(json!({"invite_path": path, "expires_at": exp.to_rfc3339(), "emailed": emailed}))
}
pub async fn create_user(State(st): State<AppState>, h: HeaderMap, Json(b): Json<NewUser>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    if !USER_ROLES.contains(&b.role.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo non valido")); }
    // TA-1: il ruolo assegnato non può avere permessi che chi crea non ha
    require_grantable(&s, &effective_caps(&st, tid, &b.role).await)?;
    if let Some(p) = &b.password { if p.len() < 8 { return Err(err(StatusCode::BAD_REQUEST, "password iniziale di almeno 8 caratteri")); } }
    if !valid_email(&b.email) { return Err(err(StatusCode::BAD_REQUEST, "email non valida")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // TA-8: senza password l'utente riceve un invito e la sceglie lui; fino ad allora nessun accesso
    let pw = b.password.clone().unwrap_or_else(auth::new_token);
    let row = sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(b.email.trim()).bind(auth::hash_password(&pw)).bind(&b.role)
        .fetch_optional(&mut *tx).await.map_err(|e| conflict_or(e, "email gia' presente"))?;
    tx.commit().await.map_err(ise)?;
    let id = row.ok_or_else(|| err(StatusCode::CONFLICT, "email gia' presente"))?.get::<Uuid,_>("id");
    let invite = if b.password.is_none() { Some(issue_invite(&st, tid, id, b.email.trim()).await?) } else { None };
    audit(&st, tid, &s.email, "create_user", json!({"email":b.email,"role":b.role,"invite":invite.is_some()})).await;
    // SA-6: licenze indicative -> l'utente è creato comunque, la risposta riporta l'eventuale superamento
    Ok(Json(json!({"id":id, "license_warnings": license_overage(&st, tid).await, "invite": invite})))
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
        if p.len() < 8 { let _=tx.rollback().await; return Err(err(StatusCode::BAD_REQUEST, "password di almeno 8 caratteri")); }
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

/// TA-8: nuovo link di accesso per un utente (primo accesso non fatto o password dimenticata).
pub async fn user_invite(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    let (_role, caps) = user_caps(&st, tid, id).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    if id != s.user_id { require_grantable(&s, &caps)?; } // TA-1: non si prende il controllo di chi ha più permessi
    let email: String = sqlx::query_scalar("SELECT email FROM app.users WHERE id=$1 AND tenant_id=$2 AND anonymized_at IS NULL")
        .bind(id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    let inv = issue_invite(&st, tid, id, &email).await?;
    audit(&st, tid, &s.email, "user_invite", json!({"user":id,"emailed":inv["emailed"]})).await;
    Ok(Json(inv))
}
#[derive(Deserialize)] pub struct OwnPassword { pub current: String, pub new: String }
/// TA-8: ognuno cambia la propria password (serve quella attuale); le altre sessioni si chiudono.
pub async fn change_own_password(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OwnPassword>) -> R {
    let s = require_tenant(&st, &h).await?; let tid = s.tenant_id.unwrap();
    if s.impersonator.is_some() { return Err(err(StatusCode::FORBIDDEN, "durante l'impersonazione non si cambia la password dell'utente")); }
    if b.new.chars().count() < MIN_OWN_PASSWORD { return Err(err(StatusCode::BAD_REQUEST, &format!("la nuova password deve avere almeno {MIN_OWN_PASSWORD} caratteri"))); }
    let hash: String = sqlx::query_scalar("SELECT pass_hash FROM app.users WHERE id=$1 AND tenant_id=$2").bind(s.user_id).bind(tid)
        .fetch_one(&st.owner_pool).await.map_err(ise)?;
    if !auth::verify_password(&b.current, &hash) { return Err(err(StatusCode::FORBIDDEN, "password attuale errata")); }
    sqlx::query("UPDATE app.users SET pass_hash=$3 WHERE id=$1 AND tenant_id=$2").bind(s.user_id).bind(tid).bind(auth::hash_password(&b.new))
        .execute(&st.owner_pool).await.map_err(ise)?;
    let cur = h.get(axum::http::header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or("")
        .split(';').find_map(|c| c.trim().strip_prefix("yw_session=").map(|x| x.to_string())).unwrap_or_default();
    let _ = sqlx::query("DELETE FROM app.sessions WHERE user_id=$1 AND tenant_id=$2 AND token<>$3").bind(s.user_id).bind(tid).bind(cur)
        .execute(&st.owner_pool).await;
    audit(&st, tid, &s.email, "change_own_password", json!({})).await;
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
