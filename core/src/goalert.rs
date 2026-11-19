use serde_json::{json, Value};

/// GoAlert è l'unico motore di escalation (contract line 50). Noi *richiediamo* la
/// notifica (apri/chiudi), GoAlert esegue la policy on-call. Routing per-tenant via key.
///
/// Evidence profile:
///   - UPSTREAM REALE con GOALERT_BASE + integration key configurati.
///   - SIMULATO altrimenti: la richiesta è loggata e accettata localmente.

/// Apre (o deduplica) un alert via Generic API. Ritorna (ok, detail) con AlertID se disponibile.
pub async fn request_notification(
    http: &reqwest::Client,
    base: &Option<String>,
    integration_key: &str,
    summary: &str,
    dedup: &str,
    details: &str,
) -> (bool, Value) {
    match base {
        Some(b) if !integration_key.is_empty() => {
            let url = format!("{b}/api/v2/generic/incoming?token={integration_key}");
            let body = json!({ "summary": summary, "dedup": dedup, "details": details });
            match http.post(&url).header("accept", "application/json").json(&body).send().await {
                Ok(r) => {
                    let status = r.status();
                    let ok = status.is_success();
                    let alert_id = r.json::<Value>().await.ok()
                        .and_then(|v| v.get("AlertID").and_then(|x| x.as_i64()));
                    (ok, json!({"profile":"UPSTREAM REALE","status":status.as_u16(),"dedup":dedup,"alert_id":alert_id}))
                }
                Err(e) => (false, json!({"profile":"UPSTREAM REALE","error": e.to_string()})),
            }
        }
        _ => {
            tracing::info!(summary, dedup, "SIMULATO: GoAlert notify requested");
            (true, json!({"profile":"SIMULATO","summary":summary,"dedup":dedup}))
        }
    }
}

/// Chiude un alert aperto con lo stesso dedup (action=close). P1 close-loop.
pub async fn close_notification(
    http: &reqwest::Client,
    base: &Option<String>,
    integration_key: &str,
    summary: &str,
    dedup: &str,
) -> (bool, Value) {
    match base {
        Some(b) if !integration_key.is_empty() => {
            let url = format!("{b}/api/v2/generic/incoming?token={integration_key}&action=close");
            let body = json!({ "summary": summary, "dedup": dedup });
            match http.post(&url).header("accept", "application/json").json(&body).send().await {
                Ok(r) => (r.status().is_success(), json!({"profile":"UPSTREAM REALE","closed":true,"dedup":dedup})),
                Err(e) => (false, json!({"error": e.to_string()})),
            }
        }
        _ => { tracing::info!(dedup, "SIMULATO: GoAlert close requested"); (true, json!({"profile":"SIMULATO","closed":true})) }
    }
}

