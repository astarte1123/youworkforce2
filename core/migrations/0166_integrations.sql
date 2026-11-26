-- Fase 2 «Integrazioni» (piano Idealverde): un gestionale (ERP), un software turni o un agente
-- (Youco via MCP) leggono e scrivono in YouWorkforce con una CHIAVE DI INTEGRAZIONE. YW non chiama
-- sistemi esterni: sono loro a chiamare YW.
-- - chiavi: agiscono per conto di chi le crea, con un sottoinsieme dei suoi permessi; si mostrano
--   una sola volta (in DB solo l'impronta SHA-256), si revocano, possono scadere;
-- - codice esterno (`external_ref`, il codice dell'ERP) su clienti, sedi, commesse, interventi,
--   persone, mezzi, tipi di attività e gruppi: inserimento o aggiornamento idempotente per codice.

CREATE TABLE IF NOT EXISTS app.api_keys (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    owner_user_id uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    name          text NOT NULL CHECK (length(btrim(name)) BETWEEN 2 AND 80),
    prefix        text NOT NULL UNIQUE,              -- parte pubblica della chiave (ricerca)
    key_hash      text NOT NULL,                     -- SHA-256 esadecimale della chiave intera
    scopes        text[] NOT NULL DEFAULT '{}',
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz,
    expires_at    timestamptz,
    revoked_at    timestamptz
);
CREATE INDEX IF NOT EXISTS api_keys_tenant_idx ON app.api_keys(tenant_id);

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['clients','sites','commesse','work_orders','users','assets','activity_types','user_groups'] LOOP
    EXECUTE format('ALTER TABLE app.%I ADD COLUMN IF NOT EXISTS external_ref text CHECK (external_ref IS NULL OR length(btrim(external_ref)) BETWEEN 1 AND 120)', t);
    EXECUTE format('CREATE UNIQUE INDEX IF NOT EXISTS %I ON app.%I(tenant_id, external_ref) WHERE external_ref IS NOT NULL', t || '_external_ref_uq', t);
  END LOOP;
END $$;

-- ultimo aggiornamento di clienti e commesse (per l'elenco delle modifiche verso l'ERP)
ALTER TABLE app.clients  ADD COLUMN IF NOT EXISTS updated_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS updated_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX IF NOT EXISTS work_orders_updated_idx ON app.work_orders(tenant_id, updated_at);
CREATE INDEX IF NOT EXISTS timesheets_updated_idx ON app.timesheets(tenant_id, updated_at);

ALTER TABLE app.api_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.api_keys FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON app.api_keys;
CREATE POLICY tenant_isolation ON app.api_keys USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
  WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.api_keys TO app_rt;

-- controllo «stesso tenant» rigenerato sui riferimenti attuali (0144 … 0165)
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
