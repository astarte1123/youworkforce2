-- SA-11 (decisione PO 25/11/2026): i canali di consegna esterni dell'escalation (Twilio per SMS e
-- chiamate, Slack, relay email) sono ECCEZIONI DICHIARATE alla regola «nessuna connessione
-- esterna tranne l'AI»: servono ad avvisare persone che non hanno l'app aperta.
-- Le credenziali le configura SOLO il superadmin dalla UI; quelle segrete (token Twilio, token
-- Slack, URL del relay email che può contenere credenziali) sono cifrate con la chiave dei
-- segreti (enc:v3). Le variabili d'ambiente restano solo come valore iniziale di installazione.
CREATE TABLE IF NOT EXISTS app.delivery_channels (
    key         text PRIMARY KEY CHECK (key IN ('YW_ESC_TWILIO_SID','YW_ESC_TWILIO_AUTH','YW_ESC_FROM',
                                                 'YW_ESC_TWILIO_LIVE','YW_ESC_SLACK_TOKEN','YW_ESC_SMTP_RELAY_URL')),
    value       text NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text
);
-- tabella di sistema (nessun tenant): solo il proprietario del DB, mai il ruolo applicativo
REVOKE ALL ON app.delivery_channels FROM PUBLIC;
