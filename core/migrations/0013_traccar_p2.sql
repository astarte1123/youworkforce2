-- ============================================================================
-- Traccar P2 — posizioni & presenza:
--   * devices: ultima posizione nota (lat/lon/speed/course/battery/pos_at)
--   * device_positions: storico posizioni (trail/mappa/audit), idempotente per source id
--   * geofence enter/exit gestiti nel core come eventi di presenza (ingresso/uscita sito)
-- ============================================================================
ALTER TABLE devices ADD COLUMN IF NOT EXISTS last_lat     double precision;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS last_lon     double precision;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS last_speed   double precision;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS last_course  double precision;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS last_battery double precision;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS last_pos_at  timestamptz;

CREATE TABLE device_positions (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  device_id   uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  source_id   text,                         -- id posizione da Traccar (idempotenza)
  lat         double precision NOT NULL,
  lon         double precision NOT NULL,
  speed       double precision,
  course      double precision,
  battery     double precision,
  attributes  jsonb,
  fixed_at    timestamptz NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, device_id, source_id)
);
CREATE INDEX device_positions_dev ON device_positions(tenant_id, device_id, fixed_at DESC);

ALTER TABLE device_positions ENABLE ROW LEVEL SECURITY;
DO $$
BEGIN
  EXECUTE 'CREATE POLICY tenant_isolation ON app.device_positions
     USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid)
     WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.device_positions TO app_rt';
END$$;
