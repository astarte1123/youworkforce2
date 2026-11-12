-- Parità Schedules — residui minori.
-- 1) Assegnazioni di tipo ROTAZIONE (non solo "tutti"): un'assegnazione può ruotare i suoi membri.
ALTER TABLE app.esc_assignments
  ADD COLUMN IF NOT EXISTS mode         text NOT NULL DEFAULT 'tutti',  -- tutti|turni
  ADD COLUMN IF NOT EXISTS oncall_type  text NOT NULL DEFAULT 'weekly', -- hourly|daily|weekly|monthly
  ADD COLUMN IF NOT EXISTS oncall_shift int  NOT NULL DEFAULT 1,
  ADD COLUMN IF NOT EXISTS handoff_time text,
  ADD COLUMN IF NOT EXISTS handoff_day  int;
DO $$ BEGIN
  ALTER TABLE app.esc_assignments ADD CONSTRAINT esc_assignments_mode_chk CHECK (mode IN ('tutti','turni'));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

-- 2) Temporary schedule: consenti esplicitamente i buchi di copertura (altrimenti rifiuta).
ALTER TABLE app.esc_temp_schedules
  ADD COLUMN IF NOT EXISTS allow_gaps boolean NOT NULL DEFAULT false;

-- 3) Abbonamento iCal PER-GRUPPO (oltre al feed per-utente): token pubblico -> turni calcolati del gruppo.
CREATE TABLE IF NOT EXISTS app.esc_group_ical_tokens (
    token      text PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id   uuid NOT NULL REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS esc_group_ical_group_idx ON app.esc_group_ical_tokens(tenant_id, group_id);
-- letto in chiaro dal feed pubblico via owner_pool (come oncall_ical_tokens); grant per completezza.
GRANT SELECT, INSERT, DELETE ON app.esc_group_ical_tokens TO app_rt;
