-- Ritiro dei connettori ESTERNI dal prodotto. YouWorkforce è una suite SELF-CONTAINED: non si
-- connette mai a servizi esterni. I motori sono tutti nativi:
--   escalation nativa (default)  <- sostituisce GoAlert
--   edge nativo Teltonika/OsmAnd <- sostituisce il server Traccar
--   IoT nativo (sensor rules + scenari "IoT Use Cases") <- sostituisce ThingsBoard
-- I connettori-ponte goalert/traccar/thingsboard diventano NON disponibili nel prodotto
-- (non più attivabili dal superadmin) e vengono disattivati ovunque fossero attivi.
-- Additiva e non distruttiva: le righe restano (storico), cambia solo availability/stato. Il codice
-- dei connettori resta inerte (i poll sono gated su modulo attivo) e sarà rimosso col modulo IoT nativo.

UPDATE app.modules_catalog SET available=false WHERE code IN ('goalert','traccar','thingsboard');

UPDATE app.tenant_modules SET state='non_abilitato', updated_at=now()
 WHERE module_code IN ('goalert','traccar','thingsboard') AND state <> 'non_abilitato';
