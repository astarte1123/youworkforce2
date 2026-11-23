-- ============================================================================
-- GH-5200 (Teltonika) come tracker PERSONALE lone-worker salva-vita.
-- Nel catalogo 0024 era declassato a category 'altro' / solo GPS: lo promuoviamo
-- a 'personale' con i segnali di sicurezza reali e la MAPPA IO->allarme del modello
-- (dato di prodotto, non hardcoded nel decoder), verificata su wiki Teltonika:
--   IO 236 = Alarm/pulsante SOS  -> etype 'sos'
--   IO 242 = ManDown/FallDown    -> etype 'man_down' (uomo a terra / brusca caduta)
--   IO 240 = Movement            -> immobilità derivata (stopped_as_immobilita)
-- Le capabilities [sos,fall,movement] fanno sì che il Device Onboarding Kit crei
-- automaticamente il routing allarmi (sos/man_down/immobilita) e il profilo di
-- protezione del lavoratore. L'io_alarm_map resta OVERRIDABILE per-tenant
-- (tool_config teltonika.io_alarm_map vince sul default del modello).
-- ============================================================================
UPDATE app.device_models SET
  category     = 'personale',
  capabilities = ARRAY['gps','battery','sos','fall','movement'],
  recommended  = true,
  default_config = jsonb_build_object(
     'sos_as','sos',
     'stopped_as_immobilita','si',
     'battery_min',15,
     'io_alarm_map', jsonb_build_object('236','sos','242','man_down')
  ),
  notes = 'Tracker personale lone-worker: SOS, uomo a terra / caduta (ManDown/FallDown), immobilità, GPS'
WHERE model_code = 'teltonika-gh-5200';

-- TMT250 (altro personale Teltonika, stessi IO 236/242): aggiungi la mappa IO al suo
-- default_config preservando le chiavi esistenti.
UPDATE app.device_models SET
  default_config = default_config || jsonb_build_object('io_alarm_map', jsonb_build_object('236','sos','242','man_down'))
WHERE model_code = 'teltonika-tmt250';
