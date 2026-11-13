-- Gruppi di persone CONDIVISI — primitiva comune, riusabile da qualunque modulo.
-- Additiva e non distruttiva: i gruppi esistenti (alarm_groups = escalation, teams = squadre,
-- protection_profiles = safety) restano intatti. Questa è SOLO "un insieme di persone" (nessun
-- comportamento), riferibile dai moduli che ne hanno bisogno. Owner canonico = core.

CREATE TABLE IF NOT EXISTS app.user_groups (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name       text NOT NULL,
    kind       text NOT NULL DEFAULT 'squadra',   -- libero: squadra|reparto|zona|turno|...
    labels     jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE TABLE IF NOT EXISTS app.user_group_members (
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id   uuid NOT NULL REFERENCES app.user_groups(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    added_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX IF NOT EXISTS user_group_members_g_idx ON app.user_group_members(tenant_id, group_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON app.user_groups        TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.user_group_members TO app_rt;

ALTER TABLE app.user_groups        ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.user_group_members ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.user_groups
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.user_group_members
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
