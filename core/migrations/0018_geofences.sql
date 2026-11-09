-- ============================================================================
-- #3 Geofence (nostre): CRUD + calcolo presenza lato core sulle posizioni.
-- Non dipendiamo dall'engine geofence di Traccar: definiamo le aree qui e
-- calcoliamo dentro/fuori ad ogni posizione -> presenza + eventi ingresso/uscita.
--   geom (jsonb):
--     cerchio : {"lat":..,"lon":..,"radius":<metri>}
--     poligono: {"points":[[lat,lon],...]}
-- ============================================================================
CREATE TABLE geofences (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name       text NOT NULL,
  kind       text NOT NULL,            -- circle | polygon
  geom       jsonb NOT NULL,
  site_id    uuid REFERENCES sites(id),
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, name)
);

CREATE TABLE device_presence (
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  device_id   uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  geofence_id uuid NOT NULL REFERENCES geofences(id) ON DELETE CASCADE,
  inside      boolean NOT NULL,
  changed_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (device_id, geofence_id)
);

ALTER TABLE geofences       ENABLE ROW LEVEL SECURITY;
ALTER TABLE device_presence ENABLE ROW LEVEL SECURITY;
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['geofences','device_presence']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;
