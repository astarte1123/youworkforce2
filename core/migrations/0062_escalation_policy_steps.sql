-- F3 — Policy di escalation multi-step configurabile per gruppo (parità GoAlert: step ordinati,
-- wait per-step, canale per-step, reorder, repeat-up-to). Prima la ladder era FISSA (SMS→VOICE
-- derivata dalle impostazioni vocali). Ora un gruppo può definire una sequenza arbitraria di step;
-- se non ne definisce nessuno, resta il comportamento derivato (retro-compatibile).
CREATE TABLE IF NOT EXISTS app.esc_policy_steps (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id    uuid NOT NULL REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
    idx         int  NOT NULL,                       -- ordine dello step (0-based)
    delay_s     int  NOT NULL DEFAULT 0,             -- attesa PRIMA di passare allo step successivo
    channel     text NOT NULL DEFAULT 'sms',         -- sms|voice|email|slack
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (group_id, idx),
    CHECK (channel IN ('sms','voice','email','slack')),
    CHECK (delay_s >= 0 AND delay_s <= 86400)
);
CREATE INDEX IF NOT EXISTS esc_policy_steps_group_idx ON app.esc_policy_steps(tenant_id, group_id, idx);

-- repeat-up-to a livello di gruppo (override del tenant_oncall_cfg.esc_repeat). NULL = usa il tenant.
ALTER TABLE app.alarm_groups
  ADD COLUMN IF NOT EXISTS esc_repeat int;

ALTER TABLE app.esc_policy_steps ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.esc_policy_steps
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.esc_policy_steps TO app_rt;
