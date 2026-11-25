-- Decisione PO 25/11/2026 sera: prima dell'allarme di silenzio dell'app il lavoratore riceve un
-- SMS di preavviso («Sei ancora lì? Apri l'app»): a schermo bloccato il telefono può sospendere
-- l'app web e far scattare falsi allarmi. Default 120 s prima della soglia (0 = nessun preavviso).
UPDATE app.module_tools
   SET default_config = default_config || '{"app_silence_warn_s":120}'::jsonb,
       config_schema = jsonb_set(config_schema, '{fields}', (config_schema->'fields') ||
         '[{"key":"app_silence_warn_s","type":"number","min":0,"max":1800,
            "label":"Preavviso al lavoratore via SMS, in secondi prima dell''allarme (0 = nessun preavviso)"}]'::jsonb)
 WHERE module_code = 'loneworker' AND tool_code = 'app_silence'
   AND NOT (config_schema->'fields') @> '[{"key":"app_silence_warn_s"}]'::jsonb;
