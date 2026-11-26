//! Login con Autentika (IdP OpenID Connect multi-tenant di YouCo), fase A1 di
//! docs/autentika-integrazione.md (mig 0172). Affianca il login con email e password: entrambi
//! restano attivi durante la prova.
//!
//! YW è il BFF: Authorization Code con PKCE S256 verso l'issuer del tenant, token tenuti sul server,
//! sessione di YW (cookie `yw_session` come il login locale) legata al `sid` di Autentika. Verifiche
//! dell'id_token come da docs/integration/oidc-rp.md: ES256 soltanto (con `ring`), `kid` dalla JWKS
//! (nuova lettura se manca), niente `jku`/`x5u`/`x5c`, `iss` uguale all'issuer, `aud` = client,
//! `exp`, `nonce`. Utente YW = (tenant, `sub`); al primo accesso si collega l'utente con la stessa
//! email verificata oppure se ne crea uno (impostabile). Back-channel logout: chiude le sessioni YW
//! di quel `sid`. Logout da YW: chiude anche la sessione di Autentika (best effort).

use axum::{extract::{State, Path, Query, Form}, http::{HeaderMap, StatusCode, header}, response::{IntoResponse, Response}, Json};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;
use crate::state::{AppState, Session};
use crate::api::{R, require_super, ise, err, audit, tenant_slug, persist_session, set_cookie_header, effective_caps_user, USER_ROLES};
use crate::{auth, db};

// =============================================================================================
// Discovery e JWKS (cache in memoria)
// =============================================================================================
struct Cached { v: Value, at: std::time::Instant }
fn cache() -> &'static Mutex<HashMap<String, Cached>> { static C: std::sync::OnceLock<Mutex<HashMap<String, Cached>>> = std::sync::OnceLock::new(); C.get_or_init(|| Mutex::new(HashMap::new())) }

async fn fetch_json(st: &AppState, url: &str, force: bool) -> Result<Value, String> {
    if !force { if let Some(c) = cache().lock().unwrap().get(url) { if c.at.elapsed() < std::time::Duration::from_secs(3600) { return Ok(c.v.clone()); } } }
    let r = st.http.get(url).timeout(std::time::Duration::from_secs(10)).send().await.map_err(|_| format!("Autentika non raggiungibile ({url})"))?;
    if !r.status().is_success() { return Err(format!("Autentika ha risposto {} a {url}", r.status().as_u16())); }
    let v: Value = r.json().await.map_err(|_| "risposta di Autentika non leggibile".to_string())?;
    cache().lock().unwrap().insert(url.to_string(), Cached { v: v.clone(), at: std::time::Instant::now() });
    Ok(v)
}
pub(crate) async fn discovery(st: &AppState, issuer: &str) -> Result<Value, String> {
    let d = fetch_json(st, &format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/')), false).await?;
    if d["issuer"].as_str() != Some(issuer.trim_end_matches('/')) { return Err("la discovery di Autentika dichiara un issuer diverso da quello configurato".into()); }
    Ok(d)
}

// =============================================================================================
// JWT ES256
// =============================================================================================
fn b64d(s: &str) -> Result<Vec<u8>, String> { B64.decode(s.trim_end_matches('=')).map_err(|_| "token non valido".to_string()) }

/// Verifica la firma ES256 con la JWKS (una nuova lettura se il `kid` manca) e restituisce (header, claims).
pub(crate) async fn verify_jws(st: &AppState, jwks_uri: &str, token: &str) -> Result<(Value, Value), String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 { return Err("token non valido".into()); }
    let hdr: Value = serde_json::from_slice(&b64d(parts[0])?).map_err(|_| "token non valido")?;
    if hdr["alg"].as_str() != Some("ES256") { return Err("algoritmo del token non ammesso".into()); }
    if hdr.get("jku").is_some() || hdr.get("x5u").is_some() || hdr.get("x5c").is_some() { return Err("intestazione del token non ammessa".into()); }
    let kid = hdr["kid"].as_str().ok_or("token senza kid")?.to_string();
    let find = |jwks: &Value| jwks["keys"].as_array().and_then(|ks| ks.iter().find(|k| k["kid"].as_str() == Some(&kid)).cloned());
    let mut key = find(&fetch_json(st, jwks_uri, false).await?);
    if key.is_none() { key = find(&fetch_json(st, jwks_uri, true).await?); }
    let key = key.ok_or("chiave del token sconosciuta")?;
    if key["kty"].as_str() != Some("EC") || key["crv"].as_str() != Some("P-256") { return Err("chiave del token non ES256".into()); }
    let (x, y) = (b64d(key["x"].as_str().unwrap_or(""))?, b64d(key["y"].as_str().unwrap_or(""))?);
    if x.len() != 32 || y.len() != 32 { return Err("chiave del token non valida".into()); }
    let mut pk = vec![4u8]; pk.extend(&x); pk.extend(&y);
    let sig = b64d(parts[2])?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &pk)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).map_err(|_| "firma del token non valida".to_string())?;
    let claims: Value = serde_json::from_slice(&b64d(parts[1])?).map_err(|_| "token non valido")?;
    Ok((hdr, claims))
}
fn aud_has(c: &Value, want: &str) -> bool {
    match &c["aud"] { Value::String(s) => s == want, Value::Array(a) => a.iter().any(|x| x.as_str() == Some(want)), _ => false }
}
fn check_time(c: &Value, now: i64) -> Result<(), String> {
    let exp = c["exp"].as_i64().ok_or("token senza scadenza")?;
    if exp + 60 < now { return Err("token scaduto".into()); }
    if c["nbf"].as_i64().map(|n| n > now + 60).unwrap_or(false) { return Err("token non ancora valido".into()); }
    Ok(())
}

// =============================================================================================
// Configurazione per tenant
// =============================================================================================
#[allow(dead_code)]
pub(crate) struct Provider { pub tenant_id: Uuid, pub issuer: String, pub client_id: String, pub client_secret: String, pub redirect_uri: String,
    pub scope: String, pub idp_hint: Option<String>, pub resource: Option<String>, pub enabled: bool, pub jit: bool, pub link_by_email: bool,
    pub default_role: String, pub role_map: Value }
pub(crate) async fn provider(st: &AppState, tid: Uuid) -> Option<Provider> {
    let r = sqlx::query("SELECT * FROM app.auth_providers WHERE tenant_id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    let secret = st.secrets.decrypt(&r.get::<String,_>("client_secret"));
    Some(Provider { tenant_id: tid, issuer: r.get("issuer"), client_id: r.get("client_id"), client_secret: secret, redirect_uri: r.get("redirect_uri"),
        scope: r.get("scope"), idp_hint: r.try_get("idp_hint").ok().flatten(), resource: r.try_get("resource").ok().flatten(), enabled: r.get("enabled"),
        jit: r.get("jit"), link_by_email: r.get("link_by_email"), default_role: r.get("default_role"), role_map: r.get("role_map") })
}
async fn tenant_by_slug(st: &AppState, slug: &str) -> Option<(Uuid, String)> {
    let r = sqlx::query("SELECT id,status FROM app.tenants WHERE slug=$1").bind(slug).fetch_optional(&st.owner_pool).await.ok().flatten()?;
    Some((r.get("id"), r.get("status")))
}
fn http_ok_issuer(u: &str) -> bool {
    // https sempre; http solo in locale (prove con Autentika in docker o con il finto provider dei test)
    u.starts_with("https://") || u.starts_with("http://127.0.0.1") || u.starts_with("http://localhost") || u.starts_with("http://[::1]")
}

#[derive(Deserialize)]
pub struct ProvQ { #[serde(default)] pub tenant: Option<String> }
/// GET /api/auth/providers — pubblico: la pagina di accesso mostra «Login con Autentika» se attivo.
pub async fn auth_providers(State(st): State<AppState>, h: HeaderMap, Query(q): Query<ProvQ>) -> R {
    let slug = q.tenant.map(|s| s.to_lowercase()).or_else(|| tenant_slug(&st, &h));
    let Some(slug) = slug else { return Ok(Json(json!({"autentika": false}))) };
    let Some((tid, _)) = tenant_by_slug(&st, &slug).await else { return Ok(Json(json!({"autentika": false}))) };
    let on: Option<bool> = sqlx::query_scalar("SELECT enabled FROM app.auth_providers WHERE tenant_id=$1").bind(tid).fetch_optional(&st.owner_pool).await.ok().flatten();
    Ok(Json(json!({"autentika": on.unwrap_or(false), "password": true})))
}

// =============================================================================================
// Avvio e ritorno del login
// =============================================================================================
fn redirect(to: &str) -> Response { (StatusCode::FOUND, [(header::LOCATION, to.to_string()), (header::CACHE_CONTROL, "no-store".to_string())]).into_response() }
fn back_with_error(return_to: &str, msg: &str) -> Response {
    let base = if return_to.starts_with('/') && !return_to.starts_with("//") { return_to.split(['?', '#']).next().unwrap_or("/") } else { "/" };
    redirect(&format!("{base}?auth_error={}", urlencoding(msg)))
}
fn urlencoding(s: &str) -> String { url_escape(s) }
fn url_escape(s: &str) -> String {
    s.bytes().map(|b| match b { b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(), _ => format!("%{b:02X}") }).collect()
}
fn safe_return(r: Option<&str>) -> String {
    match r { Some(x) if x.starts_with('/') && !x.starts_with("//") && !x.contains('\\') && x.len() < 500 => x.to_string(), _ => "/".into() }
}

#[derive(Deserialize)]
pub struct StartQ { #[serde(default)] pub tenant: Option<String>, #[serde(default)] pub return_to: Option<String> }
/// GET /api/auth/oidc/start — verso Autentika con state, nonce e PKCE S256 (il verifier resta qui).
pub async fn oidc_start(State(st): State<AppState>, h: HeaderMap, Query(q): Query<StartQ>) -> Response {
    let ret = safe_return(q.return_to.as_deref());
    let Some(slug) = q.tenant.map(|s| s.to_lowercase()).or_else(|| tenant_slug(&st, &h)) else { return back_with_error(&ret, "azienda non indicata") };
    let Some((tid, status)) = tenant_by_slug(&st, &slug).await else { return back_with_error(&ret, "azienda non trovata") };
    if status == "sospeso" { return back_with_error(&ret, "azienda sospesa: contatta l'assistenza"); }
    let Some(p) = provider(&st, tid).await.filter(|p| p.enabled) else { return back_with_error(&ret, "login con Autentika non attivo per questa azienda") };
    if p.client_secret.starts_with("enc:") { return back_with_error(&ret, "chiave dei segreti bloccata: il login con Autentika non è disponibile"); }
    let d = match discovery(&st, &p.issuer).await { Ok(d) => d, Err(e) => return back_with_error(&ret, &e) };
    let Some(authz) = d["authorization_endpoint"].as_str() else { return back_with_error(&ret, "discovery di Autentika incompleta") };
    let (state, nonce, verifier) = (auth::new_token(), auth::new_token(), format!("{}{}", auth::new_token(), auth::new_token()));
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let _ = sqlx::query("DELETE FROM app.oidc_pending WHERE created_at < now() - interval '10 minutes'").execute(&st.owner_pool).await;
    if sqlx::query("INSERT INTO app.oidc_pending(state,tenant_id,code_verifier,nonce,return_to) VALUES($1,$2,$3,$4,$5)")
        .bind(&state).bind(tid).bind(&verifier).bind(&nonce).bind(&ret).execute(&st.owner_pool).await.is_err() {
        return back_with_error(&ret, "accesso non disponibile, riprova");
    }
    let mut qs = vec![("client_id", p.client_id.clone()), ("redirect_uri", p.redirect_uri.clone()), ("response_type", "code".into()),
        ("scope", p.scope.clone()), ("state", state), ("nonce", nonce), ("code_challenge", challenge), ("code_challenge_method", "S256".into())];
    if let Some(hint) = p.idp_hint.clone().filter(|x| !x.is_empty()) { qs.push(("idp_hint", hint)); }
    if let Some(res) = p.resource.clone().filter(|x| !x.is_empty()) { qs.push(("resource", res)); }
    let sep = if authz.contains('?') { '&' } else { '?' };
    redirect(&format!("{authz}{sep}{}", qs.iter().map(|(k, v)| format!("{k}={}", url_escape(v))).collect::<Vec<_>>().join("&")))
}

#[derive(Deserialize)]
pub struct CbQ { #[serde(default)] pub code: Option<String>, #[serde(default)] pub state: Option<String>,
    #[serde(default)] pub error: Option<String>, #[serde(default)] pub error_description: Option<String> }
/// GET /api/auth/oidc/callback — codice → token, verifiche, utente YW, sessione YW.
pub async fn oidc_callback(State(st): State<AppState>, Query(q): Query<CbQ>) -> Response {
    let Some(state) = q.state.clone() else { return back_with_error("/", "risposta di Autentika senza state") };
    let pend = sqlx::query("DELETE FROM app.oidc_pending WHERE state=$1 AND created_at > now() - interval '10 minutes' RETURNING tenant_id,code_verifier,nonce,return_to")
        .bind(&state).fetch_optional(&st.owner_pool).await.ok().flatten();
    let Some(pend) = pend else { return back_with_error("/", "accesso scaduto o già usato: riprova") };
    let (tid, verifier, nonce, ret): (Uuid, String, String, String) = (pend.get("tenant_id"), pend.get("code_verifier"), pend.get("nonce"), pend.get("return_to"));
    if let Some(e) = q.error.as_deref() {
        let msg = match e { "access_denied" => "accesso negato da Autentika: l'utente non è abilitato a YouWorkforce".to_string(),
            "login_required" => "accesso a Autentika richiesto".to_string(), _ => format!("Autentika: {}", q.error_description.clone().unwrap_or_else(|| e.to_string())) };
        return back_with_error(&ret, &msg);
    }
    let Some(code) = q.code.clone() else { return back_with_error(&ret, "risposta di Autentika senza codice") };
    match finish_login(&st, tid, &code, &verifier, &nonce).await {
        Ok((tok, csrf)) => {
            let mut resp = redirect(&ret);
            for (k, v) in set_cookie_header(&tok, &csrf).iter() { resp.headers_mut().append(k.clone(), v.clone()); }
            resp
        }
        Err(e) => back_with_error(&ret, &e),
    }
}

async fn finish_login(st: &AppState, tid: Uuid, code: &str, verifier: &str, nonce: &str) -> Result<(String, String), String> {
    let p = provider(st, tid).await.filter(|p| p.enabled).ok_or("login con Autentika non attivo per questa azienda")?;
    let d = discovery(st, &p.issuer).await?;
    let (tok_ep, jwks) = (d["token_endpoint"].as_str().ok_or("discovery incompleta")?.to_string(), d["jwks_uri"].as_str().ok_or("discovery incompleta")?.to_string());
    let r = st.http.post(&tok_ep).basic_auth(&p.client_id, Some(&p.client_secret)).timeout(std::time::Duration::from_secs(15))
        .form(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", p.redirect_uri.as_str()), ("code_verifier", verifier)])
        .send().await.map_err(|_| "Autentika non raggiungibile".to_string())?;
    let status = r.status();
    let tr: Value = r.json().await.unwrap_or(json!({}));
    if !status.is_success() {
        return Err(match tr["error"].as_str() { Some("invalid_client") => "client di Autentika non valido: controlla client e segreto".into(),
            Some("invalid_grant") => "codice di Autentika non valido o scaduto: riprova".into(), Some(e) => format!("Autentika: {e}"), None => format!("Autentika ha risposto {}", status.as_u16()) });
    }
    let idt = tr["id_token"].as_str().ok_or("Autentika non ha restituito l'id_token")?;
    let (_, c) = verify_jws(st, &jwks, idt).await?;
    let now = chrono::Utc::now().timestamp();
    if c["iss"].as_str() != Some(p.issuer.trim_end_matches('/')) { return Err("id_token di un altro issuer".into()); }
    if !aud_has(&c, &p.client_id) { return Err("id_token destinato a un altro client".into()); }
    check_time(&c, now)?;
    if c["nonce"].as_str() != Some(nonce) { return Err("id_token non corrisponde all'accesso avviato".into()); }
    let sub = c["sub"].as_str().filter(|s| !s.is_empty()).ok_or("id_token senza soggetto")?.to_string();
    let email = c["email"].as_str().map(|s| s.trim().to_lowercase());
    let verified = c["email_verified"].as_bool().unwrap_or(false);
    let sid = c["sid"].as_str().map(|s| s.to_string());
    // ruoli del prodotto dall'access token (se il client ha la risorsa)
    let mut roles: Vec<String> = vec![];
    if let (Some(res), Some(at)) = (p.resource.as_deref().filter(|x| !x.is_empty()), tr["access_token"].as_str()) {
        if let Ok((h2, a)) = verify_jws(st, &jwks, at).await {
            if h2["typ"].as_str().map(|t| t.eq_ignore_ascii_case("at+jwt")).unwrap_or(true) && aud_has(&a, res) && a["iss"].as_str() == Some(p.issuer.trim_end_matches('/')) && check_time(&a, now).is_ok() {
                roles = a["roles"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
            }
        }
    }
    let mapped = map_role(&p, &roles);
    // utente YW: (tenant, sub) → altrimenti stessa email verificata → altrimenti creato
    let mut tx = db::begin_tenant(&st.rt_pool, tid).await.map_err(|_| "accesso non disponibile, riprova".to_string())?;
    let mut user = sqlx::query("SELECT id,email,role FROM app.users WHERE auth_sub=$1 AND anonymized_at IS NULL").bind(&sub).fetch_optional(&mut *tx).await.map_err(|_| "accesso non disponibile")?;
    let mut how = "sub";
    if user.is_none() && p.link_by_email && verified {
        if let Some(e) = &email {
            user = sqlx::query("UPDATE app.users SET auth_sub=$1 WHERE lower(email)=$2 AND auth_sub IS NULL AND anonymized_at IS NULL RETURNING id,email,role")
                .bind(&sub).bind(e).fetch_optional(&mut *tx).await.map_err(|_| "accesso non disponibile")?;
            if user.is_some() { how = "email"; }
        }
    }
    if user.is_none() {
        if !p.jit { let _ = tx.rollback().await; return Err("utente non presente in YouWorkforce: chiedi all'amministratore di aggiungerlo".into()); }
        let e = email.clone().ok_or("Autentika non ha fornito l'email dell'utente (scope email)")?;
        user = Some(sqlx::query("INSERT INTO app.users(tenant_id,email,pass_hash,role,auth_sub) VALUES($1,$2,'!autentika',$3,$4) RETURNING id,email,role")
            .bind(tid).bind(&e).bind(&mapped).bind(&sub).fetch_one(&mut *tx).await
            .map_err(|_| "un utente con questa email esiste già ed è collegato a un altro account di Autentika".to_string())?);
        how = "nuovo";
    }
    let u = user.unwrap();
    tx.commit().await.map_err(|_| "accesso non disponibile")?;
    let (uid, uemail, role): (Uuid, String, String) = (u.get("id"), u.get("email"), u.get("role"));
    let tok = auth::new_token(); let csrf = auth::new_token();
    let sess = Session { user_id: uid, tenant_id: Some(tid), email: uemail.clone(), role: role.clone(), is_super: false, impersonator: None,
        caps: effective_caps_user(st, tid, &role, uid).await, via_key: None };
    persist_session(st, &tok, &sess).await;
    let _ = sqlx::query("UPDATE app.sessions SET auth_sid=$2, auth_id_token=$3 WHERE token=$1").bind(&tok).bind(&sid).bind(idt).execute(&st.owner_pool).await;
    audit(st, tid, &uemail, "login_autentika", json!({"sub": sub, "match": how, "roles": roles})).await;
    Ok((tok, csrf))
}

/// Ruolo YW dai ruoli del prodotto: il più alto tra quelli mappati, altrimenti il ruolo predefinito.
pub(crate) fn map_role(p: &Provider, roles: &[String]) -> String {
    const ORDER: [&str; 7] = ["tenant_admin", "responsabile", "pianificatore", "operatore", "reperibile", "auditor", "lavoratore"];
    let mapped: Vec<String> = roles.iter().filter_map(|r| p.role_map.get(r).and_then(|v| v.as_str()).map(|s| s.to_string())).collect();
    ORDER.iter().find(|o| mapped.iter().any(|m| m == *o)).map(|s| s.to_string()).unwrap_or_else(|| p.default_role.clone())
}

// =============================================================================================
// Logout
// =============================================================================================
#[derive(Deserialize)]
pub struct BclForm { pub logout_token: String }
/// POST /api/auth/backchannel-logout — Autentika chiude la sessione: si chiudono quelle YW con quel `sid`.
pub async fn backchannel_logout(State(st): State<AppState>, Form(f): Form<BclForm>) -> Response {
    match bcl(&st, &f.logout_token).await {
        Ok(n) => (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(json!({"ok": true, "sessions": n}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, [(header::CACHE_CONTROL, "no-store")], Json(json!({"error": "invalid_request", "message": e}))).into_response(),
    }
}
async fn bcl(st: &AppState, token: &str) -> Result<u64, String> {
    // iss senza verifica solo per trovare il tenant; poi verifica completa con la sua JWKS
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 { return Err("logout_token non valido".into()); }
    let raw: Value = serde_json::from_slice(&b64d(parts[1])?).map_err(|_| "logout_token non valido")?;
    let iss = raw["iss"].as_str().ok_or("logout_token senza iss")?.to_string();
    let rows = sqlx::query("SELECT tenant_id FROM app.auth_providers WHERE tenant_id IS NOT NULL AND issuer=$1 AND enabled").bind(&iss).fetch_all(&st.owner_pool).await.map_err(|_| "errore")?;
    for r in rows {
        let tid: Uuid = r.get("tenant_id");
        let Some(p) = provider(st, tid).await else { continue };
        let d = discovery(st, &p.issuer).await?;
        let (h, c) = verify_jws(st, d["jwks_uri"].as_str().ok_or("discovery incompleta")?, token).await?;
        if h["typ"].as_str().map(|t| !t.eq_ignore_ascii_case("logout+jwt")).unwrap_or(false) { return Err("tipo di token non valido".into()); }
        if !aud_has(&c, &p.client_id) { continue; }
        check_time(&c, chrono::Utc::now().timestamp())?;
        if c.get("nonce").is_some() { return Err("logout_token con nonce".into()); }
        if c["events"].get("http://schemas.openid.net/event/backchannel-logout").is_none() { return Err("logout_token senza evento di logout".into()); }
        let sid = c["sid"].as_str().ok_or("logout_token senza sid")?;
        let n = sqlx::query("DELETE FROM app.sessions WHERE tenant_id=$1 AND auth_sid=$2").bind(tid).bind(sid).execute(&st.owner_pool).await.map_err(|_| "errore")?.rows_affected();
        audit(st, tid, "autentika", "logout_autentika", json!({"sid": sid, "sessions": n})).await;
        return Ok(n);
    }
    Err("logout_token di un issuer o client non configurato".into())
}
/// Logout da YW di una sessione nata da Autentika: chiude anche la sessione di Autentika (best effort,
/// con l'id_token come prova del client; nessun redirect, risposta 200).
pub(crate) async fn end_remote_session(st: &AppState, tid: Uuid, id_token: &str) {
    let Some(p) = provider(st, tid).await else { return };
    let Ok(d) = discovery(st, &p.issuer).await else { return };
    let Some(ep) = d["end_session_endpoint"].as_str() else { return };
    let _ = st.http.post(ep).timeout(std::time::Duration::from_secs(5)).form(&[("client_id", p.client_id.as_str()), ("id_token_hint", id_token)]).send().await;
}

// =============================================================================================
// Superadmin: configurazione per tenant
// =============================================================================================
pub async fn super_get_autentika(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let p = provider(&st, tid).await;
    let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM app.users WHERE tenant_id=$1 AND auth_sub IS NOT NULL").bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    Ok(Json(match p {
        None => json!({"configured": false, "linked_users": linked, "scope": "openid profile email groups", "default_role": "lavoratore", "jit": true, "link_by_email": true,
            "role_map": {"admin":"tenant_admin","pianificatore":"pianificatore","responsabile":"responsabile","operatore":"operatore","reperibile":"reperibile","auditor":"auditor","lavoratore":"lavoratore"}}),
        Some(p) => json!({"configured": true, "issuer": p.issuer, "client_id": p.client_id, "has_secret": !p.client_secret.is_empty(), "redirect_uri": p.redirect_uri,
            "scope": p.scope, "idp_hint": p.idp_hint, "resource": p.resource, "enabled": p.enabled, "jit": p.jit, "link_by_email": p.link_by_email,
            "default_role": p.default_role, "role_map": p.role_map, "linked_users": linked}),
    }))
}
#[derive(Deserialize)]
pub struct ProvReq { pub issuer: String, pub client_id: String, #[serde(default)] pub client_secret: Option<String>, pub redirect_uri: String,
    #[serde(default)] pub scope: Option<String>, #[serde(default)] pub idp_hint: Option<String>, #[serde(default)] pub resource: Option<String>,
    #[serde(default)] pub enabled: Option<bool>, #[serde(default)] pub jit: Option<bool>, #[serde(default)] pub link_by_email: Option<bool>,
    #[serde(default)] pub default_role: Option<String>, #[serde(default)] pub role_map: Option<Value> }
pub async fn super_set_autentika(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>, Json(b): Json<ProvReq>) -> R {
    let sa = require_super(&st, &h).await?;
    let issuer = b.issuer.trim().trim_end_matches('/').to_string();
    if !http_ok_issuer(&issuer) || issuer.matches('/').count() != 2 { return Err(err(StatusCode::BAD_REQUEST, "issuer: https://<host> senza percorso (http solo in locale)")); }
    // indirizzo di ritorno: https; http ammesso solo quando anche l'issuer è locale (prove)
    let red = b.redirect_uri.trim();
    if !(red.starts_with("https://") || (red.starts_with("http://") && !issuer.starts_with("https://"))) { return Err(err(StatusCode::BAD_REQUEST, "indirizzo di ritorno: https (http solo con un Autentika locale di prova)")); }
    if b.client_id.trim().is_empty() { return Err(err(StatusCode::BAD_REQUEST, "client_id richiesto")); }
    let role = b.default_role.clone().unwrap_or_else(|| "lavoratore".into());
    if !USER_ROLES.contains(&role.as_str()) { return Err(err(StatusCode::BAD_REQUEST, "ruolo predefinito non valido")); }
    if let Some(m) = &b.role_map { let ok = m.as_object().map(|o| o.values().all(|v| v.as_str().map(|s| USER_ROLES.contains(&s)).unwrap_or(false))).unwrap_or(false);
        if !ok { return Err(err(StatusCode::BAD_REQUEST, "corrispondenza dei ruoli: oggetto {ruolo Autentika: ruolo YouWorkforce}")); } }
    let secret = match b.client_secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(st.secrets.encrypt(s).map_err(|e| err(StatusCode::CONFLICT, &e))?), None => None };
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app.auth_providers WHERE tenant_id=$1)").bind(tid).fetch_one(&st.owner_pool).await.map_err(ise)?;
    if !exists && secret.is_none() { return Err(err(StatusCode::BAD_REQUEST, "segreto del client richiesto alla prima configurazione")); }
    let opt = |s: &Option<String>| s.as_deref().map(str::trim).filter(|x| !x.is_empty()).map(|x| x.to_string());
    sqlx::query("INSERT INTO app.auth_providers(tenant_id,issuer,client_id,client_secret,redirect_uri,scope,idp_hint,resource,enabled,jit,link_by_email,default_role,role_map)
            VALUES($1,$2,$3,COALESCE($4,''),$5,COALESCE($6,'openid profile email groups'),$7,$8,COALESCE($9,false),COALESCE($10,true),COALESCE($11,true),$12,
                   COALESCE($13,'{\"admin\":\"tenant_admin\",\"pianificatore\":\"pianificatore\",\"responsabile\":\"responsabile\",\"operatore\":\"operatore\",\"reperibile\":\"reperibile\",\"auditor\":\"auditor\",\"lavoratore\":\"lavoratore\"}'::jsonb))
         ON CONFLICT (tenant_id) DO UPDATE SET issuer=EXCLUDED.issuer, client_id=EXCLUDED.client_id, client_secret=COALESCE($4, app.auth_providers.client_secret),
            redirect_uri=EXCLUDED.redirect_uri, scope=EXCLUDED.scope, idp_hint=EXCLUDED.idp_hint, resource=EXCLUDED.resource, enabled=EXCLUDED.enabled, jit=EXCLUDED.jit,
            link_by_email=EXCLUDED.link_by_email, default_role=EXCLUDED.default_role, role_map=EXCLUDED.role_map, updated_at=now()")
        .bind(tid).bind(&issuer).bind(b.client_id.trim()).bind(&secret).bind(b.redirect_uri.trim()).bind(opt(&b.scope)).bind(opt(&b.idp_hint)).bind(opt(&b.resource))
        .bind(b.enabled).bind(b.jit).bind(b.link_by_email).bind(&role).bind(&b.role_map)
        .execute(&st.owner_pool).await.map_err(|e| crate::api::conflict_or(e, "configurazione non valida"))?;
    cache().lock().unwrap().clear();
    audit(&st, tid, &format!("superadmin:{}", sa.email), "set_autentika", json!({"issuer": issuer, "client_id": b.client_id.trim(), "enabled": b.enabled})).await;
    Ok(Json(json!({"ok": true})))
}
/// POST /api/superadmin/tenants/:id/autentika/verify — discovery, JWKS e credenziali del client.
pub async fn super_verify_autentika(State(st): State<AppState>, h: HeaderMap, Path(tid): Path<Uuid>) -> R {
    require_super(&st, &h).await?;
    let p = provider(&st, tid).await.ok_or_else(|| err(StatusCode::BAD_REQUEST, "Autentika non configurato per questo tenant"))?;
    cache().lock().unwrap().clear();
    let mut checks = vec![];
    let d = match discovery(&st, &p.issuer).await { Ok(d) => { checks.push(json!({"check": "discovery", "ok": true, "detail": d["issuer"]})); d }
        Err(e) => { checks.push(json!({"check": "discovery", "ok": false, "detail": e})); return Ok(Json(json!({"ok": false, "checks": checks}))); } };
    let jw = match d["jwks_uri"].as_str() { Some(u) => fetch_json(&st, u, true).await.map(|j| j["keys"].as_array().map(|a| a.len()).unwrap_or(0)), None => Err("jwks_uri assente".into()) };
    checks.push(match &jw { Ok(n) => json!({"check": "jwks", "ok": *n > 0, "detail": format!("{n} chiavi")}), Err(e) => json!({"check": "jwks", "ok": false, "detail": e}) });
    let pkce = d["code_challenge_methods_supported"].as_array().map(|a| a.iter().any(|x| x == "S256")).unwrap_or(false);
    checks.push(json!({"check": "pkce_s256", "ok": pkce}));
    // credenziali: un codice inventato deve dare invalid_grant (client valido), non invalid_client
    if let Some(te) = d["token_endpoint"].as_str() {
        let r = st.http.post(te).basic_auth(&p.client_id, Some(&p.client_secret)).timeout(std::time::Duration::from_secs(10))
            .form(&[("grant_type", "authorization_code"), ("code", "verifica-yw"), ("redirect_uri", p.redirect_uri.as_str()), ("code_verifier", "x".repeat(43).as_str())]).send().await;
        let e = match r { Ok(r) => r.json::<Value>().await.ok().and_then(|v| v["error"].as_str().map(|s| s.to_string())).unwrap_or_default(), Err(_) => "irraggiungibile".into() };
        checks.push(json!({"check": "client", "ok": e != "invalid_client" && e != "irraggiungibile", "detail": if e == "invalid_client" { "client o segreto non validi".to_string() } else if e.is_empty() { "risposta inattesa".into() } else { format!("credenziali accettate ({e} sul codice di prova)") }}));
    }
    let ok = checks.iter().all(|c| c["ok"].as_bool() == Some(true));
    Ok(Json(json!({"ok": ok, "checks": checks, "redirect_uri": p.redirect_uri})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prov(map: Value) -> Provider { Provider { tenant_id: Uuid::nil(), issuer: "https://x".into(), client_id: "c".into(), client_secret: "s".into(), redirect_uri: "https://r".into(),
        scope: "openid".into(), idp_hint: None, resource: None, enabled: true, jit: true, link_by_email: true, default_role: "lavoratore".into(), role_map: map } }
    #[test] fn roles() {
        let p = prov(json!({"admin":"tenant_admin","pianificatore":"pianificatore","member":"lavoratore"}));
        assert_eq!(map_role(&p, &["member".into(), "pianificatore".into()]), "pianificatore");
        assert_eq!(map_role(&p, &["admin".into(), "member".into()]), "tenant_admin");
        assert_eq!(map_role(&p, &["sconosciuto".into()]), "lavoratore");
    }
    #[test] fn returns() {
        assert_eq!(safe_return(Some("/app.html#x")), "/app.html#x");
        assert_eq!(safe_return(Some("//evil.example")), "/");
        assert_eq!(safe_return(Some("https://evil")), "/");
        assert!(http_ok_issuer("https://idealverde.autentika.youco.cloud") && http_ok_issuer("http://127.0.0.1:18097") && !http_ok_issuer("http://evil.example"));
    }
}
