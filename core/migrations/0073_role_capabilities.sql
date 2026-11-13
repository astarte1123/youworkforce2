-- Fase 2 — Capability layer. Le autorizzazioni diventano "capability" (admin/plan/approve...)
-- mappate di DEFAULT sui 7 ruoli fissi (comportamento IDENTICO a oggi). Questa tabella tiene
-- SOLO gli override per-tenant: granted=true aggiunge la capability a un ruolo, granted=false
-- la toglie. Nessuna riga = si usa la mappa di default in codice. Additiva e non distruttiva.
CREATE TABLE IF NOT EXISTS app.role_capabilities (
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    role       text NOT NULL,
    capability text NOT NULL,
    granted    boolean NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, role, capability)
);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.role_capabilities TO app_rt;
ALTER TABLE app.role_capabilities ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.role_capabilities
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
