-- 0106 — IoT nativo F1: telemetria time-series. Additiva. Sostituisce lo storico ThingsBoard.
-- YW diventa la piattaforma: la telemetria dei device è nostra, nessun server esterno.
CREATE TABLE IF NOT EXISTS app.device_telemetry (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    device_id  uuid NOT NULL REFERENCES app.devices(id) ON DELETE CASCADE,
    key        text NOT NULL,
    ts         timestamptz NOT NULL DEFAULT now(),
    num        double precision,     -- valore numerico (se misura)
    str        text                  -- valore testuale (se stato/stringa)
);
CREATE INDEX IF NOT EXISTS device_telemetry_idx ON app.device_telemetry(tenant_id, device_id, key, ts DESC);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, DELETE ON app.device_telemetry TO app_rt';
  EXECUTE 'ALTER TABLE app.device_telemetry ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.device_telemetry USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
