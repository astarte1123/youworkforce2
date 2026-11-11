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
}
