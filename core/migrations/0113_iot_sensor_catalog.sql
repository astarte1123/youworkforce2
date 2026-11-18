-- 0113 — IoT Fase 5: catalogo sensori REALI. Finora device_models = solo tracker GPS (sicurezza).
-- Aggiungiamo archetipi di sensori industriali (category 'sensore') AGGANCIATI ai blueprint, così
-- l'attivazione "per un lavoro" propone l'hardware giusto e lo configura da sé. Globale/curato.

-- Collegamento modello -> profilo-lavoro (blueprint). NULL = modello generico/flessibile.
ALTER TABLE app.device_models ADD COLUMN IF NOT EXISTS blueprint text;

-- capabilities dei sensori = metriche canoniche (iot_metrics) che il modello misura.
-- default_config porta il blueprint + la soglia di silenzio (offline_after_s) suggerita.
INSERT INTO device_models(model_code,manufacturer,model_name,protocol,default_port,category,capabilities,osmand,default_config,recommended,notes,blueprint) VALUES
-- ---- Qualità dell'aria (CO₂, temperatura, umidità) ----
 ('elsys-ers-co2','Elsys','ERS CO2','lorawan',NULL,'sensore',
    ARRAY['temperature','humidity','co2','motion'],false,'{"blueprint":"air-quality","offline_after_s":3600}'::jsonb,true,
    'Sensore ambiente LoRaWAN: CO₂ + temperatura + umidità + presenza','air-quality'),
 ('milesight-am103','Milesight','AM103','lorawan',NULL,'sensore',
    ARRAY['temperature','humidity','co2'],false,'{"blueprint":"air-quality","offline_after_s":3600}'::jsonb,false,
    'Monitor qualità aria da interno (LoRaWAN)','air-quality'),
-- ---- Catena del freddo (temperatura) ----
 ('dragino-lht65','Dragino','LHT65','lorawan',NULL,'sensore',
    ARRAY['temperature','humidity'],false,'{"blueprint":"cold-chain","offline_after_s":900}'::jsonb,true,
    'Sensore temperatura/umidità con sonda esterna (LoRaWAN)','cold-chain'),
 ('elsys-ers','Elsys','ERS','lorawan',NULL,'sensore',
    ARRAY['temperature','humidity'],false,'{"blueprint":"cold-chain","offline_after_s":900}'::jsonb,false,
    'Sensore ambiente LoRaWAN temperatura/umidità','cold-chain'),
-- ---- Livello serbatoio (distanza/ultrasuoni) ----
 ('milesight-em500-udl','Milesight','EM500-UDL','lorawan',NULL,'sensore',
    ARRAY['distance','level'],false,'{"blueprint":"tank-level","offline_after_s":900}'::jsonb,true,
    'Sensore di livello a ultrasuoni (LoRaWAN)','tank-level'),
 ('dragino-ldds75','Dragino','LDDS75','lorawan',NULL,'sensore',
    ARRAY['distance','level'],false,'{"blueprint":"tank-level","offline_after_s":900}'::jsonb,false,
    'Sensore di distanza/livello a ultrasuoni (LoRaWAN)','tank-level'),
-- ---- Varchi e porte (contatto) ----
 ('milesight-ws301','Milesight','WS301','lorawan',NULL,'sensore',
    ARRAY['door'],false,'{"blueprint":"door-contact","offline_after_s":3600}'::jsonb,true,
    'Contatto magnetico porta/finestra (LoRaWAN)','door-contact'),
 ('dragino-lds02','Dragino','LDS02','lorawan',NULL,'sensore',
    ARRAY['door'],false,'{"blueprint":"door-contact","offline_after_s":3600}'::jsonb,false,
    'Sensore apertura porta (LoRaWAN)','door-contact'),
-- ---- Energia ----
 ('shelly-em','Shelly','EM','mqtt',1883,'sensore',
    ARRAY['energy','power','voltage','current'],false,'{"blueprint":"energy-meter","offline_after_s":3600}'::jsonb,true,
    'Misuratore di energia/potenza con pinza amperometrica (MQTT)','energy-meter'),
 ('milesight-ct101','Milesight','CT101','lorawan',NULL,'sensore',
    ARRAY['current','power'],false,'{"blueprint":"energy-meter","offline_after_s":3600}'::jsonb,false,
    'Trasformatore di corrente intelligente (LoRaWAN)','energy-meter'),
-- ---- Conteggio persone ----
 ('milesight-vs121','Milesight','VS121','mqtt',1883,'sensore',
    ARRAY['count','presence'],false,'{"blueprint":"people-count","offline_after_s":3600}'::jsonb,false,
    'Contatore di persone AI da soffitto (MQTT)','people-count'),
-- ---- Generici (flessibili, nessun blueprint predefinito) ----
 ('generic-mqtt-sensor','Generico','Sensore MQTT','mqtt',1883,'sensore',
    ARRAY[]::text[],false,'{}'::jsonb,true,'Qualsiasi sensore che pubblica JSON sul broker MQTT nativo',NULL),
 ('generic-http-sensor','Generico','Sensore HTTP','http',NULL,'sensore',
    ARRAY[]::text[],false,'{}'::jsonb,false,'Qualsiasi sorgente che fa POST di telemetria JSON (HTTP)',NULL),
 ('generic-lorawan-sensor','Generico','Sensore LoRaWAN','lorawan',NULL,'sensore',
    ARRAY[]::text[],false,'{}'::jsonb,false,'Qualsiasi sensore LoRaWAN via webhook TTN/ChirpStack',NULL)
ON CONFLICT (model_code) DO UPDATE SET
  manufacturer=EXCLUDED.manufacturer, model_name=EXCLUDED.model_name, protocol=EXCLUDED.protocol,
  default_port=EXCLUDED.default_port, category=EXCLUDED.category, capabilities=EXCLUDED.capabilities,
  default_config=EXCLUDED.default_config, recommended=EXCLUDED.recommended, notes=EXCLUDED.notes, blueprint=EXCLUDED.blueprint;
