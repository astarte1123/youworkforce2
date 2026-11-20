-- 0115 — Pulizia residui schema del connettore esterno GoAlert (ritirato in 0114).
-- L'escalation è il motore NATIVO YWAlert: queste colonne/tabelle del vecchio connettore
-- non sono più referenziate da alcun codice. Additiva/pulizia, idempotente.

-- colonne vestigiali su alarm_groups (id servizio/EP/rotation/schedule/key lato GoAlert)
ALTER TABLE app.alarm_groups  DROP COLUMN IF EXISTS goalert_service_id;
ALTER TABLE app.alarm_groups  DROP COLUMN IF EXISTS goalert_ep_id;
ALTER TABLE app.alarm_groups  DROP COLUMN IF EXISTS goalert_rotation_id;
ALTER TABLE app.alarm_groups  DROP COLUMN IF EXISTS goalert_schedule_id;
ALTER TABLE app.alarm_groups  DROP COLUMN IF EXISTS goalert_key;

-- id alert esterno sull'incidente / id utente esterno
ALTER TABLE app.incidents     DROP COLUMN IF EXISTS goalert_alert_id;
ALTER TABLE app.users         DROP COLUMN IF EXISTS goalert_user_id;

-- tabella del confronto-ombra nativo-vs-GoAlert (fase di cutover, non più usata)
DROP TABLE IF EXISTS app.esc_shadow_log;
