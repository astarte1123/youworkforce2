-- Perdita di contatto (Q4, decisione PO 24/11/2026):
--  1. si chiude da sola quando il dispositivo torna in linea (codice, nessun cambio di schema);
--  2. il silenzio è sorvegliato solo durante il turno del lavoratore;
--  3. vale anche per l'app del lavoratore.
-- Turno = turno aperto nell'app (app.shifts in_corso) oppure, per i badge, un turno pianificato
-- (app.shift_plans) che copre l'istante attuale.

-- 2. badge: nuova impostazione «solo durante il turno» (default sì, come da decisione PO)
UPDATE app.module_tools
   SET default_config = default_config || '{"silence_shift_only":"si"}'::jsonb,
       config_schema = jsonb_set(config_schema, '{fields}', (config_schema->'fields') ||
         '[{"key":"silence_shift_only","type":"enum","options":["si","no"],
            "label":"Silenzio sorvegliato solo durante il turno del lavoratore (turno aperto nell''app o pianificato)"}]'::jsonb)
 WHERE module_code='gps' AND tool_code='alarm_map'
   AND NOT (config_schema->'fields') @> '[{"key":"silence_shift_only"}]'::jsonb;

-- 3. app del lavoratore: soglia di silenzio durante il turno (0 = disattivo)
INSERT INTO app.module_tools(module_code, tool_code, name, description, config_schema, default_config, sort)
VALUES ('loneworker', 'app_silence', 'Silenzio dell''app',
  'Durante il turno l''app del lavoratore dà segni di vita ogni minuto. Se tace oltre la soglia (telefono spento, scarico, senza rete, app chiusa) si apre un allarme Perdita di contatto con escalation; si chiude da solo quando l''app torna in linea. Non vale per chi ha un badge in servizio: lo sorveglia il badge.',
  '{"fields":[{"key":"app_silence_after_s","type":"int","min":0,"max":86400,"label":"App silenziosa durante il turno dopo (secondi, 0 = disattivo)"}]}'::jsonb,
  '{"app_silence_after_s":900}'::jsonb, 30)
ON CONFLICT (module_code, tool_code) DO NOTHING;
