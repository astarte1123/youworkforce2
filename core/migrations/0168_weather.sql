-- Fase 4 «Meteo operativo» (piano Idealverde, decisione 2 del PO: le previsioni le legge Youco e le
-- scrive in YW con una chiave di integrazione; YW non chiama servizi meteo esterni).
-- Zone meteo (con posizione: ogni sede usa la zona indicata o la più vicina), fasce di previsione o
-- di meteo osservato per zona, limiti meteo per tipo di attività. Il controllo è nel core
-- (core/src/weather.rs): avvisi sull'intervento, mai blocchi (una previsione è incerta).

CREATE TABLE IF NOT EXISTS app.weather_zones (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    code       text NOT NULL CHECK (length(btrim(code)) BETWEEN 1 AND 60),
    name       text NOT NULL CHECK (length(btrim(name)) BETWEEN 2 AND 120),
    lat        double precision NOT NULL CHECK (lat BETWEEN -90 AND 90),
    lon        double precision NOT NULL CHECK (lon BETWEEN -180 AND 180),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, code)
);
ALTER TABLE app.sites ADD COLUMN IF NOT EXISTS weather_zone_id uuid REFERENCES app.weather_zones(id) ON DELETE SET NULL;

CREATE TABLE IF NOT EXISTS app.weather_slots (
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    zone_id     uuid NOT NULL REFERENCES app.weather_zones(id) ON DELETE CASCADE,
    kind        text NOT NULL DEFAULT 'previsione' CHECK (kind IN ('previsione','osservato')),
    valid_from  timestamptz NOT NULL,
    valid_to    timestamptz NOT NULL,
    rain_mm     numeric(6,1) CHECK (rain_mm IS NULL OR rain_mm >= 0),
    rain_prob   int CHECK (rain_prob IS NULL OR rain_prob BETWEEN 0 AND 100),
    wind_kmh    numeric(5,1) CHECK (wind_kmh IS NULL OR wind_kmh >= 0),
    gust_kmh    numeric(5,1) CHECK (gust_kmh IS NULL OR gust_kmh >= 0),
    temp_c      numeric(4,1),
    condition   text,
    source      text,
    issued_at   timestamptz,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (valid_to > valid_from),
    PRIMARY KEY (zone_id, kind, valid_from)
);
CREATE INDEX IF NOT EXISTS weather_slots_time_idx ON app.weather_slots(tenant_id, valid_from);

-- limiti meteo del tipo di attività (vuoto = nessun limite)
ALTER TABLE app.activity_types
    ADD COLUMN IF NOT EXISTS rain_max_mm numeric(5,1) CHECK (rain_max_mm IS NULL OR rain_max_mm >= 0),
    ADD COLUMN IF NOT EXISTS wind_max_kmh int CHECK (wind_max_kmh IS NULL OR wind_max_kmh BETWEEN 0 AND 200),
    ADD COLUMN IF NOT EXISTS temp_min_c int CHECK (temp_min_c IS NULL OR temp_min_c BETWEEN -30 AND 50),
    ADD COLUMN IF NOT EXISTS temp_max_c int CHECK (temp_max_c IS NULL OR temp_max_c BETWEEN -30 AND 50),
    ADD COLUMN IF NOT EXISTS dry_hours_after int CHECK (dry_hours_after IS NULL OR dry_hours_after BETWEEN 0 AND 72);

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['weather_zones','weather_slots'] LOOP
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE app.%I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON app.%I', t);
    EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
  END LOOP;
END $$;

INSERT INTO app.modules_catalog(code, name, kind, available)
VALUES ('meteo', 'Meteo operativo', 'product', true)
ON CONFLICT (code) DO NOTHING;

-- controllo «stesso tenant» rigenerato sui riferimenti attuali (0144 … 0166)
DO $$
DECLARE r record;
BEGIN
  FOR r IN
    SELECT ch.relname AS child,
           string_agg(format('%L, %L, %L, %L', ca.attname, pa.relname, pc.attname, format_type(pc.atttypid, pc.atttypmod)), ', '
                      ORDER BY ca.attname) AS args,
           string_agg(DISTINCT quote_ident(ca.attname), ', ') AS cols
      FROM pg_constraint c
      JOIN pg_class ch ON ch.oid = c.conrelid
      JOIN pg_namespace n ON n.oid = ch.relnamespace AND n.nspname = 'app'
      JOIN pg_class pa ON pa.oid = c.confrelid
      JOIN pg_attribute ca ON ca.attrelid = ch.oid AND ca.attnum = c.conkey[1]
      JOIN pg_attribute pc ON pc.attrelid = pa.oid AND pc.attnum = c.confkey[1]
     WHERE c.contype = 'f' AND array_length(c.conkey, 1) = 1 AND pa.relname <> 'tenants'
       AND ca.attname <> 'tenant_id'
       AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = ch.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped)
       AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = pa.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped)
     GROUP BY ch.relname
  LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS yw_same_tenant ON app.%I', r.child);
    EXECUTE format('CREATE TRIGGER yw_same_tenant BEFORE INSERT OR UPDATE OF tenant_id, %s ON app.%I FOR EACH ROW EXECUTE FUNCTION app.yw_same_tenant(%s)',
                   r.cols, r.child, r.args);
  END LOOP;
END $$;
