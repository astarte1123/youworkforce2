-- "YouWorkforce pilota GoAlert": la configurazione di Gruppi allarme / Routing /
-- Escalation fatta nella suite viene sincronizzata verso GoAlert (utenti, escalation
-- policy con step sui membri + repeat, servizio + integration key). Memorizziamo gli
-- id GoAlert per una sincronizzazione idempotente.
ALTER TABLE app.users       ADD COLUMN IF NOT EXISTS goalert_user_id    text;
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS goalert_ep_id      text;
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS goalert_service_id text;
ALTER TABLE app.alarm_groups ADD COLUMN IF NOT EXISTS goalert_key        text;
