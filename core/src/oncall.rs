//! Escalation nativa e reperibilità: impostazioni, recapiti, assenze, manutenzione, calendari, consegna, ack Twilio, servizi
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

// ============================================================================
// Escalation nativa: impostazioni reperibilita', recapiti+verifica, assenze,
// manutenzione, calendario iCal. Tutto pilotato da YouWorkforce (tenant_admin).
// ============================================================================


// ---- Impostazioni reperibilita' (ladder vocale + status updates) ----
pub async fn oncall_settings_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let c = oncall_cfg(&st, tid).await;
    // gruppo di default (catch-all nativo) + motore corrente, per la UI Escalation
    let dg: Option<Uuid> = sqlx::query_scalar("SELECT default_group_id FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    Ok(Json(json!({"voice_fallback_on":c.voice_on,"voice_fallback_min":c.voice_min,"status_updates_on":c.status_on,
        "default_group_id":dg,"engine":"native"})))
}
#[derive(Deserialize)]
pub struct OncallSettingsReq { pub voice_fallback_on: Option<bool>, pub voice_fallback_min: Option<i32>, pub status_updates_on: Option<bool>,
    #[serde(default)] pub default_group_id: Option<Uuid>, #[serde(default)] pub clear_default_group: bool }
pub async fn oncall_settings_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OncallSettingsReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let cur = oncall_cfg(&st, tid).await;
    let voice_on = b.voice_fallback_on.unwrap_or(cur.voice_on);
    let voice_min = b.voice_fallback_min.unwrap_or(cur.voice_min).clamp(1, 60);
    let status_on = b.status_updates_on.unwrap_or(cur.status_on);
    // default group: validato nel tenant; clear_default_group lo azzera
    if let Some(g) = b.default_group_id {
        let ok = sqlx::query_scalar::<_, Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
            .bind(g).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "gruppo di default inesistente")); }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.tenant_oncall_cfg(tenant_id,voice_fallback_on,voice_fallback_min,status_updates_on) VALUES($1,$2,$3,$4) ON CONFLICT (tenant_id) DO UPDATE SET voice_fallback_on=EXCLUDED.voice_fallback_on,voice_fallback_min=EXCLUDED.voice_fallback_min,status_updates_on=EXCLUDED.status_updates_on,updated_at=now()")
        .bind(tid).bind(voice_on).bind(voice_min).bind(status_on).execute(&mut *tx).await.map_err(ise)?;
    if b.clear_default_group {
        sqlx::query("UPDATE app.tenant_oncall_cfg SET default_group_id=NULL, updated_at=now() WHERE tenant_id=$1").bind(tid).execute(&mut *tx).await.map_err(ise)?;
    } else if let Some(g) = b.default_group_id {
        sqlx::query("UPDATE app.tenant_oncall_cfg SET default_group_id=$2, updated_at=now() WHERE tenant_id=$1").bind(tid).bind(g).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"voice_fallback_on":voice_on,"voice_fallback_min":voice_min,"status_updates_on":status_on})))
}

// ---- Recapiti dei membri (stato verifica) ----
pub async fn oncall_contacts(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // NATIVO: recapiti = telefono/email dell'utente + stato verifica (users.phone_verified_at).
    // Membri = utenti presenti in almeno un gruppo di escalation.
    let rows = sqlx::query(
        "SELECT DISTINCT u.id,u.email,u.phone,u.phone_verified_at,u.phone_opt_out_at
           FROM app.users u JOIN app.alarm_group_members m ON m.user_id=u.id
          WHERE u.tenant_id=$1 ORDER BY u.email")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let phone: Option<String> = r.try_get("phone").ok().flatten();
        let verified: Option<chrono::DateTime<chrono::Utc>> = r.try_get("phone_verified_at").ok().flatten();
        let opt_out: Option<chrono::DateTime<chrono::Utc>> = r.try_get("phone_opt_out_at").ok().flatten();
        json!({"user_id":r.get::<Uuid,_>("id"),"email":r.get::<String,_>("email"),
               "phone":phone,"verified":verified.is_some(),"verified_at":verified,"opted_out":opt_out.is_some()})
    }).collect();
    Ok(Json(json!({"members":out,"engine":"native"})))
}
#[derive(Deserialize)] pub struct CmId { pub user_id: Uuid }
/// Invia un codice OTP al telefono dell'utente e lo memorizza (scadenza 10 min). Nativo.
pub async fn oncall_contact_verify(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CmId>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let phone: Option<String> = sqlx::query_scalar("SELECT phone FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let phone = phone.filter(|p| !p.trim().is_empty()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "l'utente non ha un numero di telefono"))?;
    let code: u32 = { use rand::Rng as _; rand::thread_rng().gen_range(100000..1000000) }; // 6 cifre
    let exp = st.clock.now() + chrono::Duration::minutes(10);
    sqlx::query("UPDATE app.users SET phone_verify_code=$1, phone_verify_expires=$2 WHERE id=$3 AND tenant_id=$4")
        .bind(code.to_string()).bind(exp).bind(b.user_id).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    let text = format!("YouWorkforce: codice di verifica {code} (valido 10 minuti).");
    let res = esc_send_sms_native(&st, &phone, &text).await;
    audit(&st, tid, "youworkforce", "oncall_contact_verify", json!({"user":b.user_id,"delivery":res.state})).await;
    Ok(Json(json!({"ok":true,"delivery":res.state,"note":"codice inviato via SMS al recapito"})))
}
#[derive(Deserialize)] pub struct CmVerify { pub user_id: Uuid, pub code: String }
/// Conferma l'OTP e marca il recapito come verificato. Nativo.
pub async fn oncall_contact_code(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CmVerify>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let row = sqlx::query("SELECT phone_verify_code,phone_verify_expires FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    let saved: Option<String> = row.try_get("phone_verify_code").ok().flatten();
    let exp: Option<chrono::DateTime<chrono::Utc>> = row.try_get("phone_verify_expires").ok().flatten();
    let saved = saved.ok_or_else(|| err(StatusCode::BAD_REQUEST, "nessun codice in attesa: richiedi prima l'invio"))?;
    if matches!(exp, Some(e) if st.clock.now() > e) { return Err(err(StatusCode::BAD_REQUEST, "codice scaduto: richiedi un nuovo invio")); }
    if saved.trim() != b.code.trim() { return Err(err(StatusCode::BAD_REQUEST, "codice non valido")); }
    sqlx::query("UPDATE app.users SET phone_verified_at=now(), phone_verify_code=NULL, phone_verify_expires=NULL WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "youworkforce", "oncall_contact_code", json!({"user":b.user_id})).await;
    Ok(Json(json!({"ok":true,"note":"recapito verificato"})))
}
/// Invia un SMS di prova al recapito dell'utente. Nativo.
pub async fn oncall_contact_test(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CmId>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let phone: Option<String> = sqlx::query_scalar("SELECT phone FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(b.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten();
    let phone = phone.filter(|p| !p.trim().is_empty()).ok_or_else(|| err(StatusCode::BAD_REQUEST, "l'utente non ha un numero di telefono"))?;
    let res = esc_send_sms_native(&st, &phone, "YouWorkforce: messaggio di prova dei reperibili.").await;
    Ok(Json(json!({"ok":true,"delivery":res.state,"note":"messaggio di prova inviato"})))
}

// ---- F3: multi-recapito + regole di notifica per-utente (ladder a 2 livelli) ----
pub async fn user_cm_list(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let cms = sqlx::query("SELECT id,kind,value,label,verified_at,disabled_at FROM app.user_contact_methods WHERE tenant_id=$1 AND user_id=$2 ORDER BY created_at")
        .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let rules = sqlx::query("SELECT r.id,r.cm_id,r.delay_min,cm.kind,cm.value FROM app.user_notif_rules r JOIN app.user_contact_methods cm ON cm.id=r.cm_id WHERE r.tenant_id=$1 AND r.user_id=$2 ORDER BY r.delay_min")
        .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let cmj: Vec<Value> = cms.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"kind":r.get::<String,_>("kind"),
        "value":r.get::<String,_>("value"),"label":r.get::<String,_>("label"),
        "verified":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("verified_at").ok().flatten().is_some(),
        "disabled":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("disabled_at").ok().flatten().is_some()})).collect();
    let rj: Vec<Value> = rules.iter().map(|r| json!({"id":r.get::<Uuid,_>("id"),"cm_id":r.get::<Uuid,_>("cm_id"),
        "delay_min":r.get::<i32,_>("delay_min"),"kind":r.get::<String,_>("kind"),"value":r.get::<String,_>("value")})).collect();
    // G3: preferenza status-update dell'utente (null=eredita) + default tenant per l'effettivo
    let upref: Option<bool> = sqlx::query_scalar("SELECT status_updates_on FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let tdef = oncall_cfg(&st, tid).await.status_on;
    Ok(Json(json!({"methods":cmj,"rules":rj,"status_pref":upref,"status_effective":upref.unwrap_or(tdef)})))
}
#[derive(Deserialize)] pub struct CmAddReq { pub kind: String, pub value: String, #[serde(default)] pub label: String }
pub async fn user_cm_add(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<CmAddReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if !matches!(b.kind.as_str(), "sms"|"voice"|"email") { return Err(err(StatusCode::BAD_REQUEST, "kind: sms|voice|email")); }
    let val = b.value.trim();
    if val.is_empty() || val.len() > 200 { return Err(err(StatusCode::BAD_REQUEST, "valore recapito non valido")); }
    let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !ok { return Err(err(StatusCode::NOT_FOUND, "utente inesistente")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.user_contact_methods(tenant_id,user_id,kind,value,label) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(uid).bind(&b.kind).bind(val).bind(b.label.trim()).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn user_cm_del(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.user_contact_methods WHERE id=$1 AND tenant_id=$2").bind(cmid).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "recapito inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct RuleAddReq { pub cm_id: Uuid, pub delay_min: Option<i32> }
pub async fn user_rule_add(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<RuleAddReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let dm = b.delay_min.unwrap_or(0).clamp(0, 1440);
    // il contact method deve appartenere all'utente nel tenant
    let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.user_contact_methods WHERE id=$1 AND user_id=$2 AND tenant_id=$3")
        .bind(b.cm_id).bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !ok { return Err(err(StatusCode::BAD_REQUEST, "recapito non valido per l'utente")); }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.user_notif_rules(tenant_id,user_id,cm_id,delay_min) VALUES($1,$2,$3,$4) RETURNING id")
        .bind(tid).bind(uid).bind(b.cm_id).bind(dm).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn user_rule_del(State(st): State<AppState>, h: HeaderMap, Path(rid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.user_notif_rules WHERE id=$1 AND tenant_id=$2").bind(rid).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "regola inesistente")); }
    Ok(Json(json!({"ok":true})))
}
/// G1 — riabilita (admin) i recapiti di un utente in opt-out: toglie dal DNC telefono primario e
/// contact method; i recapiti vanno poi ri-verificati (OTP). Compliance: azione tracciata.
pub async fn user_reactivate(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "users.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // raccogli i valori dell'utente (telefono + CM) e togli ciascuno dal DNC globale
    let mut vals: Vec<String> = sqlx::query_scalar("SELECT value FROM app.user_contact_methods WHERE tenant_id=$1 AND user_id=$2")
        .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    if let Some(ph) = sqlx::query_scalar::<_,Option<String>>("SELECT phone FROM app.users WHERE id=$1 AND tenant_id=$2").bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.flatten() {
        vals.push(ph);
    }
    for v in vals.iter().filter(|v| !v.trim().is_empty()) { esc_optout_clear(&st, v).await; }
    audit(&st, tid, "youworkforce", "contact_reactivate", json!({"user":uid})).await;
    Ok(Json(json!({"ok":true,"note":"recapiti riattivati: da ri-verificare"})))
}

// ---- G2: verifica OTP per singolo contact method (sms/voice/email) ----
pub(crate) async fn cm_fetch(st: &AppState, tid: Uuid, cmid: Uuid) -> Option<(String,String)> {
    sqlx::query("SELECT kind,value FROM app.user_contact_methods WHERE id=$1 AND tenant_id=$2")
        .bind(cmid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()
        .map(|r| (r.get::<String,_>("kind"), r.get::<String,_>("value")))
}
/// Invia un testo (es. OTP) sul canale del recapito, riusando gli adapter: sms/voice via Twilio o
/// sink, email via relay o sink.
pub(crate) async fn esc_send_code_native(st: &AppState, kind: &str, value: &str, text: &str) -> crate::delivery::SinkResult {
    let from = crate::delivery::chan("YW_ESC_FROM").unwrap_or_else(|| "+10000000000".into());
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    let relay = crate::delivery::chan("YW_ESC_SMTP_RELAY_URL").unwrap_or_default();
    let slack_token = crate::delivery::chan("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    let public_base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let live_cfg = crate::delivery::TwilioCfg::from_env();
    let ch = esc_channel_from_kind(kind);
    let body = crate::delivery::request_body(ch, &from, value, text, "verify", 0);
    esc_dispatch(st, ch, value, text, &body, live_cfg.is_some(), live_cfg.as_ref(), &slack_token, &relay, &sink, &public_base).await
}
pub async fn user_cm_verify(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let (kind, value) = cm_fetch(&st, tid, cmid).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "recapito inesistente"))?;
    if esc_is_opted_out(&st, &value).await { return Err(err(StatusCode::BAD_REQUEST, "recapito in opt-out: riabilitalo prima")); }
    let code: u32 = { use rand::Rng as _; rand::thread_rng().gen_range(100000..1000000) };
    let exp = st.clock.now() + chrono::Duration::minutes(10);
    sqlx::query("UPDATE app.user_contact_methods SET verify_code=$1, verify_expires=$2 WHERE id=$3 AND tenant_id=$4")
        .bind(code.to_string()).bind(exp).bind(cmid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    let res = esc_send_code_native(&st, &kind, &value, &format!("YouWorkforce: codice di verifica {code} (10 min).")).await;
    Ok(Json(json!({"ok":true,"delivery":res.state,"note":"codice inviato al recapito"})))
}
#[derive(Deserialize)] pub struct CmCodeReq { pub code: String }
pub async fn user_cm_code(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>, Json(b): Json<CmCodeReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let row = sqlx::query("SELECT verify_code,verify_expires FROM app.user_contact_methods WHERE id=$1 AND tenant_id=$2")
        .bind(cmid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "recapito inesistente"))?;
    let saved: Option<String> = row.try_get("verify_code").ok().flatten();
    let exp: Option<chrono::DateTime<chrono::Utc>> = row.try_get("verify_expires").ok().flatten();
    let saved = saved.ok_or_else(|| err(StatusCode::BAD_REQUEST, "nessun codice in attesa"))?;
    if matches!(exp, Some(e) if st.clock.now() > e) { return Err(err(StatusCode::BAD_REQUEST, "codice scaduto")); }
    if saved.trim() != b.code.trim() { return Err(err(StatusCode::BAD_REQUEST, "codice non valido")); }
    sqlx::query("UPDATE app.user_contact_methods SET verified_at=now(), verify_code=NULL, verify_expires=NULL WHERE id=$1 AND tenant_id=$2")
        .bind(cmid).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"note":"recapito verificato"})))
}
pub async fn user_cm_test(State(st): State<AppState>, h: HeaderMap, Path(cmid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let (kind, value) = cm_fetch(&st, tid, cmid).await.ok_or_else(|| err(StatusCode::NOT_FOUND, "recapito inesistente"))?;
    let res = esc_send_code_native(&st, &kind, &value, "YouWorkforce: messaggio di prova.").await;
    Ok(Json(json!({"ok":true,"delivery":res.state})))
}
// ---- G3: preferenza status-update per-utente (override del default tenant; null = eredita) ----
#[derive(Deserialize)] pub struct StatusPrefReq { pub on: Option<bool> }
pub async fn user_status_pref_set(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>, Json(b): Json<StatusPrefReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.users SET status_updates_on=$2 WHERE id=$1 AND tenant_id=$3")
        .bind(uid).bind(b.on).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "utente inesistente")); }
    Ok(Json(json!({"ok":true,"status_updates_on":b.on})))
}

// ---- Assenze / sostituzioni (override nativi sul gruppo, tabella app.esc_overrides) ----
#[derive(Deserialize)] pub struct OverrideQuery { pub group_id: Uuid }
pub async fn oncall_overrides_list(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<OverrideQuery>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    // Sorgente NATIVA (app.esc_overrides); nomi utente risolti localmente.
    let rows = sqlx::query(
        "SELECT o.id,o.start_at,o.end_at,o.add_user,o.remove_user,
                au.email AS add_email, ru.email AS rem_email
           FROM app.esc_overrides o
           LEFT JOIN app.users au ON au.id=o.add_user
           LEFT JOIN app.users ru ON ru.id=o.remove_user
          WHERE o.tenant_id=$1 AND o.group_id=$2 ORDER BY o.start_at DESC")
        .bind(tid).bind(q.group_id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"),
        "start": r.get::<chrono::DateTime<chrono::Utc>,_>("start_at"),
        "end": r.get::<chrono::DateTime<chrono::Utc>,_>("end_at"),
        "add_user": r.try_get::<Option<Uuid>,_>("add_user").ok().flatten(),
        "add_email": r.try_get::<Option<String>,_>("add_email").ok().flatten(),
        "remove_user": r.try_get::<Option<Uuid>,_>("remove_user").ok().flatten(),
        "remove_email": r.try_get::<Option<String>,_>("rem_email").ok().flatten(),
    })).collect();
    Ok(Json(json!({"overrides":out})))
}
#[derive(Deserialize)]
pub struct OverrideReq { pub group_id: Uuid, pub start: String, pub end: String, pub add_user_id: Option<Uuid>, pub remove_user_id: Option<Uuid> }
pub async fn oncall_override_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.add_user_id.is_none() && b.remove_user_id.is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "indica un sostituto (aggiungi) e/o la persona assente (rimuovi)"));
    }
    let start = chrono::DateTime::parse_from_rfc3339(&b.start).map_err(|_| err(StatusCode::BAD_REQUEST, "start: formato RFC3339"))?.with_timezone(&chrono::Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&b.end).map_err(|_| err(StatusCode::BAD_REQUEST, "end: formato RFC3339"))?.with_timezone(&chrono::Utc);
    if end <= start { return Err(err(StatusCode::BAD_REQUEST, "end deve essere dopo start")); }
    // utenti validati nel tenant (se indicati)
    for u in [b.add_user_id, b.remove_user_id].into_iter().flatten() {
        let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2")
            .bind(u).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "utente non appartiene al tenant")); }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.esc_overrides(tenant_id,group_id,start_at,end_at,add_user,remove_user) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.group_id).bind(start).bind(end).bind(b.add_user_id).bind(b.remove_user_id)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let oid: Uuid = row.get("id");
    audit(&st, tid, "youworkforce", "oncall_override_add", json!({"group":b.group_id,"id":oid})).await;
    Ok(Json(json!({"ok":true,"id":oid})))
}
#[derive(Deserialize)] pub struct OverrideDel { pub id: Uuid }
pub async fn oncall_override_del(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideDel>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_overrides WHERE id=$1 AND tenant_id=$2").bind(b.id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "override inesistente")); }
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct OverrideUpdate { pub id: Uuid, pub start: String, pub end: String, pub add_user_id: Option<Uuid>, pub remove_user_id: Option<Uuid> }
/// Modifica un override esistente (edit in-place).
pub async fn oncall_override_update(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideUpdate>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.add_user_id.is_none() && b.remove_user_id.is_none() { return Err(err(StatusCode::BAD_REQUEST, "indica sostituto e/o assente")); }
    let start = chrono::DateTime::parse_from_rfc3339(&b.start).map_err(|_| err(StatusCode::BAD_REQUEST, "start: RFC3339"))?.with_timezone(&chrono::Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&b.end).map_err(|_| err(StatusCode::BAD_REQUEST, "end: RFC3339"))?.with_timezone(&chrono::Utc);
    if end <= start { return Err(err(StatusCode::BAD_REQUEST, "end deve essere dopo start")); }
    for u in [b.add_user_id, b.remove_user_id].into_iter().flatten() {
        if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(u).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::BAD_REQUEST, "utente non nel tenant")); }
    }
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("UPDATE app.esc_overrides SET start_at=$3,end_at=$4,add_user=$5,remove_user=$6 WHERE id=$1 AND tenant_id=$2")
        .bind(b.id).bind(tid).bind(start).bind(end).bind(b.add_user_id).bind(b.remove_user_id).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "override inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- F3: Temporary schedule (sostituzione a tempo) ----
pub async fn temp_schedule_list(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<OverrideQuery>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let rows = sqlx::query("SELECT id,start_at,end_at,blocks FROM app.esc_temp_schedules WHERE tenant_id=$1 AND group_id=$2 AND end_at>now() ORDER BY start_at")
        .bind(tid).bind(q.group_id).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id":r.get::<Uuid,_>("id"),"start":r.get::<chrono::DateTime<chrono::Utc>,_>("start_at"),
        "end":r.get::<chrono::DateTime<chrono::Utc>,_>("end_at"),"blocks":r.try_get::<Value,_>("blocks").unwrap_or_else(|_| json!([]))})).collect();
    Ok(Json(json!({"schedules":out})))
}
#[derive(Deserialize)] pub struct TempBlock { pub user_id: Uuid, pub hours: f64 }
#[derive(Deserialize)] pub struct TempSchedReq { pub group_id: Uuid, pub start: String, pub end: String, pub blocks: Vec<TempBlock>, #[serde(default)] pub allow_gaps: bool }
pub async fn temp_schedule_add(State(st): State<AppState>, h: HeaderMap, Json(b): Json<TempSchedReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let start = chrono::DateTime::parse_from_rfc3339(&b.start).map_err(|_| err(StatusCode::BAD_REQUEST, "start: RFC3339"))?.with_timezone(&chrono::Utc);
    let end = chrono::DateTime::parse_from_rfc3339(&b.end).map_err(|_| err(StatusCode::BAD_REQUEST, "end: RFC3339"))?.with_timezone(&chrono::Utc);
    if end <= start { return Err(err(StatusCode::BAD_REQUEST, "end deve essere dopo start")); }
    if b.blocks.is_empty() || b.blocks.len() > 100 { return Err(err(StatusCode::BAD_REQUEST, "1..100 blocchi")); }
    let mut total_h = 0.0f64;
    for blk in &b.blocks {
        if blk.hours <= 0.0 || blk.hours > 24.0*366.0 { return Err(err(StatusCode::BAD_REQUEST, "ore blocco non valide")); }
        total_h += blk.hours;
        let ok = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.users WHERE id=$1 AND tenant_id=$2").bind(blk.user_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "utente blocco non nel tenant")); }
    }
    // Allow gaps: se i blocchi non coprono tutta la finestra e non è consentito, rifiuta.
    let window_h = (end - start).num_seconds() as f64 / 3600.0;
    if !b.allow_gaps && total_h + 0.001 < window_h {
        return Err(err(StatusCode::BAD_REQUEST, "buco di copertura: i blocchi non coprono tutta la finestra (spunta 'consenti buchi')"));
    }
    let exists = sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(b.group_id).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_some();
    if !exists { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    let blocks = serde_json::to_value(b.blocks.iter().map(|x| json!({"user_id":x.user_id,"hours":x.hours})).collect::<Vec<_>>()).unwrap();
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.esc_temp_schedules(tenant_id,group_id,start_at,end_at,blocks,allow_gaps) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(tid).bind(b.group_id).bind(start).bind(end).bind(&blocks).bind(b.allow_gaps).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    let id: Uuid = row.get("id");
    audit(&st, tid, "youworkforce", "temp_schedule_add", json!({"group":b.group_id,"id":id,"blocks":b.blocks.len()})).await;
    Ok(Json(json!({"ok":true,"id":id})))
}
pub async fn temp_schedule_del(State(st): State<AppState>, h: HeaderMap, Json(b): Json<OverrideDel>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.esc_temp_schedules WHERE id=$1 AND tenant_id=$2").bind(b.id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "pianificazione inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- Maintenance Mode: sospende escalation dei servizi del tenant ----
#[derive(Deserialize)] pub struct MaintReq { pub hours: Option<i64> }
pub async fn oncall_maintenance(State(st): State<AppState>, h: HeaderMap, Json(b): Json<MaintReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let hours = b.hours.unwrap_or(0).clamp(0, 24);
    let exp = if hours > 0 { st.clock.now() + chrono::Duration::hours(hours) } else { st.clock.now() - chrono::Duration::minutes(1) };
    let exp_s = exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // Sorgente di verità NATIVA: il runner nativo (escalation_delivery_run_tenant) sospende
    // le consegne finché now() < maintenance_until.
    let until_db: Option<chrono::DateTime<chrono::Utc>> = if hours > 0 { Some(exp) } else { None };
    sqlx::query("INSERT INTO app.tenant_oncall_cfg(tenant_id,maintenance_until) VALUES($1,$2)
        ON CONFLICT (tenant_id) DO UPDATE SET maintenance_until=EXCLUDED.maintenance_until,updated_at=now()")
        .bind(tid).bind(until_db).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, "youworkforce", "oncall_maintenance", json!({"hours":hours,"native":true})).await;
    Ok(Json(json!({"ok":true,"until":if hours>0 {Some(exp_s)} else {None},"active":hours>0})))
}
/// Maintenance nativo attivo adesso? (now < maintenance_until). Usato dal runner nativo.
pub async fn maintenance_active(st: &AppState, tid: Uuid) -> bool {
    let until: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT maintenance_until FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    matches!(until, Some(u) if st.clock.now() < u)
}

// ---- Calendario reperibilita' (iCal), nativo YouWorkforce ----
/// Feed personale. `?rotate=1` rigenera il token (revoca il vecchio URL); `?group=<id>` nella .ics
/// filtra ai turni dell'utente in quel gruppo (sottoscrizione per-schedule self-service).
pub async fn oncall_calendar_url(State(st): State<AppState>, h: HeaderMap, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let uid = s.user_id;
    let rotate = q.get("rotate").map(|v| v=="1"||v=="true").unwrap_or(false);
    let existing: Option<String> = sqlx::query_scalar("SELECT token FROM app.oncall_ical_tokens WHERE tenant_id=$1 AND user_id=$2")
        .bind(tid).bind(uid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let token = match (existing, rotate) {
        (Some(t), false) => t,
        _ => { let t = auth::new_token();
            sqlx::query("INSERT INTO app.oncall_ical_tokens(tenant_id,user_id,token) VALUES($1,$2,$3) ON CONFLICT (tenant_id,user_id) DO UPDATE SET token=EXCLUDED.token")
                .bind(tid).bind(uid).bind(&t).execute(&st.owner_pool).await.map_err(ise)?; t }
    };
    let grp = q.get("group").map(|g| format!("&group={g}")).unwrap_or_default();
    Ok(Json(json!({"url":format!("/api/oncall-calendar.ics?token={token}{grp}"),"token":token})))
}
/// Revoca il feed personale (il vecchio URL smette di funzionare).
pub async fn oncall_calendar_revoke(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    sqlx::query("DELETE FROM app.oncall_ical_tokens WHERE tenant_id=$1 AND user_id=$2").bind(s.tenant_id.unwrap()).bind(s.user_id).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
/// Abbonamento iCal PER-GRUPPO. `?rotate=1` rigenera il token.
pub async fn group_calendar_url(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2").bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?.is_none() { return Err(err(StatusCode::NOT_FOUND, "gruppo inesistente")); }
    let rotate = q.get("rotate").map(|v| v=="1"||v=="true").unwrap_or(false);
    let existing: Option<String> = sqlx::query_scalar("SELECT token FROM app.esc_group_ical_tokens WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let token = match (existing, rotate) {
        (Some(t), false) => t,
        (old, _) => {
            if old.is_some() { sqlx::query("DELETE FROM app.esc_group_ical_tokens WHERE tenant_id=$1 AND group_id=$2").bind(tid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?; }
            let t = auth::new_token();
            sqlx::query("INSERT INTO app.esc_group_ical_tokens(token,tenant_id,group_id) VALUES($1,$2,$3)").bind(&t).bind(tid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?; t
        }
    };
    Ok(Json(json!({"url":format!("/api/group-calendar.ics?token={token}"),"token":token})))
}
/// Revoca il feed del gruppo.
pub async fn group_calendar_revoke(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    sqlx::query("DELETE FROM app.esc_group_ical_tokens WHERE tenant_id=$1 AND group_id=$2").bind(tid).bind(gid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)] pub struct IcalQuery { pub token: String, #[serde(default)] pub group: Option<Uuid> }
pub async fn group_calendar_ics(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<IcalQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT tenant_id,group_id FROM app.esc_group_ical_tokens WHERE token=$1")
        .bind(&q.token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "token non valido").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let gid: Uuid = row.get("group_id");
    let name: String = sqlx::query_scalar("SELECT name FROM app.alarm_groups WHERE id=$1").bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or_else(|| "Gruppo".into());
    let now = st.clock.now();
    let segs = esc_group_shifts(&st, tid, gid, now, 30).await;
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    let mut ics = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//YouWorkforce//Reperibilita//IT\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\nX-WR-CALNAME:Reperibilita {}\r\n", name.replace('\n'," "));
    for (i,(s0,e0,em)) in segs.iter().filter(|(_,_,e)| !e.is_empty()).enumerate() {
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:grp-{gid}-{i}-{}@youworkforce\r\n", fmt(*s0)));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(now)));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(*s0)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(*e0)));
        ics.push_str(&format!("SUMMARY:On-call: {}\r\n", em.join(", ").replace(',', "\\,")));
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");
    ([(axum::http::header::CONTENT_TYPE, "text/calendar; charset=utf-8")], ics).into_response()
}
pub async fn oncall_calendar_ics(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<IcalQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT tenant_id,user_id FROM app.oncall_ical_tokens WHERE token=$1")
        .bind(&q.token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "token non valido").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let uid: Uuid = row.get("user_id");
    // Con filtro ?group= la sottoscrizione mostra solo i turni calcolati di quel gruppo
    // (i turni manuali di app.oncall non sono associati a un gruppo).
    let shifts = if q.group.is_some() { vec![] } else {
        sqlx::query("SELECT id,starts_at,ends_at,note FROM app.oncall WHERE tenant_id=$1 AND user_id=$2 AND ends_at > now() - interval '7 days' ORDER BY starts_at")
            .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.unwrap_or_default()
    };
    let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//YouWorkforce//Reperibilita//IT\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\nX-WR-CALNAME:Reperibilita YouWorkforce\r\n");
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    for sh in &shifts {
        let id: Uuid = sh.get("id");
        let sdt: chrono::DateTime<chrono::Utc> = sh.get("starts_at");
        let edt: chrono::DateTime<chrono::Utc> = sh.get("ends_at");
        let note: Option<String> = sh.try_get("note").ok().flatten();
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:{id}@youworkforce\r\n"));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(chrono::Utc::now())));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(sdt)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(edt)));
        ics.push_str("SUMMARY:Reperibilita\r\n");
        if let Some(n) = note { if !n.is_empty() { ics.push_str(&format!("DESCRIPTION:{}\r\n", n.replace('\n'," ").replace(',', "\\,"))); } }
        ics.push_str("END:VEVENT\r\n");
    }
    // F3 — turni CALCOLATI dalla rotazione nativa (prossimi 30 giorni), non solo i turni manuali.
    let now = st.clock.now();
    for (gi, (gname, s, e)) in esc_user_rotation_shifts_g(&st, tid, uid, now, 30, q.group).await.iter().enumerate() {
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:rot-{uid}-{gi}-{}@youworkforce\r\n", fmt(*s)));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(now)));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(*s)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(*e)));
        ics.push_str(&format!("SUMMARY:Reperibilita {}\r\n", gname.replace('\n'," ").replace(',', "\\,")));
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");
    ([(axum::http::header::CONTENT_TYPE, "text/calendar; charset=utf-8")], ics).into_response()
}

// ============================================================================
// Calendario CORE condiviso (W1): un'unica vista che aggrega gli eventi dei moduli
// Campo attivi (turni/interventi/ronde). Ogni modulo è una "sorgente eventi"; il
// calendario li fonde. Tenant-scoped (RLS). I ruoli ristretti vedono solo i propri.
// ============================================================================
#[derive(Clone)]
pub(crate) struct CalEvent { id: String, source: &'static str, title: String, color: String,
    start: chrono::DateTime<chrono::Utc>, end: chrono::DateTime<chrono::Utc>,
    user_id: Option<Uuid>, user: Option<String>, link: Option<String> }
/// Colore calendario di un utente: quello scelto, altrimenti un colore stabile da palette (per id).
pub(crate) fn user_color(chosen: Option<String>, uid: Uuid) -> String {
    if let Some(c) = chosen { if !c.trim().is_empty() { return c; } }
    const P: &[&str] = &["#2563eb","#16a34a","#9333ea","#dc2626","#d97706","#0891b2","#db2777","#65a30d",
        "#4f46e5","#0d9488","#ea580c","#7c3aed","#059669","#e11d48","#0284c7","#ca8a04"];
    P[(uid.as_bytes()[0] as usize) % P.len()].to_string()
}
pub(crate) fn cal_json(e: &CalEvent) -> Value {
    json!({"id":e.id,"source":e.source,"title":e.title,"color":e.color,
        "start":e.start.to_rfc3339(),"end":e.end.to_rfc3339(),
        "user_id":e.user_id,"user":e.user,"link":e.link})
}
/// Raccoglie gli eventi del tenant nella finestra [from,to] dai moduli attivi. `only_uid` limita
/// a una persona (ruoli ristretti: i propri). Usato sia dall'endpoint JSON sia dall'iCal.
pub(crate) async fn calendar_collect(st: &AppState, tid: Uuid, from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>, only_uid: Option<Uuid>) -> Vec<CalEvent> {
    let mut out: Vec<CalEvent> = Vec::new();
    // --- Forza lavoro e turni: shift_plans ---
    if module_active(st, tid, "workforce").await {
        let rows = sqlx::query(
            "SELECT sp.id, sp.user_id, sp.starts_at, sp.ends_at, sp.state, u.email, u.cal_color, o.title AS order_title,
                    cm.code AS commessa_code, cm.name AS commessa_name
             FROM app.shift_plans sp JOIN app.users u ON u.id=sp.user_id
             LEFT JOIN app.work_orders o ON o.id=sp.work_order_id
             LEFT JOIN app.commesse cm ON cm.id=sp.commessa_id
             WHERE sp.tenant_id=$1 AND sp.ends_at >= $2 AND sp.starts_at <= $3
               AND ($4::uuid IS NULL OR sp.user_id=$4)")
            .bind(tid).bind(from).bind(to).bind(only_uid)
            .fetch_all(&st.owner_pool).await.unwrap_or_default();
        for r in &rows {
            let em: String = r.get("email");
            let uid: Uuid = r.get("user_id");
            let ordt: Option<String> = r.try_get("order_title").ok().flatten();
            let cmc: Option<String> = r.try_get("commessa_code").ok().flatten();
            let cmn: Option<String> = r.try_get("commessa_name").ok().flatten();
            // colore PER LAVORATORE (scelto o da palette) per distinguerli sul calendario
            let ctx = cmn.as_ref().map(|n| match &cmc { Some(c)=>format!("{c} · {n}"), None=>n.clone() }).or(ordt.clone());
            let title = match &ctx { Some(t) => format!("Turno · {em} · {t}"), None => format!("Turno · {em}") };
            out.push(CalEvent{ id: format!("shift-{}", r.get::<Uuid,_>("id")), source:"turno",
                title, color: user_color(r.try_get("cal_color").ok().flatten(), uid),
                start: r.get("starts_at"), end: r.get("ends_at"),
                user_id: Some(uid), user: Some(em), link: ordt });
        }
    }
    // --- Interventi e cantieri: ordini con appuntamento programmato ---
    if module_active(st, tid, "interventi").await {
        let rows = sqlx::query(
            "SELECT o.id, o.title, o.assigned_user, o.scheduled_start, o.scheduled_end, u.email
             FROM app.work_orders o LEFT JOIN app.users u ON u.id=o.assigned_user
             WHERE o.tenant_id=$1 AND o.scheduled_start IS NOT NULL
               AND o.scheduled_start <= $3
               AND COALESCE(o.scheduled_end, o.scheduled_start + interval '1 hour') >= $2
               AND ($4::uuid IS NULL OR o.assigned_user=$4)")
            .bind(tid).bind(from).bind(to).bind(only_uid)
            .fetch_all(&st.owner_pool).await.unwrap_or_default();
        for r in &rows {
            let s0: chrono::DateTime<chrono::Utc> = r.get("scheduled_start");
            let e0: chrono::DateTime<chrono::Utc> = r.try_get("scheduled_end").ok()
                .unwrap_or_else(|| s0 + chrono::Duration::hours(1));
            let title: String = r.get("title");
            out.push(CalEvent{ id: format!("order-{}", r.get::<Uuid,_>("id")), source:"intervento",
                title: format!("Intervento · {title}"), color:"#2563eb".into(),
                start: s0, end: e0, user_id: r.try_get("assigned_user").ok(),
                user: r.try_get::<String,_>("email").ok(), link: None });
        }
    }
    // --- Ronde e punzonature: giri eseguiti (finestra reale) ---
    if module_active(st, tid, "giri").await {
        let rows = sqlx::query(
            "SELECT pr.id, pr.user_id, pr.started_at, pr.ended_at, pr.status, u.email, rt.name AS route
             FROM app.patrol_rounds pr JOIN app.users u ON u.id=pr.user_id
             JOIN app.patrol_routes rt ON rt.id=pr.route_id
             WHERE pr.tenant_id=$1 AND pr.started_at <= $3
               AND COALESCE(pr.ended_at, pr.started_at + interval '2 hours') >= $2
               AND ($4::uuid IS NULL OR pr.user_id=$4)")
            .bind(tid).bind(from).bind(to).bind(only_uid)
            .fetch_all(&st.owner_pool).await.unwrap_or_default();
        for r in &rows {
            let s0: chrono::DateTime<chrono::Utc> = r.get("started_at");
            let e0: chrono::DateTime<chrono::Utc> = r.try_get("ended_at").ok()
                .unwrap_or_else(|| s0 + chrono::Duration::hours(2));
            let route: String = r.get("route");
            out.push(CalEvent{ id: format!("round-{}", r.get::<Uuid,_>("id")), source:"ronda",
                title: format!("Ronda · {route}"), color:"#9333ea".into(),
                start: s0, end: e0, user_id: Some(r.get("user_id")),
                user: r.try_get::<String,_>("email").ok(), link: None });
        }
    }
    out.sort_by_key(|e| e.start);
    out
}
/// GET /api/calendar/events?from&to[&all=1] — eventi del calendario condiviso (JSON per FullCalendar).
pub async fn calendar_events(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), &["workforce","interventi","giri","cantieri"]).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    let parse = |k:&str, def: chrono::DateTime<chrono::Utc>| q.get(k)
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok().map(|t| t.with_timezone(&chrono::Utc)))
        .unwrap_or(def);
    let now = st.clock.now();
    let from = parse("from", now - chrono::Duration::days(7));
    let to   = parse("to",   now + chrono::Duration::days(31));
    // ruoli ristretti (operatore/lavoratore/reperibile) o chi non pianifica: solo i propri eventi.
    let only_uid = if is_planner(&s) { None } else { Some(s.user_id) };
    let evs = calendar_collect(&st, tid, from, to, only_uid).await;
    Ok(Json(json!(evs.iter().map(cal_json).collect::<Vec<_>>())))
}
/// GET /api/calendar/my-url[?rotate=1] — URL iCal personale (aggrega tutti i moduli attivi).
pub async fn calendar_my_url(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap(); let uid = s.user_id;
    let rotate = q.get("rotate").map(|v| v=="1"||v=="true").unwrap_or(false);
    let existing: Option<String> = sqlx::query_scalar("SELECT token FROM app.calendar_ical_tokens WHERE tenant_id=$1 AND user_id=$2")
        .bind(tid).bind(uid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let token = match (existing, rotate) {
        (Some(t), false) => t,
        _ => { let t = auth::new_token();
            sqlx::query("INSERT INTO app.calendar_ical_tokens(tenant_id,user_id,token) VALUES($1,$2,$3) ON CONFLICT (tenant_id,user_id) DO UPDATE SET token=EXCLUDED.token,created_at=now()")
                .bind(tid).bind(uid).bind(&t).execute(&st.owner_pool).await.map_err(ise)?; t }
    };
    Ok(Json(json!({"url":format!("/api/calendar.ics?token={token}"),"token":token})))
}
#[derive(Deserialize)] pub struct CalIcs { pub token: String }
/// GET /api/calendar.ics?token= — feed iCal PERSONALE (senza sessione; token -> utente/tenant).
pub async fn calendar_ics(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<CalIcs>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT tenant_id,user_id FROM app.calendar_ical_tokens WHERE token=$1")
        .bind(&q.token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "token non valido").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let uid: Uuid = row.get("user_id");
    let now = st.clock.now();
    let evs = calendar_collect(&st, tid, now - chrono::Duration::days(7), now + chrono::Duration::days(60), Some(uid)).await;
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
    let esc_ics = |x: &str| x.replace('\\',"\\\\").replace('\n'," ").replace(',', "\\,").replace(';', "\\;");
    let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//YouWorkforce//Calendario//IT\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\nX-WR-CALNAME:YouWorkforce\r\n");
    for e in &evs {
        ics.push_str("BEGIN:VEVENT\r\n");
        ics.push_str(&format!("UID:{}@youworkforce\r\n", e.id));
        ics.push_str(&format!("DTSTAMP:{}\r\n", fmt(now)));
        ics.push_str(&format!("DTSTART:{}\r\n", fmt(e.start)));
        ics.push_str(&format!("DTEND:{}\r\n", fmt(e.end)));
        ics.push_str(&format!("SUMMARY:{}\r\n", esc_ics(&e.title)));
        ics.push_str("END:VEVENT\r\n");
    }
    ics.push_str("END:VCALENDAR\r\n");
    ([(axum::http::header::CONTENT_TYPE, "text/calendar; charset=utf-8")], ics).into_response()
}

// ============================================================================
// Policy eventi dei dispositivi GPS
// attributes, accumulators. Tutto pilotato da YouWorkforce (tenant_admin).
// ============================================================================


// ---- Event policy (ignora|registra|critico per tipo evento) ----
pub async fn gps_event_policy_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "gps").await?;
    let amap = tool_config(&st, tid, "gps", "alarm_map").await;
    let events = amap.get("events").cloned().unwrap_or_else(|| json!({}));
    // ritorna, per ogni tipo noto, handling corrente o default
    let mut out = serde_json::Map::new();
    for &et in GPS_POLICY_EVENTS {
        let (_, def) = gps_event_default(et);
        let cur = events.get(et).and_then(|v| v.as_str()).unwrap_or(def);
        out.insert(et.to_string(), json!(cur));
    }
    Ok(Json(json!({"events":out})))
}
#[derive(Deserialize)] pub struct EventPolicyReq { pub events: std::collections::HashMap<String,String> }
pub async fn gps_event_policy_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<EventPolicyReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    require_module(&st, tid, "gps").await?;
    // valida chiavi/valori
    for (k, v) in &b.events {
        if !GPS_POLICY_EVENTS.contains(&k.as_str()) { return Err(err(StatusCode::BAD_REQUEST, &format!("tipo evento non valido: {k}"))); }
        if !matches!(v.as_str(), "ignora"|"registra"|"critico") { return Err(err(StatusCode::BAD_REQUEST, &format!("handling non valido per {k}: {v}"))); }
    }
    // merge dentro la config esistente alarm_map.events (preserva sos_as/battery_as ecc.)
    let cur: Option<Value> = sqlx::query_scalar("SELECT config FROM app.tenant_module_config WHERE tenant_id=$1 AND module_code='gps' AND tool_code='alarm_map'")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let mut cfg = cur.and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let mut events = cfg.get("events").and_then(|v| v.as_object().cloned()).unwrap_or_default();
    for (k, v) in &b.events { events.insert(k.clone(), json!(v)); }
    cfg.insert("events".into(), Value::Object(events));
    let cfg_v = Value::Object(cfg);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.tenant_module_config(tenant_id,module_code,tool_code,config,updated_by) VALUES($1,'gps','alarm_map',$2,$3) ON CONFLICT (tenant_id,module_code,tool_code) DO UPDATE SET config=EXCLUDED.config,updated_by=EXCLUDED.updated_by,updated_at=now()")
        .bind(tid).bind(&cfg_v).bind(s.user_id).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "gps_event_policy", json!({"n":b.events.len()})).await;
    Ok(Json(json!({"ok":true,"updated":b.events.len()})))
}






// ============================================================================
// #8 Osservabilità: /metrics in formato Prometheus (aggregati, nessun dato per-tenant
// sensibile). Protezione opzionale via YW_METRICS_TOKEN (Bearer). GET -> CSRF-exempt.
// ============================================================================
pub async fn metrics(State(st): State<AppState>, h: HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Ok(tok) = std::env::var("YW_METRICS_TOKEN") {
        if !tok.is_empty() {
            let ok = h.get("authorization").and_then(|v| v.to_str().ok())
                .map(|s| s == format!("Bearer {tok}")).unwrap_or(false);
            if !ok { return (StatusCode::UNAUTHORIZED, "unauthorized").into_response(); }
        }
    }
    let c = |p: &sqlx::PgPool, sql: &'static str| {
        let p = p.clone();
        async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(&p).await.unwrap_or(0) }
    };
    let tenants   = c(&st.owner_pool, "SELECT count(*) FROM app.tenants").await;
    let users     = c(&st.owner_pool, "SELECT count(*) FROM app.users").await;
    let inc_open  = c(&st.owner_pool, "SELECT count(*) FROM app.incidents WHERE state<>'chiuso'").await;
    let inc_total = c(&st.owner_pool, "SELECT count(*) FROM app.incidents").await;
    let inc_crit  = c(&st.owner_pool, "SELECT count(*) FROM app.incidents WHERE state<>'chiuso' AND priority>=3").await;
    let ob_pending= c(&st.owner_pool, "SELECT count(*) FROM app.outbox WHERE state='pending'").await;
    let ob_error  = c(&st.owner_pool, "SELECT count(*) FROM app.outbox WHERE state='error'").await;
    let ob_stuck  = c(&st.owner_pool, "SELECT count(*) FROM app.outbox WHERE state='pending' AND attempts>=5").await;
    let sess      = c(&st.owner_pool, "SELECT count(*) FROM app.sessions WHERE expires_at>now()").await;
    let ck_active = c(&st.owner_pool, "SELECT count(*) FROM app.checkins WHERE state='attivo'").await;
    let db_ok: i64 = if sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&st.owner_pool).await.is_ok() {1} else {0};
    let ver = env!("CARGO_PKG_VERSION");
    let body = format!(
"# HELP yw_build_info Build info.
# TYPE yw_build_info gauge
yw_build_info{{version=\"{ver}\"}} 1
# HELP yw_up Core process health (1=db reachable).
# TYPE yw_up gauge
yw_up {db_ok}
# HELP yw_tenants_total Number of tenants.
# TYPE yw_tenants_total gauge
yw_tenants_total {tenants}
# HELP yw_users_total Number of users.
# TYPE yw_users_total gauge
yw_users_total {users}
# HELP yw_incidents_open Open incidents (state != chiuso).
# TYPE yw_incidents_open gauge
yw_incidents_open {inc_open}
# HELP yw_incidents_critical_open Open incidents with priority >= 3.
# TYPE yw_incidents_critical_open gauge
yw_incidents_critical_open {inc_crit}
# HELP yw_incidents_total Incidents ever created.
# TYPE yw_incidents_total counter
yw_incidents_total {inc_total}
# HELP yw_outbox_pending Outbox messages waiting delivery.
# TYPE yw_outbox_pending gauge
yw_outbox_pending {ob_pending}
# HELP yw_outbox_stuck Pending outbox messages retried >= 5 times.
# TYPE yw_outbox_stuck gauge
yw_outbox_stuck {ob_stuck}
# HELP yw_outbox_error Outbox messages in error state (non-critical dropped).
# TYPE yw_outbox_error gauge
yw_outbox_error {ob_error}
# HELP yw_sessions_active Active (non-expired) sessions.
# TYPE yw_sessions_active gauge
yw_sessions_active {sess}
# HELP yw_checkins_active Active check-ins.
# TYPE yw_checkins_active gauge
yw_checkins_active {ck_active}
");
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body).into_response()
}

// ============================================================================
// #7 GDPR minimo: export dati personali (diritto d'accesso) + anonimizzazione
// (diritto all'oblio, preservando integrità storica) + ritenzione (worker).
// Solo tenant_admin. Audit di ogni operazione.
// ============================================================================


// ============================================================================
// Motore di escalation nativo: calcolo del reperibile di turno (crate::escalation) dalla
// configurazione dei gruppi. Il motore esterno è stato ritirato: questo è il motore in produzione.
// ============================================================================

/// RotationType nativo dal tipo on-call del gruppo. Monthly ora è calendar-aware (nessuna approssimazione).
pub(crate) fn esc_rot_kind(oc_type: &str) -> (crate::escalation::RotationType, i64) {
    use crate::escalation::RotationType as RT;
    match oc_type {
        "hourly" => (RT::Hourly, 1),
        "daily"  => (RT::Daily, 1),
        "monthly"=> (RT::Monthly, 1),
        _        => (RT::Weekly, 1),
    }
}
/// Costruisce la Schedule nativa dalla config del gruppo (stessi input della sync nativa).
/// Gli override (assenze) sono innestati a valle da [`esc_absence_overrides`] (qui `overrides`
/// parte vuoto: la lettura delle assenze è async e non appartiene a questa fn pura).
pub(crate) fn esc_build_schedule(g: &sqlx::postgres::PgRow, members: &[Uuid]) -> crate::escalation::Schedule {
    let mode: String = g.try_get("oncall_mode").unwrap_or_else(|_| "tutti".into());
    let coverage: String = g.try_get("coverage_mode").unwrap_or_else(|_| "24_7".into());
    // fuso del gruppo (oncall_tz); i boundary rotation e le fasce di copertura sono in ora locale.
    let tz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
    let rotation = if mode == "turni" {
        let oc_type: String = g.try_get("oncall_type").unwrap_or_else(|_| "weekly".into());
        let oc_shift: i32 = g.try_get("oncall_shift").unwrap_or(1);
        let (kind, mul) = esc_rot_kind(&oc_type);
        let ht: Option<String> = g.try_get("handoff_time").ok().flatten();
        let hd: Option<i32> = g.try_get("handoff_day").ok().flatten();
        // ancora handoff come ora LOCALE nel fuso del gruppo, poi convertita in UTC.
        use chrono::TimeZone as _;
        let naive = chrono::NaiveDateTime::parse_from_str(&rotation_start(ht.as_deref(), hd), "%Y-%m-%dT%H:%M:%SZ")
            .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(2024,1,1).unwrap().and_hms_opt(0,0,0).unwrap());
        let start = match tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(d) => d.with_timezone(&chrono::Utc),
            chrono::LocalResult::Ambiguous(a, _) => a.with_timezone(&chrono::Utc),
            chrono::LocalResult::None => { use chrono::TimeZone; chrono::Utc.from_utc_datetime(&naive) },
        };
        Some(crate::escalation::Rotation{ kind, shift_len: (oc_shift as i64).max(1) * mul, start, members: members.to_vec(), tz })
    } else { None };
    let fixed_users = if mode == "turni" { vec![] } else { members.to_vec() };
    let mut rules = vec![];
    if coverage == "orari" {
        let cs: Option<String> = g.try_get("cov_start").ok().flatten();
        let ce: Option<String> = g.try_get("cov_end").ok().flatten();
        let cd: String = g.try_get("cov_days").unwrap_or_else(|_| "1111111".into());
        let mins = |s: &str| parse_hhmm(s).map(|(h,m)| h*60+m);
        if let (Some(a), Some(b)) = (cs.as_deref().and_then(mins), ce.as_deref().and_then(mins)) {
            let mut mask: u8 = 0;
            for (i,c) in cd.chars().take(7).enumerate() { if c=='1' { mask |= 1 << i; } }
            rules.push(crate::escalation::ScheduleRule{ start_min: a, end_min: b, weekday_mask: mask, tz });
        }
    }
    crate::escalation::Schedule{ rotation, fixed_users, rules, overrides: vec![] }
}

/// Uuid sintetico deterministico da un'email (per un sostituto non membro del gruppo: serve
/// solo al confronto per-email dello shadow, non è un utente reale).
pub(crate) fn esc_synth_uuid(email: &str) -> Uuid {
    use std::hash::{Hash, Hasher};
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    email.hash(&mut h1); let a = h1.finish();
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    ("yw-esc-synth", email).hash(&mut h2); let b = h2.finish();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_le_bytes());
    bytes[8..].copy_from_slice(&b.to_le_bytes());
    Uuid::from_bytes(bytes)
}

/// Risolve il membro on-call della rotazione con STATO persistente (`esc_rotation_state`):
/// carica/semina lo stato, lo avanza fino a `now`, lo persiste, ritorna il membro.
/// `ga` (indice di partenza esterno) è un residuo del motore ritirato: oggi è sempre vuoto.
pub(crate) async fn esc_resolve_rot_member(
    st: &AppState, tid: Uuid, gid: Uuid, rot: &crate::escalation::Rotation,
    members: &[Uuid], email_of: &std::collections::HashMap<Uuid,String>, ga: &[String],
    now: chrono::DateTime<chrono::Utc>, allow_native_seed: bool, read_transient: bool,
) -> Option<Uuid> {
    use crate::escalation::RotationState;
    let row = sqlx::query("SELECT position,shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let mut transient = false; // calcolato ma NON persistito (letture: on-call-now, slack, status, bundle)
    let state = match row {
        Some(r) => RotationState{ position: r.get::<i32,_>("position").max(0) as usize, shift_start: r.get("shift_start") },
        None => {
            // SEMINA (prima osservazione):
            //  - in CONSEGNA (allow_native_seed=true) parte dalla posizione 0 e PERSISTE;
            //  - in LETTURA (read_transient=true) calcola la posizione 0 ma NON persiste (una GET non
            //    deve mutare lo stato né anticipare la semina fatta dalla consegna);
            //  - altrimenti ritorna None.
            let ga_idx = ga.first().and_then(|e| members.iter().position(|u| email_of.get(u).map(|x| x==e).unwrap_or(false)));
            let (idx, src) = match ga_idx {
                Some(i) => (i, "seed"),
                None if allow_native_seed => (0usize, "native"),
                None if read_transient => { transient = true; (0usize, "native") },
                None => return None,
            };
            let seeded = RotationState{ position: idx, shift_start: rot.start_time(now) };
            if !transient {
                let _ = sqlx::query("INSERT INTO app.esc_rotation_state(tenant_id,group_id,position,shift_start,seeded_from) VALUES($1,$2,$3,$4,$5) ON CONFLICT (tenant_id,group_id) DO NOTHING")
                    .bind(tid).bind(gid).bind(seeded.position as i32).bind(seeded.shift_start).bind(src).execute(&st.owner_pool).await;
            }
            seeded
        }
    };
    if transient { return state.advanced_to(rot, now, members.len()).active_member(members); }
    let adv = state.advanced_to(rot, now, members.len());
    if adv != state {
        let _ = sqlx::query("UPDATE app.esc_rotation_state SET position=$3,shift_start=$4,updated_at=now() WHERE tenant_id=$1 AND group_id=$2")
            .bind(tid).bind(gid).bind(adv.position as i32).bind(adv.shift_start).execute(&st.owner_pool).await;
    }
    adv.active_member(members)
}

// ============================================================================
// Escalation nativa: consegna (SMS/voce/email o sink di prova)
// ============================================================================

/// On-call nativo di un gruppo a `now`, con i recapiti per la ladder. SELF-SUFFICIENT:
/// gli override/assenze sono quelli NATIVI (app.esc_overrides); la rotation è seminata
/// nativamente (posizione 0 deterministica).
/// `_ga` è un parametro non più usato (sempre None), mantenuto nella firma per compat dei chiamanti.
/// `seed=true` SOLO nella consegna (persiste la semina rotation). Le letture (on-call-now, slack,
/// status-update, bundle) passano `seed=false`: calcolano l'on-call senza MAI mutare lo stato.
pub(crate) async fn esc_native_oncall_contacts(
    st: &AppState, tid: Uuid, gid: Uuid, _ga: Option<(&str, &str)>, now: chrono::DateTime<chrono::Utc>, seed: bool,
) -> Vec<(Uuid, Option<String>, String)> {
    let Some(g) = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() else { return vec![]; };
    let gtz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok().and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
    let mrows = sqlx::query("SELECT u.id,u.email,u.phone FROM app.alarm_group_members m JOIN app.users u ON u.id=m.user_id WHERE m.group_id=$1 ORDER BY m.position")
        .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    // NB: il gruppo può non avere membri base ma avere assegnazioni (coperture aggiuntive).
    let members: Vec<Uuid> = mrows.iter().map(|r| r.get::<Uuid,_>("id")).collect();
    let email_of: std::collections::HashMap<Uuid,String> = mrows.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email").to_lowercase())).collect();
    let phone_of: std::collections::HashMap<Uuid,Option<String>> = mrows.iter().map(|r| (r.get::<Uuid,_>("id"), r.try_get::<String,_>("phone").ok())).collect();
    // TEMPORARY SCHEDULE: se attiva, SOSTITUISCE tutto (solo il blocco corrente è on-call).
    if let Some(tu) = esc_temp_schedule_oncall(st, tid, gid, now).await {
        let (email, phone) = match email_of.get(&tu) {
            Some(em) => (Some(em.clone()), phone_of.get(&tu).cloned().flatten()),
            None => { // utente non membro del gruppo: recapiti dal profilo utente
                let row = sqlx::query("SELECT email,phone FROM app.users WHERE id=$1 AND tenant_id=$2")
                    .bind(tu).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
                match row { Some(r) => (r.try_get::<String,_>("email").ok(), r.try_get::<String,_>("phone").ok()), None => (None, None) }
            }
        };
        return match email { Some(em) => vec![(tu, phone, em)], None => vec![] };
    }
    let mut sched = esc_build_schedule(&g, &members);
    // override/assenze NATIVE
    sched.overrides = esc_native_overrides(st, tid, gid, &email_of).await;
    let rot_member = match &sched.rotation {
        Some(rot) => {
            // seed rotation NATIVO.
            let gae: Vec<String> = vec![];
            // consegna (seed): semina+persiste; lettura (!seed): calcolo transiente senza persistere.
            esc_resolve_rot_member(st, tid, gid, rot, &members, &email_of, &gae, now, seed, !seed).await
        }
        None => None,
    };
    let mut uids = sched.on_call_at_rot(now, rot_member);
    // G-A: unione con le assegnazioni multiple (coperture aggiuntive attive adesso).
    for u in esc_assignments_oncall(st, tid, gid, now, gtz).await { if !uids.contains(&u) { uids.push(u); } }
    if uids.is_empty() { return vec![]; }
    // recapiti per TUTTI gli on-call (membri base + membri delle assegnazioni, anche fuori gruppo).
    let crows = sqlx::query("SELECT id,email,phone FROM app.users WHERE id = ANY($1) AND tenant_id=$2")
        .bind(&uids).bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let cmap: std::collections::HashMap<Uuid,(Option<String>,String)> = crows.iter().map(|r|
        (r.get::<Uuid,_>("id"), (r.try_get::<String,_>("phone").ok(), r.get::<String,_>("email")))).collect();
    uids.into_iter().filter_map(|u| {
        let (phone, email) = cmap.get(&u)?.clone();
        Some((u, phone, email))
    }).collect()
}

/// G-B — "Shifts": turni calcolati del gruppo (final schedule) su una finestra. Campiona il resolver
/// (rotation stateful PURA + override + copertura + temporary schedule) a passo fisso e fonde i
/// segmenti con lo stesso on-call. Nessuna mutazione di stato (letture pure).
pub(crate) async fn esc_group_shifts(st: &AppState, tid: Uuid, gid: Uuid, now: chrono::DateTime<chrono::Utc>, days: i64)
    -> Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Vec<String>)> {
    use crate::escalation::RotationState;
    let Some(g) = sqlx::query("SELECT id,name,oncall_mode,oncall_type,oncall_shift,oncall_tz,coverage_mode,cov_start,cov_end,cov_days,handoff_time,handoff_day FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() else { return vec![]; };
    let mrows = sqlx::query("SELECT u.id,u.email FROM app.alarm_group_members m JOIN app.users u ON u.id=m.user_id WHERE m.group_id=$1 ORDER BY m.position")
        .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let members: Vec<Uuid> = mrows.iter().map(|r| r.get::<Uuid,_>("id")).collect();
    let email_of: std::collections::HashMap<Uuid,String> = mrows.iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email").to_lowercase())).collect();
    let gtz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok().and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
    // email dei membri delle assegnazioni (possono non essere membri del gruppo)
    let ass_email: std::collections::HashMap<Uuid,String> = sqlx::query(
        "SELECT DISTINCT u.id,u.email FROM app.esc_assignment_members am
           JOIN app.esc_assignments a ON a.id=am.assignment_id
           JOIN app.users u ON u.id=am.user_id
          WHERE a.tenant_id=$1 AND a.group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default()
        .iter().map(|r| (r.get::<Uuid,_>("id"), r.get::<String,_>("email").to_lowercase())).collect();
    let mut sched = esc_build_schedule(&g, &members);
    sched.overrides = esc_native_overrides(st, tid, gid, &email_of).await;
    let base_state: Option<RotationState> = match &sched.rotation {
        Some(rot) => Some(
            sqlx::query("SELECT position,shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
                .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten()
                .map(|r| RotationState{ position: r.get::<i32,_>("position").max(0) as usize, shift_start: r.get("shift_start") })
                .unwrap_or(RotationState{ position:0, shift_start: rot.start_time(now) })),
        None => None,
    };
    // PRELOAD una-tantum (niente query nel loop di campionamento): temporary schedules + assegnazioni.
    let temps: Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Vec<(Uuid,f64)>)> =
        sqlx::query("SELECT start_at,end_at,blocks FROM app.esc_temp_schedules WHERE tenant_id=$1 AND group_id=$2")
            .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default()
            .iter().map(|r| {
                let blocks: Value = r.try_get("blocks").unwrap_or_else(|_| json!([]));
                let bl = blocks.as_array().map(|a| a.iter().filter_map(|b| {
                    let u = b.get("user_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok())?;
                    Some((u, b.get("hours").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0)))
                }).collect()).unwrap_or_default();
                (r.get("start_at"), r.get("end_at"), bl)
            }).collect();
    struct AsgM { mask: u8, sm: u32, em: u32, rot: Option<crate::escalation::Rotation>, members: Vec<Uuid> }
    let mut asgs: Vec<AsgM> = vec![];
    for a in sqlx::query("SELECT id,cov_days,cov_start,cov_end,mode,oncall_type,oncall_shift,handoff_time,handoff_day FROM app.esc_assignments WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default().iter() {
        let aid: Uuid = a.get("id");
        let amem: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM app.esc_assignment_members WHERE assignment_id=$1 ORDER BY position")
            .bind(aid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        let cd: String = a.try_get("cov_days").unwrap_or_else(|_| "1111111".into());
        let mut mask: u8 = 0; for (i,c) in cd.chars().take(7).enumerate() { if c=='1' { mask |= 1 << i; } }
        let (sm, em) = match (a.try_get::<Option<String>,_>("cov_start").ok().flatten().as_deref().and_then(parse_hhmm),
                              a.try_get::<Option<String>,_>("cov_end").ok().flatten().as_deref().and_then(parse_hhmm)) {
            (Some((h1,m1)),Some((h2,m2))) => (h1*60+m1, h2*60+m2), _ => (0u32,1440u32) };
        let rot = if a.try_get::<String,_>("mode").unwrap_or_else(|_|"tutti".into())=="turni" && !amem.is_empty() {
            use chrono::TimeZone as _;
            let (kind,mul)=esc_rot_kind(&a.try_get::<String,_>("oncall_type").unwrap_or_else(|_|"weekly".into()));
            let naive = chrono::NaiveDateTime::parse_from_str(&rotation_start(a.try_get::<Option<String>,_>("handoff_time").ok().flatten().as_deref(), a.try_get("handoff_day").ok().flatten()), "%Y-%m-%dT%H:%M:%SZ").unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(2024,1,1).unwrap().and_hms_opt(0,0,0).unwrap());
            let start = match gtz.from_local_datetime(&naive){chrono::LocalResult::Single(d)=>d.with_timezone(&chrono::Utc),chrono::LocalResult::Ambiguous(d,_)=>d.with_timezone(&chrono::Utc),chrono::LocalResult::None=>chrono::Utc.from_utc_datetime(&naive)};
            Some(crate::escalation::Rotation{ kind, shift_len:(a.try_get::<i32,_>("oncall_shift").unwrap_or(1) as i64).max(1)*mul, start, members:amem.clone(), tz:gtz })
        } else { None };
        asgs.push(AsgM{ mask, sm, em, rot, members: amem });
    }
    let temp_at = |t: chrono::DateTime<chrono::Utc>| -> Option<Uuid> {
        let cur = temps.iter().filter(|(s,e,_)| *s<=t && *e>t).max_by_key(|(s,_,_)| *s)?;
        let mut cursor = cur.0;
        for (u,h) in &cur.2 { let bend = cursor + chrono::Duration::milliseconds((h*3_600_000.0) as i64); if t>=cursor && t<bend { return Some(*u); } cursor = bend; }
        None
    };
    let horizon = now + chrono::Duration::days(days.clamp(1, 60));
    let step = chrono::Duration::minutes(30);
    let mut out: Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Vec<String>)> = vec![];
    let mut seg_start = now;
    let mut prev: Option<Vec<String>> = None;
    let mut t = now;
    let mut iters = 0;
    while t <= horizon && iters < 5000 {
        iters += 1;
        let mut emails: Vec<String> = if let Some(tu) = temp_at(t) {
            email_of.get(&tu).cloned().or_else(|| ass_email.get(&tu).cloned()).into_iter().collect()
        } else {
            let rot_member = match (&sched.rotation, base_state) {
                (Some(rot), Some(bs)) if !members.is_empty() => bs.advanced_to(rot, t, members.len()).active_member(&members),
                _ => None,
            };
            let mut uids = sched.on_call_at_rot(t, rot_member);
            // assegnazioni in-memory
            for a in &asgs {
                if !(crate::escalation::ScheduleRule{ start_min:a.sm, end_min:a.em, weekday_mask:a.mask, tz:gtz }).covers(t) { continue; }
                match &a.rot {
                    Some(rot) => { if let Some(u)=rot.active_at(t) { if !uids.contains(&u) { uids.push(u); } } }
                    None => { for u in &a.members { if !uids.contains(u) { uids.push(*u); } } }
                }
            }
            uids.iter().filter_map(|u| email_of.get(u).cloned().or_else(|| ass_email.get(u).cloned())).collect()
        };
        emails.sort(); emails.dedup();
        if prev.as_ref() != Some(&emails) {
            if let Some(p) = prev.take() { out.push((seg_start, t, p)); }
            seg_start = t; prev = Some(emails);
        }
        t += step;
    }
    if let Some(p) = prev.take() { out.push((seg_start, horizon, p)); }
    out
}
pub async fn group_shifts(State(st): State<AppState>, h: HeaderMap, Path(gid): Path<Uuid>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let days: i64 = q.get("days").and_then(|d| d.parse().ok()).unwrap_or(30);
    let now = st.clock.now();
    let segs = esc_group_shifts(&st, tid, gid, now, days).await;
    let out: Vec<Value> = segs.iter().filter(|(_,_,e)| !e.is_empty()).map(|(s0,e0,em)| json!({
        "start":s0,"end":e0,"oncall":em})).collect();
    Ok(Json(json!({"shifts":out,"days":days})))
}

/// G-A — on-call aggiuntivo dalle ASSEGNAZIONI del gruppo: ogni assegnazione aggiunge i suoi membri
/// quando la sua finestra (giorni + orario, ora locale `tz`) copre `t`. Ritorna gli uuid in unione.
pub(crate) async fn esc_assignments_oncall(st: &AppState, tid: Uuid, gid: Uuid, t: chrono::DateTime<chrono::Utc>, tz: chrono_tz::Tz) -> Vec<Uuid> {
    let rows = sqlx::query("SELECT id,cov_days,cov_start,cov_end,mode,oncall_type,oncall_shift,handoff_time,handoff_day FROM app.esc_assignments WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut out: Vec<Uuid> = vec![];
    for a in &rows {
        let aid: Uuid = a.get("id");
        let cd: String = a.try_get("cov_days").unwrap_or_else(|_| "1111111".into());
        let cs: Option<String> = a.try_get("cov_start").ok().flatten();
        let ce: Option<String> = a.try_get("cov_end").ok().flatten();
        let mut mask: u8 = 0;
        for (i,c) in cd.chars().take(7).enumerate() { if c=='1' { mask |= 1 << i; } }
        let (sm, em) = match (cs.as_deref().and_then(parse_hhmm), ce.as_deref().and_then(parse_hhmm)) {
            (Some((h1,m1)),Some((h2,m2))) => (h1*60+m1, h2*60+m2),
            _ => (0u32, 1440u32),
        };
        if !(crate::escalation::ScheduleRule{ start_min:sm, end_min:em, weekday_mask:mask, tz }).covers(t) { continue; }
        let mem: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM app.esc_assignment_members WHERE assignment_id=$1 ORDER BY position")
            .bind(aid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        let mode: String = a.try_get("mode").unwrap_or_else(|_| "tutti".into());
        if mode == "turni" && !mem.is_empty() {
            // rotazione dell'assegnazione (nativa): membro corrente via formula pura (deterministica,
            // ancorata all'handoff locale). Niente stato separato: è una feature native-only.
            use chrono::TimeZone as _;
            let oc_type: String = a.try_get("oncall_type").unwrap_or_else(|_| "weekly".into());
            let oc_shift: i32 = a.try_get("oncall_shift").unwrap_or(1);
            let (kind, mul) = esc_rot_kind(&oc_type);
            let ht: Option<String> = a.try_get("handoff_time").ok().flatten();
            let hd: Option<i32> = a.try_get("handoff_day").ok().flatten();
            let naive = chrono::NaiveDateTime::parse_from_str(&rotation_start(ht.as_deref(), hd), "%Y-%m-%dT%H:%M:%SZ")
                .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(2024,1,1).unwrap().and_hms_opt(0,0,0).unwrap());
            let start = match tz.from_local_datetime(&naive) { chrono::LocalResult::Single(d)=>d.with_timezone(&chrono::Utc), chrono::LocalResult::Ambiguous(d,_)=>d.with_timezone(&chrono::Utc), chrono::LocalResult::None=>chrono::Utc.from_utc_datetime(&naive) };
            let rot = crate::escalation::Rotation{ kind, shift_len:(oc_shift as i64).max(1)*mul, start, members: mem.clone(), tz };
            if let Some(u) = rot.active_at(t) { if !out.contains(&u) { out.push(u); } }
        } else {
            for u in mem { if !out.contains(&u) { out.push(u); } }
        }
    }
    out
}

/// Override/assenze native di un gruppo (app.esc_overrides). I membri "remove/add" sono utenti
/// locali risolti per uuid; l'email serve al resolver per la proiezione on-call.
pub(crate) async fn esc_native_overrides(
    st: &AppState, tid: Uuid, gid: Uuid, email_of: &std::collections::HashMap<Uuid,String>,
) -> Vec<crate::escalation::Override> {
    let rows = sqlx::query("SELECT start_at,end_at,add_user,remove_user FROM app.esc_overrides WHERE tenant_id=$1 AND group_id=$2")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let _ = email_of; // i membri sono già locali (uuid); nessuna sintesi necessaria
    rows.iter().filter_map(|r| {
        let start: chrono::DateTime<chrono::Utc> = r.get("start_at");
        let end: chrono::DateTime<chrono::Utc> = r.get("end_at");
        let add: Option<Uuid> = r.try_get("add_user").ok().flatten();
        let remove: Option<Uuid> = r.try_get("remove_user").ok().flatten();
        if add.is_none() && remove.is_none() { return None; }
        Some(crate::escalation::Override { start, end, add, remove })
    }).collect()
}

/// Temporary schedule attivo: se un gruppo ha una pianificazione temporanea che
/// copre `now`, essa SOSTITUISCE le assegnazioni: on-call = l'utente del blocco corrente (blocchi
/// sequenziali {user_id, hours} a partire da start_at). None se nessuna temp schedule attiva.
pub(crate) async fn esc_temp_schedule_oncall(
    st: &AppState, tid: Uuid, gid: Uuid, now: chrono::DateTime<chrono::Utc>,
) -> Option<Uuid> {
    let row = sqlx::query("SELECT start_at,blocks FROM app.esc_temp_schedules WHERE tenant_id=$1 AND group_id=$2 AND start_at<=$3 AND end_at>$3 ORDER BY start_at DESC LIMIT 1")
        .bind(tid).bind(gid).bind(now).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let start: chrono::DateTime<chrono::Utc> = row.get("start_at");
    let blocks: Value = row.try_get("blocks").unwrap_or_else(|_| json!([]));
    let arr = blocks.as_array()?;
    let mut cursor = start;
    for b in arr {
        let hours = b.get("hours").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
        let bend = cursor + chrono::Duration::milliseconds((hours * 3_600_000.0) as i64);
        if now >= cursor && now < bend {
            return b.get("user_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok());
        }
        cursor = bend;
    }
    None
}

/// Turni di rotazione CALCOLATI di un utente (motore nativo calendar): per ogni gruppo 'turni'
/// di cui è membro, cammina i boundary della rotation su `days` giorni e ritorna i segmenti in cui
/// l'utente è on-call. Usa lo stato persistente (esc_rotation_state) se presente, altrimenti semina
/// nativamente alla posizione 0.
pub(crate) async fn esc_user_rotation_shifts(
    st: &AppState, tid: Uuid, uid: Uuid, now: chrono::DateTime<chrono::Utc>, days: i64,
) -> Vec<(String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    esc_user_rotation_shifts_g(st, tid, uid, now, days, None).await
}
/// Come sopra, con filtro opzionale al singolo gruppo (sottoscrizione per-schedule self-service).
pub(crate) async fn esc_user_rotation_shifts_g(
    st: &AppState, tid: Uuid, uid: Uuid, now: chrono::DateTime<chrono::Utc>, days: i64, group: Option<Uuid>,
) -> Vec<(String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    use crate::escalation::RotationState;
    let mut out = vec![];
    let groups = sqlx::query("SELECT g.id,g.name,g.oncall_mode,g.oncall_type,g.oncall_shift,g.coverage_mode,g.cov_start,g.cov_end,g.cov_days,g.handoff_time,g.handoff_day
         FROM app.alarm_groups g JOIN app.alarm_group_members m ON m.group_id=g.id
        WHERE g.tenant_id=$1 AND m.user_id=$2 AND g.oncall_mode='turni' AND ($3::uuid IS NULL OR g.id=$3)")
        .bind(tid).bind(uid).bind(group).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let horizon = now + chrono::Duration::days(days);
    for g in &groups {
        let gid: Uuid = g.get("id");
        let name: String = g.get("name");
        let mrows = sqlx::query("SELECT user_id FROM app.alarm_group_members WHERE group_id=$1 ORDER BY position")
            .bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        let members: Vec<Uuid> = mrows.iter().map(|r| r.get::<Uuid,_>("user_id")).collect();
        if members.is_empty() { continue; }
        let sched = esc_build_schedule(g, &members);
        let Some(rot) = sched.rotation.as_ref() else { continue };
        let count = members.len();
        // stato base: persistito o seminato nativo
        let base = sqlx::query("SELECT position,shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
            .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten()
            .map(|r| RotationState{ position: r.get::<i32,_>("position").max(0) as usize, shift_start: r.get("shift_start") })
            .unwrap_or_else(|| RotationState{ position: 0, shift_start: rot.start_time(now) });
        let mut stt = base.advanced_to(rot, now, count);
        let mut seg_start = stt.shift_start;
        for _ in 0..400 {
            let seg_end = rot.end_time(seg_start);
            if stt.active_member(&members) == Some(uid) && seg_end > now {
                out.push((name.clone(), seg_start.max(now), seg_end));
            }
            if seg_end > horizon { break; }
            seg_start = seg_end;
            stt = RotationState{ position: (stt.position + 1) % count, shift_start: seg_end };
        }
    }
    out
}

/// Invio SMS nativo one-shot (verifica/test recapiti), riusando gli adapter di delivery:
/// Twilio LIVE se configurato, altrimenti sink/dry (dev).
/// SMS a un lavoratore (preavviso del silenzio dell'app): stesso canale nativo dell'escalation.
pub async fn send_worker_sms(st: &AppState, to: &str, text: &str) -> &'static str { esc_send_sms_native(st, to, text).await.state }
pub(crate) async fn esc_send_sms_native(st: &AppState, to: &str, text: &str) -> crate::delivery::SinkResult {
    use crate::delivery::Channel;
    let from = crate::delivery::chan("YW_ESC_FROM").unwrap_or_else(|| "+10000000000".into());
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    let relay = crate::delivery::chan("YW_ESC_SMTP_RELAY_URL").unwrap_or_default();
    let slack_token = crate::delivery::chan("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    let public_base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let live_cfg = crate::delivery::TwilioCfg::from_env();
    let live_on = live_cfg.is_some();
    let body = crate::delivery::request_body(Channel::Sms, &from, to, text, "verify", 0);
    esc_dispatch(st, Channel::Sms, to, text, &body, live_on, live_cfg.as_ref(), &slack_token, &relay, &sink, &public_base).await
}

/// EscStatus nativo dallo stato dell'incidente (presa in carico/chiusura = ferma l'escalation).
pub(crate) fn esc_status_from_incident(state: &str) -> crate::escalation::EscStatus {
    use crate::escalation::EscStatus as S;
    match state { "chiuso" => S::Closed, "preso_in_carico" | "intervento" => S::Acked, _ => S::Unacked }
}

/// Timeline canali della ladder a livello tenant (SMS @0, VOICE dopo `voice_fallback_min`).
/// Ritorna (policy per `tick`, canale di ogni step). repeat=0 in FASE 2 (nessuna ripetizione ancora).
pub(crate) fn esc_delivery_policy(voice_on: bool, voice_min: i64, repeat: i64) -> (crate::escalation::EscPolicy, Vec<crate::delivery::Channel>) {
    use crate::escalation::{EscPolicy, EscStep};
    use crate::delivery::Channel;
    let mut steps = vec![]; let mut chans = vec![];
    if voice_on {
        // SMS, poi VOICE: lo step SMS "aspetta" voice_min prima di avanzare al VOICE.
        steps.push(EscStep{ delay_s: voice_min.max(0)*60, targets: vec![] }); chans.push(Channel::Sms);
        steps.push(EscStep{ delay_s: 0, targets: vec![] }); chans.push(Channel::Voice);
    } else {
        steps.push(EscStep{ delay_s: 0, targets: vec![] }); chans.push(Channel::Sms);
    }
    (EscPolicy{ steps, repeat: repeat.max(0) as u32 }, chans)
}

pub(crate) fn esc_channel_from_str(s: &str) -> crate::delivery::Channel {
    use crate::delivery::Channel;
    match s { "voice"=>Channel::Voice, "email"=>Channel::Email, "slack"=>Channel::Slack, _=>Channel::Sms }
}

/// Policy di consegna di un GRUPPO. se il gruppo ha step personalizzati
/// (app.esc_policy_steps) usa quelli (sequenza arbitraria canale+attesa + repeat); altrimenti
/// torna alla ladder derivata dalle impostazioni vocali del tenant (retro-compatibile).
/// Target di uno step di policy: canale + CHI notificare.
#[derive(Clone)]
pub(crate) struct EscStepTarget {
    channel: crate::delivery::Channel,
    kind: String,                 // oncall|group|user|slack
    group: Option<Uuid>,
    user: Option<Uuid>,
    slack: Option<String>,
}
pub(crate) async fn esc_group_policy(
    st: &AppState, tid: Uuid, gid: Uuid, voice_on: bool, voice_min: i64, repeat_tenant: i64,
) -> (crate::escalation::EscPolicy, Vec<EscStepTarget>) {
    use crate::escalation::{EscPolicy, EscStep};
    let rows = sqlx::query("SELECT delay_s,channel,target_kind,target_group_id,target_user_id,target_slack FROM app.esc_policy_steps WHERE tenant_id=$1 AND group_id=$2 ORDER BY idx")
        .bind(tid).bind(gid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if rows.is_empty() {
        // ladder derivata: tutti gli step verso l'on-call del gruppo (kind 'oncall').
        let (pol, chans) = esc_delivery_policy(voice_on, voice_min, repeat_tenant);
        let tgts = chans.into_iter().map(|c| EscStepTarget{ channel:c, kind:"oncall".into(), group:None, user:None, slack:None }).collect();
        return (pol, tgts);
    }
    let grp_repeat: Option<i32> = sqlx::query_scalar("SELECT esc_repeat FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
        .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let repeat = grp_repeat.map(|r| r as i64).unwrap_or(repeat_tenant).max(0) as u32;
    let mut steps = vec![]; let mut tgts = vec![];
    for r in &rows {
        let d: i32 = r.get("delay_s");
        let ch: String = r.get("channel");
        let kind: String = r.try_get("target_kind").unwrap_or_else(|_| "oncall".into());
        steps.push(EscStep{ delay_s: d.max(0) as i64, targets: vec![] });
        tgts.push(EscStepTarget{
            channel: esc_channel_from_str(&ch), kind,
            group: r.try_get("target_group_id").ok().flatten(),
            user: r.try_get("target_user_id").ok().flatten(),
            slack: r.try_get("target_slack").ok().flatten(),
        });
    }
    (EscPolicy{ steps, repeat }, tgts)
}
/// Recipienti di uno step in base al suo target. Per 'slack' ritorna vuoto + il canale Slack.
pub(crate) async fn esc_step_recipients(
    st: &AppState, tid: Uuid, inc_gid: Uuid, spec: &EscStepTarget, ga: Option<(&str,&str)>,
    now: chrono::DateTime<chrono::Utc>,
) -> (Vec<(Uuid, Option<String>, String)>, Option<String>) {
    match spec.kind.as_str() {
        "group" => match spec.group {
            Some(g) => (esc_native_oncall_contacts(st, tid, g, ga, now, false).await, None),
            None => (vec![], None),
        },
        "user" => match spec.user {
            Some(u) => {
                let row = sqlx::query("SELECT email,phone FROM app.users WHERE id=$1 AND tenant_id=$2")
                    .bind(u).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
                match row {
                    Some(r) => { let em: Option<String> = r.try_get("email").ok();
                        let ph: Option<String> = r.try_get("phone").ok();
                        match em { Some(e) => (vec![(u, ph, e)], None), None => (vec![], None) } }
                    None => (vec![], None),
                }
            }
            None => (vec![], None),
        },
        "slack" => (vec![], spec.slack.clone().filter(|s| !s.trim().is_empty())),
        _ => (esc_native_oncall_contacts(st, tid, inc_gid, ga, now, true).await, None), // oncall
    }
}

pub(crate) fn esc_channel_from_kind(k: &str) -> crate::delivery::Channel {
    use crate::delivery::Channel;
    match k { "voice"=>Channel::Voice, "email"=>Channel::Email, _=>Channel::Sms }
}

/// Parametri di trasporto della consegna, raggruppati per non esplodere le firme.
pub(crate) struct EscTransport<'a> {
    from: &'a str, sink: &'a str, relay: &'a str, slack_token: &'a str, public_base: &'a str,
    live_on: bool, live_cfg: Option<&'a crate::delivery::TwilioCfg>,
}

/// Consegna le rung DOVUTE di uno step per gli on-call. Ladder a 2 livelli:
/// - se l'utente ha REGOLE DI NOTIFICA (user_notif_rules) la ladder è la sua (contact method + ritardo
///   rispetto all'accensione dello step): le regole con ritardo già trascorso vengono consegnate;
/// - altrimenti fallback al canale dello step sul recapito di base (telefono/email).
/// Idempotente su esc_delivery_log; ri-chiamabile a ogni giro (anche in Wait) per far scattare i ritardi.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn esc_deliver_step_rungs(
    st: &AppState, tid: Uuid, iid: Uuid, step_idx: i32, step_fired_at: chrono::DateTime<chrono::Utc>,
    attempt: i32, default_channel: crate::delivery::Channel,
    oncall: &[(Uuid, Option<String>, String)], now: chrono::DateTime<chrono::Utc>, text: &str, tr: &EscTransport<'_>,
) -> i64 {
    use crate::delivery::Channel;
    let mut delivered = 0i64;
    for (uid, phone, email) in oncall {
        let rules = sqlx::query("SELECT cm.kind,cm.value,r.delay_min FROM app.user_notif_rules r JOIN app.user_contact_methods cm ON cm.id=r.cm_id WHERE r.tenant_id=$1 AND r.user_id=$2 AND cm.disabled_at IS NULL ORDER BY r.delay_min")
            .bind(tid).bind(uid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        // rung = (canale, destinatario, ritardo_min)
        let rungs: Vec<(Channel, String, i64)> = if rules.is_empty() {
            match default_channel {
                Channel::Email => vec![(Channel::Email, email.clone(), 0)],
                _ => phone.clone().map(|p| vec![(default_channel, p, 0)]).unwrap_or_default(),
            }
        } else {
            rules.iter().filter_map(|r| {
                let kind: String = r.get("kind"); let val: String = r.get("value"); let dm: i32 = r.get("delay_min");
                let v = val.trim().to_string(); if v.is_empty() { return None; }
                Some((esc_channel_from_kind(&kind), v, dm as i64))
            }).collect()
        };
        for (chn, to, dm) in rungs {
            if to.trim().is_empty() { continue; }
            if esc_is_opted_out(st, &to).await { continue; } // G1: recapito in opt-out (DNC) -> salta
            if step_fired_at + chrono::Duration::minutes(dm) > now { continue; } // non ancora dovuto
            let claimed = sqlx::query("INSERT INTO app.esc_delivery_log(tenant_id,incident_id,step,channel,to_value,attempt,state) VALUES($1,$2,$3,$4,$5,$6,'queued') ON CONFLICT (tenant_id,incident_id,step,channel,to_value,attempt) DO NOTHING RETURNING id")
                .bind(tid).bind(iid).bind(step_idx).bind(chn.as_str()).bind(&to).bind(attempt).fetch_optional(&st.owner_pool).await.ok().flatten();
            let Some(logrow) = claimed else { continue };
            let log_id: i64 = logrow.get("id");
            let body = crate::delivery::request_body(chn, tr.from, &to, text, &iid.to_string(), step_idx as i64);
            // Box::pin: il future di esc_dispatch (client reqwest/TLS) ha frame grande; su heap tiene
            // lo stack del runner escalation shallow al picco di concorrenza.
            let res = Box::pin(esc_dispatch(st, chn, &to, text, &body, tr.live_on, tr.live_cfg, tr.slack_token, tr.relay, tr.sink, tr.public_base)).await;
            esc_persist_delivery(st, tid, iid, log_id, chn.as_str(), &res).await;
            delivered += 1;
        }
    }
    delivered
}

/// Consegna idempotente di UNA rung Slack a un canale (target Slack di uno step, o canale del gruppo).
pub(crate) async fn esc_deliver_slack_rung(st: &AppState, tid: Uuid, iid: Uuid, step: i32, attempt: i32, channel_id: &str, text: &str, tr: &EscTransport<'_>) -> i64 {
    use crate::delivery::Channel;
    let claimed = sqlx::query("INSERT INTO app.esc_delivery_log(tenant_id,incident_id,step,channel,to_value,attempt,state) VALUES($1,$2,$3,'slack',$4,$5,'queued') ON CONFLICT (tenant_id,incident_id,step,channel,to_value,attempt) DO NOTHING RETURNING id")
        .bind(tid).bind(iid).bind(step).bind(channel_id).bind(attempt).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(logrow) = claimed else { return 0; };
    let log_id: i64 = logrow.get("id");
    let body = crate::delivery::request_body(Channel::Slack, tr.from, channel_id, text, &iid.to_string(), step as i64);
    let res = Box::pin(esc_dispatch(st, Channel::Slack, channel_id, text, &body, tr.live_on, tr.live_cfg, tr.slack_token, tr.relay, tr.sink, tr.public_base)).await;
    esc_persist_delivery(st, tid, iid, log_id, "slack", &res).await;
    1
}

/// Sceglie l'adapter per un canale e consegna. LIVE (Twilio/Slack/relay) se abilitato, altrimenti
/// sink/dry. Centralizza la selezione per riusarla sia sui recapiti on-call sia sul canale Slack.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn esc_dispatch(
    st: &AppState, channel: crate::delivery::Channel, to: &str, text: &str, body: &serde_json::Value,
    live_on: bool, live_cfg: Option<&crate::delivery::TwilioCfg>, slack_token: &str, relay: &str,
    sink: &str, public_base: &str,
) -> crate::delivery::SinkResult {
    use crate::delivery::Channel;
    match channel {
        Channel::Sms | Channel::Voice if live_on && live_cfg.is_some() => {
            let action = format!("{}/api/ingress/twilio/voice", public_base.trim_end_matches('/'));
            crate::delivery::deliver_twilio_live(&st.http, live_cfg.unwrap(), channel, to, text, &action).await
        }
        Channel::Email if !relay.trim().is_empty() => crate::delivery::deliver_email_relay(&st.http, relay, body).await,
        Channel::Slack if !slack_token.trim().is_empty() => crate::delivery::deliver_slack_live(&st.http, slack_token, to, text).await,
        _ => crate::delivery::deliver_to_sink(&st.http, sink, body).await,
    }
}

/// Persiste l'esito di una consegna: su successo aggiorna la riga; su FALLIMENTO rimuove il claim
/// così il prossimo giro ritenta (at-least-once, SOS mai perso) e logga in timeline.
pub(crate) async fn esc_persist_delivery(st: &AppState, tid: Uuid, iid: Uuid, log_id: i64, channel: &str, res: &crate::delivery::SinkResult) {
    if res.state == "failed" {
        let _ = sqlx::query("DELETE FROM app.esc_delivery_log WHERE id=$1").bind(log_id).execute(&st.owner_pool).await;
        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_delivery_failed',$3)")
            .bind(tid).bind(iid).bind(json!({"channel":channel,"detail":res.detail})).execute(&st.owner_pool).await;
    } else {
        let _ = sqlx::query("UPDATE app.esc_delivery_log SET state=$2,provider_ref=$3,detail=$4 WHERE id=$1")
            .bind(log_id).bind(res.state).bind(res.provider_ref.clone()).bind(res.detail.clone()).execute(&st.owner_pool).await;
    }
}

/// Pagina di incidenti processati per tenant in UN tick di consegna (bound di MEMORIA/lavoro per
/// giro, non un tetto a quanti incidenti possono esistere). Il working-set è già ristretto alla
/// fonte alle sole escalation ATTIVE (status non terminale, vedi query), quindi in esercizio è
/// piccolo di natura. Se eccezionalmente l'attivo supera la pagina (emergenza di massa reale), il
/// resto è ripreso al tick successivo (il worker gira di continuo) con priorità ai più urgenti, e
/// un alert di sistema `esc_backlog` avvisa il superadmin: mai una coda che cresce in silenzio.
pub(crate) const ESC_INCIDENTS_PER_TICK: i64 = 1000;

/// Processa UN incidente: policy di gruppo, macchina a stati (tick), consegna rung/Slack, avanzamento
/// stato. Estratta dal runner e chiamata `Box::pin` così il frame del runner resta piccolo e ogni
/// incidente è pollato da uno stack shallow (anti stack-overflow sotto accumulo). Ritorna il numero
/// di consegne effettuate.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn esc_process_incident(
    st: &AppState, tid: Uuid, inc: &sqlx::postgres::PgRow,
    voice_on: bool, voice_min: i64, repeat: i64, engine: &str, live_on: bool,
    ga_opt: Option<(&str, &str)>, now: chrono::DateTime<chrono::Utc>, tr: &EscTransport<'_>,
) -> i64 {
    use crate::delivery::Channel;
    use crate::escalation::{self, AlertState, EscAction};
    let iid: Uuid = inc.get("id");
    let gid: Uuid = inc.get("group_id");
    let etype: String = inc.get("etype");
    let istate: String = inc.get("state");
    let mut delivered = 0i64;

    // policy di consegna del GRUPPO instradato (step personalizzati o ladder derivata)
    let (policy, specs) = esc_group_policy(st, tid, gid, voice_on, voice_min, repeat).await;
    let last_step = policy.steps.len().saturating_sub(1) as i32;

    // stato macchina (seed step=-1 = usize::MAX lato Rust)
    let strow = sqlx::query("SELECT step,step_fired_at,repeat_count FROM app.esc_alert_state WHERE tenant_id=$1 AND incident_id=$2")
        .bind(tid).bind(iid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let (cur_step, fired_at, rc) = match &strow {
        Some(r) => (r.get::<i32,_>("step"), r.get::<chrono::DateTime<chrono::Utc>,_>("step_fired_at"), r.get::<i32,_>("repeat_count")),
        None => (-1, now, 0),
    };
    let rust_step = if cur_step < 0 { usize::MAX } else { cur_step as usize };
    let state = AlertState{ step: rust_step, step_fired_at: fired_at, repeat_count: rc as u32, status: esc_status_from_incident(&istate) };
    let action = escalation::tick(&policy, &state, now);

    match action {
        EscAction::Stop | EscAction::Exhausted => {
            let new_status = if matches!(action, EscAction::Stop) { if istate=="chiuso" {"closed"} else {"ack"} } else { "exhausted" };
            // SL-12: gradini finiti e nessuno ha preso in carico un allarme critico -> ultima rete
            if new_status == "exhausted" && CRITICAL.contains(&etype.as_str()) {
                esc_fallback_start(st, tid, iid, "escalation_esaurita", now).await;
            }
            let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,repeat_count,status) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT (tenant_id,incident_id) DO UPDATE SET status=$6,updated_at=now()")
                .bind(tid).bind(iid).bind(cur_step).bind(fired_at).bind(rc).bind(new_status).execute(&st.owner_pool).await;
        }
        EscAction::Wait => {
            // delay dello step non scaduto: NON si avanza, ma si consegnano le rung DOVUTE dello
            // step corrente (fa scattare i ritardi delle regole di notifica per-utente).
            if cur_step >= 0 && !matches!(state.status, crate::escalation::EscStatus::Acked | crate::escalation::EscStatus::Closed) {
                let spec = specs.get(cur_step as usize).cloned().unwrap_or(EscStepTarget{ channel:Channel::Sms, kind:"oncall".into(), group:None, user:None, slack:None });
                let (oncall, slack_ch) = esc_step_recipients(st, tid, gid, &spec, ga_opt, now).await;
                let sstr = iid.to_string(); let short = &sstr[..8];
                let text = esc_alert_text(st, tid, iid, &etype, short, &tr.public_base).await;
                delivered += esc_deliver_step_rungs(st, tid, iid, cur_step, fired_at, rc, spec.channel, &oncall, now, &text, tr).await;
                if let Some(sc) = slack_ch { delivered += esc_deliver_slack_rung(st, tid, iid, cur_step, rc, &sc, &text, tr).await; }
            }
        }
        EscAction::FireStep(idx, _) => {
            // repeat: tick ritorna FireStep(0) ripartendo dall'ultimo step -> nuovo giro.
            let is_repeat_restart = idx == 0 && cur_step == last_step && cur_step >= 0;
            let new_rc = if is_repeat_restart { rc + 1 } else { rc };
            let attempt = new_rc; // il giro di repeat fa parte della chiave di idempotenza
            let spec = specs.get(idx).cloned().unwrap_or(EscStepTarget{ channel:Channel::Sms, kind:"oncall".into(), group:None, user:None, slack:None });
            let (oncall, slack_ch) = esc_step_recipients(st, tid, gid, &spec, ga_opt, now).await;
            let sstr = iid.to_string(); let short = &sstr[..8];
            let text = esc_alert_text(st, tid, iid, &etype, short, &tr.public_base).await;
            // SL-1: nessun destinatario per questo gradino (fuori fascia di copertura, gruppo vuoto,
            // nessun recapito): lo si dichiara in cronologia e parte la rete di ultima istanza.
            if oncall.is_empty() && slack_ch.is_none() && CRITICAL.contains(&etype.as_str()) {
                let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_nessun_destinatario',$3)")
                    .bind(tid).bind(iid).bind(json!({"step":idx,"motivo":"nessun reperibile raggiungibile per questo gradino (fuori fascia o gruppo senza recapiti)"}))
                    .execute(&st.owner_pool).await;
                esc_fallback_start(st, tid, iid, "fuori_copertura", now).await;
            }
            // rung per-utente (regole di notifica o fallback al canale dello step), ritardi inclusi.
            delivered += esc_deliver_step_rungs(st, tid, iid, idx as i32, now, attempt, spec.channel, &oncall, now, &text, tr).await;
            // step con target Slack: posta sul canale dello step.
            if let Some(sc) = slack_ch { delivered += esc_deliver_slack_rung(st, tid, iid, idx as i32, attempt, &sc, &text, tr).await; }
            // Slack per-gruppo (canale del gruppo) sul PRIMO step 'oncall' se configurato.
            if idx == 0 && spec.kind == "oncall" {
                if let Some(sr) = sqlx::query("SELECT slack_channel_id,slack_notify FROM app.alarm_groups WHERE id=$1 AND tenant_id=$2")
                    .bind(gid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten() {
                    let ch: Option<String> = sr.try_get("slack_channel_id").ok().flatten();
                    let notify: String = sr.try_get("slack_notify").unwrap_or_else(|_| "off".into());
                    if notify != "off" { if let Some(ch) = ch.filter(|s| !s.trim().is_empty()) {
                        delivered += esc_deliver_slack_rung(st, tid, iid, idx as i32, attempt, &ch, &text, tr).await;
                    } }
                }
            }
            // avanza/semina lo stato alla rung appena sparata (repeat_count aggiornato)
            let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,repeat_count,status) VALUES($1,$2,$3,$4,$5,'unack') ON CONFLICT (tenant_id,incident_id) DO UPDATE SET step=$3,step_fired_at=$4,repeat_count=$5,updated_at=now()")
                .bind(tid).bind(iid).bind(idx as i32).bind(now).bind(new_rc).execute(&st.owner_pool).await;
            let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_delivery',$3)")
                .bind(tid).bind(iid).bind(json!({"step":idx,"channel":spec.channel.as_str(),"target":spec.kind,"attempt":attempt,"live":live_on,"engine":engine})).execute(&st.owner_pool).await;
        }
    }
    delivered
}

/// Runner di consegna nativo per un tenant. Motore SEMPRE autoritativo: usa gli adapter
/// live (Twilio/Slack/relay se configurati), altrimenti il sink webhook, altrimenti DRY in dev;
/// l'ack applica la transizione sull'incidente. Idempotenza su `esc_delivery_log`; i fallimenti si
/// ritentano a oltranza sugli invii di sicurezza.
pub async fn escalation_delivery_run_tenant(st: &AppState, tid: Uuid) -> (i64, i64) {
    // SA-5: tenant sospeso -> nessuna consegna (riprende alla riattivazione per gli incidenti ancora aperti).
    if tenant_suspended(st, tid).await { return (0, 0); }
    // MAINTENANCE nativo: finestra attiva -> sospendi le consegne (gli incidenti continuano a
    // essere creati; la ladder riparte a fine finestra).
    // SL-13: la manutenzione sospende solo gli allarmi NON salva-vita; SOS, uomo a terra, immobilità,
    // check-in mancato e perdita di contatto continuano a essere inoltrati anche durante la finestra.
    let in_maintenance = maintenance_active(st, tid).await;
    if in_maintenance {
        // segna gli incidenti aperti (non salva-vita) come "in bundle": a fine finestra UN riepilogo.
        let _ = sqlx::query("UPDATE app.incidents SET bundled_at=now() WHERE tenant_id=$1 AND state<>'chiuso' AND bundled_at IS NULL AND NOT (etype = ANY($2))")
            .bind(tid).bind(LIFE_SAFETY).execute(&st.owner_pool).await;
    }
    if !in_maintenance {
    // USCITA manutenzione: se ci sono incidenti accumulati, invia UN solo riepilogo (motore nativo
    // alert bundling) agli on-call del gruppo di default, poi riprende l'escalation normale.
    let bundled: i64 = sqlx::query_scalar("SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state<>'chiuso' AND bundled_at IS NOT NULL")
        .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    if bundled > 0 {
        let dg: Option<Uuid> = sqlx::query_scalar("SELECT default_group_id FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
            .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
        if let Some(dg) = dg {
            let now0 = st.clock.now();
            let oncall = esc_native_oncall_contacts(st, tid, dg, None, now0, false).await;
            let text = format!("YouWorkforce: {bundled} allarmi durante la manutenzione. Escalation ripresa.");
            for (_u, phone, _email) in &oncall {
                if let Some(p) = phone.clone().filter(|p| !p.trim().is_empty()) {
                    if esc_is_opted_out(st, &p).await { continue; } // G1: salta opt-out
                    let _ = esc_send_sms_native(st, &p, &text).await;
                }
            }
        }
        let _ = sqlx::query("UPDATE app.incidents SET bundled_at=NULL WHERE tenant_id=$1 AND bundled_at IS NOT NULL")
            .bind(tid).execute(&st.owner_pool).await;
    }
    } // fine: riepilogo all'uscita dalla manutenzione
    // Motore nativo autoritativo: nessun riferimento esterno, rotazione seminata nativamente.
    let ga_opt: Option<(&str,&str)> = None;
    // riferimento temporale del motore nativo = orologio controllabile: step_fired_at viene SCRITTO
    // e confrontato con lo stesso `now` (vedi escalation::tick), quindi e' auto-coerente e testabile
    // con TEST_CLOCK (in prod offset=0 -> identico al wall-clock).
    let now = st.clock.now();
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    let from = crate::delivery::chan("YW_ESC_FROM").unwrap_or_else(|| "+10000000000".into());
    // adapter LIVE (gate): Twilio reale, Slack, relay email. Default OFF -> sink/dry.
    let live_cfg = crate::delivery::TwilioCfg::from_env();
    let slack_token = crate::delivery::chan("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    let relay = crate::delivery::chan("YW_ESC_SMTP_RELAY_URL").unwrap_or_default();
    let public_base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    // config vocale + repeat del tenant
    let vrow = sqlx::query("SELECT voice_fallback_on,voice_fallback_min,esc_repeat FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let (voice_on, voice_min, repeat) = vrow.map(|r| (r.get::<bool,_>("voice_fallback_on"), r.get::<i32,_>("voice_fallback_min") as i64, r.get::<i32,_>("esc_repeat") as i64))
        .unwrap_or((true,5,0));
    let engine = "native".to_string(); // unico motore: escalation nativa
    // adapter live (Twilio) usati quando configurati
    let live_on = live_cfg.is_some();
    // La policy di consegna è PER-GRUPPO (F3): ogni gruppo può avere step personalizzati.
    // Costruita dentro il loop in base al gruppo instradato dell'incidente.

    // incidenti aperti instradati a un gruppo nativo.
    // Fase C: se il lavoratore dell'incidente (via assigned_user diretto o device assegnato) ha un
    // Profilo di protezione ATTIVO con un gruppo escalation, quel gruppo HA LA PRECEDENZA sul routing
    // globale per-etype. Fallback al routing globale quando non c'è profilo/gruppo (comportamento legacy).
    // Precedenza del gruppo: profilo di protezione del lavoratore > routing per-etype >
    // gruppo di DEFAULT del tenant (catch-all, mig 0060). Il default garantisce che nessun incidente
    // critico resti senza destinatari sulla strada nuova (nativo), anche senza regola di routing.
    // WORKING-SET = solo escalation ATTIVE: si ESCLUDONO gli incidenti la cui macchina a stati è in
    // uno stato TERMINALE (`exhausted` ladder finita / `ack` preso in carico / `closed`). Questi non
    // producono più lavoro: lasciarli nel set li farebbe rileggere+ticcare a ogni giro per sempre
    // (crescita O(incidenti-aperti-totali) → la vera causa di esplosione sotto accumulo). Restano
    // inclusi i nuovi (nessuno stato = 'new') e quelli ancora in corso (`unack`). Ordine per PRIORITÀ
    // DECRESCENTE (SL-3: valore più alto = più urgente, SOS 3 / altri critici 2, come in Centrale) poi
    // opened_at: un SOS fresco non attende mai dietro allarmi meno urgenti. La pagina (LIMIT) bounda memoria/lavoro per tick; l'eccedenza va al giro dopo.
    let incs = sqlx::query(
        "SELECT i.id, i.etype, i.state, i.opened_at,
                COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id) AS group_id
           FROM app.incidents i
           LEFT JOIN app.alarm_routing r ON r.tenant_id=i.tenant_id AND r.etype=i.etype
           LEFT JOIN app.events ev ON ev.id=i.event_id
           LEFT JOIN app.devices d ON d.id=ev.device_id
           LEFT JOIN app.protection_profile_members m
                  ON m.tenant_id=i.tenant_id AND m.user_id=COALESCE(i.worker_user_id, d.assigned_user)
           LEFT JOIN app.protection_profiles pp ON pp.id=m.profile_id AND pp.enabled=true
           LEFT JOIN app.tenant_oncall_cfg oc ON oc.tenant_id=i.tenant_id
           LEFT JOIN app.esc_alert_state eas ON eas.tenant_id=i.tenant_id AND eas.incident_id=i.id
          WHERE i.tenant_id=$1 AND i.state <> 'chiuso'
            AND COALESCE(eas.status,'new') NOT IN ('exhausted','ack','closed')
            AND COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id) IS NOT NULL
            AND (NOT $3 OR i.etype = ANY($4))
          ORDER BY i.priority DESC, i.opened_at ASC
          LIMIT $2")
        .bind(tid).bind(ESC_INCIDENTS_PER_TICK).bind(in_maintenance).bind(LIFE_SAFETY).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let tr = EscTransport{ from:&from, sink:&sink, relay:&relay, slack_token:&slack_token, public_base:&public_base, live_on, live_cfg: live_cfg.as_ref() };
    let (mut seen, mut delivered) = (0i64, 0i64);
    // Un incidente per volta, pollato da uno stack shallow: `Box::pin` sposta il future (grande) del
    // processing su heap, così il frame di QUESTO runner resta minimo e la profondità è costante
    // qualunque sia il numero di incidenti (anti stack-overflow sotto accumulo).
    for inc in &incs {
        seen += 1;
        delivered += Box::pin(esc_process_incident(
            st, tid, inc, voice_on, voice_min, repeat, &engine, live_on, ga_opt, now, &tr,
        )).await;
    }
    delivered += Box::pin(esc_fallback_run(st, tid, now, &tr, in_maintenance)).await;
    (seen, delivered)
}

// ============================================================================
// Rete di ultima istanza (SL-1 + SL-12, mig 0131). Decisione PO: un allarme critico che non trova
// reperibili (fuori fascia, nessun gruppo, escalation esaurita senza presa in carico) va comunque a
// una lista di contatti email e/o SMS del tenant, in parallelo o in serie. La prima presa in carico
// (risposta all'SMS, link, Centrale) ferma la catena: le rung non ancora dovute non partono più.
// ============================================================================
/// Passo fittizio della rete nei log di consegna (gli step reali partono da 0).
pub(crate) const ESC_FALLBACK_STEP: i32 = -1;

/// Avvia la rete per un incidente (idempotente: la data di partenza resta quella della prima volta).
pub(crate) async fn esc_fallback_start(st: &AppState, tid: Uuid, iid: Uuid, reason: &str, now: chrono::DateTime<chrono::Utc>) {
    let started = sqlx::query("INSERT INTO app.esc_fallback_state(tenant_id,incident_id,started_at,reason) VALUES($1,$2,$3,$4)
                               ON CONFLICT (tenant_id,incident_id) DO NOTHING RETURNING incident_id")
        .bind(tid).bind(iid).bind(now).bind(reason).fetch_optional(&st.owner_pool).await.ok().flatten();
    if started.is_some() {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM app.esc_fallback_contacts WHERE tenant_id=$1")
            .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_rete_ultima_istanza',$3)")
            .bind(tid).bind(iid).bind(json!({"motivo":reason,"contatti":n})).execute(&st.owner_pool).await;
        if n == 0 {
            // nessuna rete configurata: l'allarme resterebbe senza destinatari -> alert di sistema
            tracing::warn!(tenant=%tid, incident=%iid, reason, "allarme critico senza destinatari e senza rete di ultima istanza");
        }
    }
}

/// Consegna le rung dovute della rete per gli incidenti aperti e non presi in carico.
pub(crate) async fn esc_fallback_run(st: &AppState, tid: Uuid, now: chrono::DateTime<chrono::Utc>, tr: &EscTransport<'_>, in_maintenance: bool) -> i64 {
    use crate::delivery::Channel;
    // incidenti critici aperti SENZA alcun gruppo instradato: prima non li vedeva nessuno (SL-12)
    let orphans: Vec<Uuid> = sqlx::query_scalar(
        "SELECT i.id FROM app.incidents i
           LEFT JOIN app.alarm_routing r ON r.tenant_id=i.tenant_id AND r.etype=i.etype
           LEFT JOIN app.events ev ON ev.id=i.event_id
           LEFT JOIN app.devices d ON d.id=ev.device_id
           LEFT JOIN app.protection_profile_members m ON m.tenant_id=i.tenant_id AND m.user_id=COALESCE(i.worker_user_id, d.assigned_user)
           LEFT JOIN app.protection_profiles pp ON pp.id=m.profile_id AND pp.enabled=true
           LEFT JOIN app.tenant_oncall_cfg oc ON oc.tenant_id=i.tenant_id
          WHERE i.tenant_id=$1 AND i.state NOT IN ('chiuso','preso_in_carico','intervento') AND i.etype = ANY($2)
            AND COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id) IS NULL
            AND NOT EXISTS (SELECT 1 FROM app.esc_fallback_state f WHERE f.tenant_id=i.tenant_id AND f.incident_id=i.id)
          LIMIT 500")
        .bind(tid).bind(if in_maintenance { LIFE_SAFETY } else { CRITICAL }).fetch_all(&st.owner_pool).await.unwrap_or_default();
    for iid in orphans { esc_fallback_start(st, tid, iid, "nessun_gruppo", now).await; }

    let contacts = sqlx::query("SELECT channel,value FROM app.esc_fallback_contacts WHERE tenant_id=$1 ORDER BY position, created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default();
    if contacts.is_empty() { return 0; }
    let cfg = sqlx::query("SELECT fallback_mode, fallback_interval_s FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let (serial, interval) = cfg.map(|r| (r.get::<String,_>("fallback_mode") == "seriale", r.get::<i32,_>("fallback_interval_s") as i64))
        .unwrap_or((false, 120));
    let active = sqlx::query(
        "SELECT f.incident_id, f.started_at, i.etype, s.name AS site
           FROM app.esc_fallback_state f JOIN app.incidents i ON i.id=f.incident_id AND i.tenant_id=f.tenant_id
           LEFT JOIN app.sites s ON s.id=i.site_id
          WHERE f.tenant_id=$1 AND i.state NOT IN ('chiuso','preso_in_carico','intervento')
            AND (NOT $2 OR i.etype = ANY($3))
          ORDER BY i.priority DESC, f.started_at LIMIT 500")
        .bind(tid).bind(in_maintenance).bind(LIFE_SAFETY).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut delivered = 0i64;
    for a in &active {
        let iid: Uuid = a.get("incident_id");
        let started: chrono::DateTime<chrono::Utc> = a.get("started_at");
        let etype: String = a.get("etype");
        let site: Option<String> = a.try_get("site").ok().flatten();
        let short = iid.to_string()[..8].to_string();
        let text = format!("ALLARME {} senza reperibile disponibile{} (incidente {short}). Rispondi OK per prenderlo in carico.",
            etype_label(&etype), site.map(|s| format!(" - {s}")).unwrap_or_default());
        for (i, c) in contacts.iter().enumerate() {
            let due = if serial { started + chrono::Duration::seconds(interval * i as i64) } else { started };
            if due > now { continue; }
            let chname: String = c.get("channel");
            let to: String = c.get("value");
            let chn = if chname == "email" { Channel::Email } else { Channel::Sms };
            if esc_is_opted_out(st, &to).await { continue; }
            let claimed = sqlx::query("INSERT INTO app.esc_delivery_log(tenant_id,incident_id,step,channel,to_value,attempt,state) VALUES($1,$2,$3,$4,$5,0,'queued') ON CONFLICT (tenant_id,incident_id,step,channel,to_value,attempt) DO NOTHING RETURNING id")
                .bind(tid).bind(iid).bind(ESC_FALLBACK_STEP).bind(chn.as_str()).bind(&to).fetch_optional(&st.owner_pool).await.ok().flatten();
            let Some(logrow) = claimed else { continue };
            let log_id: i64 = logrow.get("id");
            let body = crate::delivery::request_body(chn, tr.from, &to, &text, &iid.to_string(), ESC_FALLBACK_STEP as i64);
            let res = Box::pin(esc_dispatch(st, chn, &to, &text, &body, tr.live_on, tr.live_cfg, tr.slack_token, tr.relay, tr.sink, tr.public_base)).await;
            esc_persist_delivery(st, tid, iid, log_id, chn.as_str(), &res).await;
            let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_delivery',$3)")
                .bind(tid).bind(iid).bind(json!({"rete_ultima_istanza":true,"channel":chn.as_str(),"ordine":i+1,"modo":if serial {"seriale"} else {"parallelo"}}))
                .execute(&st.owner_pool).await;
            delivered += 1;
        }
    }
    delivered
}

// ---- API: configurazione della rete di ultima istanza (escalation.manage) ----
#[derive(Deserialize)] pub struct FallbackContact { pub channel: String, pub value: String, #[serde(default)] pub label: Option<String> }
#[derive(Deserialize)] pub struct FallbackCfg { pub mode: String, #[serde(default)] pub interval_s: Option<i32>, pub contacts: Vec<FallbackContact> }
pub async fn escalation_fallback_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let cfg = sqlx::query("SELECT fallback_mode, fallback_interval_s FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let contacts = sqlx::query("SELECT channel,value,label FROM app.esc_fallback_contacts WHERE tenant_id=$1 ORDER BY position, created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({
        "mode": cfg.as_ref().map(|r| r.get::<String,_>("fallback_mode")).unwrap_or_else(|| "parallelo".into()),
        "interval_s": cfg.as_ref().map(|r| r.get::<i32,_>("fallback_interval_s")).unwrap_or(120),
        "contacts": contacts.iter().map(|r| json!({"channel":r.get::<String,_>("channel"),"value":r.get::<String,_>("value"),
            "label":r.try_get::<Option<String>,_>("label").ok().flatten()})).collect::<Vec<_>>(),
    })))
}
pub async fn escalation_fallback_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<FallbackCfg>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.mode != "parallelo" && b.mode != "seriale" { return Err(err(StatusCode::BAD_REQUEST, "modo: 'parallelo' (tutti insieme) o 'seriale' (uno dopo l'altro)")); }
    let interval = b.interval_s.unwrap_or(120);
    if !(30..=3600).contains(&interval) { return Err(err(StatusCode::BAD_REQUEST, "intervallo tra un contatto e il successivo: da 30 a 3600 secondi")); }
    if b.contacts.len() > 50 { return Err(err(StatusCode::BAD_REQUEST, "al massimo 50 contatti")); }
    for c in &b.contacts {
        let v = c.value.trim();
        match c.channel.as_str() {
            "email" if v.contains('@') && v.len() <= 254 && !v.contains(' ') => {}
            "sms" if is_e164(v) => {}
            "email" => return Err(err(StatusCode::BAD_REQUEST, &format!("email non valida: {v}"))),
            "sms" => return Err(err(StatusCode::BAD_REQUEST, &format!("telefono non valido (formato +39…): {v}"))),
            _ => return Err(err(StatusCode::BAD_REQUEST, "canale: 'email' o 'sms'")),
        }
    }
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    sqlx::query("INSERT INTO app.tenant_oncall_cfg(tenant_id,fallback_mode,fallback_interval_s) VALUES($1,$2,$3)
                 ON CONFLICT (tenant_id) DO UPDATE SET fallback_mode=$2, fallback_interval_s=$3, updated_at=now()")
        .bind(tid).bind(&b.mode).bind(interval).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.esc_fallback_contacts WHERE tenant_id=$1").bind(tid).execute(&mut *tx).await.map_err(ise)?;
    for (i, c) in b.contacts.iter().enumerate() {
        sqlx::query("INSERT INTO app.esc_fallback_contacts(tenant_id,channel,value,label,position) VALUES($1,$2,$3,$4,$5)
                     ON CONFLICT (tenant_id,channel,value) DO NOTHING")
            .bind(tid).bind(&c.channel).bind(c.value.trim()).bind(c.label.as_deref().map(str::trim).filter(|x| !x.is_empty())).bind(i as i32)
            .execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "escalation_fallback_set", json!({"mode":b.mode,"interval_s":interval,"contatti":b.contacts.len()})).await;
    Ok(Json(json!({"ok":true})))
}

/// F3 — Annunci Slack dell'on-call (motore nativo "On-Call Notifications"): per ogni gruppo con
/// `slack_notify` != off e un canale Slack, posta chi è reperibile. `onchange` = a ogni handoff
/// (cambia lo shift_start della rotation); `HH:MM` = una volta al giorno dopo quell'ora.
/// Best-effort: senza token Slack live non fa nulla.
pub async fn slack_announce_scan(st: &AppState) {
    let token = crate::delivery::chan("YW_ESC_SLACK_TOKEN").unwrap_or_default();
    if token.trim().is_empty() { return; }
    // wall-clock: annuncio verso Slack (sistema esterno) + cadenza giornaliera HH:MM e
    // slack_last_announce (SQL) sono su tempo reale; il TEST_CLOCK non deve spammare Slack.
    let now = chrono::Utc::now();
    let groups = sqlx::query("SELECT id,tenant_id,name,slack_channel_id,slack_notify,slack_last_announce,oncall_mode,oncall_tz FROM app.alarm_groups g WHERE slack_notify<>'off' AND slack_channel_id IS NOT NULL
        AND EXISTS (SELECT 1 FROM app.tenants t WHERE t.id=g.tenant_id AND t.status<>'sospeso')")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for g in &groups {
        let gid: Uuid = g.get("id");
        let tid: Uuid = g.get("tenant_id");
        let name: String = g.get("name");
        let Some(ch) = g.try_get::<Option<String>,_>("slack_channel_id").ok().flatten().filter(|s| !s.trim().is_empty()) else { continue };
        let notify: String = g.try_get("slack_notify").unwrap_or_else(|_| "off".into());
        let last: Option<chrono::DateTime<chrono::Utc>> = g.try_get("slack_last_announce").ok().flatten();
        let mode: String = g.try_get("oncall_mode").unwrap_or_else(|_| "tutti".into());
        let should = if notify == "onchange" {
            if mode != "turni" { false } else {
                // handoff: lo shift_start corrente della rotation è più recente dell'ultimo annuncio
                let ss: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT shift_start FROM app.esc_rotation_state WHERE tenant_id=$1 AND group_id=$2")
                    .bind(tid).bind(gid).fetch_optional(&st.owner_pool).await.ok().flatten();
                match (ss, last) { (Some(s), Some(l)) => s > l, (Some(_), None) => true, _ => false }
            }
        } else if let Some((hh, mm)) = parse_hhmm(&notify) {
            // orario di annuncio nell'ora LOCALE del gruppo (oncall_tz), DST gestito.
            use chrono::TimeZone as _;
            let tz: chrono_tz::Tz = g.try_get::<String,_>("oncall_tz").ok().and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::UTC);
            let lt = now.with_timezone(&tz);
            let target = lt.date_naive().and_hms_opt(hh, mm, 0)
                .and_then(|d| match tz.from_local_datetime(&d) { chrono::LocalResult::Single(x)=>Some(x), chrono::LocalResult::Ambiguous(x,_)=>Some(x), _=>None })
                .map(|d| d.with_timezone(&chrono::Utc));
            match target { Some(t) => now >= t && last.map(|l| l < t).unwrap_or(true), None => false }
        } else { false };
        if !should { continue; }
        let oncall = esc_native_oncall_contacts(st, tid, gid, None, now, false).await;
        let who = if oncall.is_empty() { "nessuno".to_string() }
            else { oncall.iter().map(|(_,_,em)| em.clone()).collect::<Vec<_>>().join(", ") };
        let text = format!("On-call «{name}»: {who}");
        let _ = crate::delivery::deliver_slack_live(&st.http, &token, &ch, &text).await;
        let _ = sqlx::query("UPDATE app.alarm_groups SET slack_last_announce=$2 WHERE id=$1").bind(gid).bind(now).execute(&st.owner_pool).await;
    }
}

/// Scansione consegna del motore nativo (dal worker). SELF-SUFFICIENT: copre OGNI tenant che ha
/// incidenti aperti (qualunque modulo li produca: sicurezza/IoT/GPS).
pub async fn escalation_delivery_scan(st: &AppState) {
    let tids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT i.tenant_id FROM app.incidents i JOIN app.tenants t ON t.id=i.tenant_id
          WHERE i.state <> 'chiuso' AND t.status <> 'sospeso'")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    // Per-tenant su task DEDICATO: il future (profondo) del tenant viene pollato dal runtime dalla
    // cima dello stack del task, non annidato nel frame dello scan -> isolamento di profondità.
    // Await sequenziale del JoinHandle: UN tenant alla volta, così non si reintroduce il picco di
    // concorrenza che causava lo stack-overflow.
    for tid in tids {
        let st2 = st.clone();
        let _ = tokio::spawn(async move { escalation_delivery_run_tenant(&st2, tid).await }).await;
    }
}

/// Riepilogo consegne in ombra (admin): conteggi per stato + ultime consegne.
pub async fn escalation_delivery_report(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    let counts = sqlx::query("SELECT state, count(*) AS n FROM app.esc_delivery_log WHERE tenant_id=$1 AND at > now() - interval '7 days' GROUP BY state")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let by_state: Value = counts.iter().map(|r| (r.get::<String,_>("state"), json!(r.get::<i64,_>("n")))).collect::<serde_json::Map<_,_>>().into();
    let recent = sqlx::query("SELECT at,incident_id,step,channel,to_value,state,provider_ref FROM app.esc_delivery_log WHERE tenant_id=$1 ORDER BY at DESC LIMIT 30")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = recent.iter().map(|r| json!({
        "at": r.get::<chrono::DateTime<chrono::Utc>,_>("at"),
        "incident_id": r.get::<Uuid,_>("incident_id"),
        "step": r.get::<i32,_>("step"), "channel": r.get::<String,_>("channel"),
        "to": r.get::<String,_>("to_value"), "state": r.get::<String,_>("state"),
        "provider_ref": r.try_get::<String,_>("provider_ref").ok() })).collect();
    let sink = std::env::var("YW_ESC_SINK_URL").unwrap_or_default();
    Ok(Json(json!({"window_days":7,"by_state":by_state,"recent":out,
        "sink_configured": !sink.is_empty(),
        "note":"Consegne dell'escalation nativa degli ultimi 7 giorni. «dry» = nessun canale configurato (né operatore telefonico né sink di prova)."})))
}

/// Trigger manuale (admin) di un giro di consegna in ombra sul proprio tenant.
pub async fn escalation_delivery_run(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    let (seen, delivered) = escalation_delivery_run_tenant(&st, tid).await;
    Ok(Json(json!({"ok":true,"incidents":seen,"deliveries":delivered})))
}

// --------------------------- Ack inbound Twilio (SMS + voce DTMF) ---------------------------

/// Risposta TwiML vuota (ack ricevuto, niente da dire).
pub(crate) fn twiml(body: &str) -> Response {
    ([(header::CONTENT_TYPE, "text/xml")], body.to_string()).into_response()
}

/// Valida la firma Twilio dell'inbound. Se `YW_ESC_TWILIO_AUTH` è impostato la firma è
/// OBBLIGATORIA (403 se assente/errata); in dev senza token si accetta e si logga.
pub(crate) fn esc_verify_inbound(h: &HeaderMap, path: &str, params: &[(String,String)]) -> Result<(), Response> {
    let auth = crate::delivery::chan("YW_ESC_TWILIO_AUTH").unwrap_or_default();
    if auth.is_empty() { return Ok(()); } // dev/shadow senza carrier reale
    let base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let sig = h.get("x-twilio-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if crate::delivery::verify_twilio_signature(&auth, &url, params, sig) { Ok(()) }
    else { Err((StatusCode::FORBIDDEN, "firma Twilio non valida").into_response()) }
}

/// Mappa un numero mittente -> incidente aperto più recente a cui era stata consegnata una rung.
/// SL-9: testo della notifica al reperibile, senza codici grezzi: tipo d'allarme in chiaro, lavoratore,
/// sede, link alla posizione e come rispondere (con il codice breve dell'incidente).
pub(crate) async fn esc_alert_text(st: &AppState, tid: Uuid, iid: Uuid, etype: &str, short: &str, _public_base: &str) -> String {
    let r = sqlx::query(
        "SELECT COALESCE(w.email, du.email) AS worker, s.name AS site, e.lat, e.lon
           FROM app.incidents i
           LEFT JOIN app.users w ON w.id=i.worker_user_id
           LEFT JOIN app.events e ON e.id=i.event_id
           LEFT JOIN app.devices d ON d.id=e.device_id
           LEFT JOIN app.users du ON du.id=d.assigned_user
           LEFT JOIN app.sites s ON s.id=i.site_id
          WHERE i.id=$1 AND i.tenant_id=$2")
        .bind(iid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let mut parts = vec![format!("ALLARME {}", etype_label(etype))];
    if let Some(r) = &r {
        if let Some(w) = r.try_get::<Option<String>,_>("worker").ok().flatten() { parts.push(w); }
        if let Some(sn) = r.try_get::<Option<String>,_>("site").ok().flatten() { parts.push(sn); }
        if let (Some(la), Some(lo)) = (r.try_get::<Option<f64>,_>("lat").ok().flatten(), r.try_get::<Option<f64>,_>("lon").ok().flatten()) {
            parts.push(format!("posizione https://maps.google.com/?q={la:.5},{lo:.5}"));
        }
    }
    format!("{}. Rispondi OK {short} per prenderlo in carico, CHIUDI {short} per chiuderlo.", parts.join(" - "))
}

/// SL-8: interpreta la risposta SMS del reperibile. La parola chiave deve essere la PRIMA parola
/// (così «NON OK» non vale come presa in carico); il secondo elemento facoltativo è il codice breve
/// dell'incidente (primi caratteri dell'identificativo, come nel messaggio ricevuto).
pub(crate) fn parse_sms_reply(body: &str) -> (Option<&'static str>, Option<String>) {
    let up = body.trim().to_uppercase();
    let mut it = up.split_whitespace();
    let first = it.next().unwrap_or("");
    let action = match first {
        "OK" | "ACK" | "1" | "PRESO" | "PRESA" => Some("ack"),
        "CHIUDI" | "CLOSE" | "RISOLTO" => Some("close"),
        _ => None,
    };
    let code = it.next().map(|c| c.to_lowercase())
        .filter(|c| c.len() >= 6 && c.len() <= 36 && c.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-'));
    (action, code)
}

pub(crate) enum ReplyTarget { One(Uuid, Uuid), Ambiguous(Vec<String>), None }
/// SL-8: incidente a cui si riferisce la risposta di `number`. Solo incidenti APERTI per cui quel
/// numero è stato effettivamente avvisato (il tenant si deduce dal log di consegna, mai indovinato).
/// Con il codice: quello. Senza codice: vale solo se il numero ha un unico allarme aperto.
pub(crate) async fn esc_incident_for_reply(st: &AppState, number: &str, code: Option<&str>, action: &str) -> ReplyTarget {
    // presa in carico: solo allarmi ancora da prendere; chiusura: anche quelli già presi in carico
    let states: &[&str] = if action == "close" { &["aperto","preso_in_carico","intervento"] } else { &["aperto"] };
    let rows = sqlx::query(
        "SELECT DISTINCT ON (d.incident_id) d.tenant_id, d.incident_id, d.at
           FROM app.esc_delivery_log d JOIN app.incidents i ON i.id=d.incident_id AND i.tenant_id=d.tenant_id
          WHERE d.to_value=$1 AND d.channel IN ('sms','voice') AND i.state = ANY($2)
          ORDER BY d.incident_id, d.at DESC")
        .bind(number).bind(states).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut cands: Vec<(Uuid, Uuid, chrono::DateTime<chrono::Utc>)> = rows.iter()
        .map(|r| (r.get("tenant_id"), r.get("incident_id"), r.get("at"))).collect();
    if let Some(c) = code {
        cands.retain(|(_, iid, _)| iid.to_string().starts_with(c));
    }
    match cands.len() {
        0 => ReplyTarget::None,
        1 => ReplyTarget::One(cands[0].0, cands[0].1),
        _ => { cands.sort_by(|a, b| b.2.cmp(&a.2));
               ReplyTarget::Ambiguous(cands.iter().take(5).map(|(_, i, _)| i.to_string()[..8].to_string()).collect()) }
    }
}
/// Voce: la chiamata riguarda l'ultimo incidente chiamato su quel numero (non ambiguo per costruzione).
pub(crate) async fn esc_incident_last_called(st: &AppState, number: &str) -> Option<(Uuid, Uuid)> {
    let row = sqlx::query(
        "SELECT d.tenant_id, d.incident_id FROM app.esc_delivery_log d JOIN app.incidents i ON i.id=d.incident_id AND i.tenant_id=d.tenant_id
          WHERE d.to_value=$1 AND d.channel='voice' AND i.state <> 'chiuso' ORDER BY d.at DESC LIMIT 1")
        .bind(number).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    Some((row.get("tenant_id"), row.get("incident_id")))
}

/// Registra l'ack: stato macchina + delivery_log(acked) + timeline e applica la transizione
/// reale dell'incidente (preso_in_carico / chiuso).
pub(crate) async fn esc_apply_ack(st: &AppState, tid: Uuid, iid: Uuid, action: &str, via: &str) {
    let status = if action == "close" { "closed" } else { "ack" };
    let _ = sqlx::query("INSERT INTO app.esc_alert_state(tenant_id,incident_id,step,step_fired_at,status) VALUES($1,$2,0,now(),$3) ON CONFLICT (tenant_id,incident_id) DO UPDATE SET status=$3,updated_at=now()")
        .bind(tid).bind(iid).bind(status).execute(&st.owner_pool).await;
    // motore di escalation nativo (unico): la risposta del reperibile applica sempre la transizione reale
    let apply = true;
    let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'esc_ack_inbound',$3)")
        .bind(tid).bind(iid).bind(json!({"action":action,"via":via,"applied":apply})).execute(&st.owner_pool).await;
    if apply {
        if action == "close" {
            let _ = sqlx::query("UPDATE app.incidents SET state='chiuso', closed_at=now() WHERE id=$1 AND tenant_id=$2 AND state<>'chiuso'").bind(iid).bind(tid).execute(&st.owner_pool).await;
        } else {
            let _ = sqlx::query("UPDATE app.incidents SET state='preso_in_carico' WHERE id=$1 AND tenant_id=$2 AND state='aperto'").bind(iid).bind(tid).execute(&st.owner_pool).await;
        }
        // G3 status-update: avvisa gli altri on-call (attore = chi ha risposto dal carrier, ignoto qui)
        esc_status_update_notify(st, tid, iid, None, &format!("recapito ({via})"), action).await;
    }
}

// ============================================================================
// Servizi nativi — S1 heartbeat monitor, S2 email ingress, S3 bulk, S4 on-call-now (nativi)
// ============================================================================

/// Crea un incidente critico INSTRADATO senza device (heartbeat/email/watchdog): usa la catena
/// persist_ingest, poi fissa il gruppo se richiesto (forced_group_id, override del routing).
pub(crate) async fn raise_routed_incident(st: &AppState, tid: Uuid, etype: &str, source: &str, correlation: &str, payload: Value, group_id: Option<Uuid>) -> Option<Uuid> {
    let res = persist_ingest(st, tid, Normalized{
        source: source.into(), source_event: correlation.into(), idem: correlation.into(),
        etype: etype.into(), device_external_id: None, site_name: None,
        lat: None, lon: None, accuracy_m: None, correlation: correlation.into(),
        payload, indoor: false, beacon_uid: None, zone: None,
    }).await.ok()?;
    let iid = res.0.get("incident_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok());
    if let (Some(iid), Some(g)) = (iid, group_id) {
        let _ = sqlx::query("UPDATE app.incidents SET forced_group_id=$2 WHERE id=$1 AND tenant_id=$3").bind(iid).bind(g).bind(tid).execute(&st.owner_pool).await;
    }
    iid
}

// ---- S1: Heartbeat monitor nativo ----
/// Ping pubblico del monitor (un sistema esterno lo chiama periodicamente). Aggiorna last_ping;
/// se il monitor era 'down', chiude l'incidente (recovery). Nessuna autenticazione di sessione.
pub async fn ingest_heartbeat(State(st): State<AppState>, Path(token): Path<String>) -> Response {
    use axum::response::IntoResponse;
    let row = sqlx::query("SELECT id,tenant_id,state,incident_id FROM app.heartbeat_monitors WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "monitor inesistente").into_response(); };
    let id: Uuid = row.get("id"); let tid: Uuid = row.get("tenant_id");
    if tenant_suspended(&st, tid).await { return (StatusCode::FORBIDDEN, "tenant sospeso").into_response(); }
    let state: String = row.get("state");
    let inc: Option<Uuid> = row.try_get("incident_id").ok().flatten();
    let _ = sqlx::query("UPDATE app.heartbeat_monitors SET last_ping_at=now(), state='ok', incident_id=NULL WHERE id=$1").bind(id).execute(&st.owner_pool).await;
    if state == "down" { if let Some(iid) = inc {
        let _ = sqlx::query("UPDATE app.incidents SET state='chiuso', closed_at=now(), outcome='heartbeat ripristinato' WHERE id=$1 AND tenant_id=$2 AND state<>'chiuso'").bind(iid).bind(tid).execute(&st.owner_pool).await;
        let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)").bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":"heartbeat ripristinato"})).execute(&st.owner_pool).await;
    }}
    (StatusCode::OK, "ok").into_response()
}
/// Watchdog heartbeat (dal worker): per ogni monitor scaduto e ancora 'ok' apre un incidente
/// 'heartbeat_down' instradato (al suo gruppo o al routing/default). Idempotente sullo stato.
pub async fn heartbeat_watchdog(st: &AppState) {
    let rows = sqlx::query("SELECT id,tenant_id,name,timeout_s,group_id,last_ping_at FROM app.heartbeat_monitors hm WHERE state='ok'
        AND EXISTS (SELECT 1 FROM app.tenants t WHERE t.id=hm.tenant_id AND t.status<>'sospeso')")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    // wall-clock: last_ping_at e' scritto con now() SQL (wall); il timeout si misura sul wall.
    let now = chrono::Utc::now();
    for r in &rows {
        let last: Option<chrono::DateTime<chrono::Utc>> = r.try_get("last_ping_at").ok().flatten();
        let Some(last) = last else { continue }; // mai pingato: in attesa del primo ping
        let timeout: i32 = r.get("timeout_s");
        if (now - last).num_seconds() < timeout as i64 { continue; }
        let id: Uuid = r.get("id"); let tid: Uuid = r.get("tenant_id");
        let name: String = r.get("name"); let g: Option<Uuid> = r.try_get("group_id").ok().flatten();
        let corr = format!("heartbeat-{}-{}", id, now.timestamp_millis());
        if let Some(iid) = raise_routed_incident(st, tid, "heartbeat_down", "heartbeat", &corr,
            json!({"monitor":name,"timeout_s":timeout,"last_ping_at":last}), g).await {
            let _ = sqlx::query("UPDATE app.heartbeat_monitors SET state='down', incident_id=$2 WHERE id=$1").bind(id).bind(iid).execute(&st.owner_pool).await;
        }
    }
}
pub async fn heartbeat_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let base = std::env::var("YW_ESC_PUBLIC_BASE").unwrap_or_default();
    let rows = sqlx::query("SELECT id,name,token,timeout_s,group_id,last_ping_at,state FROM app.heartbeat_monitors WHERE tenant_id=$1 ORDER BY created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let tok: String = r.get("token");
        json!({"id":r.get::<Uuid,_>("id"),"name":r.get::<String,_>("name"),
            "timeout_s":r.get::<i32,_>("timeout_s"),"group_id":r.try_get::<Option<Uuid>,_>("group_id").ok().flatten(),
            "last_ping_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("last_ping_at").ok().flatten(),
            "state":r.get::<String,_>("state"),
            "url":format!("{}/api/ingress/heartbeat/{}", base.trim_end_matches('/'), tok)})
    }).collect();
    Ok(Json(json!({"monitors":out})))
}
#[derive(Deserialize)] pub struct HbReq { pub name: String, pub timeout_min: Option<i32>, #[serde(default)] pub group_id: Option<Uuid> }
pub async fn heartbeat_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<HbReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    let timeout_s = b.timeout_min.unwrap_or(15).clamp(1, 1440) * 60;
    group_belongs(&st, tid, b.group_id).await?;
    let token = format!("hb-{}", auth::new_token());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.heartbeat_monitors(tenant_id,name,token,timeout_s,group_id) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(tid).bind(b.name.trim()).bind(&token).bind(timeout_s).bind(b.group_id).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "heartbeat_create", json!({"name":b.name,"timeout_s":timeout_s})).await;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn heartbeat_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "escalation.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.heartbeat_monitors WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "monitor inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- S2: Email ingress ----
/// Webhook del provider di posta (Mailgun/SES/…): un'email ricevuta a <token>[+dedup]@dominio diventa
/// un incidente 'email_alarm' instradato. Tenant risolto dal token (local-part), MAI dal corpo.
#[derive(Deserialize)] pub struct EmailIn { pub to: String, #[serde(default)] pub from: String, #[serde(default)] pub subject: String, #[serde(default)] pub body: String }
pub async fn ingest_email(State(st): State<AppState>, Json(b): Json<EmailIn>) -> Response {
    use axum::response::IntoResponse;
    // local-part dell'indirizzo "to": token[+dedup]@dominio
    let local = b.to.split('@').next().unwrap_or("").trim().to_lowercase();
    let (token, dedup) = match local.split_once('+') { Some((t,d)) => (t.to_string(), Some(d.to_string())), None => (local, None) };
    let row = sqlx::query("SELECT tenant_id,group_id FROM app.email_inboxes WHERE token=$1")
        .bind(&token).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(row) = row else { return (StatusCode::NOT_FOUND, "inbox inesistente").into_response(); };
    let tid: Uuid = row.get("tenant_id"); let g: Option<Uuid> = row.try_get("group_id").ok().flatten();
    if tenant_suspended(&st, tid).await { return (StatusCode::FORBIDDEN, "tenant sospeso").into_response(); }
    let subject = if b.subject.trim().is_empty() { "(senza oggetto)".to_string() } else { b.subject.trim().to_string() };
    // dedup (motore nativo email): chiave custom (+key) oppure firma STABILE dell'oggetto, così email
    // ripetute con lo stesso subject confluiscono sullo stesso incidente aperto (niente flood).
    let corr = match dedup {
        Some(d) => format!("email-{}-{}", token, d),
        None => format!("email-{}-{}", token, &hash_token(&subject.to_lowercase())[..16]),
    };
    let payload = json!({"from":b.from,"subject":subject,"body": b.body.chars().take(2000).collect::<String>()});
    let _ = raise_routed_incident(&st, tid, "email_alarm", "email", &corr, payload, g).await;
    (StatusCode::OK, "ok").into_response()
}
pub async fn email_inbox_list(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let domain = std::env::var("YW_EMAIL_DOMAIN").unwrap_or_else(|_| "alerts.youworkforce.local".into());
    let rows = sqlx::query("SELECT id,token,group_id FROM app.email_inboxes WHERE tenant_id=$1 ORDER BY created_at")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let tok: String = r.get("token");
        json!({"id":r.get::<Uuid,_>("id"),"group_id":r.try_get::<Option<Uuid>,_>("group_id").ok().flatten(),
            "address":format!("{}@{}", tok, domain)})
    }).collect();
    Ok(Json(json!({"inboxes":out})))
}
#[derive(Deserialize)] pub struct InboxReq { #[serde(default)] pub group_id: Option<Uuid> }
pub async fn email_inbox_create(State(st): State<AppState>, h: HeaderMap, Json(b): Json<InboxReq>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    group_belongs(&st, tid, b.group_id).await?;
    let token = format!("in-{}", &auth::new_token()[..12]);
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let row = sqlx::query("INSERT INTO app.email_inboxes(tenant_id,token,group_id) VALUES($1,$2,$3) RETURNING id")
        .bind(tid).bind(&token).bind(b.group_id).fetch_one(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"id":row.get::<Uuid,_>("id")})))
}
pub async fn email_inbox_del(State(st): State<AppState>, h: HeaderMap, Path(id): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "iot.manage")?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    let n = sqlx::query("DELETE FROM app.email_inboxes WHERE id=$1 AND tenant_id=$2").bind(id).bind(tid).execute(&mut *tx).await.map_err(ise)?.rows_affected();
    tx.commit().await.map_err(ise)?;
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "inbox inesistente")); }
    Ok(Json(json!({"ok":true})))
}

// ---- S3: bulk acknowledge / close ----
pub async fn incidents_ack_all(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    if !has_cap(&s,"incidents.handle") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let acl: Option<Vec<Uuid>> = allowed_sites(&st, &s).await.map(|h| h.into_iter().collect());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // solo gli incidenti 'aperto' visibili all'ACL dell'operatore. Gli incidenti senza sede NON sono
    // visibili a un operatore ristretto (coerente con guard_incident_site): quindi esclusi dal bulk.
    // tenant_id ESPLICITO oltre alla RLS (vedi incidents_close_all).
    let rows = match &acl {
        Some(sites) => sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE tenant_id=$2 AND state='aperto' AND site_id = ANY($1) RETURNING id")
            .bind(sites).bind(tid).fetch_all(&mut *tx).await.map_err(ise)?,
        None => sqlx::query("UPDATE app.incidents SET state='preso_in_carico',version=version+1 WHERE tenant_id=$1 AND state='aperto' RETURNING id")
            .bind(tid).fetch_all(&mut *tx).await.map_err(ise)?,
    };
    // trail per-incidente come l'ack singolo (il bulk NON invia lo status-update per non intasare)
    for r in &rows {
        let iid: Uuid = r.get("id");
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'human_ack',$3)")
            .bind(tid).bind(iid).bind(json!({"by":s.email,"bulk":true})).execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    let n = rows.len();
    audit(&st, tid, &s.email, "incidents_ack_all", json!({"count":n})).await;
    Ok(Json(json!({"ok":true,"acknowledged":n})))
}
pub async fn incidents_close_all(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CloseReq>) -> R {
    let s = require_tenant(&st, &h).await?;
    if !has_cap(&s,"incidents.handle") { return Err(err(StatusCode::FORBIDDEN, "ruolo non abilitato")); }
    let tid = s.tenant_id.unwrap();
    let acl: Option<Vec<Uuid>> = allowed_sites(&st, &s).await.map(|h| h.into_iter().collect());
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(ise)?;
    // Incidenti senza sede esclusi per operatore ristretto (coerente con guard_incident_site).
    // tenant_id ESPLICITO oltre alla RLS: un bulk destructive non deve MAI poter uscire dal tenant,
    // nemmeno se il ruolo DB fosse mal configurato (owner/BYPASSRLS).
    // SL-13: gli allarmi salva-vita restano aperti salvo conferma esplicita (include_life_safety)
    let excl: Vec<&str> = if b.include_life_safety { vec![] } else { LIFE_SAFETY.to_vec() };
    let rows = match &acl {
        Some(sites) => sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$1,closed_at=now(),version=version+1 WHERE tenant_id=$3 AND state<>'chiuso' AND site_id = ANY($2) AND NOT (etype = ANY($4)) RETURNING id,correlation,etype")
            .bind(&b.outcome).bind(sites).bind(tid).bind(&excl).fetch_all(&mut *tx).await.map_err(ise)?,
        None => sqlx::query("UPDATE app.incidents SET state='chiuso',outcome=$1,closed_at=now(),version=version+1 WHERE tenant_id=$2 AND state<>'chiuso' AND NOT (etype = ANY($3)) RETURNING id,correlation,etype")
            .bind(&b.outcome).bind(tid).bind(&excl).fetch_all(&mut *tx).await.map_err(ise)?,
    };
    let kept: i64 = if b.include_life_safety { 0 } else {
        sqlx::query_scalar("SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state<>'chiuso' AND etype = ANY($2)")
            .bind(tid).bind(LIFE_SAFETY).fetch_one(&mut *tx).await.map_err(ise)? };
    for r in &rows {
        let iid: Uuid = r.get("id"); let correlation: String = r.get("correlation"); let etype: String = r.get("etype");
        // trail per-incidente come la chiusura singola
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
            .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":b.outcome,"by":s.email,"bulk":true})).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'close',$2)")
            .bind(tid).bind(json!({"incident_id":iid,"summary":format!("close {}",correlation),"dedup":correlation,"etype":etype}))
            .execute(&mut *tx).await.map_err(ise)?;
    }
    tx.commit().await.map_err(ise)?;
    let n = rows.len();
    audit(&st, tid, &s.email, "incidents_close_all", json!({"count":n,"salva_vita_inclusi":b.include_life_safety})).await;
    Ok(Json(json!({"ok":true,"closed":n,"life_safety_kept_open":kept})))
}

// ---- S4: on-call-now NATIVO (chi è reperibile ora, per gruppo) ----
pub async fn oncall_now_native(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    require_any_module(&st, tid, ESCALATION_MODS).await?;
    let now = st.clock.now();
    let groups = sqlx::query("SELECT id,name FROM app.alarm_groups WHERE tenant_id=$1 ORDER BY name")
        .bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let mut out = Vec::new();
    let mut flat: Vec<String> = Vec::new();
    for g in &groups {
        let gid: Uuid = g.get("id"); let name: String = g.get("name");
        let oncall = esc_native_oncall_contacts(&st, tid, gid, None, now, false).await;
        let emails: Vec<String> = oncall.iter().map(|(_,_,e)| e.clone()).collect();
        for e in &emails { if !flat.contains(e) { flat.push(e.clone()); } }
        out.push(json!({"group":name,"oncall":emails}));
    }
    Ok(Json(json!({"available":true,"engine":"native","groups":out,"oncall":flat.iter().map(|e| json!({"user":e})).collect::<Vec<_>>()})))
}

/// G1 — Do-Not-Call: registra l'opt-out GLOBALE di un recapito (STOP) e ne disabilita le occorrenze
/// (contact method aggiuntivi + telefono primario) in ogni tenant. Persiste oltre lo stato del modulo.
pub(crate) async fn esc_optout_mark(st: &AppState, value: &str, kind: &str, reason: &str) {
    let v = value.trim();
    if v.is_empty() { return; }
    let _ = sqlx::query("INSERT INTO app.contact_optout(value,kind,reason) VALUES($1,$2,$3) ON CONFLICT (value) DO UPDATE SET at=now(), kind=EXCLUDED.kind, reason=EXCLUDED.reason")
        .bind(v).bind(kind).bind(reason).execute(&st.owner_pool).await;
    // tenant toccati (per audit)
    let tids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM app.user_contact_methods WHERE value=$1
         UNION SELECT tenant_id FROM app.users WHERE phone=$1")
        .bind(v).fetch_all(&st.owner_pool).await.unwrap_or_default();
    let _ = sqlx::query("UPDATE app.user_contact_methods SET disabled_at=now() WHERE value=$1 AND disabled_at IS NULL").bind(v).execute(&st.owner_pool).await;
    let _ = sqlx::query("UPDATE app.users SET phone_opt_out_at=now() WHERE phone=$1 AND phone_opt_out_at IS NULL").bind(v).execute(&st.owner_pool).await;
    for tid in tids { audit(st, tid, "carrier", "contact_optout", json!({"value":v,"kind":kind,"reason":reason})).await; }
}
/// G1 — riattivazione (START): toglie dal DNC e segna i recapiti come DA RI-VERIFICARE.
pub(crate) async fn esc_optout_clear(st: &AppState, value: &str) {
    let v = value.trim(); if v.is_empty() { return; }
    let _ = sqlx::query("DELETE FROM app.contact_optout WHERE value=$1").bind(v).execute(&st.owner_pool).await;
    let _ = sqlx::query("UPDATE app.user_contact_methods SET disabled_at=NULL, verified_at=NULL WHERE value=$1").bind(v).execute(&st.owner_pool).await;
    let _ = sqlx::query("UPDATE app.users SET phone_opt_out_at=NULL WHERE phone=$1").bind(v).execute(&st.owner_pool).await;
}
/// G1 — un recapito è in opt-out (DNC)? Il runner salta questi destinatari.
pub(crate) async fn esc_is_opted_out(st: &AppState, value: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM app.contact_optout WHERE value=$1)")
        .bind(value.trim()).fetch_one(&st.owner_pool).await.unwrap_or(false)
}

/// G3 — Status update NATIVO: quando un incidente cambia stato (ack/chiusura), avvisa gli ALTRI
/// reperibili on-call che hanno lo status-update attivo (override utente -> default tenant),
/// rispettando opt-out. Risolve il gruppo come il runner (profilo > routing > default). Best-effort.
pub(crate) async fn esc_status_update_notify(st: &AppState, tid: Uuid, iid: Uuid, actor: Option<Uuid>, actor_label: &str, action: &str) {
    // default tenant
    let tdef = oncall_cfg(st, tid).await.status_on;
    // gruppo instradato dell'incidente
    let gid: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(i.forced_group_id, pp.escalation_group_id, r.group_id, oc.default_group_id)
           FROM app.incidents i
           LEFT JOIN app.alarm_routing r ON r.tenant_id=i.tenant_id AND r.etype=i.etype
           LEFT JOIN app.events ev ON ev.id=i.event_id
           LEFT JOIN app.devices d ON d.id=ev.device_id
           LEFT JOIN app.protection_profile_members m ON m.tenant_id=i.tenant_id AND m.user_id=COALESCE(i.worker_user_id, d.assigned_user)
           LEFT JOIN app.protection_profiles pp ON pp.id=m.profile_id AND pp.enabled=true
           LEFT JOIN app.tenant_oncall_cfg oc ON oc.tenant_id=i.tenant_id
          WHERE i.id=$1 AND i.tenant_id=$2")
        .bind(iid).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
    let Some(gid) = gid else { return; };
    let oncall = esc_native_oncall_contacts(st, tid, gid, None, st.clock.now(), false).await;
    let short = iid.to_string(); let short = &short[..8];
    let verb = if action=="close" { "chiuso" } else { "preso in carico" };
    let text = format!("YouWorkforce: incidente {short} {verb} da {actor_label}.");
    for (u, phone, _email) in &oncall {
        if actor == Some(*u) { continue; } // non avvisare chi ha fatto l'azione
        // preferenza per-utente -> default tenant
        let uon: Option<bool> = sqlx::query_scalar("SELECT status_updates_on FROM app.users WHERE id=$1 AND tenant_id=$2")
            .bind(u).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().flatten();
        if !uon.unwrap_or(tdef) { continue; }
        if let Some(p) = phone.clone().filter(|p| !p.trim().is_empty()) {
            if esc_is_opted_out(st, &p).await { continue; }
            let _ = esc_send_sms_native(st, &p, &text).await;
        }
    }
}

/// Inbound SMS: risposta testuale del reperibile. "OK/ACK/1/PRESO" -> presa in carico;
/// "CHIUDI/CLOSE/RISOLTO" -> chiusura. "STOP" -> opt-out (DNC); "START" -> riattiva.
/// Firma Twilio validata (se token configurato).
pub async fn twilio_ingress_sms(State(st): State<AppState>, h: HeaderMap, axum::extract::Form(params): axum::extract::Form<Vec<(String,String)>>) -> Response {
    if let Err(r) = esc_verify_inbound(&h, "/api/ingress/twilio/sms", &params) { return r; }
    let get = |k: &str| params.iter().find(|(n,_)| n==k).map(|(_,v)| v.clone()).unwrap_or_default();
    let from = get("From"); let body = get("Body").trim().to_uppercase();
    // Opt-out / opt-in (compliance): precedenza su tutto.
    let first = body.split_whitespace().next().unwrap_or("");
    if ["STOP","STOPALL","UNSUBSCRIBE","CANCEL","END","QUIT"].contains(&first) {
        esc_optout_mark(&st, &from, "sms", "carrier_stop").await;
        return twiml("<Response><Message>Disattivato. Rispondi START per riattivare.</Message></Response>");
    }
    if ["START","UNSTOP","YES"].contains(&first) {
        esc_optout_clear(&st, &from).await;
        return twiml("<Response><Message>Riattivato. Verifica il recapito in YouWorkforce.</Message></Response>");
    }
    let (action, code) = parse_sms_reply(&body);
    let Some(action) = action else {
        return twiml("<Response><Message>Risposta non riconosciuta. Scrivi OK seguito dal codice dell'allarme per prenderlo in carico, CHIUDI seguito dal codice per chiuderlo.</Message></Response>");
    };
    match esc_incident_for_reply(&st, &from, code.as_deref(), action).await {
        ReplyTarget::One(tid, iid) => {
            esc_apply_ack(&st, tid, iid, action, "sms").await;
            let what = if action == "close" { "Chiuso" } else { "Preso in carico" };
            twiml(&format!("<Response><Message>{what}: allarme {}.</Message></Response>", &iid.to_string()[..8]))
        }
        ReplyTarget::Ambiguous(codes) => twiml(&format!(
            "<Response><Message>Hai piu' allarmi aperti: rispondi {} seguito dal codice ({}).</Message></Response>",
            if action == "close" { "CHIUDI" } else { "OK" }, codes.join(", "))),
        ReplyTarget::None => twiml("<Response><Message>Nessun allarme aperto corrisponde alla risposta.</Message></Response>"),
    }
}

/// Inbound VOCE: Twilio invia i `Digits` del Gather. "1" -> presa in carico. Se mancano i Digits
/// (hit iniziale) ripropone il Gather. Firma Twilio validata (se token configurato).
pub async fn twilio_ingress_voice(State(st): State<AppState>, h: HeaderMap, axum::extract::Form(params): axum::extract::Form<Vec<(String,String)>>) -> Response {
    if let Err(r) = esc_verify_inbound(&h, "/api/ingress/twilio/voice", &params) { return r; }
    let get = |k: &str| params.iter().find(|(n,_)| n==k).map(|(_,v)| v.clone()).unwrap_or_default();
    let from = get("From"); let digits = get("Digits");
    if digits.is_empty() {
        return twiml("<Response><Gather numDigits=\"1\" timeout=\"20\"><Say language=\"it-IT\">Allarme. Premi 1 per prendere in carico, oppure 9 per non ricevere piu' chiamate.</Say></Gather></Response>");
    }
    if digits == "1" {
        if let Some((tid, iid)) = esc_incident_last_called(&st, &from).await {
            esc_apply_ack(&st, tid, iid, "ack", "voice").await;
        }
        return twiml("<Response><Say language=\"it-IT\">Preso in carico. Grazie.</Say></Response>");
    }
    if digits == "9" {
        // opt-out voce (mappatura YW: tasto 9; l'ack resta 1). Disattiva il recapito e il DNC.
        esc_optout_mark(&st, &from, "voice", "carrier_stop").await;
        return twiml("<Response><Say language=\"it-IT\">Recapito disattivato. Riattivalo da YouWorkforce.</Say></Response>");
    }
    twiml("<Response><Say language=\"it-IT\">Tasto non valido.</Say></Response>")
}

