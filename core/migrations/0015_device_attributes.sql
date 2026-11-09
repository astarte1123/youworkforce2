-- ============================================================================
-- #1 Attributi device esposti: ultima telemetria utile (ignition, motion, heartRate,
-- steps, temperatura, odometro, segnale...) tenuta a portata di mano sul device.
-- Lo storico li ha gia' (device_positions.attributes); qui l'ultima nota.
-- ============================================================================
ALTER TABLE devices ADD COLUMN IF NOT EXISTS last_attributes jsonb;
