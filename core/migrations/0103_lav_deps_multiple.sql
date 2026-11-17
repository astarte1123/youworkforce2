-- 0103 — Cantieri F2: dipendenze MULTIPLE tra lavorazioni (finish-to-start). Additiva.
-- Prima la dipendenza era singola (commessa_lavorazioni.depends_on). Ora M:N; migriamo i dati.
CREATE TABLE IF NOT EXISTS app.commessa_lav_deps (
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    lav_id        uuid NOT NULL REFERENCES app.commessa_lavorazioni(id) ON DELETE CASCADE,
    depends_on_id uuid NOT NULL REFERENCES app.commessa_lavorazioni(id) ON DELETE CASCADE,
    PRIMARY KEY (lav_id, depends_on_id),
    CHECK (lav_id <> depends_on_id)
);
CREATE INDEX IF NOT EXISTS lav_deps_idx ON app.commessa_lav_deps(tenant_id, lav_id);
-- backfill dalla dipendenza singola esistente
INSERT INTO app.commessa_lav_deps(tenant_id, lav_id, depends_on_id)
SELECT tenant_id, id, depends_on FROM app.commessa_lavorazioni WHERE depends_on IS NOT NULL
ON CONFLICT DO NOTHING;

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.commessa_lav_deps TO app_rt';
  EXECUTE 'ALTER TABLE app.commessa_lav_deps ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.commessa_lav_deps USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
