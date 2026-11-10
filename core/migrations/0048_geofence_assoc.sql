-- Associazioni dell'area geofence, gestite dall'admin del tenant: quali dispositivi IoT
-- appartengono all'area e quali tipi di evento (etype) sono rilevanti/monitorati nell'area.
-- Usabili per contesto presenza/monitoraggio e routing per zona.
CREATE TABLE IF NOT EXISTS app.geofence_devices (
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    geofence_id uuid NOT NULL REFERENCES app.geofences(id) ON DELETE CASCADE,
    device_id   uuid NOT NULL REFERENCES app.devices(id) ON DELETE CASCADE,
    PRIMARY KEY (geofence_id, device_id)
);
CREATE TABLE IF NOT EXISTS app.geofence_events (
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    geofence_id uuid NOT NULL REFERENCES app.geofences(id) ON DELETE CASCADE,
    etype       text NOT NULL,
    PRIMARY KEY (geofence_id, etype)
);
ALTER TABLE app.geofence_devices ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.geofence_events  ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.geofence_devices
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.geofence_events
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.geofence_devices TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.geofence_events  TO app_rt;
