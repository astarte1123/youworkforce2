-- 0131 — Rete di ultima istanza dell'escalation (SL-1 + SL-12).
-- Decisione PO (24/11/2026, domanda 1): fuori dalla fascia di copertura un allarme critico deve
-- comunque arrivare a qualcuno: una lista di contatti email e/o SMS del tenant, avvisati in parallelo
-- o in serie; la prima presa in carico ferma la catena. Con SL-12 la stessa rete vale anche per gli
-- incidenti critici senza gruppo di escalation e per quelli con l'escalation esaurita senza presa in
-- carico. Prima, in tutti questi casi, nessuno veniva avvisato e il gradino risultava comunque partito.

CREATE TABLE IF NOT EXISTS app.esc_fallback_contacts (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
  channel    text NOT NULL CHECK (channel IN ('email','sms')),
  value      text NOT NULL,
  label      text,
  position   integer NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, channel, value)
);

-- Stato per incidente: quando e perché la rete è partita (base dei tempi per l'ordine seriale).
CREATE TABLE IF NOT EXISTS app.esc_fallback_state (
  tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
  incident_id uuid NOT NULL REFERENCES app.incidents(id) ON DELETE CASCADE,
  started_at  timestamptz NOT NULL,
  reason      text NOT NULL CHECK (reason IN ('fuori_copertura','nessun_gruppo','escalation_esaurita')),
  PRIMARY KEY (tenant_id, incident_id)
);

ALTER TABLE app.tenant_oncall_cfg
  ADD COLUMN IF NOT EXISTS fallback_mode text NOT NULL DEFAULT 'parallelo',
  ADD COLUMN IF NOT EXISTS fallback_interval_s integer NOT NULL DEFAULT 120;
DO $$ BEGIN
  ALTER TABLE app.tenant_oncall_cfg ADD CONSTRAINT tenant_oncall_cfg_fallback_mode_chk
    CHECK (fallback_mode IN ('parallelo','seriale'));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  ALTER TABLE app.tenant_oncall_cfg ADD CONSTRAINT tenant_oncall_cfg_fallback_interval_chk
    CHECK (fallback_interval_s BETWEEN 30 AND 3600);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

-- isolamento per tenant (difesa in profondità come 0116) + accesso del ruolo applicativo
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['esc_fallback_contacts','esc_fallback_state'] LOOP
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY;', t);
    IF NOT EXISTS (SELECT 1 FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid
                   JOIN pg_namespace n ON n.oid=c.relnamespace
                   WHERE n.nspname='app' AND c.relname=t AND p.polname='tenant_isolation') THEN
      EXECUTE format($f$
        CREATE POLICY tenant_isolation ON app.%I
          USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
          WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
      $f$, t);
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='app_rt') THEN
      EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
    END IF;
  END LOOP;
END$$;
