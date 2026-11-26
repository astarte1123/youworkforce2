-- Fase 1 «Lavoro sul campo a scala reale» (primo caso reale: Idealverde, 60+ giardinieri in squadre).
-- Le fondamenta che servono a pianificare con i vincoli: tipi di attività, finestre orarie delle
-- sedi, mezzi con stato, scadenze e prenotazioni, qualifiche richieste da attività e mezzi.
-- Il controllo dei vincoli è nel core (core/src/constraints.rs); qui solo i dati.

-- Prenotazioni senza sovrapposizioni sullo stesso mezzo: vincolo di esclusione (serve btree_gist
-- per l'uguaglianza sull'uuid insieme alla sovrapposizione degli intervalli).
CREATE EXTENSION IF NOT EXISTS btree_gist;

-- 1) Tipi di attività: cosa si fa, quanto dura, quante persone, cosa serve.
CREATE TABLE IF NOT EXISTS app.activity_types (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id        uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name             text NOT NULL CHECK (length(btrim(name)) BETWEEN 2 AND 120),
    -- unità della quantità dell'intervento: a intervento, m², metri lineari, numero (piante), ore
    unit             text NOT NULL DEFAULT 'intervento' CHECK (unit IN ('intervento','m2','ml','n','ore')),
    base_minutes     int  NOT NULL DEFAULT 60 CHECK (base_minutes BETWEEN 0 AND 1440),
    minutes_per_unit numeric(8,2) NOT NULL DEFAULT 0 CHECK (minutes_per_unit >= 0 AND minutes_per_unit <= 600),
    crew_size        int  NOT NULL DEFAULT 1 CHECK (crew_size BETWEEN 1 AND 50),
    active           boolean NOT NULL DEFAULT true,
    note             text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
-- qualifiche richieste (almeno una persona della squadra le deve avere valide)
CREATE TABLE IF NOT EXISTS app.activity_type_qualifications (
    tenant_id        uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    activity_type_id uuid NOT NULL REFERENCES app.activity_types(id) ON DELETE CASCADE,
    qualification_id uuid NOT NULL REFERENCES app.qualifications(id) ON DELETE CASCADE,
    PRIMARY KEY (activity_type_id, qualification_id)
);
-- mezzi richiesti per tipo (es. «piattaforma» × 1): si prenotano sull'intervento
CREATE TABLE IF NOT EXISTS app.activity_type_assets (
    tenant_id        uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    activity_type_id uuid NOT NULL REFERENCES app.activity_types(id) ON DELETE CASCADE,
    asset_kind       text NOT NULL CHECK (length(btrim(asset_kind)) BETWEEN 1 AND 60),
    qty              int  NOT NULL DEFAULT 1 CHECK (qty BETWEEN 1 AND 20),
    PRIMARY KEY (activity_type_id, asset_kind)
);

ALTER TABLE app.work_orders
    ADD COLUMN IF NOT EXISTS activity_type_id uuid REFERENCES app.activity_types(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS quantity numeric(10,2) CHECK (quantity IS NULL OR quantity >= 0),
    ADD COLUMN IF NOT EXISTS estimated_minutes int CHECK (estimated_minutes IS NULL OR estimated_minutes BETWEEN 1 AND 10080);
ALTER TABLE app.maintenance_plans
    ADD COLUMN IF NOT EXISTS activity_type_id uuid REFERENCES app.activity_types(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS quantity numeric(10,2) CHECK (quantity IS NULL OR quantity >= 0);
CREATE INDEX IF NOT EXISTS work_orders_activity_idx ON app.work_orders(tenant_id, activity_type_id);

-- 2) Finestre orarie della sede (quando il cliente ci fa lavorare), ora locale della sede:
--    [{"days":"1111100","from":"08:00","to":"17:00"}, …]; vuoto = sempre.
ALTER TABLE app.sites ADD COLUMN IF NOT EXISTS access_windows jsonb NOT NULL DEFAULT '[]'::jsonb;

-- 3) Mezzi e attrezzature: stato, qualifica richiesta per l'uso, scadenze, prenotazioni.
ALTER TABLE app.assets
    ADD COLUMN IF NOT EXISTS status text NOT NULL DEFAULT 'disponibile' CHECK (status IN ('disponibile','fermo','dismesso')),
    ADD COLUMN IF NOT EXISTS required_qualification_id uuid REFERENCES app.qualifications(id) ON DELETE SET NULL;

CREATE TABLE IF NOT EXISTS app.asset_deadlines (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    asset_id   uuid NOT NULL REFERENCES app.assets(id) ON DELETE CASCADE,
    kind       text NOT NULL CHECK (kind IN ('revisione','tagliando','assicurazione','verifica','altro')),
    due_date   date NOT NULL,
    note       text,
    done_at    timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS asset_deadlines_due_idx ON app.asset_deadlines(tenant_id, due_date) WHERE done_at IS NULL;

CREATE TABLE IF NOT EXISTS app.asset_bookings (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    asset_id      uuid NOT NULL REFERENCES app.assets(id) ON DELETE CASCADE,
    work_order_id uuid REFERENCES app.work_orders(id) ON DELETE CASCADE,
    group_id      uuid REFERENCES app.user_groups(id) ON DELETE SET NULL,
    user_id       uuid REFERENCES app.users(id) ON DELETE SET NULL,
    starts_at     timestamptz NOT NULL,
    ends_at       timestamptz NOT NULL,
    note          text,
    created_by    uuid,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CHECK (ends_at > starts_at),
    -- lo stesso mezzo non si prenota due volte nello stesso momento
    CONSTRAINT asset_bookings_no_overlap EXCLUDE USING gist (asset_id WITH =, tstzrange(starts_at, ends_at) WITH &&)
);
CREATE INDEX IF NOT EXISTS asset_bookings_order_idx ON app.asset_bookings(work_order_id);
CREATE INDEX IF NOT EXISTS asset_bookings_time_idx ON app.asset_bookings(tenant_id, starts_at);

-- RLS e permessi come le altre tabelle per tenant
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['activity_types','activity_type_qualifications','activity_type_assets','asset_deadlines','asset_bookings'] LOOP
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE app.%I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON app.%I', t);
    EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
  END LOOP;
END $$;

-- nuovo modulo «Mezzi e attrezzature» (disponibile, spento finché il superadmin non lo attiva)
INSERT INTO app.modules_catalog(code, name, kind, available)
VALUES ('mezzi', 'Mezzi e attrezzature', 'product', true)
ON CONFLICT (code) DO NOTHING;

-- controllo «stesso tenant» (0144/0147/0155/0160) rigenerato sui riferimenti attuali
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
