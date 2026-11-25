use serde_json::json;
use sqlx::Row;
use uuid::Uuid;
use chrono::{DateTime, Utc};
use crate::state::{AppState, TenantEvent};
use crate::{api, privacy};

/// Background worker (system scope, owner connection bypasses RLS).
/// 1) drains the outbox into the native escalation runner (at-least-once)
/// 2) sweeps durable check-in timers using the controllable clock
pub async fn run(st: AppState) {
    loop {
        if let Err(e) = tick(&st).await { tracing::warn!("worker tick error: {e}"); }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// AR-6: watchdog infrastrutturale (server/DB/worker) in un task INDIPENDENTE dal ciclo del
/// worker. Prima girava dentro `tick`: se il worker si bloccava (query appesa, sotto-watchdog
/// lento) si fermava anche chi doveva accorgersene. Ora il battito vecchio apre l'alert «worker».
pub async fn system_watchdog_loop(st: AppState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        Box::pin(api::system_watchdog(&st)).await;
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
    // SL-2: immobilità (preavviso «Stai bene?» + allarme): ~ogni 10s
    if n % 5 == 4 { Box::pin(api::immobility_watchdog(st)).await; }
    // CA-12: ronde pianificate non svolte -> incidente «Ronda mancata» (~ogni 30 s)
    if n % 15 == 13 { Box::pin(api::patrol_missed_watchdog(st)).await; }
    if n % 30 == 23 { Box::pin(api::stale_open_watchdog(st)).await; } // CA-17
    // #7 ritenzione GDPR (opt-in YW_RETENTION_DAYS): ~ogni ora (1800 tick * 2s)
    if n % 1800 == 0 { Box::pin(privacy::purge_retention(st)).await; }
    if n % 1800 == 900 { api::purge_connect_attempts(st).await; }
    // #4 escalation nativo FASE 2 (consegna in ombra): ~ogni 30s, verso sink di prova, idempotente
    if n % 15 == 7 { Box::pin(api::escalation_delivery_scan(st)).await; }
    // F3 annunci Slack on-call (onchange/schedulati): ~ogni minuto, best-effort
    if n % 30 == 11 { Box::pin(api::slack_announce_scan(st)).await; }
    // S1 heartbeat monitor nativo: ~ogni 30s controlla i monitor scaduti
    if n % 15 == 3 { Box::pin(api::heartbeat_watchdog(st)).await; }
    // IoT Fase 3: watchdog offline dei device IoT (alert in vista separata) ~ogni 6s
    if n % 3 == 1 { if let Err(e) = Box::pin(api::iot_offline_watchdog(st)).await { tracing::warn!("iot watchdog: {e}"); } }
    // (AR-6) il watchdog infrastrutturale NON gira più qui: vedi system_watchdog_loop
    // SA-5: sospensioni programmate dal superadmin giunte a scadenza: ~ogni 60s
    if n % 30 == 17 { Box::pin(api::tenant_suspension_scan(st)).await; }
    if n % 30 == 19 { Box::pin(api::secrets_sync(st)).await; } // chiave dei segreti tra istanze
    Ok(())
}

// ============================================================================
// Watchdog NATIVO del silenzio dei dispositivi personali (modulo `gps`).
// Sicurezza: un badge/tracker che smette di trasmettere (spento, scarico, fuori copertura,
// manomesso) non può più segnalare SOS o caduta. Oltre la soglia `silence_after_s` dello
// strumento gps/alarm_map (default 600 s, 0 = disattivo) si apre un incidente `perdita_contatto`.
// Perimetro: SOLO i badge IN SERVIZIO (`service_state='in_servizio'`, mig 0128), cioè messi in
// servizio esplicitamente e assegnati a un lavoratore. In attesa di messa in servizio o fuori
// servizio = visibili in console, nessun allarme.
// Riferimento del conteggio = GREATEST(ultimo contatto, `silence_armed_at`): l'armamento avviene
// alla MESSA IN SERVIZIO e a ogni passaggio di consegne (trigger DB). Così un badge appena messo
// in servizio che non ha MAI trasmesso fa scattare l'allarme alla soglia (proprio il caso del badge
// consegnato spento), mentre upgrade e consegne non generano allarmi a raffica.
// Il contatto è qualsiasi pacchetto autenticato, anche senza fix: mai l'ultima posizione.
// Un episodio di silenzio = un incidente: l'idem è legato al riferimento del conteggio, quindi lo
// stesso silenzio non si duplica e un nuovo silenzio (dopo un contatto o una nuova consegna) ne
// apre uno nuovo.
// ============================================================================
pub async fn gps_silence_watchdog(st: &AppState) -> anyhow::Result<()> {
    let now = st.clock.now();
    // Q4 (PO 24/11/2026): silenzio sorvegliato solo durante il turno; vale anche per l'app.
    // Turno = turno aperto nell'app, oppure (solo badge) un turno pianificato che copre adesso.
    let rows = sqlx::query(
        "SELECT d.tenant_id, d.source, d.external_id, d.last_lat, d.last_lon, d.last_seen_at, d.last_pos_at,
                d.silence_armed_at, sh.since AS shift_since
         FROM app.devices d
         JOIN app.tenants tn ON tn.id=d.tenant_id AND tn.status<>'sospeso'
         LEFT JOIN LATERAL (SELECT min(x) AS since FROM (
               SELECT s.started_at AS x FROM app.shifts s
                WHERE s.tenant_id=d.tenant_id AND s.user_id=d.assigned_user AND s.state='in_corso'
               UNION ALL
               SELECT p.starts_at FROM app.shift_plans p
                WHERE d.source='gps' AND p.tenant_id=d.tenant_id AND p.user_id=d.assigned_user
                  AND p.state<>'annullato' AND p.starts_at <= $1 AND p.ends_at > $1) q) sh ON true
         WHERE COALESCE(d.enabled,true) AND d.assigned_user IS NOT NULL AND (
               (d.source='gps' AND d.service_state='in_servizio' AND d.silence_armed_at IS NOT NULL
                AND EXISTS (SELECT 1 FROM app.tenant_modules m WHERE m.tenant_id=d.tenant_id AND m.module_code=ANY($2) AND m.state='attivo'))
            OR (d.source='pwa' AND sh.since IS NOT NULL
                AND EXISTS (SELECT 1 FROM app.tenant_modules m WHERE m.tenant_id=d.tenant_id AND m.module_code=ANY($3) AND m.state='attivo')
                -- chi ha un badge in servizio è sorvegliato dal badge: l'app non raddoppia gli allarmi
                AND NOT EXISTS (SELECT 1 FROM app.devices g WHERE g.tenant_id=d.tenant_id AND g.assigned_user=d.assigned_user
                                  AND g.source='gps' AND g.service_state='in_servizio' AND COALESCE(g.enabled,true))))")
        // SL-20: moduli dalla matrice unica modules::FEATURE_MODULES
        .bind(now).bind(crate::modules::feature_modules("badge_silence")).bind(crate::modules::feature_modules("app_silence"))
        .fetch_all(&st.owner_pool).await?;
    let mut gcfg: std::collections::HashMap<Uuid, (i64, bool)> = std::collections::HashMap::new();
    let mut acfg: std::collections::HashMap<Uuid, i64> = std::collections::HashMap::new();
    for r in rows {
        let tid: Uuid = r.get("tenant_id");
        let source: String = r.get("source");
        let app = source == "pwa";
        let shift_since: Option<DateTime<Utc>> = r.try_get("shift_since").ok().flatten();
        let last: Option<DateTime<Utc>> = r.try_get("last_seen_at").ok().flatten();
        let last_pos: Option<DateTime<Utc>> = r.try_get("last_pos_at").ok().flatten();
        let armed: Option<DateTime<Utc>> = r.try_get("silence_armed_at").ok().flatten();
        let (after, reference) = if app {
            let after = match acfg.get(&tid) { Some(v) => *v, None => {
                let v = api::tool_config(st, tid, "loneworker", "app_silence").await
                    .get("app_silence_after_s").and_then(|v| v.as_i64()).unwrap_or(900);
                acfg.insert(tid, v); v } };
            // l'app conta dall'ultimo segno di vita (battito o posizione) o dall'inizio del turno
            (after, [last, last_pos, shift_since].into_iter().flatten().max())
        } else {
            let (after, shift_only) = match gcfg.get(&tid) { Some(v) => *v, None => {
                let cfg = api::tool_config(st, tid, "gps", "alarm_map").await;
                let v = (cfg.get("silence_after_s").and_then(|v| v.as_i64()).unwrap_or(600),
                         cfg.get("silence_shift_only").and_then(|v| v.as_str()).map(|s| s != "no").unwrap_or(true));
                gcfg.insert(tid, v); v } };
            if shift_only && shift_since.is_none() { continue; } // fuori turno: non sorvegliato
            (after, [last, armed, if shift_only { shift_since } else { None }].into_iter().flatten().max())
        };
        if after <= 0 { continue; }
        let Some(reference) = reference else { continue; };
        let Some(silent_for) = silence_exceeded(now, reference, after) else { continue; };
        let ext: String = r.get("external_id");
        let srcev = format!("{}-silence-{}-{}", if app { "app" } else { "gps" }, ext, reference.timestamp_millis());
        // già segnalato questo episodio? (evita una scrittura per tick per ogni device silente)
        let seen: Option<i32> = sqlx::query_scalar("SELECT 1 FROM app.events WHERE tenant_id=$1 AND idem_key=$2")
            .bind(tid).bind(&srcev).fetch_optional(&st.owner_pool).await?;
        if seen.is_some() { continue; }
        let res = api::persist_ingest(st, tid, api::Normalized{
            source: source.clone(),
            source_event: srcev.clone(),
            idem: srcev.clone(),
            etype: "perdita_contatto".into(),
            device_external_id: Some(ext.clone()),
            site_name: None,
            lat: r.try_get("last_lat").ok(), lon: r.try_get("last_lon").ok(), accuracy_m: None,
            correlation: srcev,
            payload: json!({"watchdog":"silenzio","channel": if app { "app" } else { "badge" },
                "last_seen_at":last,"last_pos_at":last_pos,"shift_since":shift_since,
                "armed_from":armed,"never_seen":last.is_none() && last_pos.is_none(),
                "silence_after_s":after,"silent_for_s":silent_for}),
            indoor: false, beacon_uid: None, zone: None,
        }).await;
        if let Err(e) = res { tracing::error!("watchdog silenzio {ext} tenant {tid}: incidente non creato: {:?}", e); }
    }
    Ok(())
}

/// Silenzio oltre la soglia? Ritorna i secondi di silenzio solo se `> after` (alla soglia esatta
/// non scatta). Pura: testata in unità.
pub fn silence_exceeded(now: DateTime<Utc>, reference: DateTime<Utc>, after: i64) -> Option<i64> {
    if after <= 0 { return None; }
    let s = (now - reference).num_seconds();
    if s > after { Some(s) } else { None }
}

#[cfg(test)]
mod silence_tests {
    use super::*;
    use chrono::TimeZone;
    fn t(s: i64) -> DateTime<Utc> { Utc.with_ymd_and_hms(2026, 1, 1, 8, 0, 0).unwrap() + chrono::Duration::seconds(s) }

    #[test]
    fn before_and_at_threshold_no_alarm() {
        assert_eq!(silence_exceeded(t(599), t(0), 600), None);
        assert_eq!(silence_exceeded(t(600), t(0), 600), None);
    }
    #[test]
    fn after_threshold_alarm() {
        assert_eq!(silence_exceeded(t(601), t(0), 600), Some(601));
    }
    #[test]
    fn disabled_threshold_never_alarms() {
        assert_eq!(silence_exceeded(t(100_000), t(0), 0), None);
    }
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
    // SL-5: UN check-in scaduto per transazione. Prima il check-in veniva marcato 'scaduto' in blocco
    // e l'incidente creato dopo, fuori transazione: un errore a metà lasciava il check-in scaduto
    // SENZA allarme (e non sarebbe più stato ripreso). Ora marcatura, evento, incidente e notifica
    // si confermano insieme o per niente; FOR UPDATE SKIP LOCKED evita doppioni tra istanze.
    let now = st.clock.now();
    for _ in 0..500 {
        let mut tx = st.owner_pool.begin().await?;
        let row = sqlx::query(
            "SELECT c.id, c.tenant_id, c.user_id, c.site_id FROM app.checkins c
              WHERE c.state='attivo' AND c.due_at <= $1
                AND NOT EXISTS (SELECT 1 FROM app.tenants t WHERE t.id=c.tenant_id AND t.status='sospeso')
              ORDER BY c.due_at LIMIT 1 FOR UPDATE SKIP LOCKED")
            .bind(now).fetch_optional(&mut *tx).await?;
        let Some(r) = row else { tx.rollback().await?; break; };
        let cid: Uuid = r.get("id");
        let tid: Uuid = r.get("tenant_id");
        let uid: Option<Uuid> = r.try_get("user_id").ok().flatten();
        sqlx::query("UPDATE app.checkins SET state='scaduto' WHERE id=$1 AND tenant_id=$2").bind(cid).bind(tid).execute(&mut *tx).await?;
        // Fase C gating: un profilo attivo del lavoratore può disabilitare il trigger mancato_checkin
        // (fail-open). Il check-in resta 'scaduto', ma non si apre l'incidente.
        if crate::api::trigger_suppressed(st, tid, uid, None, "mancato_checkin").await {
            tx.commit().await?;
            crate::api::audit(st, tid, "system", "trigger_suppressed_by_profile", json!({"etype":"mancato_checkin","user_id":uid})).await;
            continue;
        }
        // SL-6: sede del check-in, altrimenti quella del lavoratore se univoca; lavoratore sull'incidente
        let mut site_id: Option<Uuid> = r.try_get("site_id").ok().flatten();
        if site_id.is_none() { if let Some(u) = uid { site_id = crate::api::user_single_site(st, tid, u).await; } }
        let correlation = Uuid::new_v4().to_string();
        let ev = sqlx::query(
            "INSERT INTO app.events(tenant_id,source,source_event,idem_key,site_id,etype,origin_ts,received_ts,correlation)
             VALUES($1,'core',$2,$2,$3,'mancato_checkin',$4,$4,$5)
             ON CONFLICT (tenant_id,source,idem_key) DO NOTHING RETURNING id")
            .bind(tid).bind(format!("checkin-{cid}")).bind(site_id).bind(now).bind(&correlation).fetch_optional(&mut *tx).await?;
        let Some(ev) = ev else { tx.commit().await?; continue; }; // già gestito
        let eid: Uuid = ev.get("id");
        let irow = sqlx::query(
            "INSERT INTO app.incidents(tenant_id,event_id,site_id,etype,correlation,priority,worker_user_id) VALUES($1,$2,$3,'mancato_checkin',$4,2,$5) RETURNING id")
            .bind(tid).bind(eid).bind(site_id).bind(&correlation).bind(uid).fetch_one(&mut *tx).await?;
        let iid: Uuid = irow.get("id");
        sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'notify_requested',$3)")
            .bind(tid).bind(iid).bind(json!({"etype":"mancato_checkin","checkin_id":cid})).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO app.outbox(tenant_id,kind,payload) VALUES($1,'notify',$2)")
            .bind(tid).bind(json!({"incident_id":iid,"summary":format!("mancato_checkin [{}]",correlation),"dedup":correlation,"integration_key":"","etype":"mancato_checkin","details":format!("mancato check-in; correlazione={}",correlation)}))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        crate::api::publish_event(&st, TenantEvent{tenant_id:tid,payload:json!({"type":"incident_opened","incident_id":iid,"etype":"mancato_checkin"})}).await;
    }
    Ok(())
}
