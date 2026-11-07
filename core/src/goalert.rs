use serde_json::json;

/// GoAlert is the single escalation engine (contract line 50). Rust only *requests*
/// a notification; GoAlert runs the on-call policy. Per tenant routing via separate keys.
///
/// Evidence profile:
///   - UPSTREAM REALE when GOALERT_BASE + a generic integration key are configured.
///   - SIMULATO otherwise: the request is logged and acknowledged locally so the
///     SOS chain is demonstrable without a live provider. A local ack is NOT proof
///     the provider delivered (contract line 48).
pub async fn request_notification(
    http: &reqwest::Client,
    base: &Option<String>,
    integration_key: &str,
    summary: &str,
    dedup: &str,
) -> (bool, serde_json::Value) {
    match base {
        Some(b) if !integration_key.is_empty() => {
            // GoAlert generic API integration endpoint.
            let url = format!("{b}/api/v2/generic/incoming?token={integration_key}");
            let body = json!({ "summary": summary, "dedup": dedup });
            match http.post(&url).json(&body).send().await {
                Ok(r) => {
                    let ok = r.status().is_success();
                    (ok, json!({"profile":"UPSTREAM REALE","status": r.status().as_u16(), "dedup": dedup}))
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
