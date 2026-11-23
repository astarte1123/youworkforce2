-- 0125 — Rimozione definitiva dei connettori esterni (Traccar server, GoAlert, ThingsBoard).
--
-- YouWorkforce è self-contained: non si collega MAI a server esterni (eccetto i provider AI).
-- La decodifica dei dispositivi (Teltonika Codec8/8E, OsmAnd/HTTP, JSON HTTP) è nativa da tempo;
-- qui si eliminano i residui dell'integrazione con il server Traccar (connessione, poller, stato,
-- id remoti) e si rinomina il modulo "traccar" in "gps" (Localizzazione GPS e badge), che è ciò che
-- il modulo fa davvero. I dati storici immutabili (audit_log, outbox, migrazioni) non si toccano.


-- 1) Modulo traccar -> gps -----------------------------------------------------------------
INSERT INTO app.modules_catalog(code,name,kind,available)
SELECT 'gps','Localizzazione GPS e badge','product',available FROM app.modules_catalog WHERE code='traccar'
ON CONFLICT (code) DO NOTHING;
INSERT INTO app.modules_catalog(code,name,kind,available)
SELECT 'gps','Localizzazione GPS e badge','product',true
WHERE NOT EXISTS (SELECT 1 FROM app.modules_catalog WHERE code='gps');

INSERT INTO app.module_tools(module_code,tool_code,name,description,config_schema,default_config,sort)
SELECT 'gps',tool_code,'Allarmi e posizione dei dispositivi',
       'Come trattare SOS, batteria, fermo prolungato e silenzio dei dispositivi GPS/badge.',
       config_schema,default_config,sort
FROM app.module_tools WHERE module_code='traccar'
ON CONFLICT DO NOTHING;

UPDATE app.tenant_modules       SET module_code='gps' WHERE module_code='traccar';
UPDATE app.tenant_module_config SET module_code='gps' WHERE module_code='traccar';
UPDATE app.tenant_tool_enabled  SET module_code='gps' WHERE module_code='traccar';
DELETE FROM app.module_tools    WHERE module_code='traccar';
DELETE FROM app.modules_catalog WHERE code='traccar';

-- Soglia di silenzio del dispositivo (prima viveva nella connessione Traccar): ora è un parametro
-- nativo dello strumento gps/alarm_map. 0 = disattivo. Default prodotto: 600 s.
UPDATE app.module_tools
SET config_schema = jsonb_set(config_schema, '{fields}',
      (config_schema->'fields') || '[{"key":"silence_after_s","type":"int","min":0,"max":86400,
        "label":"Dispositivo silenzioso dopo (secondi, 0 = disattivo)"}]'::jsonb),
    default_config = default_config || '{"silence_after_s":600}'::jsonb
WHERE module_code='gps' AND tool_code='alarm_map'
  AND NOT (config_schema->'fields') @> '[{"key":"silence_after_s"}]'::jsonb;

-- 2) Sorgente dispositivi / token / eventi: 'traccar' -> 'gps' -----------------------------------
UPDATE app.devices       SET source='gps' WHERE source='traccar';
UPDATE app.ingest_tokens SET source='gps' WHERE source='traccar';
UPDATE app.events        SET source='gps' WHERE source='traccar';
DELETE FROM app.ingest_tokens WHERE source='thingsboard';
UPDATE app.events        SET source='iot' WHERE source='thingsboard';
UPDATE app.events        SET etype='device_event' WHERE etype='traccar_event';
UPDATE app.incidents     SET etype='device_event' WHERE etype='traccar_event';
UPDATE app.incident_timeline SET kind='notify_dispatched' WHERE kind='goalert_sent';

-- 3) Colonne con id/stato del server Traccar ------------------------------------------------------
ALTER TABLE app.devices             DROP COLUMN IF EXISTS traccar_device_id;
ALTER TABLE app.devices             DROP COLUMN IF EXISTS traccar_status;
ALTER TABLE app.device_provisioning DROP COLUMN IF EXISTS traccar_device_id;
UPDATE app.device_provisioning SET step='in_attesa_primo_fix' WHERE step='traccar_registrato';
ALTER TABLE app.device_commands RENAME COLUMN traccar_status TO status_code;

-- Ultimo contatto del dispositivo (qualsiasi pacchetto, anche senza fix GPS): alimenta il
-- watchdog nativo del silenzio, che prima dipendeva dal "lastUpdate" del server Traccar.
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS last_seen_at timestamptz;
UPDATE app.devices SET last_seen_at = GREATEST(last_pos_at, last_telemetry_at) WHERE last_seen_at IS NULL;

-- 4) Connessione e poller del server Traccar -------------------------------------------------------
DROP TABLE IF EXISTS app.traccar_poll_state;
DROP TABLE IF EXISTS app.traccar_connections;

-- 5) Catalogo dispositivi: le note non rimandano più a un server esterno -------------------------------
UPDATE app.device_models
SET notes = 'App OsmAnd sul telefono del lavoratore (protocollo OsmAnd/HTTP, decodifica nativa)'
WHERE model_code='smartphone-osmand';

