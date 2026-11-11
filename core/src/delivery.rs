//! #4 Escalation engine nativo — FASE 2: adapter di consegna (modalità OMBRA).
//!
//! In shadow NON si contattano i carrier reali: ogni rung viene "consegnata" a un SINK DI PROVA
//! (receiver webhook, es. `goalert-delivery.sh`) o, se il sink non è configurato, resta DRY
//! (solo log). Serve a validare la catena decisione→consegna + idempotenza + ack inbound PRIMA
//! di spegnere GoAlert. Il corpo della richiesta è modellato come una vera chiamata Twilio REST
//! (`Messages`/`Calls`) così lo shaping è già pronto per il cutover.
//!
//! Parte PURA (ladder, shaping, chiave idempotenza): testabile senza rete. Parte async: un solo
//! POST al sink, best-effort.

use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel { Sms, Voice, Email, Slack }
impl Channel {
    pub fn as_str(&self) -> &'static str {
        match self { Channel::Sms => "sms", Channel::Voice => "voice", Channel::Email => "email", Channel::Slack => "slack" }
    }
}

/// Una rung della ladder: a che ritardo (dall'apertura alert) e su che canale verso chi.
#[derive(Clone, Debug, PartialEq)]
pub struct Rung { pub delay_s: i64, pub channel: Channel, pub to: String }

/// Ladder per un singolo destinatario on-call, dai recapiti noti e dalla config vocale del tenant.
/// Replica la ladder GoAlert tipica: SMS immediato, poi VOICE dopo `voice_fallback_min` minuti se
/// abilitata. Email come fallback finale se non c'è telefono. Deterministica.
pub fn ladder_for(phone: Option<&str>, email: Option<&str>, voice_on: bool, voice_fallback_min: i64) -> Vec<Rung> {
    let mut out = vec![];
    if let Some(p) = phone.map(str::trim).filter(|s| !s.is_empty()) {
        out.push(Rung { delay_s: 0, channel: Channel::Sms, to: p.to_string() });
        if voice_on {
            out.push(Rung { delay_s: voice_fallback_min.max(0) * 60, channel: Channel::Voice, to: p.to_string() });
        }
    } else if let Some(e) = email.map(str::trim).filter(|s| !s.is_empty()) {
        out.push(Rung { delay_s: 0, channel: Channel::Email, to: e.to_string() });
    }
    out
}

/// Chiave di idempotenza di una consegna (= UNIQUE di `esc_delivery_log`). La stessa rung non
/// viene consegnata due volte anche tra riavvii/istanze.
pub fn delivery_key(incident_id: &str, step: i64, channel: Channel, to: &str) -> String {
    format!("{incident_id}:{step}:{}:{}", channel.as_str(), to)
}

/// Corpo della richiesta, modellato sulle API Twilio REST (shadow: va al sink, non a Twilio).
/// SMS/VOICE → forma `Messages`/`Calls`; email/slack → payload generico. Puro.
pub fn request_body(channel: Channel, from: &str, to: &str, text: &str, incident_id: &str, step: i64) -> Value {
    let base = json!({
        "channel": channel.as_str(), "incident_id": incident_id, "step": step,
        "shadow": true, "engine": "native",
    });
    let mut v = base;
    match channel {
        Channel::Sms => { v["twilio"] = json!({ "resource": "Messages", "From": from, "To": to, "Body": text }); }
        Channel::Voice => {
            // TwiML con Gather DTMF: "premi 1 per prendere in carico" (shaping pronto per il cutover).
            let twiml = format!("<Response><Gather numDigits=\"1\" timeout=\"20\"><Say language=\"it-IT\">{}. Premi 1 per prendere in carico.</Say></Gather></Response>", xml_escape(text));
            v["twilio"] = json!({ "resource": "Calls", "From": from, "To": to, "Twiml": twiml });
        }
        Channel::Email => { v["email"] = json!({ "to": to, "subject": "Allarme", "body": text }); }
        Channel::Slack => { v["slack"] = json!({ "channel": to, "text": text }); }
    }
    v
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Esito di una consegna shadow.
pub struct SinkResult { pub state: &'static str, pub provider_ref: Option<String>, pub detail: Value }

/// Consegna una rung al sink di prova. Se `sink_url` è vuoto → DRY (solo log, nessuna rete).
/// Best-effort: un fallimento ritorna `state="failed"` ma non interrompe il runner (l'outbox/retry
/// vero arriverà col cutover; in shadow non vogliamo perdere il giro per un sink giù).
pub async fn deliver_to_sink(http: &reqwest::Client, sink_url: &str, body: &Value) -> SinkResult {
    if sink_url.trim().is_empty() {
        return SinkResult { state: "dry", provider_ref: None, detail: json!({"note":"nessun YW_ESC_SINK_URL: dry-run"}) };
    }
    match http.post(sink_url).json(body).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let txt = resp.text().await.unwrap_or_default();
            let pref = format!("sink-{code}");
            if (200..300).contains(&code) {
                SinkResult { state: "sent", provider_ref: Some(pref), detail: json!({"http":code,"body":txt.chars().take(200).collect::<String>()}) }
            } else {
                SinkResult { state: "failed", provider_ref: Some(pref), detail: json!({"http":code,"body":txt.chars().take(200).collect::<String>()}) }
            }
        }
        Err(e) => SinkResult { state: "failed", provider_ref: None, detail: json!({"error": e.to_string()}) },
    }
}

// --------------------------- Adapter LIVE (carrier reali) ---------------------------
// Attivi solo col gate `YW_ESC_TWILIO_LIVE=1` (Twilio) / token presente (Slack). Default OFF:
// in shadow non si tocca nessun carrier. Account Twilio NATIVO (env), separato dalla config
// GoAlert per-tenant: un solo numero/credenziali per il motore nativo durante il pilota.

/// Credenziali Twilio del motore nativo, da env. `None` se non configurate → live non disponibile.
pub struct TwilioCfg { pub sid: String, pub auth: String, pub from: String }
impl TwilioCfg {
    pub fn from_env() -> Option<TwilioCfg> {
        let sid = std::env::var("YW_ESC_TWILIO_SID").ok().filter(|s| !s.is_empty())?;
        let auth = std::env::var("YW_ESC_TWILIO_AUTH").ok().filter(|s| !s.is_empty())?;
        let from = std::env::var("YW_ESC_FROM").ok().filter(|s| !s.is_empty())?;
        Some(TwilioCfg { sid, auth, from })
    }
    pub fn live_enabled() -> bool { std::env::var("YW_ESC_TWILIO_LIVE").ok().as_deref() == Some("1") }
}

/// Consegna REALE via Twilio REST. SMS → `Messages.json`; VOICE → `Calls.json` con TwiML inline
/// (`<Gather action=gather_action>` per il DTMF di ack). Basic auth SID:AuthToken.
pub async fn deliver_twilio_live(
    http: &reqwest::Client, cfg: &TwilioCfg, channel: Channel, to: &str, text: &str, gather_action: &str,
) -> SinkResult {
    let (url, form): (String, Vec<(&str, String)>) = match channel {
        Channel::Sms => (
            format!("https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json", cfg.sid),
            vec![("From", cfg.from.clone()), ("To", to.to_string()), ("Body", text.to_string())],
        ),
        Channel::Voice => {
            let twiml = format!("<Response><Gather numDigits=\"1\" timeout=\"20\" action=\"{}\" method=\"POST\"><Say language=\"it-IT\">{}. Premi 1 per prendere in carico.</Say></Gather></Response>", xml_escape(gather_action), xml_escape(text));
            (
                format!("https://api.twilio.com/2010-04-01/Accounts/{}/Calls.json", cfg.sid),
                vec![("From", cfg.from.clone()), ("To", to.to_string()), ("Twiml", twiml)],
            )
        }
        _ => return SinkResult { state: "failed", provider_ref: None, detail: json!({"error":"canale non Twilio"}) },
    };
    match http.post(&url).basic_auth(&cfg.sid, Some(&cfg.auth)).form(&form).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            let body: Value = resp.json().await.unwrap_or(json!({}));
            let sid = body.get("sid").and_then(|x| x.as_str()).map(|s| s.to_string());
            if (200..300).contains(&code) {
                SinkResult { state: "live", provider_ref: sid, detail: json!({"http":code,"status":body.get("status")}) }
            } else {
                SinkResult { state: "failed", provider_ref: sid, detail: json!({"http":code,"twilio":body}) }
            }
        }
        Err(e) => SinkResult { state: "failed", provider_ref: None, detail: json!({"error": e.to_string()}) },
    }
}

/// Consegna REALE su Slack (`chat.postMessage`). Token da env `YW_ESC_SLACK_TOKEN`.
pub async fn deliver_slack_live(http: &reqwest::Client, token: &str, channel: &str, text: &str) -> SinkResult {
    match http.post("https://slack.com/api/chat.postMessage")
        .bearer_auth(token).json(&json!({"channel":channel,"text":text})).send().await {
        Ok(resp) => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            if body.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                SinkResult { state: "live", provider_ref: body.get("ts").and_then(|x| x.as_str()).map(|s| s.to_string()), detail: json!({"slack":"ok"}) }
            } else {
                SinkResult { state: "failed", provider_ref: None, detail: json!({"slack_error": body.get("error")}) }
            }
        }
        Err(e) => SinkResult { state: "failed", provider_ref: None, detail: json!({"error": e.to_string()}) },
    }
}

/// Consegna email via relay HTTP (`YW_ESC_SMTP_RELAY_URL`: un webhook che inoltra a SMTP). Scelta
/// deliberata per non introdurre una dipendenza SMTP pesante nel pilota; se il relay non è
/// configurato l'email resta su sink/dry. Il corpo è lo stesso `request_body(Email,…)`.
pub async fn deliver_email_relay(http: &reqwest::Client, relay_url: &str, body: &Value) -> SinkResult {
    if relay_url.trim().is_empty() {
        return SinkResult { state: "dry", provider_ref: None, detail: json!({"note":"nessun YW_ESC_SMTP_RELAY_URL: email non consegnata (dry)"}) };
    }
    match http.post(relay_url).json(body).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            if (200..300).contains(&code) { SinkResult { state: "live", provider_ref: Some(format!("relay-{code}")), detail: json!({"http":code}) } }
            else { SinkResult { state: "failed", provider_ref: None, detail: json!({"http":code}) } }
        }
        Err(e) => SinkResult { state: "failed", provider_ref: None, detail: json!({"error": e.to_string()}) },
    }
}

// --------------------------- Verifica firma webhook Twilio ---------------------------

/// Valida `X-Twilio-Signature`: HMAC-SHA1(AuthToken) su `url` + concat di (nome+valore) dei
/// parametri POST ordinati per nome, poi base64. Replica l'algoritmo ufficiale Twilio.
/// Confronto a tempo costante. Vedi https://www.twilio.com/docs/usage/security#validating-requests.
pub fn verify_twilio_signature(auth_token: &str, url: &str, params: &[(String, String)], signature: &str) -> bool {
    use hmac::{Hmac, Mac};
    use base64::Engine;
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut data = String::from(url);
    for (k, v) in sorted { data.push_str(k); data.push_str(v); }
    let Ok(mut mac) = Hmac::<sha1::Sha1>::new_from_slice(auth_token.as_bytes()) else { return false; };
    mac.update(data.as_bytes());
    let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    // confronto costante
    let (a, b) = (expected.as_bytes(), signature.as_bytes());
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for i in 0..a.len() { diff |= a[i] ^ b[i]; }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_sms_then_voice() {
        let l = ladder_for(Some("+391112223334"), Some("a@b.c"), true, 5);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], Rung { delay_s: 0, channel: Channel::Sms, to: "+391112223334".into() });
        assert_eq!(l[1], Rung { delay_s: 300, channel: Channel::Voice, to: "+391112223334".into() });
    }
    #[test]
    fn ladder_voice_off_is_sms_only() {
        let l = ladder_for(Some("+39111"), None, false, 5);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].channel, Channel::Sms);
    }
    #[test]
    fn ladder_email_fallback_without_phone() {
        let l = ladder_for(None, Some("a@b.c"), true, 5);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0], Rung { delay_s: 0, channel: Channel::Email, to: "a@b.c".into() });
    }
    #[test]
    fn ladder_empty_without_contacts() {
        assert!(ladder_for(None, None, true, 5).is_empty());
        assert!(ladder_for(Some("  "), Some(""), true, 5).is_empty());
    }
    #[test]
    fn delivery_key_is_stable_and_distinct() {
        let a = delivery_key("inc1", 0, Channel::Sms, "+39111");
        assert_eq!(a, "inc1:0:sms:+39111");
        assert_ne!(a, delivery_key("inc1", 1, Channel::Sms, "+39111"));   // step diverso
        assert_ne!(a, delivery_key("inc1", 0, Channel::Voice, "+39111")); // canale diverso
    }
    #[test]
    fn voice_body_has_gather_and_escapes() {
        let v = request_body(Channel::Voice, "+39000", "+39111", "SOS <A&B>", "inc1", 1);
        let twiml = v.pointer("/twilio/Twiml").and_then(|x| x.as_str()).unwrap();
        assert!(twiml.contains("<Gather"));
        assert!(twiml.contains("Premi 1"));
        assert!(twiml.contains("&lt;A&amp;B&gt;")); // escaping XML
        assert_eq!(v.pointer("/twilio/resource").unwrap(), "Calls");
    }
    #[test]
    fn sms_body_shape() {
        let v = request_body(Channel::Sms, "+39000", "+39111", "ciao", "inc1", 0);
        assert_eq!(v.pointer("/twilio/resource").unwrap(), "Messages");
        assert_eq!(v.pointer("/twilio/Body").unwrap(), "ciao");
        assert_eq!(v["shadow"], true);
    }
    #[test]
    fn twilio_signature_official_vector() {
        // Vettore dalla doc Twilio (Security / Validating Requests).
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let params: Vec<(String,String)> = vec![
            ("CallSid","CA1234567890ABCDE"),("Caller","+14158675309"),("Digits","1234"),
            ("From","+14158675309"),("To","+18005551212"),
        ].into_iter().map(|(k,v)| (k.to_string(), v.to_string())).collect();
        // atteso verificato con: openssl dgst -sha1 -hmac 12345 -binary | openssl base64
        assert!(verify_twilio_signature("12345", url, &params, "RSOYDt4T1cUTdK1PDd93/VVr8B8="));
        // firma errata -> rifiutata
        assert!(!verify_twilio_signature("12345", url, &params, "wrongsignaturevalue=="));
        // token errato -> rifiutata
        assert!(!verify_twilio_signature("99999", url, &params, "RSOYDt4T1cUTdK1PDd93/VVr8B8="));
    }
}
