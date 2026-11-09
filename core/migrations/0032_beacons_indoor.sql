-- Allarmi INDOOR: nelle fabbriche il GPS spesso non prende. I dispositivi riportano
-- la zona tramite BEACON BLE (iBeacon major/minor o id). Mappiamo ogni beacon a una
-- zona nominata (reparto/piano) con un'ancora opzionale sulla mappa, cosi' un allarme
-- indoor si localizza per zona anche senza coordinate GPS valide.
CREATE TABLE IF NOT EXISTS app.beacons (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
  beacon_uid  text NOT NULL,                 -- id riportato dal device (es. "major-minor" o UUID)
  zone_name   text NOT NULL,                 -- zona leggibile: "Reparto verniciatura - Piano 1"
  site_id     uuid REFERENCES app.sites(id) ON DELETE SET NULL,
  lat         double precision,              -- ancora opzionale per il pin sulla mappa
  lon         double precision,
  floor       text,
  created_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, beacon_uid)
);

-- gli eventi/allarmi registrano se sono indoor e la zona risolta (snapshot al momento)
ALTER TABLE app.events ADD COLUMN IF NOT EXISTS indoor boolean NOT NULL DEFAULT false;
ALTER TABLE app.events ADD COLUMN IF NOT EXISTS beacon_uid text;
ALTER TABLE app.events ADD COLUMN IF NOT EXISTS zone text;

ALTER TABLE app.beacons ENABLE ROW LEVEL SECURITY;
DO $$
BEGIN
  EXECUTE format($f$
    CREATE POLICY tenant_isolation ON app.beacons
      USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
      WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
  $f$);
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.beacons TO app_rt';
END$$;
