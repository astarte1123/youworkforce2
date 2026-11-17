-- 0108 — Ritiro definitivo del connettore ThingsBoard esterno. IoT industriale è ora NATIVO
-- nel modulo `iot` (telemetria/attributi/comandi/soglie→incidenti). Additiva/pulizia.
UPDATE app.modules_catalog SET name='Dispositivi e IoT industriale' WHERE code='iot';
-- rimuovi il modulo thingsboard e tutte le righe dipendenti (FK su modules_catalog.code)
DELETE FROM app.tenant_module_config WHERE module_code='thingsboard';
DELETE FROM app.tenant_tool_enabled  WHERE module_code='thingsboard';
DELETE FROM app.module_tools         WHERE module_code='thingsboard';
DELETE FROM app.tenant_modules       WHERE module_code='thingsboard';
DELETE FROM app.modules_catalog      WHERE code='thingsboard';
