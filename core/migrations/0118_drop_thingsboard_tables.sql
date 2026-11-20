-- Smantellamento del connettore ESTERNO ThingsBoard. Il modulo era già disattivato (0078) e rimosso
-- dal catalogo/config (0108); le route HTTP esterne erano già sparite. Qui si rimuovono le ultime
-- tabelle di CONNESSIONE/PULL, popolate solo dal poller esterno (ora eliminato). Il motore IoT
-- NATIVO (sensor_rules, device_telemetry/attributes/commands, scenari, alert IoT) resta intatto.
-- Analogo a 0115 per GoAlert.
DROP TABLE IF EXISTS app.thingsboard_poll_state;
DROP TABLE IF EXISTS app.thingsboard_connections;

-- Eventuali device-specchio residui dal vecchio pull: normalizzati a origine nativa.
UPDATE app.devices SET source='iot' WHERE source='thingsboard';
