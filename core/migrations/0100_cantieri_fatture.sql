-- 0100 — Cantieri C5: fatturazione da SAL. Additiva.
-- Dal SAL (stato avanzamento) si emette un documento con l'importo maturato.
CREATE TABLE IF NOT EXISTS app.commessa_fatture (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    sal_id      uuid REFERENCES app.commessa_sal(id) ON DELETE SET NULL,
    number      text NOT NULL,                       -- numero documento
    amount      numeric(14,2) NOT NULL DEFAULT 0,
    issue_date  date NOT NULL DEFAULT current_date,
    status      text NOT NULL DEFAULT 'emessa',      -- emessa | pagata | annullata
    note        text,
    created_by  uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS commessa_fatture_idx ON app.commessa_fatture(tenant_id, commessa_id, issue_date);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.commessa_fatture TO app_rt';
  EXECUTE 'ALTER TABLE app.commessa_fatture ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.commessa_fatture USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
