use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use chrono::{DateTime, Utc};
use crate::state::{AppState, TenantEvent};
use crate::{goalert, api};

/// Background worker (system scope, owner connection bypasses RLS).
/// 1) drains the outbox to GoAlert (at-least-once, retry with backoff)
/// 2) sweeps durable check-in timers using the controllable clock
pub async fn run(st: AppState) {
    loop {
        if let Err(e) = tick(&st).await { tracing::warn!("worker tick error: {e}"); }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn tick(st: &AppState) -> anyhow::Result<()> {
    drain_outbox(st).await?;
    sweep_checkins(st).await?;
    heartbeats(st).await?;
    traccar_poll(st).await?;
    thingsboard_poll(st).await?;
    // auto-integrazioni: configura da sola i moduli integrati attivi (idempotente), ~ogni 60s
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    if n % 30 == 0 { api::reconcile_integrations(st).await; }
    // #5 sync ack/chiusura da GoAlert -> incidenti (~ogni 14s)
    if n % 7 == 0 { api::goalert_ack_sync(st).await; }
    // #7 ritenzione GDPR (opt-in YW_RETENTION_DAYS): ~ogni ora (1800 tick * 2s)
    if n % 1800 == 0 { api::purge_retention(st).await; }
    // #4 escalation nativo FASE 1 (shadow): confronto nativo vs GoAlert ~ogni 5 min, nessuna consegna
    if n % 150 == 0 { api::escalation_shadow_scan(st).await; }
    // #4 escalation nativo FASE 2 (consegna in ombra): ~ogni 30s, verso sink di prova, idempotente
    if n % 15 == 7 { api::escalation_delivery_shadow_scan(st).await; }
    Ok(())
}

// ThingsBoard PULL in background: per ogni tenant con connessione + modulo attivo + poll
// abilitato, alla cadenza configurata fa login JWT, specchia i device e pulla la telemetria
// applicando il motore regole (incidenti). Converge col push via Rule Engine.
async fn thingsboard_poll(st: &AppState) -> anyhow::Result<()> {
    let now = st.clock.now();
    let conns = sqlx::query(
        "SELECT c.tenant_id, c.poll_interval_s
         FROM app.thingsboard_connections c
         JOIN app.tenant_modules m ON m.tenant_id=c.tenant_id AND m.module_code='thingsboard' AND m.state='attivo'
         WHERE c.poll_enabled=true")
        .fetch_all(&st.owner_pool).await?;
    for c in conns {
        let tid: Uuid = c.get("tenant_id");
        let interval: i32 = c.get("poll_interval_s");
        let last = sqlx::query("SELECT last_poll_at FROM app.thingsboard_poll_state WHERE tenant_id=$1")
            .bind(tid).fetch_optional(&st.owner_pool).await?;
        if let Some(lp) = last.as_ref().and_then(|s| s.try_get::<DateTime<Utc>,_>("last_poll_at").ok()) {
            if (now - lp).num_seconds() < interval as i64 { continue; }
        }
        // tb_poll_now aggiorna gia' lo stato salute (reachable-aware); un tenant giu' non blocca gli altri.
        let _ = api::tb_poll_now(st, tid).await;
    }
    Ok(())
}

// ============================================================================
// Traccar P1 — polling via API (spina dorsale pull) + watchdog del silenzio.
// Per ogni tenant con connessione Traccar + modulo attivo + poll abilitato, alla
// cadenza configurata: login, recupero eventi dal cursore (backfill), sync delle
// posizioni correnti, watchdog offline indipendente, aggiornamento stato salute.
// Converge col push forwarding: stesso idem_key evento / stesso source_id posizione.
// ============================================================================
async fn traccar_poll(st: &AppState) -> anyhow::Result<()> {
    let now = st.clock.now();
    let conns = sqlx::query(
        "SELECT c.tenant_id, c.base_url, c.username, c.password, c.poll_interval_s, c.silence_after_s, c.conn_down_after_s
         FROM app.traccar_connections c
         JOIN app.tenant_modules m ON m.tenant_id=c.tenant_id AND m.module_code='traccar' AND m.state='attivo'
         WHERE c.poll_enabled=true")
        .fetch_all(&st.owner_pool).await?;
    for c in conns {
        let tid: Uuid = c.get("tenant_id");
        let interval: i32 = c.get("poll_interval_s");
        let state = sqlx::query("SELECT last_poll_at,last_ok_at,last_event_id FROM app.traccar_poll_state WHERE tenant_id=$1")
            .bind(tid).fetch_optional(&st.owner_pool).await?;
        // gate: rispetta la cadenza per-tenant
        if let Some(lp) = state.as_ref().and_then(|s| s.try_get::<DateTime<Utc>,_>("last_poll_at").ok()) {
            if (now - lp).num_seconds() < interval as i64 { continue; }
        }
        let last_ok: Option<DateTime<Utc>> = state.as_ref().and_then(|s| s.try_get::<DateTime<Utc>,_>("last_ok_at").ok());
        let last_event_id: i64 = state.as_ref().map(|s| s.get::<i64,_>("last_event_id")).unwrap_or(0);
        let base: String = c.get("base_url");
        let user: String = c.get("username");
        let pass: String = st.secrets.decrypt(&c.get::<String,_>("password"));   // P3
        let silence_after: i32 = c.get("silence_after_s");
        let conn_down_after: i32 = c.get("conn_down_after_s");
        if let Err(e) = poll_tenant(st, tid, &base, &user, &pass, last_ok, last_event_id, silence_after, now).await {
            // marca non raggiungibile ma non interrompe gli altri tenant
            let _ = mark_poll(st, tid, false, last_event_id, None, None, Some(&e.to_string()), now).await;
        }
        // watchdog della CONNESSIONE: se Traccar e' down oltre la soglia -> incidente + escalation.
        let _ = conn_watchdog(st, tid, conn_down_after, now).await;
    }
    Ok(())
}

/// Watchdog della connessione Traccar (sicurezza): se il server e' irraggiungibile
/// oltre `conn_down_after` secondi alza un incidente critico `traccar_conn_down`
/// (escalation GoAlert); al ripristino lo chiude. Un episodio = un incidente.
async fn conn_watchdog(st: &AppState, tid: Uuid, conn_down_after: i32, now: DateTime<Utc>) -> anyhow::Result<()> {
    let Some(r) = sqlx::query("SELECT reachable,down_since,conn_incident_open,conn_incident_corr FROM app.traccar_poll_state WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await? else { return Ok(()); };
    let reachable: bool = r.get("reachable");
    let down_since: Option<DateTime<Utc>> = r.try_get("down_since").ok();
    let open: bool = r.get("conn_incident_open");
    let corr: Option<String> = r.try_get("conn_incident_corr").ok();

    if !reachable {
        let ds = match down_since {
            Some(d) => d,
            None => {
                sqlx::query("UPDATE app.traccar_poll_state SET down_since=$2 WHERE tenant_id=$1")
                    .bind(tid).bind(now).execute(&st.owner_pool).await?;
                now
            }
        };
        if (now - ds).num_seconds() >= conn_down_after as i64 {
            // C'e' davvero un incidente APERTO per questo episodio? (non fidarsi del
            // solo flag: persist puo' fallire, oppure un operatore puo' aver chiuso
            // l'incidente mentre l'outage continua -> in sicurezza si ri-escalation.)
            let has_open = match &corr {
                Some(c) => sqlx::query_scalar::<_,Uuid>("SELECT id FROM app.incidents WHERE tenant_id=$1 AND correlation=$2 AND state<>'chiuso'")
                    .bind(tid).bind(c).fetch_optional(&st.owner_pool).await?.is_some(),
                None => false,
            };
            if !has_open {
                // correlazione fresca: evita che l'ON CONFLICT deduplichi contro un
                // evento di un incidente gia' chiuso (altrimenti non si riaprirebbe).
                let correlation = format!("traccar-conndown-{}-{}-{}", tid, ds.timestamp_millis(), now.timestamp_millis());
                let res = crate::api::persist_ingest(st, tid, crate::api::Normalized{
                    source: "traccar".into(),
                    source_event: correlation.clone(),
                    idem: correlation.clone(),
                    etype: "traccar_conn_down".into(),
                    device_external_id: None,
                    site_name: None,
                    lat: None, lon: None, accuracy_m: None,
                    correlation: correlation.clone(),
                    payload: json!({"watchdog":"connessione","down_since":ds,"conn_down_after_s":conn_down_after}),
                    indoor: false, beacon_uid: None, zone: None,
                }).await;
                // segna aperto SOLO se l'incidente e' stato davvero creato (+ outbox).
                let opened = matches!(&res, Ok(j) if j.0.get("incident_id").and_then(|v| v.as_str()).is_some());
                if opened {
                    sqlx::query("UPDATE app.traccar_poll_state SET conn_incident_open=true, conn_incident_corr=$2 WHERE tenant_id=$1")
                        .bind(tid).bind(&correlation).execute(&st.owner_pool).await?;
                    tracing::warn!("Traccar connessione down per tenant {tid}: incidente alzato");
                } else {
                    // persist fallito: non marcare aperto -> ritenta al prossimo poll.
                    sqlx::query("UPDATE app.traccar_poll_state SET conn_incident_open=false WHERE tenant_id=$1")
                        .bind(tid).execute(&st.owner_pool).await?;
                    tracing::error!("Traccar conn_down: persist incidente fallito tenant {tid}, ritento");
                }
            }
        }
    } else if open {
        // ripristino: chiudi l'incidente e chiedi a GoAlert di chiudere l'alert
        if let Some(corr) = &corr {
            if let Some(row) = sqlx::query("UPDATE app.incidents SET state='chiuso',outcome='connessione Traccar ripristinata',closed_at=now(),version=version+1
                 WHERE tenant_id=$1 AND correlation=$2 AND state<>'chiuso' RETURNING id,etype")
                .bind(tid).bind(corr).fetch_optional(&st.owner_pool).await? {
                let iid: Uuid = row.get("id"); let etype: String = row.get("etype");
                sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'transition',$3)")
                    .bind(tid).bind(iid).bind(json!({"to":"chiuso","outcome":"connessione ripristinata","by":"watchdog"})).execute(&st.owner_pool).await?;
                sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_close',$2)")
                    .bind(tid).bind(json!({"incident_id":iid,"summary":format!("close {}",corr),"dedup":corr,"etype":etype}))
                    .execute(&st.owner_pool).await?;
            }
        }
        sqlx::query("UPDATE app.traccar_poll_state SET conn_incident_open=false, down_since=NULL, conn_incident_corr=NULL WHERE tenant_id=$1")
            .bind(tid).execute(&st.owner_pool).await?;
        tracing::info!("Traccar connessione ripristinata per tenant {tid}");
    } else if down_since.is_some() {
        // blip rientrato prima della soglia
        sqlx::query("UPDATE app.traccar_poll_state SET down_since=NULL WHERE tenant_id=$1")
            .bind(tid).execute(&st.owner_pool).await?;
    }
    Ok(())
}

/// Forza un ciclo di poll per un tenant, ignorando la cadenza (usato dall'azione
/// "Verifica collegamento" del pannello). Ritorna errore solo se la connessione
/// manca; i problemi di raggiungibilita' finiscono in traccar_poll_state.
pub async fn poll_now(st: &AppState, tid: Uuid) -> anyhow::Result<()> {
    let now = st.clock.now();
    let c = sqlx::query("SELECT base_url,username,password,silence_after_s FROM app.traccar_connections WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await?
        .ok_or_else(|| anyhow::anyhow!("connessione Traccar non configurata"))?;
    let state = sqlx::query("SELECT last_ok_at,last_event_id FROM app.traccar_poll_state WHERE tenant_id=$1")
        .bind(tid).fetch_optional(&st.owner_pool).await?;
    let last_ok: Option<DateTime<Utc>> = state.as_ref().and_then(|s| s.try_get::<DateTime<Utc>,_>("last_ok_at").ok());
    let last_event_id: i64 = state.as_ref().map(|s| s.get::<i64,_>("last_event_id")).unwrap_or(0);
    let base: String = c.get("base_url");
    let user: String = c.get("username");
    let pass: String = st.secrets.decrypt(&c.get::<String,_>("password"));   // P3
    let silence_after: i32 = c.get("silence_after_s");
    poll_tenant(st, tid, &base, &user, &pass, last_ok, last_event_id, silence_after, now).await
}

async fn mark_poll(st: &AppState, tid: Uuid, reachable: bool, last_event_id: i64,
    dc: Option<i32>, oc: Option<i32>, last_error: Option<&str>, now: DateTime<Utc>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO app.traccar_poll_state(tenant_id,last_event_id,last_poll_at,last_ok_at,reachable,last_error,device_count,online_count,updated_at)
         VALUES($1,$2,$3,$4,$5,$6,COALESCE($7,0),COALESCE($8,0),now())
         ON CONFLICT (tenant_id) DO UPDATE SET
           last_event_id=EXCLUDED.last_event_id,
           last_poll_at=EXCLUDED.last_poll_at,
           last_ok_at=CASE WHEN $5 THEN EXCLUDED.last_poll_at ELSE app.traccar_poll_state.last_ok_at END,
           reachable=EXCLUDED.reachable,
           last_error=EXCLUDED.last_error,
           device_count=COALESCE($7, app.traccar_poll_state.device_count),
           online_count=COALESCE($8, app.traccar_poll_state.online_count),
           updated_at=now()")
        .bind(tid).bind(last_event_id).bind(now).bind(now).bind(reachable).bind(last_error).bind(dc).bind(oc)
        .execute(&st.owner_pool).await?;
    Ok(())
}

async fn poll_tenant(st: &AppState, tid: Uuid, base: &str, user: &str, pass: &str,
    last_ok: Option<DateTime<Utc>>, last_event_id: i64, silence_after: i32, now: DateTime<Utc>) -> anyhow::Result<()> {
    let base = base.trim_end_matches('/');
    let cookie = match api::traccar_login(&st.http, base, user, pass).await {
        Some(c) => c,
        None => { mark_poll(st, tid, false, last_event_id, None, None, Some("login Traccar fallito"), now).await?; return Ok(()); }
    };
    let get = |path: String| st.http.get(format!("{base}{path}")).header("Cookie", &cookie).header("Accept","application/json").send();

    // 1) device: id numerico -> (uniqueId, lastUpdate, status).
    // Un Traccar che AUTENTICA ma degrada (es. 5xx o body invalido su /api/devices)
    // NON e' raggiungibile ai fini sicurezza: segnalo reachable=false cosi' il
    // watchdog connessione scatta (altrimenti entrambi i watchdog resterebbero ciechi).
    let dresp = get("/api/devices".into()).await?;
    if !dresp.status().is_success() {
        mark_poll(st, tid, false, last_event_id, None, None, Some(&format!("Traccar /api/devices {}", dresp.status())), now).await?;
        return Ok(());
    }
    let devs: Value = match dresp.json().await {
        Ok(v) => v,
        Err(_) => { mark_poll(st, tid, false, last_event_id, None, None, Some("Traccar /api/devices: json non valido"), now).await?; return Ok(()); }
    };
    if !devs.is_array() {
        mark_poll(st, tid, false, last_event_id, None, None, Some("Traccar /api/devices: risposta inattesa"), now).await?;
        return Ok(());
    }
    let mut id2uid: std::collections::HashMap<i64,String> = std::collections::HashMap::new();
    let mut device_count = 0i32; let mut online_count = 0i32;
    if let Some(arr) = devs.as_array() {
        for d in arr {
            device_count += 1;
            let did = d.get("id").and_then(|v| v.as_i64());
            let uid = d.get("uniqueId").and_then(|v| v.as_str()).map(|s| s.to_string());
            let tstatus = d.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
            if tstatus == "online" { online_count += 1; }
            // P4 sync: importa/aggiorna il device nel nostro inventario (stato, id Traccar).
            if let Some(uid) = &uid {
                let name = d.get("name").and_then(|v| v.as_str()).unwrap_or(uid.as_str());
                let _ = sqlx::query(
                    "INSERT INTO app.devices(tenant_id,external_id,source,label,traccar_device_id,traccar_status,updated_at)
                     VALUES($1,$2,'traccar',$3,$4,$5,now())
                     ON CONFLICT (tenant_id,source,external_id) DO UPDATE SET
                       traccar_status=EXCLUDED.traccar_status,
                       traccar_device_id=EXCLUDED.traccar_device_id,
                       label=COALESCE(app.devices.label, EXCLUDED.label),
                       updated_at=now()")
                    .bind(tid).bind(uid).bind(name).bind(did).bind(tstatus)
                    .execute(&st.owner_pool).await;
            }
            if let (Some(did), Some(uid)) = (did, uid) { id2uid.insert(did, uid); }
        }
    }

    // 2) posizioni correnti: sync ultima posizione/presenza/storico via pipeline condivisa
    let positions: Value = get("/api/positions".into()).await?.json().await.unwrap_or(json!([]));
    let mut pos_by_dev: std::collections::HashMap<i64,Value> = std::collections::HashMap::new();
    if let Some(arr) = positions.as_array() {
        for p in arr {
            if let Some(did) = p.get("deviceId").and_then(|v| v.as_i64()) { pos_by_dev.insert(did, p.clone()); }
            if let Some(uid) = p.get("deviceId").and_then(|v| v.as_i64()).and_then(|d| id2uid.get(&d)) {
                let _ = api::process_traccar_position(st, tid, p, uid).await;
            }
        }
    }

    // 3) eventi dal cursore (backfill): recupera cio' che il push potrebbe aver perso
    let amap = api::tool_config(st, tid, "traccar", "alarm_map").await;
    let from = last_ok.map(|t| t - chrono::Duration::seconds(120)).unwrap_or_else(|| now - chrono::Duration::hours(1));
    let mut max_evt = last_event_id;
    if !id2uid.is_empty() {
        let mut params: Vec<(String,String)> = vec![
            ("from".into(), from.to_rfc3339()),
            ("to".into(), now.to_rfc3339()),
        ];
        for did in id2uid.keys() { params.push(("deviceId".into(), did.to_string())); }
        let evresp = st.http.get(format!("{base}/api/reports/events"))
            .header("Cookie", &cookie).header("Accept","application/json")
            .query(&params).send().await?;
        if evresp.status().is_success() {
            let events: Value = evresp.json().await.unwrap_or(json!([]));
            if let Some(arr) = events.as_array() {
                // ordina per id per avanzare il cursore in modo monotono
                let mut evs: Vec<&Value> = arr.iter().collect();
                evs.sort_by_key(|e| e.get("id").and_then(|v| v.as_i64()).unwrap_or(0));
                for e in evs {
                    let eid = e.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                    if eid <= last_event_id { continue; }           // gia' visto
                    let did = e.get("deviceId").and_then(|v| v.as_i64());
                    let uid = did.and_then(|d| id2uid.get(&d)).cloned();
                    let etype_t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let alarm = e.get("attributes").and_then(|a| a.get("alarm")).and_then(|v| v.as_str());
                    let pos = did.and_then(|d| pos_by_dev.get(&d));
                    let battery = pos.and_then(|p| p.get("attributes")).and_then(|a| a.get("batteryLevel")).and_then(|v| v.as_f64());
                    let etype = api::map_traccar_etype(&amap, etype_t, alarm, battery);
                    let lat = pos.and_then(|p| p.get("latitude")).and_then(|v| v.as_f64());
                    let lon = pos.and_then(|p| p.get("longitude")).and_then(|v| v.as_f64());
                    // indoor/beacon come nel push path: il beacon della posizione localizza per zona
                    let attrs = pos.and_then(|p| p.get("attributes")).or_else(|| e.get("attributes"));
                    let (indoor, beacon_uid, zone, anchor) = api::resolve_indoor(st, tid, attrs).await;
                    let (lat, lon) = if indoor { if let Some((a,o))=anchor {(Some(a),Some(o))} else {(lat,lon)} } else {(lat,lon)};
                    let srcev = format!("traccar-evt-{}", eid);
                    let _ = api::persist_ingest(st, tid, api::Normalized{
                        source: "traccar".into(),
                        source_event: srcev.clone(),
                        idem: srcev.clone(),
                        etype,
                        device_external_id: uid,
                        site_name: None,
                        lat, lon, accuracy_m: None,
                        correlation: srcev,
                        payload: e.clone(),
                        indoor, beacon_uid, zone,
                    }).await;
                    if eid > max_evt { max_evt = eid; }
                }
            }
        }
    }

    // 4) watchdog silenzio: offline calcolato da NOI, indipendente da deviceOffline di Traccar.
    //    Un episodio di silenzio = un incidente (idem legato a lastUpdate: nuovo silenzio -> nuovo id).
    if silence_after > 0 {
        if let Some(arr) = devs.as_array() {
            // solo i device che tracciamo (source traccar) nel tenant
            let ours: std::collections::HashSet<String> = sqlx::query_scalar::<_,String>(
                "SELECT external_id FROM app.devices WHERE tenant_id=$1 AND source='traccar'")
                .bind(tid).fetch_all(&st.owner_pool).await.unwrap_or_default().into_iter().collect();
            for d in arr {
                let Some(uid) = d.get("uniqueId").and_then(|v| v.as_str()) else { continue; };
                if !ours.contains(uid) { continue; }
                let last_update = d.get("lastUpdate").and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc));
                let Some(lu) = last_update else { continue; };
                if (now - lu).num_seconds() > silence_after as i64 {
                    let did = d.get("id").and_then(|v| v.as_i64());
                    let pos = did.and_then(|dd| pos_by_dev.get(&dd));
                    let lat = pos.and_then(|p| p.get("latitude")).and_then(|v| v.as_f64());
                    let lon = pos.and_then(|p| p.get("longitude")).and_then(|v| v.as_f64());
                    let srcev = format!("traccar-silence-{}-{}", uid, lu.timestamp_millis());
                    let _ = api::persist_ingest(st, tid, api::Normalized{
                        source: "traccar".into(),
                        source_event: srcev.clone(),
                        idem: srcev.clone(),
                        etype: "perdita_contatto".into(),
                        device_external_id: Some(uid.to_string()),
                        site_name: None,
                        lat, lon, accuracy_m: None,
                        correlation: srcev,
                        payload: json!({"watchdog":"silenzio","last_update":lu,"silence_after_s":silence_after}),
                        indoor: false, beacon_uid: None, zone: None,
                    }).await;
                }
            }
        }
    }

    mark_poll(st, tid, true, max_evt, Some(device_count), Some(online_count), None, now).await?;
    Ok(())
}

/// P3: ping agli heartbeat monitor GoAlert configurati -> GoAlert allerta se la
/// nostra pipeline smette di pingare.
async fn heartbeats(st: &AppState) -> anyhow::Result<()> {
    let urls: Vec<String> = sqlx::query_scalar("SELECT heartbeat_url FROM app.goalert_services WHERE heartbeat_url IS NOT NULL")
        .fetch_all(&st.owner_pool).await.unwrap_or_default();
    for u in urls { let _ = st.http.post(&u).send().await; }
    Ok(())
}

// Backoff e policy di ritentativo per l'outbox (#6). Gli invii di SICUREZZA (goalert_*)
// non vengono MAI abbandonati: ritentano con backoff capato finche' non passano, cosi' un
// outage di GoAlert non puo' far sparire un SOS. Solo gli invii non critici vanno in 'error'
// dopo 6 tentativi. last_error rende visibile all'admin perche' sono bloccati.
async fn requeue_outbox(st: &AppState, id: Uuid, tid: Uuid, kind: &str, attempts: i32, err: &str, incident_id: Option<Uuid>) -> anyhow::Result<()> {
    let critical = kind == "goalert_notify" || kind == "goalert_close";
    // backoff esponenziale: 5,10,20,40,80,160,300(cap) secondi
    let shift = std::cmp::min((attempts.max(1) - 1) as u32, 6);
    let backoff_s: i64 = std::cmp::min(300i64, 5i64 * (1i64 << shift));
    if !critical && attempts >= 6 {
        sqlx::query("UPDATE app.outbox SET state='error', last_error=$2 WHERE id=$1")
            .bind(id).bind(err).execute(&st.owner_pool).await?;
        return Ok(());
    }
    sqlx::query("UPDATE app.outbox SET state='pending', last_error=$2, next_attempt_at=now()+($3||' seconds')::interval WHERE id=$1")
        .bind(id).bind(err).bind(backoff_s.to_string()).execute(&st.owner_pool).await?;
    // allarme operativo UNA volta, al superamento della soglia, ma si continua a ritentare
    if critical && attempts == 6 {
        crate::api::publish_event(st, TenantEvent{tenant_id:tid, payload:json!({"type":"notify_failed","incident_id":incident_id,"reason":err})}).await;
    }
    Ok(())
}

async fn drain_outbox(st: &AppState) -> anyhow::Result<()> {
    // claim with a lease so a restart doesn't drop pending sends
    let rows = sqlx::query(
        "UPDATE app.outbox SET state='in_flight', attempts=attempts+1, claimed_until=now()+interval '30 seconds'
         WHERE id IN (
            SELECT id FROM app.outbox
            WHERE (state='pending' AND next_attempt_at <= now()) OR (state='in_flight' AND claimed_until < now())
            ORDER BY created_at LIMIT 10 FOR UPDATE SKIP LOCKED)
         RETURNING id, tenant_id, kind, payload, attempts")
        .fetch_all(&st.owner_pool).await?;

    for r in rows {
        let id: Uuid = r.get("id");
        let tid: Uuid = r.get("tenant_id");
        let payload: serde_json::Value = r.get("payload");
        let attempts: i32 = r.get("attempts");
        let kind: String = r.try_get("kind").unwrap_or_else(|_| "goalert_notify".into());
        let summary = payload.get("summary").and_then(|v| v.as_str()).unwrap_or("incident");
        let dedup = payload.get("dedup").and_then(|v| v.as_str()).unwrap_or("");
        let details = payload.get("details").and_then(|v| v.as_str()).unwrap_or("");
        let incident_id = payload.get("incident_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok());
        // Routing per tipo evento (P3): key specifica per etype, altrimenti key del tenant, altrimenti payload.
        let etype = payload.get("etype").and_then(|v| v.as_str());
        let key: String = sqlx::query_scalar(
            "SELECT COALESCE(
                (SELECT integration_key FROM app.goalert_routing WHERE tenant_id=$1 AND etype=$2),
                (SELECT integration_key FROM app.goalert_services WHERE tenant_id=$1))")
            .bind(tid).bind(etype).fetch_optional(&st.owner_pool).await?.flatten()
            .unwrap_or_else(|| payload.get("integration_key").and_then(|v| v.as_str()).unwrap_or("").to_string());

        // FASE 4 (cutover native): sopprime la notifica/chiusura GoAlert quando il nativo è
        // autoritativo e ha un percorso di consegna reale. Per 'goalert_notify' scatena subito la
        // consegna nativa (latenza ~drain, non il prossimo scan). GoAlert resta provisionato.
        if (kind == "goalert_notify" || kind == "goalert_close") && crate::api::esc_suppress_goalert(st, tid).await {
            sqlx::query("UPDATE app.outbox SET state='done' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
            if let Some(iid) = incident_id {
                let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'goalert_suppressed_native',$3)")
                    .bind(tid).bind(iid).bind(json!({"outbox_kind":kind.clone()})).execute(&st.owner_pool).await;
            }
            if kind == "goalert_notify" {
                let _ = crate::api::escalation_delivery_shadow_tenant(st, tid).await; // consegna nativa immediata (idempotente)
            }
            continue;
        }

        // P1: chiusura alert (quando l'incidente è risolto da noi)
        if kind == "goalert_close" {
            let (ok, d) = goalert::close_notification(&st.http, &st.goalert_base, &key, summary, dedup).await;
            if ok {
                sqlx::query("UPDATE app.outbox SET state='done' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
                if let Some(iid) = incident_id {
                    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'goalert_closed',$3)")
                        .bind(tid).bind(iid).bind(json!({"dedup":dedup})).execute(&st.owner_pool).await?;
                }
            } else {
                requeue_outbox(st, id, tid, &kind, attempts, &format!("goalert_close: {}", d), incident_id).await?;
            }
            continue;
        }

        let (ok, detail) = goalert::request_notification(&st.http, &st.goalert_base, &key, summary, dedup, details).await;
        if ok {
            sqlx::query("UPDATE app.outbox SET state='done' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
            // P2: salva l'AlertID GoAlert sull'incidente per leggere lo stato dopo
            if let (Some(iid), Some(aid)) = (incident_id, detail.get("alert_id").and_then(|v| v.as_i64())) {
                let _ = sqlx::query("UPDATE app.incidents SET goalert_alert_id=$2 WHERE id=$1")
                    .bind(iid).bind(aid as i32).execute(&st.owner_pool).await;
            }
            if let Some(iid) = incident_id {
                // 'goalert_sent' = inviato/accettato da GoAlert (NON ack umano)
                sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'goalert_sent',$3)")
                    .bind(tid).bind(iid).bind(detail).execute(&st.owner_pool).await?;
            }
            crate::api::publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"notify_sent","incident_id":incident_id})}).await;
        } else {
            requeue_outbox(&st, id, tid, &kind, attempts, &format!("goalert_notify: {}", detail), incident_id).await?;
        }
    }
    Ok(())
}

async fn sweep_checkins(st: &AppState) -> anyhow::Result<()> {
    let now = st.clock.now();
    let rows = sqlx::query(
        "UPDATE app.checkins SET state='scaduto'
         WHERE state='attivo' AND due_at <= $1
         RETURNING id, tenant_id, user_id, site_id")
        .bind(now).fetch_all(&st.owner_pool).await?;

    for r in rows {
        let tid: Uuid = r.get("tenant_id");
        let site_id: Option<Uuid> = r.try_get("site_id").ok();
        let correlation = Uuid::new_v4().to_string();
        // mancato_checkin -> event + incident + outbox, system scope
        let ev = sqlx::query(
            "INSERT INTO app.events(tenant_id,source,source_event,idem_key,site_id,etype,origin_ts,received_ts,correlation)
             VALUES($1,'core',$2,$2,$3,'mancato_checkin',$4,$4,$5) RETURNING id")
            .bind(tid).bind(format!("checkin-{}", r.get::<Uuid,_>("id")))
            .bind(site_id).bind(now).bind(&correlation).fetch_one(&st.owner_pool).await?;
        let eid: Uuid = ev.get("id");
        let irow = sqlx::query(
            "INSERT INTO app.incidents(tenant_id,event_id,site_id,etype,correlation,priority) VALUES($1,$2,$3,'mancato_checkin',$4,2) RETURNING id")
            .bind(tid).bind(eid).bind(site_id).bind(&correlation).fetch_one(&st.owner_pool).await?;
        let iid: Uuid = irow.get("id");
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_requested',$3)")
            .bind(tid).bind(iid).bind(json!({"etype":"mancato_checkin"})).execute(&st.owner_pool).await?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'goalert_notify',$2)")
            .bind(tid).bind(json!({"incident_id":iid,"summary":format!("mancato_checkin [{}]",correlation),"dedup":correlation,"integration_key":"","etype":"mancato_checkin","details":format!("mancato check-in; correlazione={}",correlation)}))
            .execute(&st.owner_pool).await?;
        crate::api::publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_opened","incident_id":iid,"etype":"mancato_checkin"})}).await;
    }
    Ok(())
}
