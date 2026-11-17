-- 0099 — Cantieri C4: giornale di cantiere (diario datato). Additiva.
CREATE TABLE IF NOT EXISTS app.commessa_diario (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id   uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    diary_date    date NOT NULL DEFAULT current_date,
    weather       text,                 -- meteo (opz.)
    present_count integer,              -- persone presenti (opz.)
    body          text NOT NULL,        -- attività svolte / note
    created_by    uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS commessa_diario_idx ON app.commessa_diario(tenant_id, commessa_id, diary_date);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.commessa_diario TO app_rt';
  EXECUTE 'ALTER TABLE app.commessa_diario ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.commessa_diario USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
