-- 0083 — Qualifiche e scadenze (W3b). Additiva.
-- Attiva il modulo (era placeholder non disponibile) e crea catalogo qualifiche + assegnazioni.
-- Un turno può RICHIEDERE una qualifica: il motore regole la tratta come vincolo DURO
-- (solo chi la possiede, non scaduta, può coprire il turno).
UPDATE app.modules_catalog SET available=true WHERE code='qualifiche';

CREATE TABLE IF NOT EXISTS app.qualifications (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name       text NOT NULL,
    note       text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE TABLE IF NOT EXISTS app.user_qualifications (
    tenant_id        uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id          uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    qualification_id uuid NOT NULL REFERENCES app.qualifications(id) ON DELETE CASCADE,
    expires_at       timestamptz,  -- NULL = senza scadenza
    PRIMARY KEY (tenant_id, user_id, qualification_id)
);
ALTER TABLE app.shift_plans ADD COLUMN IF NOT EXISTS required_qualification uuid REFERENCES app.qualifications(id) ON DELETE SET NULL;

ALTER TABLE app.qualifications      ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.user_qualifications ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON app.qualifications;
DROP POLICY IF EXISTS tenant_isolation ON app.user_qualifications;
CREATE POLICY tenant_isolation ON app.qualifications
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
CREATE POLICY tenant_isolation ON app.user_qualifications
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.qualifications      TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.user_qualifications TO app_rt;
