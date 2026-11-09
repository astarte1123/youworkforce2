-- Man Down/SOS (D.Lgs. 81/08) punto 1d: "batteria in esaurimento -> lavoratore privo
-- di protezione". Prima batteria_bassa era hardcoded come warning non critico: l'admin
-- non poteva decidere che diventasse un allarme. Aggiungiamo la policy configurabile
-- battery_as (off | warning | critico), stesso schema degli altri toggle di severita'.
UPDATE app.module_tools
SET config_schema = jsonb_set(
      config_schema,
      '{fields}',
      (config_schema->'fields') || jsonb_build_array(
        jsonb_build_object(
          'key','battery_as',
          'type','enum',
          'label','Batteria in esaurimento',
          'options', jsonb_build_array('warning','critico','off')
        )
      )
    ),
    default_config = default_config || '{"battery_as":"warning"}'::jsonb
WHERE module_code='traccar' AND tool_code='alarm_map'
  AND NOT (config_schema->'fields' @> '[{"key":"battery_as"}]');

-- i tenant che hanno gia' una config propria: aggiungi il default senza sovrascrivere scelte
UPDATE app.tenant_module_config
SET config = '{"battery_as":"warning"}'::jsonb || config
WHERE module_code='traccar' AND tool_code='alarm_map'
  AND NOT (config ? 'battery_as');
