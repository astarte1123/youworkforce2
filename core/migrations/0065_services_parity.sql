-- Parità pagina Services GoAlert — heartbeat monitor nativo (S1), ingress email (S2).
-- (S3 bulk ack/close e S4 on-call-now nativo non richiedono schema.)
-- Gating applicativo ESCALATION_MODS: config del tenant admin, modulo acceso dal superadmin.

-- S1) HEARTBEAT MONITOR nativo: un sistema esterno POSTa periodicamente; se non arriva un POST entro
--     `timeout_s`, il watchdog apre un incidente 'heartbeat_down' instradato. Alla ripresa del ping
--     l'incidente si chiude da solo (recovery). Opposto al ping che YW fa verso GoAlert.
CREATE TABLE IF NOT EXISTS app.heartbeat_monitors (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name        text NOT NULL,
    token       text NOT NULL UNIQUE,                 -- segreto nell'URL di ping
    timeout_s   int  NOT NULL DEFAULT 900,            -- finestra massima senza ping (default 15 min)
    group_id    uuid REFERENCES app.alarm_groups(id) ON DELETE SET NULL,  -- routing override
    last_ping_at timestamptz,
    state       text NOT NULL DEFAULT 'ok',           -- ok|down
    incident_id uuid,                                 -- incidente down aperto corrente
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS heartbeat_monitors_tenant_idx ON app.heartbeat_monitors(tenant_id);

-- S2) EMAIL INBOX: indirizzo per-tenant (local-part = token). Un'email ricevuta (via webhook del
--     provider) diventa un incidente 'email_alarm' instradato. dedup custom via +chiave nel local-part.
CREATE TABLE IF NOT EXISTS app.email_inboxes (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    token       text NOT NULL UNIQUE,                 -- local-part dell'indirizzo
    group_id    uuid REFERENCES app.alarm_groups(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS email_inboxes_tenant_idx ON app.email_inboxes(tenant_id);

ALTER TABLE app.heartbeat_monitors ENABLE ROW LEVEL SECURITY;
ALTER TABLE app.email_inboxes      ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.heartbeat_monitors
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.email_inboxes
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.heartbeat_monitors TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.email_inboxes      TO app_rt;

-- gruppo forzato su un incidente (override generale: heartbeat/email instradati a un gruppo
-- specifico). Ha la precedenza su profilo/routing/default nel resolver del motore nativo.
ALTER TABLE app.incidents
  ADD COLUMN IF NOT EXISTS forced_group_id uuid REFERENCES app.alarm_groups(id) ON DELETE SET NULL;
