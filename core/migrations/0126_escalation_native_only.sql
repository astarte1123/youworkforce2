-- 0126 — Escalation solo nativa: tipi della timeline con nomi nativi (al posto dei vecchi nomi
-- GoAlert) e rimozione della modalità "motore esterno". Il contenuto delle voci resta invariato.
UPDATE app.incident_timeline SET kind='notify_requested'  WHERE kind='goalert_notify';
UPDATE app.incident_timeline SET kind='notify_dispatched' WHERE kind IN ('goalert_sent','goalert_suppressed_native');
UPDATE app.incident_timeline SET kind='alarm_closed'      WHERE kind='goalert_closed';
UPDATE app.outbox            SET kind='notify'            WHERE kind='goalert_notify';
UPDATE app.outbox            SET kind='close'             WHERE kind='goalert_close';

-- Motore di escalation: esiste solo quello nativo. La colonna che permetteva di tornare al vecchio
-- motore esterno (modalità "ombra") non ha più senso.
ALTER TABLE app.tenant_oncall_cfg DROP COLUMN IF EXISTS escalation_engine;
