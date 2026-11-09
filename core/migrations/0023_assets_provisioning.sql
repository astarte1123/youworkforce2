-- ============================================================================
-- Fase C — Asset (mezzi/attrezzature) + binding device->(utente|asset).
-- Fase B — Provisioning: stato onboarding del device (fino ad "attivo al primo fix").
-- ============================================================================
CREATE TABLE assets (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name       text NOT NULL,
  kind       text NOT NULL DEFAULT 'attrezzatura',  -- veicolo|attrezzatura|varco|altro
  identifier text,                                   -- targa/matricola
  site_id    uuid REFERENCES sites(id),
  note       text,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, name)
);

ALTER TABLE devices ADD COLUMN IF NOT EXISTS model_code     text REFERENCES device_models(model_code);
ALTER TABLE devices ADD COLUMN IF NOT EXISTS assigned_asset uuid REFERENCES assets(id);

CREATE TABLE device_provisioning (
  device_id         uuid PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
  tenant_id         uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  step              text NOT NULL DEFAULT 'configurato',  -- configurato|traccar_registrato|in_attesa_primo_fix|attivo
  traccar_device_id int,
  last_error        text,
  updated_at        timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE assets              ENABLE ROW LEVEL SECURITY;
ALTER TABLE device_provisioning ENABLE ROW LEVEL SECURITY;
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['assets','device_provisioning']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;
