-- AR-7 (glossario unico): la spiegazione predefinita di «Ronda mancata» seminata in 0161 diceva
-- «giro di ronda». Si corregge solo se il superadmin non l'ha già cambiata.
UPDATE app.event_types
   SET description = 'Una ronda pianificata non è stata avviata in orario.', updated_at = now()
 WHERE code = 'ronda_mancata'
   AND description = 'Un giro di ronda pianificato non è stato svolto in orario.';
