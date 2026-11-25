-- SL-18 (25/11/2026): il trigger «geofence» dei profili di protezione era legato a un tipo di
-- allarme inesistente (attraversamento_geofence) e quindi non aveva mai effetto: gli allarmi di
-- area partivano sempre, anche se il profilo diceva «geofence: no» (default dei profili).
-- Ora il trigger vale per i tipi reali (geofence_ingresso/uscita, ingresso/uscita sito). Per non
-- spegnere all'improvviso allarmi che oggi arrivano, nei profili esistenti il flag viene acceso
-- (comportamento invariato); chi vuole spegnerli lo fa ora consapevolmente dal profilo.
UPDATE app.protection_profiles
   SET triggers = jsonb_set(COALESCE(triggers, '{}'::jsonb), '{geofence}', 'true'::jsonb)
 WHERE COALESCE(triggers->>'geofence', 'false') <> 'true';
