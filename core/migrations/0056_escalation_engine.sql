-- #4 escalation FASE 3 — cutover per-tenant. Flag che sceglie il motore di escalation del tenant.
-- 'goalert' (default) = GoAlert è il motore, il nativo gira in shadow. 'native' = il nativo è
-- autoritativo (consegna live se configurata + ack applica la transizione sull'incidente).
-- GoAlert resta provisionato come rete di sicurezza (spegnimento = FASE 4). Rollback = rimettere 'goalert'.
ALTER TABLE app.tenant_oncall_cfg
  ADD COLUMN IF NOT EXISTS escalation_engine text NOT NULL DEFAULT 'goalert';

-- vincolo valori ammessi (idempotente)
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'tenant_oncall_cfg_engine_chk') THEN
    ALTER TABLE app.tenant_oncall_cfg
      ADD CONSTRAINT tenant_oncall_cfg_engine_chk CHECK (escalation_engine IN ('goalert','native'));
  END IF;
END$$;
