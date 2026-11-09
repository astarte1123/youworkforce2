-- L'admin del tenant deve poter abilitare in modo indipendente la voce di menu
-- "Mappa" (vista outdoor/GPS: allarmi e dispositivi su cartografia) e la voce
-- "Piantine" (vista indoor: planimetrie con beacon e geofence). Prima erano una
-- sola voce "Mappa/Piantine" non configurabile. Aggiungiamo due toggle parametrici
-- allo schema del tool alarm_map (stesso meccanismo config schema-driven degli altri).
UPDATE app.module_tools
SET config_schema = jsonb_set(
      config_schema,
      '{fields}',
      (config_schema->'fields') || jsonb_build_array(
        jsonb_build_object(
          'key','view_mappa',
          'type','enum',
          'label','Voce di menu «Mappa» (outdoor/GPS)',
          'options', jsonb_build_array('si','no')
        ),
        jsonb_build_object(
          'key','view_piantine',
          'type','enum',
          'label','Voce di menu «Piantine» (indoor/beacon)',
          'options', jsonb_build_array('si','no')
        )
      )
    ),
    -- default: Mappa attiva, Piantine spenta (indoor e' specialistico, si abilita a richiesta)
    default_config = default_config || '{"view_mappa":"si","view_piantine":"no"}'::jsonb
WHERE module_code='traccar' AND tool_code='alarm_map'
  AND NOT (config_schema->'fields' @> '[{"key":"view_mappa"}]');
