-- 0096 — Cantieri C3: costi vs budget. Additiva.
-- Costi di commessa (materiali, noli, subappalti, varie) + costo orario manodopera per valorizzare
-- le ore di presenza. Il budget è già su app.commesse.budget_amount.
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS labor_cost_hour numeric(10,2) NOT NULL DEFAULT 0;
CREATE TABLE IF NOT EXISTS app.commessa_costi (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    description text NOT NULL,
    category    text NOT NULL DEFAULT 'materiale',  -- materiale | nolo | subappalto | altro
    amount      numeric(14,2) NOT NULL DEFAULT 0,
    cost_date   date NOT NULL DEFAULT current_date,
    created_by  uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS commessa_costi_idx ON app.commessa_costi(tenant_id, commessa_id, cost_date);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.commessa_costi TO app_rt';
  EXECUTE 'ALTER TABLE app.commessa_costi ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.commessa_costi USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
