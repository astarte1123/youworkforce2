//! Piano di controllo (superadmin): tenant, moduli, monitoraggio di sistema, chiave dei segreti, canali di consegna, account e MFA, impersonazione
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

// ----------------------------- control plane -----------------------------
#[derive(Deserialize)]
pub struct LoginReq { pub email: String, pub password: String, #[serde(default)] pub otp: Option<String> }

pub async fn superadmin_login(State(st): State<AppState>, h: HeaderMap, Json(b): Json<LoginReq>) -> impl IntoResponse {
    let key = rl_keys(&h, "", &b.email);
    if let Some(ra) = rl_check_all(&st, &key).await { return too_many(ra); }
    let row = sqlx::query("SELECT id, pass_hash, disabled, totp_secret, totp_last_step FROM app.superadmins WHERE email=$1")
        .bind(&b.email).fetch_optional(&st.owner_pool).await;
    match row {
        Ok(Some(r)) => {
            let id: Uuid = r.get("id");
            let hash: String = r.get("pass_hash");
            if auth::verify_password(&b.password, &hash) {
                if r.get::<bool,_>("disabled") {
                    rl_fail_all(&st, &key).await;
                    return (StatusCode::FORBIDDEN, Json(json!({"error":"account superadmin disattivato"}))).into_response();
                }
                if auth::needs_rehash(&hash) {
                    let _ = sqlx::query("UPDATE app.superadmins SET pass_hash=$1 WHERE id=$2")
                        .bind(auth::hash_password(&b.password)).bind(id).execute(&st.owner_pool).await;
                }
                // SA-12: secondo fattore (TOTP). Senza MFA attivo, dove è obbligatorio, la sessione
                // serve SOLO ad attivarlo (ruolo superadmin_setup, nessun accesso al piano di controllo).
                let enc: Option<String> = r.try_get("totp_secret").ok().flatten();
                let mut setup_reason: Option<&str> = None;
                if let Some(enc) = enc {
                    let secret = st.secrets.decrypt(&enc);
                    if secret.starts_with("enc:") {
                        // chiave dei segreti bloccata (server nuovo): il codice non si può verificare;
                        // la sessione permette solo lo sblocco con la passphrase di recupero (2° fattore)
                        setup_reason = Some("chiave_bloccata");
                    } else {
                        let Some(otp) = b.otp.as_deref().filter(|o| !o.trim().is_empty()) else {
                            return (StatusCode::UNAUTHORIZED, Json(json!({"error":"inserisci il codice dell'app di autenticazione","mfa_required":true}))).into_response();
                        };
                        match auth::totp_verify(&secret, otp, st.clock.now().timestamp(), r.try_get::<Option<i64>,_>("totp_last_step").ok().flatten()) {
                            Some(step) => { let _ = sqlx::query("UPDATE app.superadmins SET totp_last_step=$2 WHERE id=$1").bind(id).bind(step).execute(&st.owner_pool).await; }
                            None => {
                                rl_fail_all(&st, &key).await;
                                return (StatusCode::UNAUTHORIZED, Json(json!({"error":"codice dell'app di autenticazione non valido","mfa_required":true}))).into_response();
                            }
                        }
                    }
                } else if superadmin_mfa_required() {
                    setup_reason = Some("mfa_da_attivare");
                }
                rl_clear_all(&st, &key).await;
                let _ = sqlx::query("UPDATE app.superadmins SET last_login_at=now() WHERE id=$1").bind(id).execute(&st.owner_pool).await;
                let tok = auth::new_token();
                let csrf = auth::new_token();
                let full = setup_reason.is_none();
                let sess = Session { user_id: id, tenant_id: None, email: b.email.clone(),
                    role: if full { "superadmin".into() } else { "superadmin_setup".into() }, is_super: full, impersonator: None, caps: vec![] };
                persist_session(&st, &tok, &sess).await;
                (StatusCode::OK, set_cookie_header(&tok, &csrf), Json(json!({"ok":true,"role":sess.role,"csrf":csrf,
                    "mfa_setup_required": setup_reason == Some("mfa_da_attivare"), "secrets_locked": setup_reason == Some("chiave_bloccata")}))).into_response()
            } else {
                rl_fail_all(&st, &key).await;
                (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response()
            }
        }
        _ => {
            rl_fail_all(&st, &key).await;
            (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad credentials"}))).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CreateTenant { pub slug: String, pub name: String, pub admin_email: String,
    #[serde(default)] pub admin_password: Option<String>,
    #[serde(default)] pub plan: Option<String>,
    #[serde(default)] pub modules: Option<Vec<String>>,
    #[serde(default)] pub scenario: Option<String>,
    #[serde(default)] pub with_demo: Option<bool>,
    #[serde(default)] pub mobile_app_licenses: Option<i32>,
    #[serde(default)] pub iot_device_licenses: Option<i32>,
    pub brand_primary: Option<String>, pub brand_ink: Option<String>, pub brand_soft: Option<String>, pub brand_accent: Option<String> }
/// Pacchetti/piano: preset di moduli da attivare in un colpo alla creazione del tenant.
/// Catalogo definito nel codice (come gli scenari), esposto via API; l'UI lo consuma.
pub struct Plan { pub code: &'static str, pub name: &'static str, pub desc: &'static str, pub modules: &'static [&'static str] }
pub fn plan_catalog() -> Vec<Plan> { vec![
    Plan{ code:"sicurezza", name:"Sicurezza lavoratore", desc:"SOS / uomo a terra / reperibilità (Lone Worker).", modules:&["loneworker"] },
    Plan{ code:"campo", name:"Lavoro sul campo", desc:"Interventi, cantieri, ronde, consuntivi, turni.", modules:&["interventi","cantieri","giri","consuntivi","workforce"] },
    Plan{ code:"iot", name:"IoT", desc:"Sensori, telemetria, soglie→incidenti, cruscotti.", modules:&["iot"] },
    Plan{ code:"full", name:"Completo", desc:"Tutti i moduli + assistente AI.", modules:&["loneworker","gps","interventi","cantieri","giri","consuntivi","workforce","iot","assistant"] },
]}
pub async fn super_plans_list(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    Ok(Json(json!(plan_catalog().iter().map(|p| json!({
        "code":p.code,"name":p.name,"desc":p.desc,"modules":p.modules })).collect::<Vec<_>>())))
}
/// Normalizza+valida uno slug tenant (finisce nel sottodominio): minuscole, a-z0-9-, 2..40, niente trattini ai bordi.
pub(crate) fn clean_slug(raw: &str) -> Result<String, (StatusCode,String)> {
    let s: String = raw.trim().to_lowercase().chars()
        .map(|c| if c.is_ascii_alphanumeric() || c=='-' { c } else if c==' '||c=='_' { '-' } else { '\0' })
        .filter(|c| *c!='\0').collect();
    let s = s.trim_matches('-').to_string();
    let ok = (2..=40).contains(&s.len()) && s.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false);
    if !ok { return Err(err(StatusCode::BAD_REQUEST, "slug non valido: usa 2-40 caratteri a-z, 0-9, trattino")); }
    Ok(s)
}
/// Validazione email minimale (formato), non deliverability.
/// Niente virgolette, parentesi angolari ecc.: l'email finisce in pagine e attributi HTML.
pub fn valid_email(e: &str) -> bool {
    let e=e.trim(); let at=e.find('@');
    matches!(at, Some(i) if i>0 && i<e.len()-1) && e.rsplit('@').next().map(|d| d.contains('.')).unwrap_or(false)
        && !e.chars().any(|c| c.is_whitespace() || c.is_control() || "\"'<>`\\(),;:[]".contains(c))
}

pub async fn create_tenant(State(st): State<AppState>, h: HeaderMap, Json(b): Json<CreateTenant>) -> R {
    require_super(&st, &h).await?;
    let slug = clean_slug(&b.slug)?;
    const RESERVED: &[&str] = &["tenants","www","admin","api","app","invite","static","health"];
    if RESERVED.contains(&slug.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "slug riservato")); }
    if b.name.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "nome richiesto")); }
    if !valid_email(&b.admin_email) { return Err(err(StatusCode::BAD_REQUEST, "email admin non valida")); }
    let (bp, bi, bs, ba) = (clean_color(&b.brand_primary)?, clean_color(&b.brand_ink)?, clean_color(&b.brand_soft)?, clean_color(&b.brand_accent)?);
    // Modalità onboarding: password esplicita (demo/dev) oppure INVITO monouso (produzione, default).
    let direct_pw = b.admin_password.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty());
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    let trow = sqlx::query("INSERT INTO app.tenants(slug,name,brand_primary,brand_ink,brand_soft,brand_accent) VALUES($1,$2,$3,$4,$5,$6) RETURNING id")
        .bind(&slug).bind(b.name.trim()).bind(bp).bind(bi).bind(bs).bind(ba).fetch_one(&mut *tx).await
        .map_err(|e| conflict_or(e, "slug già esistente"))?;
    let tid: Uuid = trow.get("id");
    // L'admin è sempre creato; la password è quella fornita (demo) o una random inutilizzabile finché
    // l'admin non accetta l'invito e imposta la propria (produzione: nessuna password nota al superadmin).
    let pass_hash = auth::hash_password(direct_pw.unwrap_or(&auth::new_token()));
    let uid: Uuid = sqlx::query_scalar("INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES($1,$2,$3,'tenant_admin') RETURNING id")
        .bind(tid).bind(b.admin_email.trim()).bind(&pass_hash)
        .fetch_one(&mut *tx).await.map_err(ise)?;
    // licenze iniziali (null = illimitato)
    let lic = |v: Option<i32>| v.filter(|n| *n >= 0);
    sqlx::query("UPDATE app.tenants SET mobile_app_licenses=$2, iot_device_licenses=$3 WHERE id=$1")
        .bind(tid).bind(lic(b.mobile_app_licenses)).bind(lic(b.iot_device_licenses)).execute(&mut *tx).await.map_err(ise)?;
    // seed module rows (all non_abilitato; core identity always on)
    sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state)
        SELECT $1, code, 'non_abilitato' FROM app.modules_catalog")
        .bind(tid).execute(&mut *tx).await.map_err(ise)?;
    // Pacchetto/piano: espande in moduli; unione con eventuali moduli espliciti. Attiva solo i
    // moduli disponibili nel prodotto (available=true). Nessun errore se un codice non esiste.
    let mut mods: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(pc) = b.plan.as_deref() { if let Some(p) = plan_catalog().into_iter().find(|p| p.code==pc) {
        for m in p.modules { mods.insert(m.to_string()); } } }
    if let Some(ms) = &b.modules { for m in ms { mods.insert(m.clone()); } }
    if !mods.is_empty() {
        let codes: Vec<String> = mods.into_iter().collect();
        sqlx::query("UPDATE app.tenant_modules SET state='attivo'
            WHERE tenant_id=$1 AND module_code = ANY($2)
              AND module_code IN (SELECT code FROM app.modules_catalog WHERE available=true)")
            .bind(tid).bind(&codes).execute(&mut *tx).await.map_err(ise)?;
    }
    // Invito monouso (se non è stata data una password diretta): token in chiaro una volta sola.
    let mut invite_token: Option<String> = None;
    if direct_pw.is_none() {
        let tok = auth::new_token();
        let exp = st.clock.now() + chrono::Duration::days(7);
        sqlx::query("INSERT INTO app.user_invites(token_hash,tenant_id,user_id,email,expires_at) VALUES($1,$2,$3,$4,$5)")
            .bind(hash_token(&tok)).bind(tid).bind(uid).bind(b.admin_email.trim()).bind(exp)
            .execute(&mut *tx).await.map_err(ise)?;
        invite_token = Some(tok);
    }
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,'superadmin','create_tenant',$1)")
        .bind(json!({"slug":slug,"onboarding": if invite_token.is_some() {"invite"} else {"password"},
            "plan":b.plan,"scenario":b.scenario})).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    // Scenario opzionale (attiva i suoi moduli + eventuale demo): riusa il motore scenari, post-commit.
    let mut scenario_applied: Option<String> = None;
    if let Some(sc) = b.scenario.as_deref().filter(|s| !s.is_empty()) {
        match scenario_apply_inner(&st, tid, sc, b.with_demo.unwrap_or(false)).await {
            Ok((code,_,_,_)) => scenario_applied = Some(code),
            Err(e) => tracing::warn!("create_tenant scenario {sc}: {}", e.1),
        }
    }
    Ok(Json(json!({"id":tid,"slug":slug,"invite_token":invite_token,"scenario_applied":scenario_applied})))
}
#[derive(Deserialize)]
pub struct InviteAccept { pub slug: String, pub token: String, pub password: String }
/// Accetta un invito: l'admin imposta la PROPRIA password. Pubblico (no sessione), CSRF-exempt, monouso.
pub async fn accept_invite(State(st): State<AppState>, Json(b): Json<InviteAccept>) -> R {
    if b.password.trim().len() < 8 { return Err(err(StatusCode::BAD_REQUEST, "la password deve avere almeno 8 caratteri")); }
    let tid: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.tenants WHERE slug=$1").bind(b.slug.trim().to_lowercase())
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let tid = tid.ok_or_else(|| err(StatusCode::NOT_FOUND, "tenant inesistente"))?;
    let row = sqlx::query("SELECT user_id,expires_at,used_at FROM app.user_invites WHERE token_hash=$1 AND tenant_id=$2")
        .bind(hash_token(b.token.trim())).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let row = row.ok_or_else(|| err(StatusCode::NOT_FOUND, "invito non valido"))?;
    if row.try_get::<chrono::DateTime<chrono::Utc>,_>("used_at").is_ok() { return Err(err(StatusCode::CONFLICT, "invito già usato")); }
    let exp: chrono::DateTime<chrono::Utc> = row.get("expires_at");
    if exp < st.clock.now() { return Err(err(StatusCode::GONE, "invito scaduto")); }
    let uid: Uuid = row.get("user_id");
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    sqlx::query("UPDATE app.users SET pass_hash=$1 WHERE id=$2").bind(auth::hash_password(b.password.trim())).bind(uid).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("UPDATE app.user_invites SET used_at=now() WHERE token_hash=$1").bind(hash_token(b.token.trim())).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"slug":b.slug.trim().to_lowercase()})))
}
/// Verifica un invito (per la pagina di accettazione): ritorna l'email destinataria se valido.
pub async fn check_invite(State(st): State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String,String>>) -> R {
    let (slug, token) = (q.get("slug").cloned().unwrap_or_default(), q.get("token").cloned().unwrap_or_default());
    let tid: Option<Uuid> = sqlx::query_scalar("SELECT id FROM app.tenants WHERE slug=$1").bind(slug.trim().to_lowercase())
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let Some(tid) = tid else { return Ok(Json(json!({"valid":false}))) };
    let row = sqlx::query("SELECT email,expires_at,used_at FROM app.user_invites WHERE token_hash=$1 AND tenant_id=$2")
        .bind(hash_token(token.trim())).bind(tid).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    match row {
        Some(r) if r.try_get::<chrono::DateTime<chrono::Utc>,_>("used_at").is_err()
                && r.get::<chrono::DateTime<chrono::Utc>,_>("expires_at") >= st.clock.now()
            => Ok(Json(json!({"valid":true,"email":r.get::<String,_>("email")}))),
        _ => Ok(Json(json!({"valid":false}))),
    }
}

pub async fn list_tenants(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    // seat app mobile = utenti che usano l'app (lavoratore/reperibile); IoT = dispositivi registrati.
    let rows = sqlx::query(
        "SELECT t.id,t.slug,t.name,t.created_at,t.status,t.suspend_at,t.suspend_note,
            t.mobile_app_licenses, t.iot_device_licenses,
            t.brand_primary,t.brand_ink,t.brand_soft,t.brand_accent,(t.logo IS NOT NULL) AS has_logo,
            (SELECT count(*) FROM app.users u WHERE u.tenant_id=t.id AND u.role IN ('lavoratore','reperibile') AND u.anonymized_at IS NULL) AS mobile_used,
            (SELECT count(*) FROM app.devices d WHERE d.tenant_id=t.id AND d.source<>'pwa') AS iot_used,
            (SELECT count(*) FROM app.tenant_modules m WHERE m.tenant_id=t.id AND m.state='attivo') AS mods_active,
            (SELECT count(*) FROM app.users u WHERE u.tenant_id=t.id) AS users_tot,
            (SELECT count(*) FROM app.sites s WHERE s.tenant_id=t.id) AS sites_tot,
            (SELECT count(*) FROM app.incidents i WHERE i.tenant_id=t.id AND i.state<>'chiuso') AS incidents_open,
            EXISTS(SELECT 1 FROM app.user_invites i WHERE i.tenant_id=t.id AND i.used_at IS NULL AND i.expires_at>now()) AS invite_pending
         FROM app.tenants t ORDER BY t.created_at")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "slug": r.get::<String,_>("slug"), "name": r.get::<String,_>("name"),
        "status": r.try_get::<String,_>("status").unwrap_or_else(|_| "attivo".into()),
        "suspend_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>,_>("suspend_at").ok().flatten(),
        "suspend_note": r.try_get::<Option<String>,_>("suspend_note").ok().flatten(),
        "mobile_app_licenses": r.try_get::<Option<i32>,_>("mobile_app_licenses").ok().flatten(),
        "iot_device_licenses": r.try_get::<Option<i32>,_>("iot_device_licenses").ok().flatten(),
        "mobile_used": r.get::<i64,_>("mobile_used"),
        "iot_used": r.get::<i64,_>("iot_used"),
        "mods_active": r.get::<i64,_>("mods_active"),
        "users_tot": r.get::<i64,_>("users_tot"),
        "sites_tot": r.get::<i64,_>("sites_tot"),
        "incidents_open": r.get::<i64,_>("incidents_open"),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at").to_rfc3339(),
        "invite_pending": r.get::<bool,_>("invite_pending"),
        "brand_primary": r.try_get::<Option<String>,_>("brand_primary").ok().flatten(),
        "brand_ink": r.try_get::<Option<String>,_>("brand_ink").ok().flatten(),
        "brand_soft": r.try_get::<Option<String>,_>("brand_soft").ok().flatten(),
        "brand_accent": r.try_get::<Option<String>,_>("brand_accent").ok().flatten(),
        "has_logo": r.get::<bool,_>("has_logo"),
    })).collect();
    Ok(Json(json!(out)))
}
// ===================== Monitoring sistema (superadmin) =====================
pub(crate) fn sys_env_i64(k: &str, d: i64) -> i64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
/// Soglie infrastrutturali (override via env YW_SYS_*): disco/mem/cpu % e staleness worker (s).
pub fn sys_thresholds() -> (f64,f64,f64,i64) {
    (sys_env_i64("YW_SYS_DISK_MAX_PCT",90) as f64, sys_env_i64("YW_SYS_MEM_MAX_PCT",90) as f64,
     sys_env_i64("YW_SYS_CPU_MAX_PCT",95) as f64, sys_env_i64("YW_SYS_WORKER_STALE_SECS",120))
}
/// Soglie + destinatari EFFETTIVI: valore dal pannello (DB) se impostato, altrimenti default env/prodotto.
pub struct SysEff { pub disk: f64, pub mem: f64, pub cpu: f64, pub worker_stale: i64, pub email: Option<String>, pub phone: Option<String> }
pub async fn sys_settings(st: &AppState) -> SysEff {
    let (d,m,c,w) = sys_thresholds();
    let row = sqlx::query("SELECT disk_max_pct,mem_max_pct,cpu_max_pct,worker_stale_secs,alert_email,alert_phone FROM app.system_settings WHERE id=true")
        .fetch_optional(&st.owner_pool).await.ok().flatten();
    let nz = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    match row {
        Some(r) => SysEff{
            disk: r.try_get::<i32,_>("disk_max_pct").ok().map(|x| x as f64).unwrap_or(d),
            mem:  r.try_get::<i32,_>("mem_max_pct").ok().map(|x| x as f64).unwrap_or(m),
            cpu:  r.try_get::<i32,_>("cpu_max_pct").ok().map(|x| x as f64).unwrap_or(c),
            worker_stale: r.try_get::<i32,_>("worker_stale_secs").ok().map(|x| x as i64).unwrap_or(w),
            email: nz(r.try_get::<String,_>("alert_email").ok()), phone: nz(r.try_get::<String,_>("alert_phone").ok()),
        },
        None => SysEff{ disk:d, mem:m, cpu:c, worker_stale:w, email:None, phone:None },
    }
}
pub(crate) fn lvl_pct(pct: f64, max: f64) -> &'static str { if pct>=max {"bad"} else if pct>=max-10.0 {"warn"} else {"ok"} }
/// Snapshot completo dello stato di sistema: hardware (CPU/RAM/disco/host via sysinfo), DB,
/// runtime app, code e worker. Include una lista `checks` (kind/severity/detail) riusata dal watchdog.
pub async fn system_snapshot(st: &AppState) -> Value {
    // --- hardware (bloccante: eseguito su thread dedicato) ---
    let hw: Value = tokio::task::spawn_blocking(|| {
        use sysinfo::{System, Disks};
        let mut sys = System::new();
        sys.refresh_cpu_all();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let (mut dt, mut dfree) = (0u64, 0u64);
        for d in Disks::new_with_refreshed_list().list() { if d.total_space() > dt { dt = d.total_space(); dfree = d.available_space(); } }
        let (mt, mu) = (sys.total_memory(), sys.used_memory());
        let r1 = |x: f64| (x*10.0).round()/10.0;
        json!({
            "host": System::host_name(), "os": System::name(), "os_version": System::os_version(),
            "kernel": System::kernel_version(), "arch": System::cpu_arch(), "cpus": sys.cpus().len(),
            "cpu_pct": r1(sys.global_cpu_usage() as f64),
            "mem_total": mt, "mem_used": mu, "mem_pct": if mt>0 { r1(mu as f64/mt as f64*100.0) } else {0.0},
            "disk_total": dt, "disk_used": dt.saturating_sub(dfree),
            "disk_pct": if dt>0 { r1((dt-dfree) as f64/dt as f64*100.0) } else {0.0},
        })
    }).await.unwrap_or_else(|_| json!({}));
    // --- DB ---
    let db_size: Option<i64> = sqlx::query_scalar("SELECT pg_database_size(current_database())").fetch_one(&st.owner_pool).await.ok();
    let db_ok = db_size.is_some();
    let db_conns: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database()").fetch_one(&st.owner_pool).await.unwrap_or(0);
    let db_max: i64 = sqlx::query_scalar("SELECT setting::bigint FROM pg_settings WHERE name='max_connections'").fetch_one(&st.owner_pool).await.unwrap_or(0);
    // --- code / runtime ---
    let c = |sql: &'static str| async move { sqlx::query_scalar::<_,i64>(sql).fetch_one(&st.owner_pool).await.unwrap_or(0) };
    let tenants = c("SELECT count(*) FROM app.tenants").await;
    let users = c("SELECT count(*) FROM app.users").await;
    let devices = c("SELECT count(*) FROM app.devices").await;
    let incidents_open = c("SELECT count(*) FROM app.incidents WHERE state<>'chiuso'").await;
    let outbox_pending = c("SELECT count(*) FROM app.outbox WHERE state='pending'").await;
    let outbox_error = c("SELECT count(*) FROM app.outbox WHERE state='error'").await;
    let sessions = c("SELECT count(*) FROM app.sessions").await;
    // --- worker heartbeat ---
    let hb = st.worker_heartbeat.load(std::sync::atomic::Ordering::Relaxed);
    let hb_age = if hb>0 { (chrono::Utc::now().timestamp() - hb).max(0) } else { -1 };
    let eff = sys_settings(st).await;
    let (disk_max, mem_max, cpu_max, worker_stale) = (eff.disk, eff.mem, eff.cpu, eff.worker_stale);
    let disk_pct = hw.get("disk_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let mem_pct = hw.get("mem_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let cpu_pct = hw.get("cpu_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
    // --- checks (kind, severity, detail) ---
    let mut checks: Vec<Value> = Vec::new();
    let mut add = |k:&str,sev:&str,detail:String| checks.push(json!({"kind":k,"severity":sev,"detail":detail}));
    add("db", if db_ok {"ok"} else {"bad"}, if db_ok {"database raggiungibile".into()} else {"database IRRAGGIUNGIBILE".into()});
    if db_max>0 { let cp=db_conns as f64/db_max as f64*100.0; add("db_conns", lvl_pct(cp,90.0), format!("connessioni DB {}/{}", db_conns, db_max)); }
    add("disk", lvl_pct(disk_pct,disk_max), format!("disco al {:.0}%", disk_pct));
    add("mem", lvl_pct(mem_pct,mem_max), format!("memoria al {:.0}%", mem_pct));
    add("cpu", lvl_pct(cpu_pct,cpu_max), format!("CPU al {:.0}%", cpu_pct));
    let wsev = if hb_age<0 || hb_age>worker_stale {"bad"} else if hb_age>worker_stale/2 {"warn"} else {"ok"};
    add("worker", wsev, if hb_age<0 {"worker mai visto".into()} else {format!("ultimo giro worker {}s fa", hb_age)});
    if outbox_error>0 { add("outbox", if outbox_error>20 {"bad"} else {"warn"}, format!("{} messaggi outbox in errore", outbox_error)); }
    // Backlog di ESCALATION ATTIVA del tenant più carico (stesso working-set dello scan consegna):
    // sorveglia la coda che il motore deve drenare. In salute è piccolo; se cresce (emergenza di
    // massa o on-call che non prende in carico) l'alert avvisa il superadmin → mai coda silenziosa.
    let esc_active_max: i64 = sqlx::query_scalar(
        "SELECT COALESCE(max(cnt),0) FROM (
           SELECT count(*) AS cnt FROM app.incidents i
           LEFT JOIN app.esc_alert_state eas ON eas.tenant_id=i.tenant_id AND eas.incident_id=i.id
          WHERE i.state<>'chiuso' AND COALESCE(eas.status,'new') NOT IN ('exhausted','ack','closed')
          GROUP BY i.tenant_id) q")
        .fetch_one(&st.owner_pool).await.unwrap_or(0);
    let (esc_warn, esc_bad) = (sys_env_i64("YW_SYS_ESC_BACKLOG_WARN",500), sys_env_i64("YW_SYS_ESC_BACKLOG_BAD",2000));
    if esc_active_max >= esc_warn {
        add("esc_backlog", if esc_active_max>=esc_bad {"bad"} else {"warn"},
            format!("{} incidenti in escalation attiva su un tenant (coda consegna)", esc_active_max));
    }
    // Chiave dei segreti: bloccata = rosso (AI senza chiavi); senza passphrase di recupero = giallo
    if !st.secrets.enabled() { add("secrets", "bad", "cifratura dei segreti bloccata: sbloccala con la passphrase di recupero".to_string()); }
    else {
        let rec: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.secret_keys WHERE active AND recovery_wrapped IS NOT NULL)")
            .fetch_one(&st.owner_pool).await.unwrap_or(true);
        if rec { add("secrets", "ok", "cifratura dei segreti attiva".to_string()); }
        else { add("secrets", "warn", "imposta una passphrase di recupero per la chiave dei segreti (serve per ripristini e migrazioni)".to_string()); }
    }
    // SA-3: consegna degli allarmi reale, di prova o simulata. In produzione (orologio reale)
    // «simulata» è critico; il check «ok» quando reale chiude l'alert aperto.
    let delivery = crate::delivery::delivery_status();
    match delivery["mode"].as_str().unwrap_or("simulata") {
        "reale" => add("delivery", "ok", "consegna allarmi reale".to_string()),
        "prova" => add("delivery", "warn", delivery["label"].as_str().unwrap_or("").to_string()),
        _ => add("delivery", if st.clock.test_mode {"warn"} else {"bad"}, delivery["label"].as_str().unwrap_or("").to_string()),
    }
    let overall = if checks.iter().any(|c| c["severity"]=="bad") {"bad"} else if checks.iter().any(|c| c["severity"]=="warn") {"warn"} else {"ok"};
    json!({
        "status": overall,
        "delivery": delivery,
        "app": {"version": env!("CARGO_PKG_VERSION"), "uptime_secs": st.started.elapsed().as_secs(), "test_clock": st.clock.test_mode},
        "server": hw,
        "db": {"reachable": db_ok, "size_bytes": db_size, "connections": db_conns, "max_connections": db_max},
        "queues": {"outbox_pending": outbox_pending, "outbox_error": outbox_error, "sessions": sessions,
                   "incidents_open": incidents_open, "esc_active_max": esc_active_max,
                   "tenants": tenants, "users": users, "devices": devices},
        "worker": {"heartbeat_age_secs": hb_age},
        "thresholds": {"disk_max_pct": disk_max, "mem_max_pct": mem_max, "cpu_max_pct": cpu_max, "worker_stale_secs": worker_stale},
        "checks": checks,
    })
}
/// SA-3: stato della consegna allarmi per chi segue gli allarmi o configura l'escalation nel
/// tenant (avviso in Centrale ed Escalation quando non è reale).
pub async fn delivery_status_get(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), ESCALATION_MODS).await?; // AR-1: controllo del modulo dichiarato
    if !has_cap(&s, "incidents.view") && !has_cap(&s, "incidents.handle") && !has_cap(&s, "escalation.manage") {
        return Err(err(StatusCode::FORBIDDEN, "permesso richiesto: vedere gli allarmi"));
    }
    Ok(Json(crate::delivery::delivery_status()))
}
// ---- Chiave dei segreti gestita dal superadmin (decisione PO 25/11/2026, mig 0146) ----
pub(crate) fn host_name() -> String { std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "server".into()) }
/// Avvio: carica le chiavi dal DB aprendo il sigillo del server; se non ce n'è nessuna la genera
/// (sigillata subito). Se la chiave attiva non si apre (DB spostato su un altro server) la
/// cifratura resta BLOCCATA finché il superadmin non la sblocca con la passphrase di recupero.
pub async fn secrets_init(st: &AppState) {
    let rows = sqlx::query("SELECT key_id, sealed_server, active FROM app.secret_keys ORDER BY key_id")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    if rows.is_empty() {
        let dek = crate::crypto::random_key();
        match crate::crypto::seal_server(&dek) {
            Ok(sealed) => {
                let ok = sqlx::query("INSERT INTO app.secret_keys(key_id,sealed_server,server_name,active) VALUES(1,$1,$2,true) ON CONFLICT DO NOTHING")
                    .bind(&sealed).bind(host_name()).execute(&st.owner_pool).await.map(|r| r.rows_affected() > 0).unwrap_or(false);
                if ok { st.secrets.load_key(1, &dek, true); tracing::info!("chiave dei segreti generata e sigillata ({})", crate::crypto::seal_method()); }
                else { Box::pin(secrets_init_existing(st)).await; } // creata nel frattempo da un'altra istanza
            }
            Err(e) => { tracing::error!("chiave dei segreti: sigillo del server non disponibile ({e})"); st.secrets.set_locked(true); }
        }
        return;
    }
    secrets_init_existing(st).await;
}
pub(crate) async fn secrets_init_existing(st: &AppState) {
    let rows = sqlx::query("SELECT key_id, sealed_server, active FROM app.secret_keys ORDER BY key_id")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    let mut active_ok = false;
    for r in &rows {
        let (kid, active): (i32, bool) = (r.get("key_id"), r.get("active"));
        let sealed: Option<Vec<u8>> = r.try_get("sealed_server").ok().flatten();
        match sealed.map(|b| crate::crypto::open_server(&b)) {
            Some(Ok(dek)) => { st.secrets.load_key(kid, &dek, active); if active { active_ok = true; } }
            _ => if active { tracing::warn!("chiave dei segreti {kid}: il sigillo non si apre su questo server"); },
        }
    }
    if !active_ok { st.secrets.set_locked(true); }
}
pub(crate) async fn secrets_status(st: &AppState) -> Result<Value, (StatusCode,String)> {
    let r = sqlx::query("SELECT key_id, server_name, created_at, created_by, recovery_set_at, recovery_wrapped IS NOT NULL AS rec
            FROM app.secret_keys WHERE active").fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let n: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM app.ai_connections WHERE api_key<>'') + (SELECT count(*) FROM app.delivery_channels WHERE value LIKE 'enc:%')").fetch_one(&st.owner_pool).await.map_err(ise)?;
    let legacy: i64 = sqlx::query_scalar("SELECT count(*) FROM app.ai_connections WHERE api_key<>'' AND api_key NOT LIKE 'enc:v3:%'")
        .fetch_one(&st.owner_pool).await.map_err(ise)?;
    Ok(json!({
        "state": if st.secrets.enabled() { "attiva" } else { "bloccata" },
        "key_id": r.as_ref().map(|x| x.get::<i32,_>("key_id")),
        "server": r.as_ref().and_then(|x| x.try_get::<String,_>("server_name").ok()),
        "created_at": r.as_ref().map(|x| x.get::<Dt,_>("created_at").to_rfc3339()),
        "recovery_set": r.as_ref().map(|x| x.get::<bool,_>("rec")).unwrap_or(false),
        "recovery_set_at": r.as_ref().and_then(|x| x.try_get::<Dt,_>("recovery_set_at").ok()).map(|d| d.to_rfc3339()),
        "seal_method": crate::crypto::seal_method(), "secrets": n, "not_migrated": legacy,
        "legacy_env_key": st.secrets.has_legacy(),
    }))
}
pub async fn super_secrets_get(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    Ok(Json(secrets_status(&st).await?))
}
#[derive(Deserialize)] pub struct PassReq { pub passphrase: String, #[serde(default)] pub current: Option<String> }
pub(crate) fn pass_ok(p: &str) -> Result<(), (StatusCode,String)> {
    if p.chars().count() < crate::crypto::MIN_PASSPHRASE {
        return Err(err(StatusCode::BAD_REQUEST, &format!("passphrase di almeno {} caratteri", crate::crypto::MIN_PASSPHRASE))); }
    Ok(())
}
/// Imposta o cambia la passphrase di recupero della chiave attiva (serve la chiave aperta; per
/// cambiarla serve anche quella attuale).
pub async fn super_secrets_recovery(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PassReq>) -> R {
    let sa = require_super(&st, &h).await?;
    pass_ok(&b.passphrase)?;
    let r = sqlx::query("SELECT key_id, recovery_salt, recovery_wrapped FROM app.secret_keys WHERE active").fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::CONFLICT, "nessuna chiave attiva"))?;
    let kid: i32 = r.get("key_id");
    let (salt, wrapped): (Option<Vec<u8>>, Option<Vec<u8>>) = (r.try_get("recovery_salt").ok().flatten(), r.try_get("recovery_wrapped").ok().flatten());
    // la chiave si ricava dal sigillo del server (o, se impostata, dalla passphrase attuale)
    let dek: Vec<u8> = if let (Some(s), Some(w)) = (&salt, &wrapped) {
        let cur = b.current.as_deref().ok_or_else(|| err(StatusCode::BAD_REQUEST, "indica la passphrase attuale"))?;
        crate::crypto::unwrap_recovery(s, w, cur).map_err(|_| err(StatusCode::FORBIDDEN, "passphrase attuale errata"))?
    } else {
        let sealed: Vec<u8> = sqlx::query_scalar("SELECT sealed_server FROM app.secret_keys WHERE key_id=$1").bind(kid)
            .fetch_one(&st.owner_pool).await.map_err(ise)?;
        crate::crypto::open_server(&sealed).map_err(|e| err(StatusCode::CONFLICT, &e))?
    };
    let (ns, nw) = crate::crypto::wrap_recovery(&dek, &b.passphrase).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    sqlx::query("UPDATE app.secret_keys SET recovery_salt=$2, recovery_wrapped=$3, recovery_set_at=now() WHERE key_id=$1")
        .bind(kid).bind(&ns).bind(&nw).execute(&st.owner_pool).await.map_err(ise)?;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'secrets_recovery_set',$2)")
        .bind(format!("superadmin:{}", sa.email)).bind(json!({"key_id":kid,"changed":wrapped.is_some()})).execute(&st.owner_pool).await;
    Ok(Json(secrets_status(&st).await?))
}
/// Sblocca la chiave su QUESTO server con la passphrase di recupero e la risigilla qui.
pub async fn super_secrets_unlock(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PassReq>) -> R {
    // SA-12: con la chiave bloccata l'MFA non si verifica: password + passphrase di recupero bastano
    // per sbloccarla (poi si rientra con il codice). Con la chiave attiva serve il superadmin completo.
    let sa = if st.secrets.enabled() { require_super(&st, &h).await? } else { require_super_or_setup(&st, &h).await? };
    let r = sqlx::query("SELECT key_id, recovery_salt, recovery_wrapped FROM app.secret_keys WHERE active").fetch_optional(&st.owner_pool).await.map_err(ise)?
        .ok_or_else(|| err(StatusCode::CONFLICT, "nessuna chiave attiva"))?;
    let kid: i32 = r.get("key_id");
    let (Some(salt), Some(w)) = (r.try_get::<Option<Vec<u8>>,_>("recovery_salt").ok().flatten(), r.try_get::<Option<Vec<u8>>,_>("recovery_wrapped").ok().flatten())
        else { return Err(err(StatusCode::CONFLICT, "nessuna passphrase di recupero impostata per questa chiave")); };
    let dek = crate::crypto::unwrap_recovery(&salt, &w, &b.passphrase).map_err(|_| err(StatusCode::FORBIDDEN, "passphrase errata"))?;
    let sealed = crate::crypto::seal_server(&dek).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    sqlx::query("UPDATE app.secret_keys SET sealed_server=$2, server_name=$3 WHERE key_id=$1").bind(kid).bind(&sealed).bind(host_name())
        .execute(&st.owner_pool).await.map_err(ise)?;
    st.secrets.load_key(kid, &dek, true);
    let _ = crate::crypto_migrate_hook(&st).await;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'secrets_unlock',$2)")
        .bind(format!("superadmin:{}", sa.email)).bind(json!({"key_id":kid,"server":host_name()})).execute(&st.owner_pool).await;
    Ok(Json(secrets_status(&st).await?))
}
/// Ruota la chiave: nuova chiave sigillata dal server, segreti ricifrati, vecchia chiave eliminata.
/// Se c'è una passphrase di recupero, serve per proteggere anche la nuova chiave.
pub async fn super_secrets_rotate(State(st): State<AppState>, h: HeaderMap, Json(b): Json<PassReq>) -> R {
    let sa = require_super(&st, &h).await?;
    if !st.secrets.enabled() { return Err(err(StatusCode::CONFLICT, "cifratura bloccata: sbloccala prima di ruotare la chiave")); }
    let cur = sqlx::query("SELECT key_id, recovery_salt, recovery_wrapped FROM app.secret_keys WHERE active").fetch_one(&st.owner_pool).await.map_err(ise)?;
    let old: i32 = cur.get("key_id");
    let rec = match (cur.try_get::<Option<Vec<u8>>,_>("recovery_salt").ok().flatten(), cur.try_get::<Option<Vec<u8>>,_>("recovery_wrapped").ok().flatten()) {
        (Some(s), Some(w)) => {
            crate::crypto::unwrap_recovery(&s, &w, &b.passphrase).map_err(|_| err(StatusCode::FORBIDDEN, "passphrase di recupero errata"))?;
            true }
        _ => false,
    };
    let dek = crate::crypto::random_key();
    let sealed = crate::crypto::seal_server(&dek).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    let (rs, rw) = if rec { let (s, w) = crate::crypto::wrap_recovery(&dek, &b.passphrase).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?; (Some(s), Some(w)) } else { (None, None) };
    let new = old + 1;
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    sqlx::query("UPDATE app.secret_keys SET active=false WHERE key_id=$1").bind(old).execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.secret_keys(key_id,sealed_server,server_name,recovery_salt,recovery_wrapped,recovery_set_at,active,created_by)
            VALUES($1,$2,$3,$4,$5,CASE WHEN $4::bytea IS NULL THEN NULL ELSE now() END,true,$6)")
        .bind(new).bind(&sealed).bind(host_name()).bind(&rs).bind(&rw).bind(format!("superadmin:{}", sa.email)).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    st.secrets.load_key(new, &dek, true);
    crate::crypto_migrate_hook(&st).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    // la vecchia chiave NON si elimina subito: altre istanze del core possono ancora usarla finché
    // non ricaricano (secrets_sync, ogni minuto). La pulizia la elimina quando nessun segreto la usa.
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'secrets_rotate',$2)")
        .bind(format!("superadmin:{}", sa.email)).bind(json!({"from":old,"to":new})).execute(&st.owner_pool).await;
    Ok(Json(secrets_status(&st).await?))
}
/// Ultima risorsa (review 25/11): la chiave attiva non si apre su questo server e non c'è
/// passphrase di recupero (o è persa). Si genera una chiave NUOVA e i segreti non più leggibili
/// si svuotano: il superadmin deve reinserire le chiavi dei provider AI. Solo se bloccata.
#[derive(Deserialize)] pub struct ResetReq { pub confirm: String }
pub async fn super_secrets_reset(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ResetReq>) -> R {
    let sa = require_super(&st, &h).await?;
    if st.secrets.enabled() { return Err(err(StatusCode::CONFLICT, "la chiave è attiva: per cambiarla usa la rotazione")); }
    if b.confirm.trim() != "AZZERA" { return Err(err(StatusCode::BAD_REQUEST, "scrivi AZZERA per confermare")); }
    let dek = crate::crypto::random_key();
    let sealed = crate::crypto::seal_server(&dek).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    let mut tx = st.owner_pool.begin().await.map_err(ise)?;
    let new: i32 = sqlx::query_scalar("SELECT COALESCE(max(key_id),0)+1 FROM app.secret_keys").fetch_one(&mut *tx).await.map_err(ise)?;
    // i segreti già nel formato nuovo sono illeggibili (chiave persa): si svuotano. Quelli in chiaro
    // o della vecchia YW_SECRET_KEY li ricifra la conversione qui sotto.
    let lost = sqlx::query("UPDATE app.ai_connections SET api_key='' WHERE api_key LIKE 'enc:v3:%'").execute(&mut *tx).await.map_err(ise)?.rows_affected()
        + sqlx::query("DELETE FROM app.delivery_channels WHERE value LIKE 'enc:v3:%'").execute(&mut *tx).await.map_err(ise)?.rows_affected();
    // SA-12: con la chiave persa anche l'MFA dei superadmin va riattivato (al prossimo accesso)
    sqlx::query("UPDATE app.superadmins SET totp_secret=NULL, totp_pending=NULL, totp_enabled_at=NULL, totp_last_step=NULL WHERE totp_secret LIKE 'enc:v3:%' OR totp_pending LIKE 'enc:v3:%'")
        .execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.secret_keys").execute(&mut *tx).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.secret_keys(key_id,sealed_server,server_name,active,created_by) VALUES($1,$2,$3,true,$4)")
        .bind(new).bind(&sealed).bind(host_name()).bind(format!("superadmin:{}", sa.email)).execute(&mut *tx).await.map_err(ise)?;
    tx.commit().await.map_err(ise)?;
    st.secrets.load_key(new, &dek, true);
    let _ = crate::crypto_migrate_hook(&st).await;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'secrets_reset',$2)")
        .bind(format!("superadmin:{}", sa.email)).bind(json!({"key_id":new,"secrets_cleared":lost,"server":host_name()})).execute(&st.owner_pool).await;
    let mut v = secrets_status(&st).await?;
    v["secrets_cleared"] = json!(lost);
    Ok(Json(v))
}
/// Worker, ogni minuto: se un'altra istanza ha ruotato/sbloccato la chiave, la si ricarica e si
/// ricifrano i segreti; poi si eliminano le chiavi vecchie che nessun segreto usa più (dopo 5
/// minuti dalla rotazione, così ogni istanza ha avuto il tempo di ricaricare).
pub async fn secrets_sync(st: &AppState) {
    let db_active: Option<i32> = sqlx::query_scalar("SELECT key_id FROM app.secret_keys WHERE active")
        .fetch_optional(&st.owner_pool).await.ok().flatten();
    if db_active.is_some() && db_active != st.secrets.active_id() {
        tracing::info!("chiave dei segreti cambiata da un'altra istanza ({:?} -> {:?}): ricarico", st.secrets.active_id(), db_active);
        secrets_init_existing(st).await;
        if st.secrets.active_id() != db_active { st.secrets.set_locked(true); }
    }
    load_delivery_channels(st).await; // SA-11: modifiche fatte da altre istanze
    if !st.secrets.enabled() { return; }
    let _ = crate::crypto_migrate_hook(st).await;
    let old: Vec<i32> = sqlx::query_scalar("SELECT k.key_id FROM app.secret_keys k WHERE NOT k.active
            AND EXISTS (SELECT 1 FROM app.secret_keys a WHERE a.active AND a.created_at < now()-interval '5 minutes')
            AND NOT EXISTS (SELECT 1 FROM app.ai_connections c WHERE c.api_key LIKE 'enc:v3:'||k.key_id||':%')
            AND NOT EXISTS (SELECT 1 FROM app.delivery_channels c WHERE c.value LIKE 'enc:v3:'||k.key_id||':%')
            AND NOT EXISTS (SELECT 1 FROM app.superadmins a WHERE a.totp_secret LIKE 'enc:v3:'||k.key_id||':%' OR a.totp_pending LIKE 'enc:v3:'||k.key_id||':%')")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for k in old {
        if sqlx::query("DELETE FROM app.secret_keys WHERE key_id=$1 AND NOT active").bind(k).execute(&st.owner_pool).await.is_ok() {
            st.secrets.forget_key(k);
            tracing::info!("chiave dei segreti {k} eliminata: nessun segreto la usa più");
        }
    }
}
// ---- SA-11: canali di consegna (eccezioni dichiarate), configurati dal superadmin ----
pub(crate) fn chan_secret(key: &str) -> Option<bool> { crate::delivery::CHANNEL_KEYS.iter().find(|(k,_,_)| *k == key).map(|(_,s,_)| *s) }
/// Carica in memoria i canali impostati dal superadmin (decifrando i segreti). Avvio, salvataggio,
/// ogni minuto (altre istanze). Un segreto non decifrabile (chiave bloccata) resta fuori: vale
/// allora il valore di installazione, mai il testo cifrato.
pub async fn load_delivery_channels(st: &AppState) {
    let rows = match sqlx::query("SELECT key,value FROM app.delivery_channels").fetch_all(&st.owner_pool).await { Ok(r) => r, Err(_) => return };
    let mut m = std::collections::HashMap::new();
    for r in rows {
        let (k, v): (String, String) = (r.get("key"), r.get("value"));
        let plain = if chan_secret(&k) == Some(true) { st.secrets.decrypt(&v) } else { v };
        if plain.starts_with("enc:") { tracing::warn!("canale {k}: segreto non decifrabile (chiave bloccata)"); continue; }
        m.insert(k, plain);
    }
    crate::delivery::set_channels(m);
}
pub async fn super_channels_get(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let out: Vec<Value> = crate::delivery::CHANNEL_KEYS.iter().map(|(k, secret, label)| {
        let v = crate::delivery::chan(k);
        json!({"key":k,"label":label,"secret":secret,"set":v.is_some(),"source":crate::delivery::chan_source(k),
               "value": if *secret { Value::Null } else { v.map(Value::String).unwrap_or(Value::Null) }})
    }).collect();
    Ok(Json(json!({"channels":out,"delivery":crate::delivery::delivery_status(),"encryption":st.secrets.enabled()})))
}
#[derive(Deserialize)] pub struct ChannelSet { pub key: String, #[serde(default)] pub value: Option<String> }
pub async fn super_channels_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<ChannelSet>) -> R {
    let sa = require_super(&st, &h).await?;
    let secret = chan_secret(&b.key).ok_or_else(|| err(StatusCode::BAD_REQUEST, "canale sconosciuto"))?;
    let v = b.value.as_deref().map(str::trim).filter(|x| !x.is_empty());
    match v {
        None => { sqlx::query("DELETE FROM app.delivery_channels WHERE key=$1").bind(&b.key).execute(&st.owner_pool).await.map_err(ise)?; }
        Some(val) => {
            if b.key == "YW_ESC_FROM" && !is_e164(val) { return Err(err(StatusCode::BAD_REQUEST, "numero mittente in formato internazionale, es. +390212345678")); }
            if b.key == "YW_ESC_TWILIO_LIVE" && val != "0" && val != "1" { return Err(err(StatusCode::BAD_REQUEST, "invio reale: 1 = acceso, 0 = spento")); }
            if b.key == "YW_ESC_SMTP_RELAY_URL" && !(val.starts_with("https://") || val.starts_with("http://")) { return Err(err(StatusCode::BAD_REQUEST, "URL del relay non valido")); }
            let stored = if secret { st.secrets.encrypt(val).map_err(|e| err(StatusCode::CONFLICT, &format!("cifratura dei segreti non disponibile: {e}")))? } else { val.to_string() };
            sqlx::query("INSERT INTO app.delivery_channels(key,value,updated_by) VALUES($1,$2,$3)
                    ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=now(), updated_by=EXCLUDED.updated_by")
                .bind(&b.key).bind(&stored).bind(format!("superadmin:{}", sa.email)).execute(&st.owner_pool).await.map_err(ise)?;
        }
    }
    load_delivery_channels(&st).await;
    // mai il valore nell'audit: solo quale canale e se è stato impostato o tolto
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'delivery_channel_set',$2)")
        .bind(format!("superadmin:{}", sa.email)).bind(json!({"key":b.key,"set":v.is_some()})).execute(&st.owner_pool).await;
    super_channels_get(State(st), h).await
}
pub async fn super_system(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    Ok(Json(system_snapshot(&st).await))
}
pub async fn super_system_alerts(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query("SELECT id,kind,severity,detail,created_at,notified,resolved_at FROM app.system_alerts ORDER BY created_at DESC LIMIT 100")
        .fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.get::<Uuid,_>("id"), "kind": r.get::<String,_>("kind"), "severity": r.get::<String,_>("severity"),
        "detail": r.get::<String,_>("detail"), "created_at": r.get::<chrono::DateTime<chrono::Utc>,_>("created_at").to_rfc3339(),
        "notified": r.get::<bool,_>("notified"),
        "resolved_at": r.try_get::<chrono::DateTime<chrono::Utc>,_>("resolved_at").ok().map(|d| d.to_rfc3339()),
    })).collect();
    Ok(Json(json!(out)))
}
#[derive(Deserialize)] pub struct SysSettingsReq {
    #[serde(default)] pub disk_max_pct: Option<i32>, #[serde(default)] pub mem_max_pct: Option<i32>,
    #[serde(default)] pub cpu_max_pct: Option<i32>, #[serde(default)] pub worker_stale_secs: Option<i32>,
    #[serde(default)] pub alert_email: Option<String>, #[serde(default)] pub alert_phone: Option<String> }
/// Legge la configurazione monitoring (valori salvati dal pannello + effettivi + default env).
pub async fn super_system_settings_get(State(st): State<AppState>, h: HeaderMap) -> R {
    require_super(&st, &h).await?;
    let r = sqlx::query("SELECT disk_max_pct,mem_max_pct,cpu_max_pct,worker_stale_secs,alert_email,alert_phone FROM app.system_settings WHERE id=true")
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let (dd,dm,dc,dw) = sys_thresholds();
    let stored = match &r { Some(r) => json!({
        "disk_max_pct": r.try_get::<i32,_>("disk_max_pct").ok(), "mem_max_pct": r.try_get::<i32,_>("mem_max_pct").ok(),
        "cpu_max_pct": r.try_get::<i32,_>("cpu_max_pct").ok(), "worker_stale_secs": r.try_get::<i32,_>("worker_stale_secs").ok(),
        "alert_email": r.try_get::<String,_>("alert_email").ok(), "alert_phone": r.try_get::<String,_>("alert_phone").ok() }),
        None => json!({}) };
    Ok(Json(json!({"stored": stored, "defaults": {"disk_max_pct":dd as i64,"mem_max_pct":dm as i64,"cpu_max_pct":dc as i64,"worker_stale_secs":dw}})))
}
/// Salva la configurazione monitoring (soglie + destinatari). Vuoto/None = usa il default.
pub async fn super_system_settings_set(State(st): State<AppState>, h: HeaderMap, Json(b): Json<SysSettingsReq>) -> R {
    let s = require_super(&st, &h).await?;
    let pct = |v: Option<i32>, name: &str| -> Result<Option<i32>,(StatusCode,String)> {
        match v { Some(x) if !(1..=100).contains(&x) => Err(err(StatusCode::BAD_REQUEST, &format!("{name}: usa 1-100"))), x => Ok(x) } };
    let disk=pct(b.disk_max_pct,"disco")?; let mem=pct(b.mem_max_pct,"memoria")?; let cpu=pct(b.cpu_max_pct,"cpu")?;
    let worker = match b.worker_stale_secs { Some(x) if !(10..=86400).contains(&x) => return Err(err(StatusCode::BAD_REQUEST,"worker: 10-86400 secondi")), x => x };
    let nz = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let (email, phone) = (nz(b.alert_email), nz(b.alert_phone));
    let (has_email, has_phone) = (email.is_some(), phone.is_some());
    sqlx::query("UPDATE app.system_settings SET disk_max_pct=$1,mem_max_pct=$2,cpu_max_pct=$3,worker_stale_secs=$4,alert_email=$5,alert_phone=$6,updated_at=now() WHERE id=true")
        .bind(disk).bind(mem).bind(cpu).bind(worker).bind(&email).bind(&phone)
        .execute(&st.owner_pool).await.map_err(ise)?;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,$1,'system_settings',$2)")
        .bind(&s.email).bind(json!({"disk":disk,"mem":mem,"cpu":cpu,"worker":worker,"email":has_email,"phone":has_phone})).execute(&st.owner_pool).await;
    Ok(Json(json!({"ok":true})))
}
/// Watchdog infrastrutturale: valuta i check di sistema, apre un alert per ogni problema NUOVO
/// (dedup: un alert aperto per kind entro 60 min), notifica il superadmin (sink/email/SMS) e
/// risolve gli alert quando il check torna ok. Chiamato dal worker.
pub async fn system_watchdog(st: &AppState) {
    let snap = system_snapshot(st).await;
    let checks = snap.get("checks").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for c in &checks {
        let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let sev = c.get("severity").and_then(|v| v.as_str()).unwrap_or("ok");
        let detail = c.get("detail").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if kind.is_empty() { continue; }
        if sev == "ok" {
            let _ = sqlx::query("UPDATE app.system_alerts SET resolved_at=now() WHERE kind=$1 AND resolved_at IS NULL")
                .bind(kind).execute(&st.owner_pool).await;
            continue;
        }
        // dedup: già un alert aperto per questo kind nell'ultima ora?
        let recent: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM app.system_alerts WHERE kind=$1 AND resolved_at IS NULL AND created_at > now() - interval '60 minutes' LIMIT 1")
            .bind(kind).fetch_optional(&st.owner_pool).await.ok().flatten();
        if recent.is_some() { continue; }
        let id: Option<Uuid> = sqlx::query_scalar("INSERT INTO app.system_alerts(kind,severity,detail) VALUES($1,$2,$3) RETURNING id")
            .bind(kind).bind(sev).bind(&detail).fetch_one(&st.owner_pool).await.ok();
        let sent = system_alert_notify(st, sev, kind, &detail).await;
        if sent { if let Some(i)=id { let _=sqlx::query("UPDATE app.system_alerts SET notified=true WHERE id=$1").bind(i).execute(&st.owner_pool).await; } }
        let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,'system','system_alert',$1)")
            .bind(json!({"kind":kind,"severity":sev,"detail":detail,"notified":sent})).execute(&st.owner_pool).await;
        tracing::warn!("system alert [{sev}] {kind}: {detail} (notified={sent})");
    }
}
/// Notifica un alert di sistema al superadmin su tutti i canali configurati (sink webhook per e2e,
/// email relay, SMS Twilio). Ritorna true se almeno un canale ha consegnato.
pub(crate) async fn system_alert_notify(st: &AppState, sev: &str, kind: &str, detail: &str) -> bool {
    let subj = format!("[YW {}] {} — {}", sev.to_uppercase(), kind, detail);
    let eff = sys_settings(st).await;
    let mut emails: Vec<String> = sqlx::query_scalar("SELECT email FROM app.superadmins").fetch_all(&st.owner_pool).await.unwrap_or_default();
    // destinatari extra dal pannello (o env legacy), separati da virgola
    let extra = eff.email.clone().or_else(|| std::env::var("YW_SYS_ALERT_EMAIL").ok());
    if let Some(x) = extra { for e in x.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) { emails.push(e); } }
    let alert_phone = eff.phone.clone().or_else(|| std::env::var("YW_SYS_ALERT_PHONE").ok()).filter(|s| !s.trim().is_empty());
    let mut any = false;
    // 1) sink webhook (e2e + integrazione generica)
    if let Ok(sink) = std::env::var("YW_ESC_SINK_URL") { if !sink.trim().is_empty() {
        let body = json!({"kind":"system_alert","severity":sev,"check":kind,"text":subj});
        if crate::delivery::deliver_to_sink(&st.http, &sink, &body).await.state == "sent" { any = true; }
    }}
    // 2) email relay ai superadmin + destinatari extra dal pannello
    if let Some(relay) = crate::delivery::chan("YW_ESC_SMTP_RELAY_URL") { if !relay.trim().is_empty() {
        for e in &emails { let body = json!({"to":e,"subject":subj,"text":detail});
            if crate::delivery::deliver_email_relay(&st.http, &relay, &body).await.state == "live" { any = true; } }
    }}
    // 3) SMS Twilio (gated live) al numero configurato dal pannello (o env)
    if crate::delivery::TwilioCfg::live_enabled() { if let (Some(cfg), Some(ph)) = (crate::delivery::TwilioCfg::from_env(), alert_phone.clone()) {
        if crate::delivery::deliver_twilio_live(&st.http, &cfg, crate::delivery::Channel::Sms, &ph, &subj, "system").await.state == "live" { any = true; }
    }}
    any
}
#[derive(Deserialize)] pub struct TenantStatusReq {
    pub status: String,
    /// solo per 'sospeso': data/ora futura (RFC 3339) -> sospensione PROGRAMMATA con avviso all'admin.
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    pub note: Option<String>,
}
/// Chiude le sessioni del tenant (non quelle d'impersonazione del superadmin per assistenza).
pub(crate) async fn close_tenant_sessions(st: &AppState, tid: Uuid) -> u64 {
    sqlx::query("DELETE FROM app.sessions WHERE tenant_id=$1 AND impersonator IS NULL")
        .bind(tid).execute(&st.owner_pool).await.map(|r| r.rows_affected()).unwrap_or(0)
}
/// Sospende/riattiva un tenant. Superadmin. SA-5 (decisione PO): un tenant SOSPESO non riceve né
/// scala allarmi (moduli spenti in `module_active`, worker ed escalation fermi), i suoi utenti sono
/// disconnessi e non accedono. Con `at` nel futuro la sospensione è PROGRAMMATA: il tenant resta
/// attivo, l'amministratore vede l'avviso con data e note, alla data il worker la applica.
/// 'attivo' riattiva e annulla una sospensione programmata.
pub async fn set_tenant_status(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<TenantStatusReq>) -> R {
    let sa_ = require_super(&st, &h).await?;
    if !["attivo","sospeso"].contains(&b.status.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "stato non valido")); }
    let note = b.note.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.chars().take(500).collect::<String>());
    let now = st.clock.now();
    let scheduled = b.status == "sospeso" && b.at.map_or(false, |a| a > now);
    let q = if b.status == "attivo" {
        sqlx::query("UPDATE app.tenants SET status='attivo', suspend_at=NULL, suspend_note=NULL WHERE id=$1").bind(tid)
    } else if scheduled {
        sqlx::query("UPDATE app.tenants SET suspend_at=$2, suspend_note=$3 WHERE id=$1").bind(tid).bind(b.at).bind(note.clone())
    } else {
        sqlx::query("UPDATE app.tenants SET status='sospeso', suspend_at=NULL, suspend_note=COALESCE($2,suspend_note) WHERE id=$1").bind(tid).bind(note.clone())
    };
    let n = q.execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    if n==0 { return Err(err(StatusCode::NOT_FOUND, "tenant inesistente")); }
    let closed = if b.status == "sospeso" && !scheduled { close_tenant_sessions(&st, tid).await } else { 0 };
    let eff = if scheduled { "sospensione_programmata" } else { b.status.as_str() };
    let at = if scheduled { b.at } else { None };
    audit(&st, tid, &format!("superadmin:{}", sa_.email), "tenant_status", json!({"status":eff,"at":at,"note":note,"sessions_closed":closed})).await;
    Ok(Json(json!({"ok":true,"status": if scheduled {"attivo"} else {b.status.as_str()},
        "suspend_at": at, "sessions_closed": closed})))
}
/// Worker (~60 s): applica le sospensioni programmate giunte a scadenza (orologio unico st.clock).
pub async fn tenant_suspension_scan(st: &AppState) {
    let now = st.clock.now();
    let due: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE app.tenants SET status='sospeso', suspend_at=NULL
          WHERE status='attivo' AND suspend_at IS NOT NULL AND suspend_at <= $1 RETURNING id")
        .bind(now).fetch_all(&st.owner_pool).await.unwrap_or_default();
    for tid in due {
        let closed = close_tenant_sessions(st, tid).await;
        audit(st, tid, "system", "tenant_status", json!({"status":"sospeso","programmata":true,"sessions_closed":closed})).await;
        tracing::warn!(tenant=%tid, "sospensione programmata applicata: allarmi e accessi fermi");
    }
}
/// SA-6 (decisione PO 24/11/2026): le licenze sono SOLO INDICATIVE, mai bloccanti. Uso:
/// - app mobile = utenti con ruolo lavoratore o reperibile non anonimizzati (i ruoli personalizzati
///   NON contano come utenti: aggiungono permessi a un utente, non un posto);
/// - dispositivi = dispositivi censiti esclusa l'app personale del lavoratore (`pwa`).
/// Ritorna l'elenco dei superamenti (vuoto se tutto entro i limiti o limiti assenti).
pub async fn license_overage(st: &AppState, tid: Uuid) -> Vec<Value> {
    let r = sqlx::query(
        "SELECT t.mobile_app_licenses AS ml, t.iot_device_licenses AS il,
           (SELECT count(*) FROM app.users u WHERE u.tenant_id=t.id AND u.role IN ('lavoratore','reperibile') AND u.anonymized_at IS NULL) AS mu,
           (SELECT count(*) FROM app.devices d WHERE d.tenant_id=t.id AND d.source<>'pwa') AS iu
         FROM app.tenants t WHERE t.id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(r) = r else { return vec![] };
    let mut out = vec![];
    let check = |out: &mut Vec<Value>, kind: &str, label: &str, used: i64, lim: Option<i32>| {
        if let Some(l) = lim { if used > l as i64 {
            out.push(json!({"kind":kind,"label":label,"used":used,"limit":l}));
        } }
    };
    check(&mut out, "mobile", "utenti dell'app (lavoratori e reperibili)", r.get::<i64,_>("mu"), r.try_get::<Option<i32>,_>("ml").ok().flatten());
    check(&mut out, "iot", "dispositivi", r.get::<i64,_>("iu"), r.try_get::<Option<i32>,_>("il").ok().flatten());
    out
}
// ============================================================================
// SL-2 — Rilevamento NATIVO dell'immobilità (mig 0133). Per i lavoratori di un profilo di protezione
// con l'immobilità attiva: nessuno spostamento oltre la soglia del profilo (né movimento segnalato dal
// sensore del badge, IO 240) per `immobility_after_s` -> incidente critico `immobilita`. Prima, a
// `after_s - prewarn_s`, l'app mostra «Stai bene?»: toccando «Sto bene» il conteggio riparte.
// Vale solo con dispositivo in servizio che sta trasmettendo e, se richiesto, durante il turno.
// ============================================================================
/// Registra una posizione/segnale: se è uno spostamento reale (oltre la soglia del profilo del
/// lavoratore, default 15 m) o il badge segnala movimento, riparte il conteggio dell'immobilità.
pub async fn immobility_note_movement(st: &AppState, tid: Uuid, dev_id: Uuid, lat: f64, lon: f64,
    attrs: Option<&Value>, at: chrono::DateTime<chrono::Utc>) {
    let r = sqlx::query(
        "SELECT d.move_anchor_lat, d.move_anchor_lon,
                (SELECT p.immobility_min_move_m FROM app.protection_profile_members m
                   JOIN app.protection_profiles p ON p.id=m.profile_id AND p.enabled AND p.immobility_enabled
                  WHERE m.tenant_id=d.tenant_id AND m.user_id=d.assigned_user ORDER BY p.immobility_min_move_m LIMIT 1) AS mm
           FROM app.devices d WHERE d.id=$1 AND d.tenant_id=$2")
        .bind(dev_id).bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(r) = r else { return };
    let min_move = r.try_get::<Option<i32>,_>("mm").ok().flatten().unwrap_or(15) as f64;
    let sensor_moving = attrs.and_then(|a| a.get("movement")).and_then(|v| v.as_f64().or_else(|| v.as_bool().map(|b| if b {1.0} else {0.0})))
        .map(|v| v >= 1.0).unwrap_or(false);
    let moved = match (r.try_get::<Option<f64>,_>("move_anchor_lat").ok().flatten(), r.try_get::<Option<f64>,_>("move_anchor_lon").ok().flatten()) {
        (Some(alat), Some(alon)) => sensor_moving || haversine_m(alat, alon, lat, lon) >= min_move,
        _ => true, // primo fix: punto di riferimento e inizio del conteggio
    };
    if moved {
        let _ = sqlx::query("UPDATE app.devices SET moved_at=$3, move_anchor_lat=$4, move_anchor_lon=$5
                             WHERE id=$1 AND tenant_id=$2 AND (moved_at IS NULL OR moved_at <= $3)")
            .bind(dev_id).bind(tid).bind(at).bind(lat).bind(lon).execute(&st.owner_pool).await;
    }
}

/// Worker (~10 s): preavvisi e allarmi di immobilità. Orologio unico `st.clock`.
pub async fn immobility_watchdog(st: &AppState) {
    let now = st.clock.now();
    let rows = sqlx::query(
        "SELECT DISTINCT ON (d.id) d.id, d.tenant_id, d.external_id, d.source, d.assigned_user, d.moved_at,
                d.last_lat, d.last_lon, d.imm_prewarn_for, d.imm_alarm_for,
                p.immobility_after_s AS after_s, p.immobility_prewarn_s AS pre_s
           FROM app.devices d
           JOIN app.protection_profile_members m ON m.tenant_id=d.tenant_id AND m.user_id=d.assigned_user
           JOIN app.protection_profiles p ON p.id=m.profile_id AND p.tenant_id=d.tenant_id AND p.enabled AND p.immobility_enabled
           JOIN app.tenants t ON t.id=d.tenant_id AND t.status<>'sospeso'
          WHERE d.assigned_user IS NOT NULL AND COALESCE(d.enabled,true) AND d.moved_at IS NOT NULL
            AND d.source IN ('gps','pwa') AND (d.source<>'gps' OR d.service_state='in_servizio')
            AND GREATEST(d.last_seen_at, d.last_pos_at) >= $1 - make_interval(secs => p.immobility_after_s)
            AND (NOT p.immobility_shift_only OR EXISTS (SELECT 1 FROM app.shifts s
                   WHERE s.tenant_id=d.tenant_id AND s.user_id=d.assigned_user AND s.state='in_corso'))
            AND EXISTS (SELECT 1 FROM app.tenant_modules tm WHERE tm.tenant_id=d.tenant_id
                          AND tm.module_code IN ('loneworker','gps') AND tm.state='attivo')
          ORDER BY d.id, p.immobility_after_s ASC")
        .bind(now).fetch_all(&st.owner_pool).await.unwrap_or_default();
    for r in rows {
        let dev: Uuid = r.get("id"); let tid: Uuid = r.get("tenant_id");
        let moved: chrono::DateTime<chrono::Utc> = r.get("moved_at");
        let after_s = r.get::<i32,_>("after_s") as i64; let pre_s = r.get::<i32,_>("pre_s") as i64;
        let elapsed = (now - moved).num_seconds();
        let pre_for: Option<chrono::DateTime<chrono::Utc>> = r.try_get("imm_prewarn_for").ok().flatten();
        let alarm_for: Option<chrono::DateTime<chrono::Utc>> = r.try_get("imm_alarm_for").ok().flatten();
        let uid: Option<Uuid> = r.try_get("assigned_user").ok().flatten();
        if pre_s > 0 && elapsed >= after_s - pre_s && elapsed < after_s && pre_for != Some(moved) && alarm_for != Some(moved) {
            let _ = sqlx::query("UPDATE app.devices SET imm_prewarn_for=$2 WHERE id=$1").bind(dev).bind(moved).execute(&st.owner_pool).await;
            publish_event(st, TenantEvent{tenant_id: tid, payload: json!({"type":"immobility_prewarn","user_id":uid,"device_id":dev,"alarm_in_s":after_s-elapsed})}).await;
        }
        if elapsed >= after_s && alarm_for != Some(moved) {
            // presa atomica: un solo allarme per periodo di immobilità anche con più istanze
            let claimed = sqlx::query("UPDATE app.devices SET imm_alarm_for=$2 WHERE id=$1 AND imm_alarm_for IS DISTINCT FROM $2 RETURNING id")
                .bind(dev).bind(moved).fetch_optional(&st.owner_pool).await.ok().flatten();
            if claimed.is_none() { continue; }
            let ext: String = r.get("external_id"); let source: String = r.get("source");
            let key = format!("imm-{dev}-{}", moved.timestamp());
            let _ = persist_ingest(st, tid, Normalized{
                source, source_event: key.clone(), idem: key.clone(), etype: "immobilita".into(),
                device_external_id: Some(ext), site_name: None,
                lat: r.try_get("last_lat").ok().flatten(), lon: r.try_get("last_lon").ok().flatten(), accuracy_m: None,
                correlation: key, payload: json!({"immobile_da_s": elapsed, "soglia_s": after_s, "fermo_dal": moved}),
                indoor: false, beacon_uid: None, zone: None }).await;
        }
    }
}

/// App del lavoratore: stato dell'immobilità (preavviso in corso? allarme già aperto?).
pub async fn worker_immobility_status(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("immobility")).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    let now = st.clock.now();
    let r = sqlx::query(
        "SELECT d.moved_at, d.imm_prewarn_for, d.imm_alarm_for, p.immobility_after_s AS after_s
           FROM app.devices d
           JOIN app.protection_profile_members m ON m.tenant_id=d.tenant_id AND m.user_id=d.assigned_user
           JOIN app.protection_profiles p ON p.id=m.profile_id AND p.enabled AND p.immobility_enabled
          WHERE d.tenant_id=$1 AND d.assigned_user=$2 AND d.moved_at IS NOT NULL
          ORDER BY (d.imm_prewarn_for = d.moved_at) DESC NULLS LAST, d.moved_at ASC LIMIT 1")
        .bind(tid).bind(s.user_id).fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let Some(r) = r else { return Ok(Json(json!({"state":"non_attivo"}))) };
    let moved: chrono::DateTime<chrono::Utc> = r.get("moved_at");
    let pre: Option<chrono::DateTime<chrono::Utc>> = r.try_get("imm_prewarn_for").ok().flatten();
    let alarm: Option<chrono::DateTime<chrono::Utc>> = r.try_get("imm_alarm_for").ok().flatten();
    let after_s = r.get::<i32,_>("after_s") as i64;
    Ok(Json(if alarm == Some(moved) { json!({"state":"allarme"}) }
        else if pre == Some(moved) { json!({"state":"preavviso","alarm_in_s": (after_s - (now - moved).num_seconds()).max(0)}) }
        else { json!({"state":"ok"}) }))
}
/// «Sto bene»: il lavoratore conferma di stare bene, il conteggio riparte da adesso.
pub async fn worker_immobility_ok(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    require_any_module(&st, s.tenant_id.unwrap(), crate::modules::feature_modules("immobility")).await?; // AR-1: controllo del modulo dichiarato
    let tid = s.tenant_id.unwrap();
    let now = st.clock.now();
    let n = sqlx::query("UPDATE app.devices SET moved_at=$3, imm_prewarn_for=NULL WHERE tenant_id=$1 AND assigned_user=$2 AND moved_at IS NOT NULL")
        .bind(tid).bind(s.user_id).bind(now).execute(&st.owner_pool).await.map_err(ise)?.rows_affected();
    audit(&st, tid, &s.email, "immobility_ok", json!({"dispositivi":n})).await;
    Ok(Json(json!({"ok":true})))
}

/// true se il tenant è sospeso: niente ricezione, niente escalation, niente accessi.
pub async fn tenant_suspended(st: &AppState, tid: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT status='sospeso' FROM app.tenants WHERE id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten().unwrap_or(false)
}
#[derive(Deserialize)] pub struct DeleteTenantReq { pub slug: String }
/// Elimina DEFINITIVAMENTE un tenant e tutti i suoi dati (FK ON DELETE CASCADE). Richiede
/// la conferma dello slug esatto nel body (anti-errore). Superadmin.
pub async fn delete_tenant(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<DeleteTenantReq>) -> R {
    require_super(&st, &h).await?;
    let slug: Option<String> = sqlx::query_scalar("SELECT slug FROM app.tenants WHERE id=$1").bind(tid)
        .fetch_optional(&st.owner_pool).await.map_err(ise)?;
    let slug = slug.ok_or_else(|| err(StatusCode::NOT_FOUND, "tenant inesistente"))?;
    if b.slug.trim() != slug { return Err(err(StatusCode::BAD_REQUEST, "conferma slug non corrispondente")); }
    // audit PRIMA della delete (le righe audit del tenant sparirebbero con la cascade): riga globale.
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES(NULL,'superadmin','delete_tenant',$1)")
        .bind(json!({"slug":slug,"tenant_id":tid})).execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("DELETE FROM app.tenants WHERE id=$1").bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true,"slug":slug})))
}

// ---- superadmin: set licenze + branding del tenant (il form invia sempre tutti i campi) ----
// licenze: null = illimitato. colori: stringa vuota = reset al default globale YouCo.
#[derive(Deserialize)]
pub struct TenantAdminPatch {
    pub mobile_app_licenses: Option<i32>, pub iot_device_licenses: Option<i32>,
    pub brand_primary: Option<String>, pub brand_ink: Option<String>,
    pub brand_soft: Option<String>, pub brand_accent: Option<String>,
}
// Valida un colore CSS semplice (#rgb/#rrggbb/#rrggbbaa). Vuoto/None = reset al default.
pub(crate) fn clean_color(v: &Option<String>) -> Result<Option<String>, (StatusCode,String)> {
    match v.as_deref().map(|s| s.trim()) {
        None | Some("") => Ok(None),
        Some(s) => {
            let ok = s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
                && (s.len()==4 || s.len()==7 || s.len()==9);
            if ok { Ok(Some(s.to_string())) } else { Err(err(StatusCode::BAD_REQUEST, "colore non valido (usa #rgb o #rrggbb)")) }
        }
    }
}
pub async fn super_set_tenant_admin(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<TenantAdminPatch>) -> R {
    let s = require_super(&st, &h).await?;
    let lic = |v: Option<i32>| v.filter(|n| *n >= 0); // negativi -> illimitato
    let (bp, bi, bs, ba) = (clean_color(&b.brand_primary)?, clean_color(&b.brand_ink)?, clean_color(&b.brand_soft)?, clean_color(&b.brand_accent)?);
    sqlx::query("UPDATE app.tenants SET mobile_app_licenses=$1, iot_device_licenses=$2,
            brand_primary=$3, brand_ink=$4, brand_soft=$5, brand_accent=$6 WHERE id=$7")
        .bind(lic(b.mobile_app_licenses)).bind(lic(b.iot_device_licenses))
        .bind(bp).bind(bi).bind(bs).bind(ba).bind(tid)
        .execute(&st.owner_pool).await.map_err(ise)?;
    sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'set_tenant_admin','{}')")
        .bind(tid).bind(&s.email).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---- superadmin: upload / serve logo del tenant ----
/// SA-8: tipo del logo riconosciuto dai PRIMI BYTE, mai dall'intestazione del client: un SVG o un
/// HTML servito come immagine potrebbe eseguire script sul dominio della console.
pub(crate) fn logo_kind(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) { return Some("image/png"); }
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) { return Some("image/jpeg"); }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" { return Some("image/webp"); }
    None
}
pub(crate) const LOGO_MAX_BYTES: usize = 512 * 1024;
pub async fn super_set_logo(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, body: axum::body::Bytes) -> R {
    let sa_ = require_super(&st, &h).await?;
    if body.is_empty() { return Err(err(StatusCode::BAD_REQUEST, "file vuoto")); }
    if body.len() > LOGO_MAX_BYTES { return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "logo troppo grande: massimo 512 KB")); }
    let ct = logo_kind(&body).ok_or_else(|| err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "formato non ammesso: carica un'immagine PNG, JPEG o WebP"))?;
    sqlx::query("UPDATE app.tenants SET logo=$1, logo_ct=$2 WHERE id=$3")
        .bind(body.as_ref()).bind(ct).bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    audit(&st, tid, &format!("superadmin:{}", sa_.email), "tenant_logo", json!({"type":ct,"bytes":body.len()})).await;
    Ok(Json(json!({"ok":true,"type":ct})))
}
pub async fn super_del_logo(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    sqlx::query("UPDATE app.tenants SET logo=NULL, logo_ct=NULL WHERE id=$1").bind(tid).execute(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({"ok":true})))
}

// ---- branding visibile al tenant (qualsiasi ruolo autenticato): colori + logo ----
/// FE-12: informazioni pubbliche per la pagina di login (nessun dato del tenant). `demo` è vero
/// solo con YW_SEED_DEMO=1: solo allora la console mostra password e accessi rapidi di prova.
pub async fn login_info() -> R {
    Ok(Json(json!({"demo": std::env::var("YW_SEED_DEMO").ok().as_deref() == Some("1")})))
}
pub async fn branding(State(st): State<AppState>, h: HeaderMap) -> R {
    let s = require_tenant(&st, &h).await?;
    let tid = s.tenant_id.unwrap();
    let r = sqlx::query("SELECT name,brand_primary,brand_ink,brand_soft,brand_accent,(logo IS NOT NULL) AS has_logo FROM app.tenants WHERE id=$1")
        .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(json!({
        "name": r.get::<String,_>("name"),
        "brand_primary": r.try_get::<Option<String>,_>("brand_primary").ok().flatten(),
        "brand_ink": r.try_get::<Option<String>,_>("brand_ink").ok().flatten(),
        "brand_soft": r.try_get::<Option<String>,_>("brand_soft").ok().flatten(),
        "brand_accent": r.try_get::<Option<String>,_>("brand_accent").ok().flatten(),
        "has_logo": r.get::<bool,_>("has_logo"),
    })))
}
pub(crate) async fn serve_logo(st: &AppState, tid: Uuid) -> Response {
    let r = sqlx::query("SELECT logo,logo_ct FROM app.tenants WHERE id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let bytes: Option<Vec<u8>> = r.and_then(|r| r.try_get::<Option<Vec<u8>>,_>("logo").ok().flatten());
    // SA-8: il tipo servito è quello riconosciuto dai byte (anche per i logo caricati prima della
    // correzione); un logo non riconosciuto (es. SVG) non viene servito. nosniff + CSP chiusa.
    match bytes.as_deref().and_then(|b| logo_kind(b).map(|k| (b.to_vec(), k))) {
        Some((bytes, ct)) => ([(header::CONTENT_TYPE, ct.to_string()), (header::CACHE_CONTROL, "no-cache".into()),
                               (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
                               (header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox".into())], bytes).into_response(),
        None => (StatusCode::NOT_FOUND, "no logo").into_response(),
    }
}
pub async fn branding_logo(State(st): State<AppState>, h: HeaderMap) -> Response {
    match require_tenant(&st, &h).await { Ok(s) => serve_logo(&st, s.tenant_id.unwrap()).await, Err(e) => e.into_response() }
}
pub async fn super_get_logo(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> Response {
    match require_super(&st, &h).await { Ok(_) => serve_logo(&st, tid).await, Err(e) => e.into_response() }
}

// ---- impersonazione: il superadmin assume la sessione di un admin del tenant ----
pub async fn super_impersonate(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> impl IntoResponse {
    let s = match require_super(&st, &h).await { Ok(s) => s, Err(e) => return e.into_response() };
    // SA-7: si impersona SOLO un amministratore reale del tenant. Prima, senza admin, si entrava come
    // il primo utente qualunque ma con ruolo e permessi di amministratore (identità e audit falsati).
    let u = sqlx::query("SELECT id AS user_id,email FROM app.users WHERE tenant_id=$1 AND role='tenant_admin' ORDER BY created_at LIMIT 1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(u) = u else {
        return err(StatusCode::CONFLICT, "il tenant non ha un amministratore: crealo (o reinvia l'invito) prima di impersonarlo").into_response();
    };
    let sess = Session {
        user_id: u.get("user_id"), tenant_id: Some(tid), email: u.get("email"),
        role: "tenant_admin".into(), is_super: false, impersonator: Some(s.email.clone()),
        caps: effective_caps_user(&st, tid, "tenant_admin", u.get("user_id")).await,
    };
    let tok = auth::new_token(); let csrf = auth::new_token();
    persist_session(&st, &tok, &sess).await;
    let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'impersonate_start',$3)")
        .bind(tid).bind(&s.email).bind(json!({"as":sess.email})).execute(&st.owner_pool).await;
    (set_cookie_header(&tok, &csrf), Json(json!({"ok":true,"as":sess.email}))).into_response()
}
// stop impersonazione: riconia la sessione superadmin (senza nuovo login) e torna al control plane.
pub async fn impersonate_stop(State(st): State<AppState>, h: HeaderMap) -> impl IntoResponse {
    let s = match session_of(&st, &h).await { Some(s) => s, None => return err(StatusCode::UNAUTHORIZED, "no session").into_response() };
    let imp = match &s.impersonator { Some(e) => e.clone(), None => return err(StatusCode::BAD_REQUEST, "non sei in impersonazione").into_response() };
    // verifica che l'impersonator sia davvero un superadmin
    let sa = sqlx::query("SELECT id FROM app.superadmins WHERE email=$1").bind(&imp).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(sa) = sa else { return err(StatusCode::FORBIDDEN, "impersonator non valido").into_response(); };
    if let Some(t) = get_cookie(&h, "yw_session") { let _ = sqlx::query("DELETE FROM app.sessions WHERE token=$1").bind(&t).execute(&st.owner_pool).await; }
    // SA-7: la sessione superadmin ripristinata porta l'id del SUPERADMIN, non quello dell'utente impersonato.
    let sess = Session { user_id: sa.get("id"), tenant_id: None, email: imp.clone(), role: "superadmin".into(), is_super: true, impersonator: None, caps: vec![] };
    let tok = auth::new_token(); let csrf = auth::new_token();
    persist_session(&st, &tok, &sess).await;
    if let Some(tid) = s.tenant_id {
        let _ = sqlx::query("INSERT INTO app.audit_log(tenant_id,actor,action,detail) VALUES($1,$2,'impersonate_stop','{}')")
            .bind(tid).bind(&imp).execute(&st.owner_pool).await;
    }
    (set_cookie_header(&tok, &csrf), Json(json!({"ok":true}))).into_response()
}

pub async fn super_list_modules(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let rows = sqlx::query(
        "SELECT m.code,m.name,m.kind,m.available,tm.state
         FROM app.modules_catalog m LEFT JOIN app.tenant_modules tm ON tm.module_code=m.code AND tm.tenant_id=$1
         ORDER BY m.kind,m.code").bind(tid).fetch_all(&st.owner_pool).await.map_err(ise)?;
    let out: Vec<Value> = rows.iter().map(|r| {
        let code: String = r.get("code");
        let man = crate::modules::manifest(&code);
        json!({
        "code": code, "name": r.get::<String,_>("name"), "kind": r.get::<String,_>("kind"),
        "available": r.get::<bool,_>("available"), "state": r.try_get::<String,_>("state").ok().unwrap_or("non_abilitato".into()),
        // #3 manifest: dipendenze + descrizione + metadata (single source of truth in core::modules)
        "requires": man.map(|m| m.requires).unwrap_or(&[]),
        "provides": man.map(|m| m.provides).unwrap_or(&[]),
        "mkind": man.map(|m| m.kind),
        "capabilities": man.map(|m| m.capabilities).unwrap_or(&[]),
        "widgets": man.map(|m| m.widgets).unwrap_or(&[]),
        "description": man.map(|m| m.description) }) }).collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct SetModule { pub module_code: String, pub state: String }

pub async fn set_module(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<SetModule>) -> R {
    let sa_ = require_super(&st, &h).await?;
    let valid = ["non_abilitato","da_configurare","attivo","degradato","in_disattivazione"];
    if !valid.contains(&b.state.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "bad state")); }
    // Ritiro connettori esterni: un modulo non disponibile nel prodotto (available=false) non può
    // essere attivato (suite self-contained). Disattivarlo resta sempre consentito.
    if b.state != "non_abilitato" {
        let avail: Option<bool> = sqlx::query_scalar("SELECT available FROM app.modules_catalog WHERE code=$1")
            .bind(&b.module_code).fetch_optional(&st.owner_pool).await.map_err(ise)?;
        if avail == Some(false) { return Err(err(StatusCode::BAD_REQUEST, "modulo ritirato dal prodotto (non attivabile)")); }
    }
    // Block disabling loneworker if it is the only SOS path for a tenant with open incidents.
    if b.module_code == "loneworker" && b.state == "non_abilitato" {
        let open = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM app.incidents WHERE tenant_id=$1 AND state<>'chiuso'")
            .bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
        if open > 0 { return Err(err(StatusCode::CONFLICT, "open incidents: disattivazione bloccata (apri piano di disattivazione)")); }
    }
    // UPSERT: i moduli aggiunti al catalogo DOPO la creazione del tenant non hanno la riga
    // (il seed la crea solo al create). Un semplice UPDATE non troverebbe nulla e lo stato
    // resterebbe 'non_abilitato'. Con ON CONFLICT l'attivazione funziona per qualsiasi modulo.
    sqlx::query("INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES($2,$3,$1)
                 ON CONFLICT (tenant_id,module_code) DO UPDATE SET state=EXCLUDED.state,updated_at=now()")
        .bind(&b.state).bind(tid).bind(&b.module_code).execute(&st.owner_pool).await.map_err(ise)?;
    // SA-15: una sola riga di audit (in fondo, con l'email del superadmin)
    // #3 manifest: avvisi soft sulle dipendenze (non bloccano).
    let active: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT module_code FROM app.tenant_modules WHERE tenant_id=$1 AND state='attivo'")
        .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default().into_iter().collect();
    let mut warning: Option<String> = None;
    if b.state == "attivo" {
        let miss = crate::modules::missing_deps(&b.module_code, &active);
        if !miss.is_empty() { warning = Some(format!("dipendenze non attive: {}", miss.join(", "))); }
    } else if b.state == "non_abilitato" {
        let dep = crate::modules::dependents_active(&b.module_code, &active);
        if !dep.is_empty() { warning = Some(format!("moduli attivi che dipendono da questo: {}", dep.join(", "))); }
    }
    audit(&st, tid, &format!("superadmin:{}", sa_.email), "set_module", json!({"module":b.module_code,"state":b.state})).await;
    Ok(Json(json!({"ok":true,"warning":warning})))
}




// ---- orari dei turni di reperibilità (motore di escalation nativo) ----
pub(crate) fn parse_hhmm(s: &str) -> Option<(u32,u32)> {
    let mut it = s.split(':'); let h: u32 = it.next()?.trim().parse().ok()?; let m: u32 = it.next()?.trim().parse().ok()?;
    if h < 24 && m < 60 { Some((h,m)) } else { None }
}
/// Inizio della rotazione: l'istante determina l'ora (e per 'weekly' il giorno) di
/// handoff. 2024-01-01 e' un lunedi'; handoff_day 0=lun..6=dom mappa 2024-01-01..07.
pub(crate) fn rotation_start(handoff_time: Option<&str>, handoff_day: Option<i32>) -> String {
    let day = handoff_day.unwrap_or(0).clamp(0,6);
    let (h,m) = handoff_time.and_then(parse_hhmm).unwrap_or((0,0));
    format!("2024-01-{:02}T{:02}:{:02}:00Z", 1+day, h, m)
}
/// Impostazioni reperibilità del tenant (ladder vocale + status updates). Default sicuri.
pub struct OncallCfg { pub voice_on: bool, pub voice_min: i32, pub status_on: bool }
pub(crate) async fn oncall_cfg(st: &AppState, tid: Uuid) -> OncallCfg {
    let r = sqlx::query("SELECT voice_fallback_on,voice_fallback_min,status_updates_on FROM app.tenant_oncall_cfg WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    match r {
        Some(row) => OncallCfg{
            voice_on: row.try_get("voice_fallback_on").unwrap_or(true),
            voice_min: row.try_get("voice_fallback_min").unwrap_or(5),
            status_on: row.try_get("status_updates_on").unwrap_or(true) },
        None => OncallCfg{ voice_on:true, voice_min:5, status_on:true },
    }
}
