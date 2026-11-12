-- F3 residui — completamento parità GoAlert.

-- 1) TEMPORARY SCHEDULE: sostituisce per un periodo tutte le assegnazioni di un gruppo con una
--    sequenza ordinata di blocchi (utente + durata ore). Mentre è attivo, solo lui riceve gli allarmi.
CREATE TABLE IF NOT EXISTS app.esc_temp_schedules (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id    uuid NOT NULL REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
    start_at    timestamptz NOT NULL,
    end_at      timestamptz NOT NULL,
    blocks      jsonb NOT NULL DEFAULT '[]'::jsonb,  -- [{user_id, hours}] in ordine
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (end_at > start_at)
);
CREATE INDEX IF NOT EXISTS esc_temp_sched_group_idx ON app.esc_temp_schedules(tenant_id, group_id, start_at, end_at);

-- 2) MULTI-RECAPITO per utente: più contact method (sms/voice/email) con stato di verifica.
CREATE TABLE IF NOT EXISTS app.user_contact_methods (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    kind        text NOT NULL,                       -- sms|voice|email
    value       text NOT NULL,                       -- numero E.164 o email
    label       text NOT NULL DEFAULT '',
    verified_at timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (kind IN ('sms','voice','email'))
);
CREATE INDEX IF NOT EXISTS user_cm_user_idx ON app.user_contact_methods(tenant_id, user_id);

-- 3) REGOLE DI NOTIFICA per utente: ladder personale (contact method + ritardo). Secondo livello
--    rispetto agli step di policy del gruppo: quando un utente è on-call e lo step lo notifica,
--    le sue regole decidono QUALE recapito contattare e DOPO QUANTI minuti.
CREATE TABLE IF NOT EXISTS app.user_notif_rules (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    cm_id       uuid NOT NULL REFERENCES app.user_contact_methods(id) ON DELETE CASCADE,
    delay_min   int  NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (delay_min >= 0 AND delay_min <= 1440)
);
CREATE INDEX IF NOT EXISTS user_notif_rules_user_idx ON app.user_notif_rules(tenant_id, user_id);

-- 4) LABEL/metadata sui gruppi (parità GoAlert labels): coppie chiave=valore per filtrare/cercare.
ALTER TABLE app.alarm_groups
  ADD COLUMN IF NOT EXISTS labels jsonb NOT NULL DEFAULT '{}'::jsonb;

-- 5) BUNDLING maintenance + annunci Slack schedulati: tracciano l'ultima emissione per non duplicare.
ALTER TABLE app.alarm_groups
  ADD COLUMN IF NOT EXISTS slack_last_announce timestamptz;
-- incidenti arrivati durante la maintenance (per il bundle a fine finestra).
ALTER TABLE app.incidents
  ADD COLUMN IF NOT EXISTS bundled_at timestamptz;

ALTER TABLE app.esc_temp_schedules    ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.user_contact_methods  ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.user_notif_rules      ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.esc_temp_schedules
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.user_contact_methods
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.user_notif_rules
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.esc_temp_schedules   TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.user_contact_methods TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.user_notif_rules     TO app_rt;
