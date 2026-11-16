-- 0081 — Calendario core condiviso (W1). Token iCal PERSONALE unificato (un feed per persona,
-- aggrega turni/interventi/ronde dei moduli attivi). Additiva.
CREATE TABLE IF NOT EXISTS app.calendar_ical_tokens (
    tenant_id uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id   uuid NOT NULL REFERENCES app.users(id)   ON DELETE CASCADE,
    token     text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, user_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS calendar_ical_tokens_token ON app.calendar_ical_tokens(token);
-- Token tabella di sistema (lookup per token senza sessione): accesso solo via owner_pool.
