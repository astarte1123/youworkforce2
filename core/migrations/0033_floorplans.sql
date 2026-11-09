-- Piantine INDOOR: l'admin carica una o piu' mappe interne (immagine; i PDF sono
-- rasterizzati a immagine lato client). Su una piantina si posizionano i beacon
-- (zone) e si disegnano geofence in coordinate immagine (pixel), non lat/lon.
CREATE TABLE IF NOT EXISTS app.floorplans (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id    uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
  name         text NOT NULL,
  site_id      uuid REFERENCES app.sites(id) ON DELETE SET NULL,
  content_type text NOT NULL DEFAULT 'image/png',
  width        int  NOT NULL,            -- pixel immagine (sistema di coordinate CRS.Simple)
  height       int  NOT NULL,
  image        bytea NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now()
);

-- un beacon puo' essere posizionato su una piantina (x,y pixel) oltre che geo (lat/lon)
ALTER TABLE app.beacons ADD COLUMN IF NOT EXISTS floorplan_id uuid REFERENCES app.floorplans(id) ON DELETE SET NULL;
ALTER TABLE app.beacons ADD COLUMN IF NOT EXISTS x double precision;
ALTER TABLE app.beacons ADD COLUMN IF NOT EXISTS y double precision;

-- una geofence puo' appartenere a una piantina (geom in coordinate immagine)
ALTER TABLE app.geofences ADD COLUMN IF NOT EXISTS floorplan_id uuid REFERENCES app.floorplans(id) ON DELETE CASCADE;

ALTER TABLE app.floorplans ENABLE ROW LEVEL SECURITY;
DO $$
BEGIN
  EXECUTE format($f$
    CREATE POLICY tenant_isolation ON app.floorplans
      USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
      WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
  $f$);
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.floorplans TO app_rt';
END$$;
