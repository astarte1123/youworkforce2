-- Fase 5 «Pianificazione automatica» (piano Idealverde, modulo `pianificazione`, requires interventi).
-- Decisioni PO: YW espone il problema con un contratto dati, valida OGNI proposta con gli stessi vincoli
-- della pianificazione a mano e ha un motore di base nativo (il cliente ha sempre un piano valido); un
-- motore esterno (Youco) può proporre sullo stesso contratto. Tempi di spostamento = distanza in linea
-- d'aria × fattore di percorso / velocità media (nessun servizio esterno). Il motore propone, la persona
-- decide: la proposta è una BOZZA da rivedere e applicare.

CREATE TABLE IF NOT EXISTS app.planning_settings (
    tenant_id      uuid PRIMARY KEY REFERENCES app.tenants(id) ON DELETE CASCADE,
    depot_site_id  uuid REFERENCES app.sites(id) ON DELETE SET NULL,
    day_start      time NOT NULL DEFAULT '07:30',
    day_end        time NOT NULL DEFAULT '16:30',
    workdays       text NOT NULL DEFAULT '1111100' CHECK (workdays ~ '^[01]{7}$'),
    travel_kmh     int  NOT NULL DEFAULT 25 CHECK (travel_kmh BETWEEN 5 AND 120),
    detour_factor  numeric(3,2) NOT NULL DEFAULT 1.30 CHECK (detour_factor BETWEEN 1 AND 3),
    setup_minutes  int  NOT NULL DEFAULT 5 CHECK (setup_minutes BETWEEN 0 AND 60),
    max_overtime_minutes int NOT NULL DEFAULT 0 CHECK (max_overtime_minutes BETWEEN 0 AND 240),
    weather_policy text NOT NULL DEFAULT 'evita' CHECK (weather_policy IN ('evita','ignora')),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CHECK (day_end > day_start)
);
-- punto di partenza della squadra (deposito); vuoto = quello del tenant
ALTER TABLE app.user_groups ADD COLUMN IF NOT EXISTS depot_site_id uuid REFERENCES app.sites(id) ON DELETE SET NULL;
-- intervento fissato dal pianificatore: il motore non lo sposta
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS plan_locked boolean NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS app.planning_runs (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    source      text NOT NULL DEFAULT 'nativo' CHECK (source IN ('nativo','esterno')),
    engine      text NOT NULL,
    status      text NOT NULL DEFAULT 'bozza' CHECK (status IN ('bozza','applicato','scartato')),
    period_from timestamptz NOT NULL,
    period_to   timestamptz NOT NULL,
    params      jsonb NOT NULL DEFAULT '{}',
    summary     jsonb NOT NULL DEFAULT '{}',
    created_by  uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    applied_by  uuid REFERENCES app.users(id) ON DELETE SET NULL,
    applied_at  timestamptz,
    CHECK (period_to > period_from)
);
CREATE INDEX IF NOT EXISTS planning_runs_tenant_idx ON app.planning_runs(tenant_id, created_at DESC);

CREATE TABLE IF NOT EXISTS app.planning_run_items (
    run_id        uuid NOT NULL REFERENCES app.planning_runs(id) ON DELETE CASCADE,
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    work_order_id uuid NOT NULL REFERENCES app.work_orders(id) ON DELETE CASCADE,
    order_version int  NOT NULL,
    group_id      uuid REFERENCES app.user_groups(id) ON DELETE SET NULL,
    user_id       uuid REFERENCES app.users(id) ON DELETE SET NULL,
    starts_at     timestamptz,          -- NULL = non pianificato (motivo in reason)
    ends_at       timestamptz,
    asset_ids     uuid[] NOT NULL DEFAULT '{}',
    seq           int,
    travel_minutes int,
    travel_km     numeric(7,1),
    previous      jsonb NOT NULL DEFAULT '{}',  -- com'era prima (orario, squadra) per il confronto
    violations    jsonb NOT NULL DEFAULT '[]',
    reason        text,
    applied       boolean NOT NULL DEFAULT false,
    apply_error   text,
    PRIMARY KEY (run_id, work_order_id),
    CHECK ((starts_at IS NULL) = (ends_at IS NULL)),
    CHECK (ends_at IS NULL OR ends_at > starts_at)
);

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['planning_settings','planning_runs','planning_run_items'] LOOP
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE app.%I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON app.%I', t);
    EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
  END LOOP;
END $$;

INSERT INTO app.modules_catalog(code, name, kind, available)
VALUES ('pianificazione', 'Pianificazione automatica', 'product', true)
ON CONFLICT (code) DO NOTHING;

-- controllo «stesso tenant» rigenerato sui riferimenti attuali (0144 … 0170)
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
