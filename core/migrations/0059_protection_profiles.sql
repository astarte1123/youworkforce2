-- Fase C — "Profilo di protezione": il legante dei 5 stadi del perimetro Worker Safety.
-- Un profilo lega 1..n lavoratori a: una policy di escalation (alarm_group), l'insieme dei
-- trigger abilitati, e i parametri di check-in. I device coperti derivano da devices.assigned_user
-- dei lavoratori membri. E' il punto UNICO di configurazione del perimetro per un gruppo di lavoratori.
CREATE TABLE IF NOT EXISTS app.protection_profiles (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name                text NOT NULL,
    descr               text NOT NULL DEFAULT '',
    -- policy escalation: gruppo che gestisce gli incidenti dei lavoratori del profilo.
    -- Se NULL -> fallback al routing globale per-etype (app.alarm_routing).
    escalation_group_id uuid REFERENCES app.alarm_groups(id) ON DELETE SET NULL,
    -- trigger abilitati per i lavoratori del profilo (informativo + gating futuro).
    triggers            jsonb NOT NULL DEFAULT '{"sos":true,"man_down":true,"immobilita":true,"mancato_checkin":true,"geofence":false}'::jsonb,
    -- check-in: minuti attesi tra un check-in e l'altro (NULL = usa default tenant).
    checkin_minutes     int,
    enabled             boolean NOT NULL DEFAULT true,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- Un lavoratore appartiene ad al massimo UN profilo (UNIQUE tenant_id,user_id) -> risoluzione
-- deterministica del profilo a partire dall'incidente/worker.
CREATE TABLE IF NOT EXISTS app.protection_profile_members (
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    profile_id  uuid NOT NULL REFERENCES app.protection_profiles(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    added_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (profile_id, user_id),
    UNIQUE (tenant_id, user_id)
);
CREATE INDEX IF NOT EXISTS protection_profile_members_user_idx ON app.protection_profile_members(tenant_id, user_id);

ALTER TABLE app.protection_profiles        ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.protection_profile_members ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.protection_profiles
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.protection_profile_members
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.protection_profiles        TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.protection_profile_members TO app_rt;
