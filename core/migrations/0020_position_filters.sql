-- ============================================================================
-- #7 Filtri posizione: scarta "salti" impossibili e rumore/duplicati.
-- Config nello strumento traccar/alarm_map:
--   max_jump_kmh : velocita' implicita oltre cui la posizione e' scartata (glitch GPS)
--   min_move_m   : spostamento minimo (m) entro 5 min per registrare una nuova posizione
-- ============================================================================
UPDATE app.module_tools
SET config_schema = '{"fields":[
      {"key":"sos_as","label":"Mappa sos -> ","type":"enum","options":["sos","man_down"]},
      {"key":"offline_critical","label":"Offline = perdita contatto (incidente)","type":"enum","options":["no","si"]},
      {"key":"stopped_as_immobilita","label":"Fermo prolungato = immobilita (incidente)","type":"enum","options":["no","si"]},
      {"key":"battery_min","label":"Soglia batteria bassa (%)","type":"int","min":0,"max":100},
      {"key":"max_jump_kmh","label":"Velocita max plausibile (km/h, 0=off)","type":"int","min":0,"max":2000},
      {"key":"min_move_m","label":"Spostamento minimo (m, 0=off)","type":"int","min":0,"max":1000}
    ]}'::jsonb,
    default_config = '{"sos_as":"sos","offline_critical":"no","stopped_as_immobilita":"no","battery_min":15,"max_jump_kmh":300,"min_move_m":5}'::jsonb
WHERE module_code='traccar' AND tool_code='alarm_map';
