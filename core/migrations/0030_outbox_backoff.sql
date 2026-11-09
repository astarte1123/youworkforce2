-- #6 — hardening dell'outbox di escalation.
-- Problema: i goalert_notify/goalert_close falliti tornavano subito 'pending' (nessun
-- backoff) e venivano abbandonati ('error') dopo 6 tentativi, cioe' ~12s con tick da 2s.
-- Per un prodotto di sicurezza un blip di GoAlert NON deve abbandonare un SOS.
-- Soluzione: backoff esponenziale con next_attempt_at; gli invii critici (notifiche di
-- sicurezza) non vanno mai in 'error' permanente, ritentano con backoff capato; last_error
-- rende visibile all'admin perche' sono bloccati.
ALTER TABLE app.outbox ADD COLUMN IF NOT EXISTS next_attempt_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE app.outbox ADD COLUMN IF NOT EXISTS last_error text;

-- il gate di claim ora filtra anche per next_attempt_at: indice utile al poll
CREATE INDEX IF NOT EXISTS outbox_ready_idx ON app.outbox (state, next_attempt_at);
