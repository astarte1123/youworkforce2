//! Privacy e audit — servizio CORE (trasversale): GDPR (export/anonimizzazione), elenco utenti,
//! ritenzione e registro attività del tenant. Estratto da api.rs (Fase B, modularizzazione core).
//! Vedi docs/gdpr-privacy-audit.md e docs/architettura-moduli.md (strato core).

use axum::{extract::{State, Path}, http::{HeaderMap, StatusCode}, Json, response::{Response, IntoResponse}};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use crate::state::AppState;
use crate::api::{R, require_tenant, require_cap, ise, err, audit};
use crate::auth;

/// Export dei dati personali di un utente del tenant (JSON). Diritto d'accesso.
pub async fn gdpr_export(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "gdpr.manage")?;
    let tid = s.tenant_id.unwrap();
    let u = sqlx::query("SELECT id,email,role,phone,created_at,anonymized_at FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    let profile = json!({
        "id": u.get::<Uuid,_>("id"), "email": u.get::<String,_>("email"), "role": u.get::<String,_>("role"),
        "phone": u.try_get::<Option<String>,_>("phone").ok().flatten(),
        "created_at": u.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
        "anonymized_at": u.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("anonymized_at").ok().flatten() });
    let jrows = |rows: Vec<sqlx::postgres::PgRow>, f: &dyn Fn(&sqlx::postgres::PgRow)->Value| rows.iter().map(f).collect::<Vec<_>>();
    let checkins = sqlx::query("SELECT id,state,created_at,due_at FROM app.checkins WHERE user_id=$1 AND tenant_id=$2 ORDER BY created_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let diary = sqlx::query("SELECT id,created_at,body FROM app.diary_entries WHERE user_id=$1 AND tenant_id=$2 ORDER BY created_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let oncall = sqlx::query("SELECT id,starts_at,ends_at,note FROM app.oncall WHERE user_id=$1 AND tenant_id=$2 ORDER BY starts_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let incidents = sqlx::query("SELECT id,etype,state,opened_at,closed_at FROM app.incidents WHERE assigned_user=$1 AND tenant_id=$2 ORDER BY opened_at DESC LIMIT 1000")
        .bind(uid).bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    // TA-9: tutte le altre categorie di dati personali dal catalogo unico (riga completa in JSON).
    let mut extra = serde_json::Map::new();
    for (key, table, col) in PII_EXPORT {
        let sql = format!("SELECT to_jsonb(t) AS j FROM app.{table} t WHERE t.{col}=$1 AND t.tenant_id=$2 LIMIT 5000");
        let rows: Vec<Value> = sqlx::query_scalar(&sql).bind(uid).bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default();
        extra.insert(key.to_string(), json!(rows));
    }
    audit(&st, tid, &s.email, "gdpr_export", json!({"user":uid})).await;
    Ok(Json(json!({
        "profile": profile,
        "altri_dati": extra,
        "checkins": jrows(checkins, &|r| json!({"id":r.get::<Uuid,_>("id"),"state":r.get::<String,_>("state"),
            "created_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("created_at").ok().flatten(),
            "due_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("due_at").ok().flatten()})),
        "diary": jrows(diary, &|r| json!({"id":r.get::<Uuid,_>("id"),
            "created_at":r.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
            "body":r.try_get::<Option<String>,_>("body").ok().flatten()})),
        "oncall": jrows(oncall, &|r| json!({"id":r.get::<Uuid,_>("id"),
            "starts_at":r.get::<chrono::DateTime<chrono::Utc>,_>("starts_at"),
            "ends_at":r.get::<chrono::DateTime<chrono::Utc>,_>("ends_at"),
            "note":r.try_get::<Option<String>,_>("note").ok().flatten()})),
        "incidents_assigned": jrows(incidents, &|r| json!({"id":r.get::<Uuid,_>("id"),"etype":r.get::<String,_>("etype"),
            "state":r.get::<String,_>("state"),"opened_at":r.get::<chrono::DateTime<chrono::Utc>,_>("opened_at"),
            "closed_at":r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("closed_at").ok().flatten()})),
        "exported_at": st.clock.now(), "note": "dati personali dell'utente nel tenant"
    })))
}

/// CATALOGO UNICO dei dati personali per utente (TA-5/TA-9): (chiave export, tabella, colonna utente).
/// Ogni tabella ha `tenant_id`. Usato dall'export; l'anonimizzazione agisce sulle stesse categorie.
const PII_EXPORT: &[(&str, &str, &str)] = &[
    ("recapiti", "user_contact_methods", "user_id"),
    ("regole_notifica", "user_notif_rules", "user_id"),
    ("gruppi_allarme", "alarm_group_members", "user_id"),
    ("assegnazioni_escalation", "esc_assignment_members", "user_id"),
    ("profili_protezione", "protection_profile_members", "user_id"),
    ("gruppi_persone", "user_group_members", "user_id"),
    ("squadre", "team_members", "user_id"),
    ("sedi_visibili", "operator_sites", "user_id"),
    ("dispositivi_assegnati", "device_assignments", "user_id"),
    ("turni", "shifts", "user_id"),
    ("piani_turno", "shift_plans", "user_id"),
    ("assenze", "worker_absences", "user_id"),
    ("consuntivi", "timesheets", "user_id"),
    ("ronde", "patrol_rounds", "user_id"),
    ("punzonature", "patrol_punches", "user_id"),
    ("presenze_cantiere", "commessa_presenze", "user_id"),
    ("qualifiche", "user_qualifications", "user_id"),
    ("interventi_assegnati", "work_orders", "assigned_user"),
    ("incidenti_come_lavoratore", "incidents", "worker_user_id"),
];
/// Tabelle da cui l'utente anonimizzato viene RIMOSSO: recapiti, token, inviti e soprattutto ogni
/// instradamento degli allarmi (non deve più essere destinatario di avvisi né comparire in turni futuri).
const PII_REMOVE: &[(&str, &str)] = &[
    ("user_contact_methods", "user_id"), ("user_notif_rules", "user_id"),
    ("calendar_ical_tokens", "user_id"), ("oncall_ical_tokens", "user_id"), ("user_invites", "user_id"),
    ("alarm_group_members", "user_id"), ("esc_assignment_members", "user_id"),
    ("esc_overrides", "add_user"), ("esc_overrides", "remove_user"), ("esc_policy_steps", "target_user_id"),
    ("protection_profile_members", "user_id"), ("team_members", "user_id"), ("user_group_members", "user_id"),
    ("operator_sites", "user_id"), ("user_custom_roles", "user_id"),
];

/// Anonimizza un utente (diritto all'oblio). Non elimina la riga utente per non rompere l'integrità
/// storica di incidenti, consuntivi e punzonature (restano legati a un identificativo pseudonimo).
/// TA-5: rimuove TUTTI i dati che identificano la persona: email/telefono/verifiche, recapiti,
/// token di calendario, inviti, sessioni; la toglie da ogni instradamento degli allarmi e dai turni di
/// reperibilità futuri; libera i dispositivi assegnati; cancella le posizioni dell'app personale;
/// sostituisce email e telefono nell'audit, nei log di consegna e nei comandi ai dispositivi.
pub async fn gdpr_anonymize(State(st): State<AppState>, h: HeaderMap, Path(uid): Path<Uuid>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "gdpr.manage")?;
    let tid = s.tenant_id.unwrap();
    if uid == s.user_id { return Err(err(StatusCode::BAD_REQUEST, "non puoi anonimizzare te stesso")); }
    let u = sqlx::query("SELECT role,email,phone,anonymized_at FROM app.users WHERE id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "utente inesistente"))?;
    if u.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("anonymized_at").ok().flatten().is_some() {
        return Err(err(StatusCode::CONFLICT, "utente già anonimizzato"));
    }
    let role: String = u.get("role");
    let old_email: String = u.get("email");
    let old_phone: Option<String> = u.try_get("phone").ok().flatten();
    if role == "tenant_admin" {
        let admins: i64 = sqlx::query_scalar("SELECT count(*) FROM app.users WHERE tenant_id=$1 AND role='tenant_admin' AND anonymized_at IS NULL")
            .bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
        if admins <= 1 { return Err(err(StatusCode::BAD_REQUEST, "non puoi anonimizzare l'ultimo admin")); }
    }
    // TA-18: filtro di tenant esplicito (owner_pool scavalca la RLS)
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM app.incidents WHERE assigned_user=$1 AND tenant_id=$2 AND state<>'chiuso'")
        .bind(uid).bind(tid).fetch_one(&st.owner_pool).await.unwrap_or(0);
    if open > 0 { return Err(err(StatusCode::CONFLICT, "l'utente ha incidenti aperti assegnati: riassegnali o chiudili prima")); }
    let anon_email = format!("anon-{}@anonymized.invalid", &uid.to_string()[..8]);
    let anon_label = format!("anonimizzato-{}", &uid.to_string()[..8]);
    let lock = auth::new_token();
    let now = st.clock.now();
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    sqlx::query("UPDATE app.users SET email=$2, phone=NULL, pass_hash=$3, phone_verified_at=NULL, phone_verify_code=NULL,
                 phone_verify_expires=NULL, phone_opt_out_at=NULL, cal_color=NULL, anonymized_at=now() WHERE id=$1 AND tenant_id=$4")
        .bind(uid).bind(&anon_email).bind(&lock).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    for (table, col) in PII_REMOVE {
        sqlx::query(&format!("DELETE FROM app.{table} WHERE {col}=$1 AND tenant_id=$2"))
            .bind(uid).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    }
    // reperibilità: via i turni futuri e in corso (lo storico passato resta, pseudonimo)
    sqlx::query("DELETE FROM app.oncall WHERE user_id=$1 AND tenant_id=$2 AND ends_at > $3")
        .bind(uid).bind(tid).bind(now).execute(&mut *tx).await.map_err(ise)?;
    // app personale: posizioni cancellate, etichetta senza email
    sqlx::query("DELETE FROM app.device_positions p USING app.devices d
                 WHERE p.device_id=d.id AND d.tenant_id=$2 AND p.tenant_id=$2 AND d.source='pwa' AND d.assigned_user=$1")
        .bind(uid).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.devices SET label=$3 WHERE tenant_id=$2 AND source='pwa' AND assigned_user=$1")
        .bind(uid).bind(tid).bind(format!("App {anon_label}")).execute(&mut *tx).await.map_err(ise)?;
    // badge e dispositivi: liberati (il trigger di servizio li porta in_attesa: niente falsi allarmi)
    sqlx::query("UPDATE app.device_assignments SET unassigned_at=$3 WHERE user_id=$1 AND tenant_id=$2 AND unassigned_at IS NULL")
        .bind(uid).bind(tid).bind(now).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.devices SET assigned_user=NULL WHERE assigned_user=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.diary_entries SET body='[rimosso: anonimizzato]' WHERE user_id=$1 AND tenant_id=$2")
        .bind(uid).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    // email/telefono in chiaro in audit, log di consegna e comandi: sostituiti
    let mut idents: Vec<String> = vec![old_email.clone()];
    if let Some(p) = old_phone.clone().filter(|p| !p.is_empty()) { idents.push(p); }
    for ident in &idents {
        sqlx::query("UPDATE app.audit_log SET actor=CASE WHEN actor=$2 THEN $3 ELSE actor END,
                     detail=CASE WHEN detail::text LIKE '%'||$2||'%' THEN replace(detail::text,$2,$3)::jsonb ELSE detail END
                     WHERE tenant_id=$1 AND (actor=$2 OR detail::text LIKE '%'||$2||'%')")
            .bind(tid).bind(ident).bind(&anon_label).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("UPDATE app.esc_delivery_log SET to_value=$3 WHERE tenant_id=$1 AND to_value=$2")
            .bind(tid).bind(ident).bind(&anon_label).execute(&mut *tx).await.map_err(ise)?;
        sqlx::query("UPDATE app.device_commands SET actor=$3 WHERE tenant_id=$1 AND actor=$2")
            .bind(tid).bind(ident).bind(&anon_label).execute(&mut *tx).await.map_err(ise)?;
    }
    sqlx::query("DELETE FROM app.sessions WHERE user_id=$1 AND tenant_id=$2").bind(uid).bind(tid).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    audit(&st, tid, &s.email, "gdpr_anonymize", json!({"user":uid,"routing_rimosso":true})).await;
    Ok(Json(json!({"ok":true,"anonymized_email":anon_email})))
}

/// Costruisce la clausola WHERE (dopo `tenant_id=$1`) dai filtri della query e i relativi bind
/// (stringhe, legate in ordine a partire da $2). Filtri: action (esatto), actor (sottostringa),
/// q (testo libero su attore/azione/dettaglio), since/until (ISO, su `at`).
fn audit_filters(q: &std::collections::HashMap<String,String>) -> (String, Vec<String>) {
    let mut conds: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    let mut p = 2;
    let get = |k: &str| q.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if let Some(a) = get("action") { conds.push(format!("action=${p}")); binds.push(a); p+=1; }
    if let Some(a) = get("actor")  { conds.push(format!("actor ILIKE ${p}")); binds.push(format!("%{a}%")); p+=1; }
    if let Some(t) = get("q")      { conds.push(format!("(actor ILIKE ${p} OR action ILIKE ${p} OR detail::text ILIKE ${p})")); binds.push(format!("%{t}%")); p+=1; }
    if let Some(d) = get("since")  { conds.push(format!("at >= ${p}::timestamptz")); binds.push(d); p+=1; }
    if let Some(d) = get("until")  { conds.push(format!("at <= ${p}::timestamptz")); binds.push(d); p+=1; }
    let where_extra = if conds.is_empty() { String::new() } else { format!(" AND {}", conds.join(" AND ")) };
    (where_extra, binds)
}

/// Viewer del registro attività (audit log) del tenant. Capability `audit.view`.
/// Filtri: action/actor/q/since/until; paginazione limit (1..500, def 100) + offset.
/// Ritorna righe pagina, total (righe totali col filtro), elenco azioni distinte, limit/offset.
pub async fn audit_log_list(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "audit.view")?;
    let tid = s.tenant_id.unwrap();
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100).clamp(1, 500);
    let offset: i64 = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0).max(0);
    let (where_extra, binds) = audit_filters(&q);
    // pagina
    let sql = format!("SELECT id,at,actor,action,detail FROM app.audit_log WHERE tenant_id=$1{where_extra} ORDER BY at DESC LIMIT {limit} OFFSET {offset}");
    let mut qy = sqlx::query(&sql).bind(tid);
    for b in &binds { qy = qy.bind(b.clone()); }
    let rows = qy.fetch_all(&st.owner_pool).await.map_err(ise)?;
    let items: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"),
        "at": r.get::<chrono::DateTime<chrono::Utc>,_>("at"),
        "actor": r.get::<String,_>("actor"),
        "action": r.get::<String,_>("action"),
        "detail": r.try_get::<Option<Value>,_>("detail").ok().flatten(),
    })).collect();
    // totale col filtro (per la paginazione)
    let csql = format!("SELECT count(*) FROM app.audit_log WHERE tenant_id=$1{where_extra}");
    let mut cq = sqlx::query_scalar::<_,i64>(&csql).bind(tid);
    for b in &binds { cq = cq.bind(b.clone()); }
    let total: i64 = cq.fetch_one(&st.owner_pool).await.unwrap_or(0);
    let acts = sqlx::query("SELECT DISTINCT action FROM app.audit_log WHERE tenant_id=$1 ORDER BY action")
        .bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let actions: Vec<String> = acts.iter().map(|r| r.get::<String,_>("action")).collect();
    Ok(Json(json!({"rows": items, "actions": actions, "count": items.len(), "total": total, "limit": limit, "offset": offset})))
}

/// Export CSV del registro attività con gli stessi filtri del viewer (senza paginazione, max 50000).
pub async fn audit_log_export(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "audit.view")?;
    let tid = s.tenant_id.unwrap();
    let (where_extra, binds) = audit_filters(&q);
    let sql = format!("SELECT at,actor,action,detail FROM app.audit_log WHERE tenant_id=$1{where_extra} ORDER BY at DESC LIMIT 50000");
    let mut qy = sqlx::query(&sql).bind(tid);
    for b in &binds { qy = qy.bind(b.clone()); }
    let rows = qy.fetch_all(&st.owner_pool).await.map_err(ise)?;
    // CSV RFC4180: racchiude ogni campo tra virgolette, raddoppia le virgolette interne.
    let csv_field = |v: &str| format!("\"{}\"", v.replace('"', "\"\""));
    let mut out = String::from("quando,attore,azione,dettaglio\n");
    for r in &rows {
        let at = r.get::<chrono::DateTime<chrono::Utc>,_>("at").to_rfc3339();
        let actor = r.get::<String,_>("actor");
        let action = r.get::<String,_>("action");
        let detail = r.try_get::<Option<Value>,_>("detail").ok().flatten().map(|v| v.to_string()).unwrap_or_default();
        out.push_str(&format!("{},{},{},{}\n", csv_field(&at), csv_field(&actor), csv_field(&action), csv_field(&detail)));
    }
    audit(&st, tid, &s.email, "audit_export", json!({"rows": rows.len()})).await;
    Ok(([(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
         (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"audit-log.csv\"")], out).into_response())
}

/// Filtri condivisi tra elenco GDPR utenti e relativo export CSV.
/// q→email ILIKE; role→esatto; status→active (anonymized_at NULL) / anonymized (NOT NULL).
/// Parametri posizionali da $2 (il $1 è sempre tenant_id).
fn gdpr_user_filters(q: &std::collections::HashMap<String,String>) -> (String, Vec<String>) {
    let mut conds: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    let mut p = 2;
    let get = |k: &str| q.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if let Some(t) = get("q")    { conds.push(format!("email ILIKE ${p}")); binds.push(format!("%{t}%")); p+=1; }
    if let Some(r) = get("role") { conds.push(format!("role=${p}")); binds.push(r); p+=1; }
    match get("status").as_deref() {
        Some("active")     => conds.push("anonymized_at IS NULL".into()),
        Some("anonymized") => conds.push("anonymized_at IS NOT NULL".into()),
        _ => {}
    }
    let where_extra = if conds.is_empty() { String::new() } else { format!(" AND {}", conds.join(" AND ")) };
    (where_extra, binds)
}

/// Elenco GDPR utenti del tenant con ricerca/filtri + paginazione. Capability `gdpr.manage`.
/// Filtri: q (email), role, status (active/anonymized); limit (1..500, def 100) + offset.
/// Ritorna righe pagina, total (col filtro), elenco ruoli distinti, limit/offset.
pub async fn gdpr_users_list(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "gdpr.manage")?;
    let tid = s.tenant_id.unwrap();
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100).clamp(1, 500);
    let offset: i64 = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0).max(0);
    let (where_extra, binds) = gdpr_user_filters(&q);
    let sql = format!("SELECT id,email,role,phone,created_at,anonymized_at FROM app.users WHERE tenant_id=$1{where_extra} ORDER BY role,email LIMIT {limit} OFFSET {offset}");
    let mut qy = sqlx::query(&sql).bind(tid);
    for b in &binds { qy = qy.bind(b.clone()); }
    let rows = qy.fetch_all(&st.owner_pool).await.map_err(ise)?;
    let items: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "email": r.get::<String,_>("email"), "role": r.get::<String,_>("role"),
        "phone": r.try_get::<Option<String>,_>("phone").ok().flatten(),
        "created_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("created_at").ok().flatten(),
        "anonymized_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("anonymized_at").ok().flatten(),
    })).collect();
    let csql = format!("SELECT count(*) FROM app.users WHERE tenant_id=$1{where_extra}");
    let mut cq = sqlx::query_scalar::<_,i64>(&csql).bind(tid);
    for b in &binds { cq = cq.bind(b.clone()); }
    let total: i64 = cq.fetch_one(&st.owner_pool).await.unwrap_or(0);
    let rls = sqlx::query("SELECT DISTINCT role FROM app.users WHERE tenant_id=$1 ORDER BY role")
        .bind(tid).fetch_all(&st.owner_pool).await.ok().unwrap_or_default();
    let roles: Vec<String> = rls.iter().map(|r| r.get::<String,_>("role")).collect();
    Ok(Json(json!({"rows": items, "roles": roles, "count": items.len(), "total": total, "limit": limit, "offset": offset})))
}

/// Export CSV dell'elenco GDPR utenti con gli stessi filtri (senza paginazione, max 50000).
pub async fn gdpr_users_export(State(st): State<AppState>, h: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> Result<Response,(StatusCode,String)> {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "gdpr.manage")?;
    let tid = s.tenant_id.unwrap();
    let (where_extra, binds) = gdpr_user_filters(&q);
    let sql = format!("SELECT email,role,phone,created_at,anonymized_at FROM app.users WHERE tenant_id=$1{where_extra} ORDER BY role,email LIMIT 50000");
    let mut qy = sqlx::query(&sql).bind(tid);
    for b in &binds { qy = qy.bind(b.clone()); }
    let rows = qy.fetch_all(&st.owner_pool).await.map_err(ise)?;
    let csv_field = |v: &str| format!("\"{}\"", v.replace('"', "\"\""));
    let mut out = String::from("email,ruolo,telefono,creato,stato,anonimizzato_il\n");
    for r in &rows {
        let email = r.get::<String,_>("email");
        let role = r.get::<String,_>("role");
        let phone = r.try_get::<Option<String>,_>("phone").ok().flatten().unwrap_or_default();
        let created = r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("created_at").ok().flatten().map(|v| v.to_rfc3339()).unwrap_or_default();
        let anon = r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("anonymized_at").ok().flatten();
        let status = if anon.is_some() { "anonimizzato" } else { "attivo" };
        let anon_s = anon.map(|v| v.to_rfc3339()).unwrap_or_default();
        out.push_str(&format!("{},{},{},{},{},{}\n",
            csv_field(&email), csv_field(&role), csv_field(&phone), csv_field(&created), csv_field(status), csv_field(&anon_s)));
    }
    audit(&st, tid, &s.email, "gdpr_users_export", json!({"rows": rows.len()})).await;
    Ok(([(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
         (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"gdpr-utenti.csv\"")], out).into_response())
}

/// Informazioni sulla policy di ritenzione (sola lettura: configurata via env). Solo tenant_admin.
pub async fn gdpr_retention_info(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?; require_cap(&s, "gdpr.manage")?;
    let days: i64 = std::env::var("YW_RETENTION_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(Json(json!({
        "days": days,
        "enabled": days > 0,
        "note": "purga conservativa: incidenti chiusi + eventi non referenziati + audit log oltre N giorni. Configurata da amministratore di sistema (YW_RETENTION_DAYS)."
    })))
}

/// #7 Ritenzione: purga dati vecchi. Opt-in via YW_RETENTION_DAYS (0=off, default).
/// Conservativa: elimina incidenti CHIUSI + eventi non referenziati + audit_log oltre N giorni.
pub async fn purge_retention(st: &AppState) {
    let days: i64 = std::env::var("YW_RETENTION_DAYS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    if days <= 0 { return; }
    let cutoff = st.clock.now() - chrono::Duration::days(days);
    let _ = sqlx::query("DELETE FROM app.incident_timeline WHERE incident_id IN (SELECT id FROM app.incidents WHERE state='chiuso' AND closed_at < $1)")
        .bind(cutoff).execute(&st.owner_pool).await;
    let n_inc = sqlx::query("DELETE FROM app.incidents WHERE state='chiuso' AND closed_at < $1")
        .bind(cutoff).execute(&st.owner_pool).await.map(|r| r.rows_affected()).unwrap_or(0);
    let _ = sqlx::query("DELETE FROM app.events WHERE received_ts < $1 AND id NOT IN (SELECT event_id FROM app.incidents WHERE event_id IS NOT NULL)")
        .bind(cutoff).execute(&st.owner_pool).await;
    let _ = sqlx::query("DELETE FROM app.audit_log WHERE at < $1").bind(cutoff).execute(&st.owner_pool).await;
    if n_inc > 0 { tracing::info!("retention: purgati {} incidenti chiusi oltre {} giorni", n_inc, days); }
}
