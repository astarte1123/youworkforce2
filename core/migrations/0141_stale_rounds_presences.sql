-- CA-17 (25/11/2026): ronde e presenze non restano aperte per sempre; una sola ronda in corso per giro.
--  - ronda in corso oltre il limite (finestra del giro + tolleranza + 60 min, o 8 ore senza
--    finestra): chiusa dal worker come «abbandonata» (closed_by='auto') con anomalia
--  - presenza di cantiere aperta oltre 16 ore: chiusa dal worker a durata zero e marcata
--    `auto_closed` (da verificare): non gonfia le ore né i costi
--  - indice unico parziale «una ronda in corso per giro» se i dati esistenti lo permettono
--    (l'API rifiuta comunque la seconda ronda)
ALTER TABLE app.patrol_rounds ADD COLUMN IF NOT EXISTS closed_by text;
ALTER TABLE app.commessa_presenze ADD COLUMN IF NOT EXISTS auto_closed boolean NOT NULL DEFAULT false;
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM app.patrol_rounds WHERE status='in_corso' GROUP BY route_id HAVING count(*) > 1) THEN
    CREATE UNIQUE INDEX IF NOT EXISTS patrol_rounds_one_open ON app.patrol_rounds(route_id) WHERE status='in_corso';
  ELSE
    RAISE NOTICE 'patrol_rounds: più ronde in corso sullo stesso giro, indice unico non creato (le chiude il worker)';
  END IF;
END $$;
