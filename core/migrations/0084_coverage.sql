-- 0084 — Copertura minima (W3c). Additiva.
-- Un requisito di copertura dice: su questa sede (o ovunque), in questi giorni, in questa fascia
-- oraria, servono almeno N persone (eventualmente con una qualifica). Il controllo copertura segnala
-- i giorni/fasce scoperti del piano turni: il prodotto trova i buchi da solo.
CREATE TABLE IF NOT EXISTS app.coverage_requirements (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    site_id     uuid REFERENCES app.sites(id) ON DELETE CASCADE,   -- NULL = qualsiasi sede
    days_mask   text NOT NULL DEFAULT '1111111',  -- lun..dom
    cov_start   text NOT NULL,                     -- 'HH:MM'
    cov_end     text NOT NULL,                     -- 'HH:MM' (> cov_start, stessa giornata)
    min_people  int  NOT NULL DEFAULT 1,
    required_qualification uuid REFERENCES app.qualifications(id) ON DELETE SET NULL,
    note        text,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS coverage_req_tenant ON app.coverage_requirements(tenant_id);
ALTER TABLE app.coverage_requirements ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON app.coverage_requirements;
CREATE POLICY tenant_isolation ON app.coverage_requirements
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.coverage_requirements TO app_rt;
