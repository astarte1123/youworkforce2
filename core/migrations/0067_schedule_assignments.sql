-- Parità Schedules GoAlert — ASSEGNAZIONI MULTIPLE con regole per-assegnazione.
-- Oltre alla rotazione/copertura base del gruppo, si possono aggiungere N assegnazioni: ciascuna è
-- un insieme di persone attivo in una propria FINESTRA (giorni + orario, ora locale del gruppo).
-- L'on-call del gruppo a un istante = on-call base UNIONE le assegnazioni la cui finestra copre t.
-- Esempio: rotazione base (feriali) + assegnazione "Weekend" (sab/dom, sempre) + persona fissa di notte.
CREATE TABLE IF NOT EXISTS app.esc_assignments (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id    uuid NOT NULL REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
    label       text NOT NULL DEFAULT '',
    cov_days    text NOT NULL DEFAULT '1111111',   -- lun..dom; '1'=attivo
    cov_start   text,                               -- HH:MM ora locale (NULL = 24h)
    cov_end     text,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS esc_assignments_group_idx ON app.esc_assignments(tenant_id, group_id);

CREATE TABLE IF NOT EXISTS app.esc_assignment_members (
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    assignment_id uuid NOT NULL REFERENCES app.esc_assignments(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    position      int  NOT NULL DEFAULT 1,
    PRIMARY KEY (assignment_id, user_id)
);

ALTER TABLE app.esc_assignments        ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.esc_assignment_members ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.esc_assignments
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.esc_assignment_members
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.esc_assignments        TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.esc_assignment_members TO app_rt;
