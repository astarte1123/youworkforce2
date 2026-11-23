use serde_json::json;
use sqlx::Row;
use uuid::Uuid;
use chrono::{DateTime, Utc};
use crate::state::{AppState, TenantEvent};
use crate::{api, privacy};

/// Background worker (system scope, owner connection bypasses RLS).
/// 1) drains the outbox to YWAlert (at-least-once, retry with backoff)
/// 2) sweeps durable check-in timers using the controllable clock
pub async fn run(st: AppState) {
    loop {
        if let Err(e) = tick(&st).await { tracing::warn!("worker tick error: {e}"); }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn tick(st: &AppState) -> anyhow::Result<()> {
    // heartbeat monitoring: timestamp dell'ultimo giro del worker (per il cruscotto superadmin).
    st.worker_heartbeat.store(chrono::Utc::now().timestamp(), std::sync::atomic::Ordering::Relaxed);
    // STACK: ogni sub-worker è una `async fn` con future profondo (debug: frame grandi). Se awaitati
    // direttamente, il future di `tick` conterrebbe TUTTI i loro stati come campi annidati (somma dei
    // sotto-alberi) -> il frame di poll cresce fino allo stack-overflow sotto carico. `Box::pin` sposta
    // ciascun future sullo heap: `tick` tiene solo puntatori e la profondità di stack resta limitata.
    Box::pin(drain_outbox(st)).await?;
    Box::pin(sweep_checkins(st)).await?;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    // watchdog silenzio dispositivi personali (salva-vita): ~ogni 10s
    if n % 5 == 2 { if let Err(e) = Box::pin(gps_silence_watchdog(st)).await { tracing::warn!("gps silence watchdog: {e}"); } }
    // #7 ritenzione GDPR (opt-in YW_RETENTION_DAYS): ~ogni ora (1800 tick * 2s)
    if n % 1800 == 0 { Box::pin(privacy::purge_retention(st)).await; }
    // #4 escalation nativo FASE 2 (consegna in ombra): ~ogni 30s, verso sink di prova, idempotente
    if n % 15 == 7 { Box::pin(api::escalation_delivery_scan(st)).await; }
    // F3 annunci Slack on-call (onchange/schedulati): ~ogni minuto, best-effort
    if n % 30 == 11 { Box::pin(api::slack_announce_scan(st)).await; }
    // S1 heartbeat monitor nativo: ~ogni 30s controlla i monitor scaduti
    if n % 15 == 3 { Box::pin(api::heartbeat_watchdog(st)).await; }
    // IoT Fase 3: watchdog offline dei device IoT (alert in vista separata) ~ogni 6s
    if n % 3 == 1 { if let Err(e) = Box::pin(api::iot_offline_watchdog(st)).await { tracing::warn!("iot watchdog: {e}"); } }
    // watchdog infrastrutturale (server/DB/worker): ~ogni 60s, apre alert + notifica superadmin
    if n % 30 == 13 { Box::pin(api::system_watchdog(st)).await; }
    Ok(())
}

// ============================================================================
// Watchdog NATIVO del silenzio dei dispositivi personali (modulo `gps`).
// Sicurezza: un badge/tracker che smette di trasmettere (spento, scarico, fuori copertura,
// manomesso) non può più segnalare SOS o caduta. Oltre la soglia `silence_after_s` dello
// strumento gps/alarm_map (default 600 s, 0 = disattivo) si apre un incidente `perdita_contatto`.
// Vale per i dispositivi ASSEGNATI a un lavoratore: un device a magazzino non protegge nessuno e
// non deve generare allarmi.
// Un episodio di silenzio = un incidente: l'idem è legato all'ultimo contatto, quindi lo stesso
// silenzio non si duplica e un nuovo silenzio dopo una ripresa ne apre uno nuovo.
// ============================================================================
pub async fn gps_silence_watchdog(st: &AppState) -> anyhow::Result<()> {
    let now = st.clock.now();
    let rows = sqlx::query(
        "SELECT d.tenant_id, d.external_id, d.last_lat, d.last_lon, d.last_seen_at
         FROM app.devices d
         JOIN app.tenant_modules m ON m.tenant_id=d.tenant_id AND m.module_code='gps' AND m.state='attivo'
         WHERE d.source='gps' AND COALESCE(d.enabled,true) AND d.last_seen_at IS NOT NULL
           AND d.assigned_user IS NOT NULL")
        .fetch_all(&st.owner_pool).await?;
    let mut threshold: std::collections::HashMap<Uuid, i64> = std::collections::HashMap::new();
    for r in rows {
        let tid: Uuid = r.get("tenant_id");
        let after = match threshold.get(&tid) {
            Some(v) => *v,
            None => {
                let cfg = api::tool_config(st, tid, "gps", "alarm_map").await;
                let v = cfg.get("silence_after_s").and_then(|v| v.as_i64()).unwrap_or(600);
                threshold.insert(tid, v);
                v
            }
        };
        if after <= 0 { continue; }
        let last: DateTime<Utc> = r.get("last_seen_at");
        let silent_for = (now - last).num_seconds();
        if silent_for <= after { continue; }
        let ext: String = r.get("external_id");
        let srcev = format!("gps-silence-{}-{}", ext, last.timestamp_millis());
        // già segnalato questo episodio? (evita una scrittura per tick per ogni device silente)
        let seen: Option<i32> = sqlx::query_scalar("SELECT 1 FROM app.events WHERE tenant_id=$1 AND idem_key=$2")
            .bind(tid).bind(&srcev).fetch_optional(&st.owner_pool).await?;
        if seen.is_some() { continue; }
        let res = api::persist_ingest(st, tid, api::Normalized{
            source: "gps".into(),
            source_event: srcev.clone(),
            idem: srcev.clone(),
            etype: "perdita_contatto".into(),
            device_external_id: Some(ext.clone()),
            site_name: None,
            lat: r.try_get("last_lat").ok(), lon: r.try_get("last_lon").ok(), accuracy_m: None,
            correlation: srcev,
            payload: json!({"watchdog":"silenzio","last_seen_at":last,"silence_after_s":after,"silent_for_s":silent_for}),
            indoor: false, beacon_uid: None, zone: None,
        }).await;
        if let Err(e) = res { tracing::error!("watchdog silenzio {ext} tenant {tid}: incidente non creato: {:?}", e); }
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
        let kind: String = r.try_get("kind").unwrap_or_else(|_| "notify".into());
        let incident_id = payload.get("incident_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok());
        let _ = attempts; // lease gia' incrementato; la consegna nativa e' idempotente, nessun requeue
        // Consegna NATIVA (self-contained): 'notify' scatena subito il runner nativo
        // (escalation_delivery_run_tenant, idempotente, latenza ~drain); 'close' e' no-op (il
        // runner nativo smette da se' quando l'incidente risulta chiuso). La riga outbox si chiude.
        sqlx::query("UPDATE app.outbox SET state='done' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
        if kind == "notify" {
            if let Some(iid) = incident_id {
                let _ = sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_dispatched',$3)")
                    .bind(tid).bind(iid).bind(json!({"engine":"native"})).execute(&st.owner_pool).await;
            }
            let _ = Box::pin(crate::api::escalation_delivery_run_tenant(st, tid)).await;
            crate::api::publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"notify_sent","incident_id":incident_id})}).await;
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
        let uid: Option<Uuid> = r.try_get("user_id").ok();
        let site_id: Option<Uuid> = r.try_get("site_id").ok();
        let correlation = Uuid::new_v4().to_string();
        // Fase C gating: un profilo attivo del lavoratore può disabilitare il trigger mancato_checkin
        // (fail-open). Il check-in resta 'scaduto', ma non si apre l'incidente.
        if crate::api::trigger_suppressed(st, tid, uid, None, "mancato_checkin").await {
            crate::api::audit(st, tid, "system", "trigger_suppressed_by_profile", json!({"etype":"mancato_checkin","user_id":uid})).await;
            continue;
        }
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
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'notify',$2)")
            .bind(tid).bind(json!({"incident_id":iid,"summary":format!("mancato_checkin [{}]",correlation),"dedup":correlation,"integration_key":"","etype":"mancato_checkin","details":format!("mancato check-in; correlazione={}",correlation)}))
            .execute(&st.owner_pool).await?;
        crate::api::publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_opened","incident_id":iid,"etype":"mancato_checkin"})}).await;
    }
    Ok(())
}
