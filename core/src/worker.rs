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
        "SELECT c.tenant_id, c.base_url, c.username, c.password, c.poll_interval_s, c.silence_after_s
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
        let pass: String = c.get("password");
        let silence_after: i32 = c.get("silence_after_s");
        if let Err(e) = poll_tenant(st, tid, &base, &user, &pass, last_ok, last_event_id, silence_after, now).await {
            // marca non raggiungibile ma non interrompe gli altri tenant
            let _ = mark_poll(st, tid, false, last_event_id, None, None, Some(&e.to_string()), now).await;
        }
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
    let pass: String = c.get("password");
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

    // 1) device: id numerico -> (uniqueId, lastUpdate, status)
    let devs: Value = get("/api/devices".into()).await?.json().await.unwrap_or(json!([]));
    let mut id2uid: std::collections::HashMap<i64,String> = std::collections::HashMap::new();
    let mut device_count = 0i32; let mut online_count = 0i32;
    if let Some(arr) = devs.as_array() {
        for d in arr {
            device_count += 1;
            let did = d.get("id").and_then(|v| v.as_i64());
            let uid = d.get("uniqueId").and_then(|v| v.as_str()).map(|s| s.to_string());
            if d.get("status").and_then(|v| v.as_str()) == Some("online") { online_count += 1; }
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

async fn drain_outbox(st: &AppState) -> anyhow::Result<()> {
    // claim with a lease so a restart doesn't drop pending sends
    let rows = sqlx::query(
        "UPDATE app.outbox SET state='in_flight', attempts=attempts+1, claimed_until=now()+interval '30 seconds'
         WHERE id IN (
            SELECT id FROM app.outbox
            WHERE state='pending' OR (state='in_flight' AND claimed_until < now())
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

        // P1: chiusura alert (quando l'incidente è risolto da noi)
        if kind == "goalert_close" {
            let (ok, _d) = goalert::close_notification(&st.http, &st.goalert_base, &key, summary, dedup).await;
            if ok {
                sqlx::query("UPDATE app.outbox SET state='done' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
                if let Some(iid) = incident_id {
                    sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'goalert_closed',$3)")
                        .bind(tid).bind(iid).bind(json!({"dedup":dedup})).execute(&st.owner_pool).await?;
                }
            } else if attempts >= 6 {
                sqlx::query("UPDATE app.outbox SET state='error' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
            } else {
                sqlx::query("UPDATE app.outbox SET state='pending' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
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
        } else if attempts >= 6 {
            sqlx::query("UPDATE app.outbox SET state='error' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
            // operational alarm for stuck events (contract line 48): surfaced as degraded on the bus
            crate::api::publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"notify_failed","incident_id":incident_id})}).await;
        } else {
            sqlx::query("UPDATE app.outbox SET state='pending' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
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
