-- GoAlert parity: copertura oraria, handoff, Slack, ladder vocale/status updates,
-- override (assenze), calendario iCal. Tutto pilotato da YouWorkforce.

-- Config per-gruppo: copertura a fasce, handoff rotation, canale Slack.
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS coverage_mode      text NOT NULL DEFAULT '24_7'; -- '24_7' | 'orari'
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS cov_start          text;        -- 'HH:MM'
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS cov_end            text;        -- 'HH:MM'
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS cov_days           text NOT NULL DEFAULT '1111111'; -- lun..dom
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS handoff_time       text;        -- 'HH:MM' per rotation
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS handoff_day        int;         -- 0=lun..6=dom (weekly)
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS slack_channel_id   text;        -- target step/on-call
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS slack_notify       text NOT NULL DEFAULT 'off'; -- 'off'|'onchange'|'HH:MM'
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS goalert_schedule_id text;

-- Impostazioni reperibilità per-tenant (ladder vocale + status updates).
CREATE TABLE IF NOT EXISTS app.tenant_oncall_cfg (
    tenant_id          uuid PRIMARY KEY REFERENCES app.tenants(id) ON DELETE CASCADE,
    voice_fallback_on  boolean NOT NULL DEFAULT true,
    voice_fallback_min int     NOT NULL DEFAULT 5,
    status_updates_on  boolean NOT NULL DEFAULT true,
    updated_at         timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE app.tenant_oncall_cfg ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON app.tenant_oncall_cfg;
CREATE POLICY tenant_isolation ON app.tenant_oncall_cfg
    USING (tenant_id = (current_setting('app.current_tenant', true))::uuid)
    WITH CHECK (tenant_id = (current_setting('app.current_tenant', true))::uuid);

-- Token per il feed iCal per-utente (pubblico col token, nessuna sessione).
CREATE TABLE IF NOT EXISTS app.oncall_ical_tokens (
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    token      text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, user_id)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON app.tenant_oncall_cfg  TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.oncall_ical_tokens TO app_rt;
