-- ============================================================================
-- #5 Report: log storico dei cambi di presenza (ingressi/uscite) per i report.
-- device_presence tiene lo stato CORRENTE; qui lo storico append-only.
-- ============================================================================
CREATE TABLE presence_log (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  device_id   uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  geofence_id uuid NOT NULL REFERENCES geofences(id) ON DELETE CASCADE,
  inside      boolean NOT NULL,
  at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX presence_log_win ON presence_log(tenant_id, at DESC);

ALTER TABLE presence_log ENABLE ROW LEVEL SECURITY;
DO $$
BEGIN
  EXECUTE 'CREATE POLICY tenant_isolation ON app.presence_log
     USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid)
     WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXECUTE 'GRANT SELECT, INSERT, DELETE ON app.presence_log TO app_rt';
END$$;
