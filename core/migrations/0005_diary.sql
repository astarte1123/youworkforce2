-- Prompt 8: app lavoratore — turni (server-confirmed) e diario operativo (nota dettata/manuale).
-- STT reale lato browser (Web Speech API); il server riceve solo testo. Nessun provider esterno.
SET search_path TO app, public;

CREATE TABLE shifts (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES users(id),
  state       text NOT NULL DEFAULT 'in_corso',   -- in_corso|terminato
  started_at  timestamptz NOT NULL DEFAULT now(),
  ended_at    timestamptz,
  note        text
);
-- At most one open shift per user (server-confirmed, prevents double-start races).
CREATE UNIQUE INDEX shifts_one_open ON shifts(tenant_id, user_id) WHERE state = 'in_corso';

CREATE TABLE diary_entries (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id       uuid NOT NULL REFERENCES users(id),
  work_order_id uuid REFERENCES work_orders(id),
  body          text NOT NULL,
  source        text NOT NULL DEFAULT 'manuale',  -- manuale|voce
  idem_key      text NOT NULL,                     -- client-supplied, for offline re-send dedup
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, user_id, idem_key)
);

ALTER TABLE shifts        ENABLE ROW LEVEL SECURITY;
ALTER TABLE diary_entries ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['shifts','diary_entries']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;
