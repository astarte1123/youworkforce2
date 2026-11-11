-- #4 escalation engine nativo — FASE 2 (consegna in ombra).
-- Il motore nativo decide chi/quando e consegna verso un SINK DI PROVA (receiver webhook),
-- NON verso i carrier reali. GoAlert resta il motore di produzione. Due tabelle:
--   * esc_alert_state: macchina a stati per incidente (step corrente, quando ha sparato, repeat).
--   * esc_delivery_log: audit + IDEMPOTENZA della consegna (una rung consegnata una sola volta).

-- Stato della macchina di escalation, una riga per incidente seguito in shadow.
CREATE TABLE IF NOT EXISTS app.esc_alert_state (
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    incident_id   uuid NOT NULL REFERENCES app.incidents(id) ON DELETE CASCADE,
    step          int  NOT NULL DEFAULT -1,   -- -1 = nessuna rung ancora sparata (usize::MAX lato Rust)
    step_fired_at timestamptz NOT NULL DEFAULT now(),
    repeat_count  int  NOT NULL DEFAULT 0,
    status        text NOT NULL DEFAULT 'unack', -- unack|ack|closed|exhausted
    updated_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, incident_id)
);

-- Log di consegna: la UNIQUE è la chiave di idempotenza (nessun doppio invio della stessa rung).
CREATE TABLE IF NOT EXISTS app.esc_delivery_log (
    id           bigserial PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    incident_id  uuid NOT NULL REFERENCES app.incidents(id) ON DELETE CASCADE,
    step         int  NOT NULL,
    channel      text NOT NULL,            -- sms|voice|email|slack
    to_value     text NOT NULL,            -- numero/email/canale destinatario
    state        text NOT NULL,            -- dry|sent|failed  (shadow: mai carrier reale)
    provider_ref text,                     -- id risposta del sink di prova
    detail       jsonb,
    at           timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, incident_id, step, channel, to_value)
);
CREATE INDEX IF NOT EXISTS esc_delivery_log_tenant_at ON app.esc_delivery_log(tenant_id, at DESC);
-- Owner-pool only (come esc_shadow_log/esc_rotation_state): scrittura dal worker/runner con
-- filtro tenant esplicito. Nessuna RLS: nessun accesso dal pool RT applicativo.
