-- Dashboard unica configurabile — layout per-tenant dei widget (cross-cutting, non di un modulo).
-- Additiva e non distruttiva: assenza di righe = default (tutti i widget dei moduli attivi, ordine
-- naturale del catalogo) = comportamento storico. Il layout è deciso dal tenant_admin; la scelta di
-- quali MODULI sono attivi resta del superadmin (tenant_modules).

CREATE TABLE IF NOT EXISTS app.dashboard_layouts (
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    widget_key text NOT NULL,                 -- chiave del catalogo widget (codice)
    enabled    boolean NOT NULL DEFAULT true,
    position   integer NOT NULL DEFAULT 0,
    size       text NOT NULL DEFAULT 'half',  -- half | full
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, widget_key)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON app.dashboard_layouts TO app_rt;

ALTER TABLE app.dashboard_layouts ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.dashboard_layouts
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
