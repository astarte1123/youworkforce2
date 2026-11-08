use serde_json::json;
use sqlx::Row;
use uuid::Uuid;
use crate::state::{AppState, TenantEvent};
use crate::goalert;

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
         RETURNING id, tenant_id, payload, attempts")
        .fetch_all(&st.owner_pool).await?;

    for r in rows {
        let id: Uuid = r.get("id");
        let tid: Uuid = r.get("tenant_id");
        let payload: serde_json::Value = r.get("payload");
        let attempts: i32 = r.get("attempts");
        let summary = payload.get("summary").and_then(|v| v.as_str()).unwrap_or("incident");
        let dedup = payload.get("dedup").and_then(|v| v.as_str()).unwrap_or("");
        // Per-tenant GoAlert integration key (tenant-aware routing, Prompt 5).
        // Fall back to a key embedded in the payload, else SIMULATO.
        let key: String = sqlx::query_scalar("SELECT integration_key FROM app.goalert_services WHERE tenant_id=$1")
            .bind(tid).fetch_optional(&st.owner_pool).await?
            .unwrap_or_else(|| payload.get("integration_key").and_then(|v| v.as_str()).unwrap_or("").to_string());
        let incident_id = payload.get("incident_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok());

        let (ok, detail) = goalert::request_notification(&st.http, &st.goalert_base, &key, summary, dedup).await;
        if ok {
            sqlx::query("UPDATE app.outbox SET state='done' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
            if let Some(iid) = incident_id {
                sqlx::query("INSERT INTO app.incident_timeline(tenant_id,incident_id,kind,detail) VALUES($1,$2,'goalert_ack',$3)")
                    .bind(tid).bind(iid).bind(detail).execute(&st.owner_pool).await?;
            }
            let _ = st.bus.send(TenantEvent{tenant_id:tid,payload:json!({"type":"notify_sent","incident_id":incident_id})});
        } else if attempts >= 6 {
            sqlx::query("UPDATE app.outbox SET state='error' WHERE id=$1").bind(id).execute(&st.owner_pool).await?;
            // operational alarm for stuck events (contract line 48): surfaced as degraded on the bus
            let _ = st.bus.send(TenantEvent{tenant_id:tid,payload:json!({"type":"notify_failed","incident_id":incident_id})});
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
            .bind(tid).bind(json!({"incident_id":iid,"summary":format!("mancato_checkin [{}]",correlation),"dedup":correlation,"integration_key":""}))
            .execute(&st.owner_pool).await?;
        let _ = st.bus.send(TenantEvent{tenant_id:tid,payload:json!({"type":"incident_opened","incident_id":iid,"etype":"mancato_checkin"})});
    }
    Ok(())
}
