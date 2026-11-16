-- 0082 — Assenze / indisponibilità del lavoratore (W3a). Additiva.
-- Un'assenza è un periodo in cui la persona NON è pianificabile: il motore regole lo tratta
-- come vincolo DURO (niente turni sopra un'assenza).
CREATE TABLE IF NOT EXISTS app.worker_absences (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES app.users(id)   ON DELETE CASCADE,
    starts_at  timestamptz NOT NULL,
    ends_at    timestamptz NOT NULL,
    kind       text NOT NULL DEFAULT 'assenza',  -- ferie | malattia | permesso | assenza
    note       text,
    created_by uuid REFERENCES app.users(id),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS worker_absences_win ON app.worker_absences(tenant_id, user_id, starts_at);
ALTER TABLE app.worker_absences ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON app.worker_absences;
CREATE POLICY tenant_isolation ON app.worker_absences
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.worker_absences TO app_rt;
