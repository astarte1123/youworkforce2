-- Decisione PO 25/11/2026 sera (rivede Q4 / mig 0137): il silenzio di un badge in servizio fa
-- scattare l'allarme SEMPRE; «solo durante il turno» diventa una scelta esplicita (opt-in).
-- Motivo: un'azienda che non usa i turni non riceveva più alcun allarme di silenzio.
-- Vale anche per i tenant esistenti: chi aveva il valore «si» (il vecchio default) torna a «no».
UPDATE app.module_tools
   SET default_config = default_config || '{"silence_shift_only":"no"}'::jsonb,
       config_schema = jsonb_set(config_schema, '{fields}', (
         SELECT jsonb_agg(CASE WHEN f->>'key' = 'silence_shift_only'
                  THEN f || '{"label":"Sorveglia il silenzio solo durante il turno del lavoratore (turno aperto nell''app o pianificato). Con «no» il silenzio allarma sempre quando il badge è in servizio."}'::jsonb
                  ELSE f END)
           FROM jsonb_array_elements(config_schema->'fields') f))
 WHERE module_code = 'gps' AND tool_code = 'alarm_map';

UPDATE app.tenant_module_config
   SET config = config || '{"silence_shift_only":"no"}'::jsonb
 WHERE module_code = 'gps' AND tool_code = 'alarm_map' AND config->>'silence_shift_only' = 'si';
