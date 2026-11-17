-- 0105 — Vigilanza V2: ronde pianificate/ricorrenti. Additiva.
-- Un piano genera le "ronde dovute": per i giorni della settimana scelti, dalla fascia oraria,
-- una ronda ogni N minuti. Il cruscotto confronta le dovute con quelle effettivamente svolte.
CREATE TABLE IF NOT EXISTS app.patrol_schedules (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    route_id      uuid NOT NULL REFERENCES app.patrol_routes(id) ON DELETE CASCADE,
    days_mask     text NOT NULL DEFAULT '1111111',  -- lun..dom
    from_time     time NOT NULL DEFAULT '22:00',
    to_time       time NOT NULL DEFAULT '06:00',
    every_minutes integer NOT NULL DEFAULT 60,       -- cadenza
    active        boolean NOT NULL DEFAULT true,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS patrol_sched_idx ON app.patrol_schedules(tenant_id, active);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.patrol_schedules TO app_rt';
  EXECUTE 'ALTER TABLE app.patrol_schedules ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.patrol_schedules USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
