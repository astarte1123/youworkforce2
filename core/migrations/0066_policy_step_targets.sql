-- Parità Escalation Policies GoAlert — target PER-STEP: uno step può notificare un destinatario
-- diverso (on-call del gruppo, un ALTRO gruppo, una persona specifica, un canale Slack), non solo
-- un canale verso l'on-call dello stesso gruppo. Abilita l'escalation gerarchica (es. step1=reperibile,
-- step2=responsabili, step3=manager). Retro-compatibile: default target_kind='oncall'.
ALTER TABLE app.esc_policy_steps
  ADD COLUMN IF NOT EXISTS target_kind     text NOT NULL DEFAULT 'oncall',   -- oncall|group|user|slack
  ADD COLUMN IF NOT EXISTS target_group_id uuid REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
  ADD COLUMN IF NOT EXISTS target_user_id  uuid REFERENCES app.users(id)        ON DELETE CASCADE,
  ADD COLUMN IF NOT EXISTS target_slack    text;

DO $$ BEGIN
  ALTER TABLE app.esc_policy_steps ADD CONSTRAINT esc_policy_steps_target_kind_chk
    CHECK (target_kind IN ('oncall','group','user','slack'));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
