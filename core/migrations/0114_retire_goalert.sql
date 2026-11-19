-- 0114 — Ritiro definitivo del connettore GoAlert esterno. L'escalation è NATIVA
-- (delivery.rs: ladder SMS/voce/email/Slack, sink nativo/Twilio). Il modulo 'goalert' e le
-- tabelle del connettore esterno non servono più. Additiva/pulizia.

-- rimuovi il modulo goalert dal catalogo e le righe dipendenti (FK su modules_catalog.code)
DELETE FROM app.tenant_module_config WHERE module_code='goalert';
DELETE FROM app.tenant_tool_enabled  WHERE module_code='goalert';
DELETE FROM app.module_tools         WHERE module_code='goalert';
DELETE FROM app.tenant_modules       WHERE module_code='goalert';
DELETE FROM app.modules_catalog      WHERE code='goalert';

-- normalizza l'outbox: i vecchi kind del connettore diventano i kind nativi
UPDATE app.outbox SET kind='notify' WHERE kind='goalert_notify';
UPDATE app.outbox SET kind='close'  WHERE kind='goalert_close';

-- tabelle del connettore esterno GoAlert: non più usate da alcun codice
DROP TABLE IF EXISTS app.goalert_routing;
DROP TABLE IF EXISTS app.goalert_services;

-- l'engine di escalation ora è sempre nativo: azzera eventuali rollback 'goalert'
UPDATE app.tenant_oncall_cfg SET escalation_engine='native' WHERE escalation_engine='goalert';
