-- ============================================================================
-- Traccar P3: opzione deviceStopped -> immobilita (uomo a terra per fermo prolungato).
-- Aggiunto allo strumento traccar/alarm_map.
-- ============================================================================
UPDATE app.module_tools
SET config_schema = '{"fields":[
      {"key":"sos_as","label":"Mappa sos -> ","type":"enum","options":["sos","man_down"]},
      {"key":"offline_critical","label":"Offline = perdita contatto (incidente)","type":"enum","options":["no","si"]},
      {"key":"stopped_as_immobilita","label":"Fermo prolungato = immobilita (incidente)","type":"enum","options":["no","si"]},
      {"key":"battery_min","label":"Soglia batteria bassa (%)","type":"int","min":0,"max":100}
    ]}'::jsonb,
    default_config = '{"sos_as":"sos","offline_critical":"no","stopped_as_immobilita":"no","battery_min":15}'::jsonb
WHERE module_code='traccar' AND tool_code='alarm_map';
