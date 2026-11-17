-- 0107 — IoT nativo F2: attributi device (client/shared/server) + coda comandi/RPC. Additiva.
-- Sostituisce attributes/shared-attributes/rpc di ThingsBoard, nativamente.
-- NB: 'device_commands' è già usata (comandi Traccar), quindi la coda IoT è 'iot_commands'.
CREATE TABLE IF NOT EXISTS app.device_attributes (
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    device_id  uuid NOT NULL REFERENCES app.devices(id) ON DELETE CASCADE,
    scope      text NOT NULL,            -- client | shared | server
    key        text NOT NULL,
    value      jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, scope, key)
);
CREATE INDEX IF NOT EXISTS device_attributes_idx ON app.device_attributes(tenant_id, device_id, scope);

CREATE TABLE IF NOT EXISTS app.iot_commands (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    device_id  uuid NOT NULL REFERENCES app.devices(id) ON DELETE CASCADE,
    method     text NOT NULL,
    params     jsonb,
    status     text NOT NULL DEFAULT 'pending',  -- pending | sent | done | error
    result     text,
    created_by uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    sent_at    timestamptz
);
CREATE INDEX IF NOT EXISTS iot_commands_idx ON app.iot_commands(tenant_id, device_id, status, created_at);

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['device_attributes','iot_commands'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
