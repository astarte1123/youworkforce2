-- #4 escalation FASE 2 — repeat della policy + firma/ack inbound.
-- La ripetizione (ri-notifica dopo l'ultimo step) deve poter ri-consegnare la STESSA rung in un
-- giro successivo: la chiave di idempotenza include quindi il numero di giro (attempt).

ALTER TABLE app.esc_delivery_log ADD COLUMN IF NOT EXISTS attempt int NOT NULL DEFAULT 0;

-- Sostituisce la UNIQUE di 0054 (senza attempt) con una che include attempt.
DO $$
DECLARE c text;
BEGIN
  FOR c IN SELECT conname FROM pg_constraint
           WHERE conrelid = 'app.esc_delivery_log'::regclass AND contype = 'u'
  LOOP EXECUTE format('ALTER TABLE app.esc_delivery_log DROP CONSTRAINT %I', c); END LOOP;
END$$;
CREATE UNIQUE INDEX IF NOT EXISTS esc_delivery_log_idem
  ON app.esc_delivery_log(tenant_id, incident_id, step, channel, to_value, attempt);

-- Numero di ripetizioni dell'escalation nativa (0 = nessuna), per-tenant.
ALTER TABLE app.tenant_oncall_cfg ADD COLUMN IF NOT EXISTS esc_repeat int NOT NULL DEFAULT 0;
