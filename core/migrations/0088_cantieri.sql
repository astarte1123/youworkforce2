-- 0088 — Modulo Cantieri. Additivo, indipendente.
-- Il lavoro di cantiere è un PROGETTO (commessa) lungo su un sito, scomposto in LAVORAZIONI
-- (WBS) con avanzamento, e misurato nel tempo con gli stati avanzamento lavori (SAL).
-- Riusa il core condiviso (sedi, persone, turni di workforce) senza duplicarlo:
-- i turni possono essere legati a una commessa (shift_plans.commessa_id).
INSERT INTO app.modules_catalog(code,name,kind,available)
VALUES ('cantieri','Cantieri e commesse','product',true)
ON CONFLICT (code) DO NOTHING;

-- Commessa = progetto/cantiere (entità padre, lunga).
CREATE TABLE IF NOT EXISTS app.commesse (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    code        text NOT NULL,                       -- codice commessa (es. C-2026-014)
    name        text NOT NULL,
    site_id     uuid REFERENCES app.sites(id) ON DELETE SET NULL,
    client_name text,                                -- committente (anagrafica leggera per ora)
    start_date  date,
    end_date    date,
    status      text NOT NULL DEFAULT 'pianificato', -- pianificato | in_corso | sospeso | chiuso
    budget_amount numeric(14,2),                     -- importo contratto (opz.)
    note        text,
    created_by  uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, code)
);
-- Lavorazione = voce di WBS, con peso (per la media pesata) e avanzamento proprio.
CREATE TABLE IF NOT EXISTS app.commessa_lavorazioni (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id  uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    name         text NOT NULL,
    position     integer NOT NULL DEFAULT 1,
    weight       numeric(8,2) NOT NULL DEFAULT 1,     -- peso relativo nella commessa
    progress_pct integer NOT NULL DEFAULT 0,          -- 0..100
    status       text NOT NULL DEFAULT 'da_fare',     -- da_fare | in_corso | fatto
    note         text,
    CHECK (progress_pct >= 0 AND progress_pct <= 100)
);
-- SAL = Stato Avanzamento Lavori: fotografia datata dell'avanzamento (per fatturazione a milestone).
CREATE TABLE IF NOT EXISTS app.commessa_sal (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    sal_date    date NOT NULL DEFAULT current_date,
    pct         integer NOT NULL,                     -- avanzamento dichiarato alla data
    amount      numeric(14,2),                        -- importo maturato (opz.)
    note        text,
    created_by  uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (pct >= 0 AND pct <= 100)
);
-- Legame turno ↔ commessa (riuso del motore turni per i cantieri).
ALTER TABLE app.shift_plans ADD COLUMN IF NOT EXISTS commessa_id uuid REFERENCES app.commesse(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS commesse_tenant_idx ON app.commesse(tenant_id, status);
CREATE INDEX IF NOT EXISTS commessa_lav_idx ON app.commessa_lavorazioni(tenant_id, commessa_id, position);
CREATE INDEX IF NOT EXISTS commessa_sal_idx ON app.commessa_sal(tenant_id, commessa_id, sal_date);
CREATE INDEX IF NOT EXISTS shift_plans_commessa ON app.shift_plans(commessa_id) WHERE commessa_id IS NOT NULL;

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['commesse','commessa_lavorazioni','commessa_sal'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
