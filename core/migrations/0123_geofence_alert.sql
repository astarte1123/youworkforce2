-- Allarme su attraversamento geofence calcolato da YW: una geofence può alzare un allarme
-- all'INGRESSO (zona vietata) o all'USCITA (area sicura/consentita). NULL = nessun allarme
-- (solo presenza/log, comportamento attuale).
ALTER TABLE app.geofences ADD COLUMN IF NOT EXISTS alert_on text;  -- NULL | 'enter' | 'exit'
