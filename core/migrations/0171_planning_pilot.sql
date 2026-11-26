-- Fase 6 «Pilota» (piano Idealverde): la pianificazione automatica si prova su alcune squadre per
-- qualche settimana, confrontando con le settimane prima, poi si decide se estenderla a tutte.
-- Misure del piano: piani proposti accettati senza modifiche, ore di spostamento, extra gestiti entro
-- lo SLA, ore di pianificazione risparmiate a settimana (dichiarate dal pianificatore).
CREATE TABLE IF NOT EXISTS app.planning_pilots (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id         uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name              text NOT NULL CHECK (length(btrim(name)) BETWEEN 2 AND 120),
    group_ids         uuid[] NOT NULL CHECK (cardinality(group_ids) BETWEEN 1 AND 50),
    starts_on         date NOT NULL,
    weeks             int  NOT NULL DEFAULT 4 CHECK (weeks BETWEEN 1 AND 26),
    baseline_weeks    int  NOT NULL DEFAULT 4 CHECK (baseline_weeks BETWEEN 1 AND 26),
    default_sla_hours int  NOT NULL DEFAULT 48 CHECK (default_sla_hours BETWEEN 1 AND 720),
    baseline_planning_hours numeric(5,1) CHECK (baseline_planning_hours IS NULL OR baseline_planning_hours BETWEEN 0 AND 168),
    targets           jsonb NOT NULL DEFAULT '{"accepted_pct":70,"travel_reduction_pct":10,"sla_pct":90,"hours_saved":4}',
    status            text NOT NULL DEFAULT 'attivo' CHECK (status IN ('attivo','esteso','concluso')),
    decision_note     text,
    created_by        uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    closed_at         timestamptz,
    closed_by         uuid REFERENCES app.users(id) ON DELETE SET NULL
);
-- un solo pilota attivo per azienda
CREATE UNIQUE INDEX IF NOT EXISTS planning_pilots_one_active ON app.planning_pilots(tenant_id) WHERE status = 'attivo';

CREATE TABLE IF NOT EXISTS app.planning_pilot_weeks (
    pilot_id       uuid NOT NULL REFERENCES app.planning_pilots(id) ON DELETE CASCADE,
    tenant_id      uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    week_start     date NOT NULL CHECK (extract(isodow FROM week_start) = 1),
    planning_hours numeric(5,1) NOT NULL CHECK (planning_hours BETWEEN 0 AND 168),
    note           text,
    updated_by     uuid REFERENCES app.users(id) ON DELETE SET NULL,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (pilot_id, week_start)
);

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['planning_pilots','planning_pilot_weeks'] LOOP
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE app.%I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON app.%I', t);
    EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
  END LOOP;
END $$;

-- controllo «stesso tenant» rigenerato sui riferimenti attuali (0144 … 0171)
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
