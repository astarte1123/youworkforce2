-- ============================================================================
-- Traccar P1 (sicurezza): lo strumento traccar/alarm_map diventa piu' ricco.
--   sos_as          : instrada l'allarme sos (sos | man_down)
--   offline_critical: deviceOffline apre incidente (perdita contatto)  (si | no)
--   battery_min     : soglia % sotto cui una posizione genera batteria_bassa
-- La mappa completa allarme->etype e' nel core (traccar_alarm_etype), con
-- default orientati al lavoratore isolato (removing/tampering, powerCut, jamming...).
-- ============================================================================
UPDATE app.module_tools
SET config_schema = '{"fields":[
      {"key":"sos_as","label":"Mappa sos -> ","type":"enum","options":["sos","man_down"]},
      {"key":"offline_critical","label":"Offline = perdita contatto (incidente)","type":"enum","options":["no","si"]},
      {"key":"battery_min","label":"Soglia batteria bassa (%)","type":"int","min":0,"max":100}
    ]}'::jsonb,
    default_config = '{"sos_as":"sos","offline_critical":"no","battery_min":15}'::jsonb
WHERE module_code='traccar' AND tool_code='alarm_map';
