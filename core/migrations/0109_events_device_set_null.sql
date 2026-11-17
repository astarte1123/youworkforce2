-- 0109 — events.device_id: ON DELETE SET NULL.
-- Prima: la FK events.device_id non aveva azione ON DELETE -> un dispositivo che
-- avesse mai generato un evento NON era piu' eliminabile (restava "fantasma",
-- al massimo disabilitabile). Ora: eliminando il device lo storico eventi resta
-- (audit preservato) ma il collegamento al device viene azzerato. Tutte le altre
-- tabelle dipendenti (telemetria, attributi, comandi, posizioni, presenze,
-- geofence, provisioning) sono gia' ON DELETE CASCADE, quindi il device diventa
-- eliminabile in modo pulito e completo.
ALTER TABLE app.events DROP CONSTRAINT IF EXISTS events_device_id_fkey;
ALTER TABLE app.events
  ADD CONSTRAINT events_device_id_fkey
  FOREIGN KEY (device_id) REFERENCES app.devices(id) ON DELETE SET NULL;
