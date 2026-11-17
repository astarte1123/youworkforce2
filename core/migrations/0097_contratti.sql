-- 0097 — Interventi A4: contratti/SLA + piani di manutenzione ricorrente. Additiva. Modulo interventi.
CREATE TABLE IF NOT EXISTS app.service_contracts (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    client_id    uuid REFERENCES app.clients(id) ON DELETE SET NULL,
    name         text NOT NULL,
    sla_hours    integer,                 -- tempo di risposta previsto (ore)
    hours_bundle numeric(10,2),           -- monte ore incluso (opz.)
    start_date   date,
    end_date     date,
    active       boolean NOT NULL DEFAULT true,
    note         text,
    created_by   uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS app.maintenance_plans (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    client_id    uuid REFERENCES app.clients(id) ON DELETE SET NULL,
    site_id      uuid REFERENCES app.sites(id) ON DELETE SET NULL,
    contract_id  uuid REFERENCES app.service_contracts(id) ON DELETE SET NULL,
    title        text NOT NULL,
    every_days   integer NOT NULL DEFAULT 30,   -- cadenza in giorni
    next_date    date NOT NULL DEFAULT current_date,
    active       boolean NOT NULL DEFAULT true,
    last_order   uuid REFERENCES app.work_orders(id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS contract_id uuid REFERENCES app.service_contracts(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS contracts_idx ON app.service_contracts(tenant_id, active);
CREATE INDEX IF NOT EXISTS mplans_idx ON app.maintenance_plans(tenant_id, active, next_date);

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['service_contracts','maintenance_plans'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
