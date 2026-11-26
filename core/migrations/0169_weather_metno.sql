-- Fase 4b «Meteo operativo»: fonte delle previsioni per tenant. Decisione PO 26/11: oltre al sistema
-- collegato con la chiave di integrazione (Youco), YW può prendere da solo le previsioni da MET Norway
-- (api.met.no, gratuito anche per uso commerciale, licenza CC BY 4.0). È l'unica eccezione alla regola
-- «nessuna connessione verso l'esterno» oltre all'AI, limitata al meteo e scelta per tenant.
CREATE TABLE IF NOT EXISTS app.weather_settings (
    tenant_id  uuid PRIMARY KEY REFERENCES app.tenants(id) ON DELETE CASCADE,
    provider   text NOT NULL DEFAULT 'integrazione' CHECK (provider IN ('integrazione','metno')),
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by uuid REFERENCES app.users(id) ON DELETE SET NULL
);
ALTER TABLE app.weather_settings ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.weather_settings FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON app.weather_settings;
CREATE POLICY tenant_isolation ON app.weather_settings USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.weather_settings TO app_rt;

-- stato dello scaricamento per zona (cache: il prossimo solo dopo expires_at, con If-Modified-Since)
ALTER TABLE app.weather_zones
    ADD COLUMN IF NOT EXISTS fetched_at    timestamptz,
    ADD COLUMN IF NOT EXISTS expires_at    timestamptz,
    ADD COLUMN IF NOT EXISTS last_modified text,
    ADD COLUMN IF NOT EXISTS fetch_error   text;
CREATE INDEX IF NOT EXISTS weather_zones_due_idx ON app.weather_zones(expires_at);

-- controllo «stesso tenant» rigenerato sui riferimenti attuali (0144 … 0169)
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
