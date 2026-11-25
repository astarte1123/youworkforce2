-- Consuntivi (25/11/2026):
--  CA-8  un consuntivo rimandato «da correggere» si può correggere (modifica/cancellazione di righe
--        e spese, endpoint nel codice); la nota del responsabile ha un campo suo invece di
--        sovrascrivere la nota del consuntivo (che porta anche i materiali dall'intervento);
--        lo storico delle note resta in timesheet_events.
--  CA-14 niente minuti/spese/tariffe negative, valuta a 3 lettere. I vincoli nascono NOT VALID
--        (non bloccano l'upgrade se esistono righe storiche fuori regola) e vengono validati
--        subito quando i dati lo permettono.
ALTER TABLE app.timesheets ADD COLUMN IF NOT EXISTS review_note text;

DO $$ BEGIN
  ALTER TABLE app.timesheet_lines ADD CONSTRAINT timesheet_lines_values_chk
    CHECK (minutes >= 0 AND minutes <= 1440 AND (rate_cents IS NULL OR rate_cents >= 0) AND currency ~ '^[A-Z]{3}$') NOT VALID;
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  ALTER TABLE app.expenses ADD CONSTRAINT expenses_values_chk
    CHECK (amount_cents >= 0 AND currency ~ '^[A-Z]{3}$') NOT VALID;
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
DO $$ BEGIN
  ALTER TABLE app.timesheet_lines VALIDATE CONSTRAINT timesheet_lines_values_chk;
EXCEPTION WHEN check_violation THEN RAISE NOTICE 'timesheet_lines: righe storiche fuori regola, vincolo resta NOT VALID'; END $$;
DO $$ BEGIN
  ALTER TABLE app.expenses VALIDATE CONSTRAINT expenses_values_chk;
EXCEPTION WHEN check_violation THEN RAISE NOTICE 'expenses: righe storiche fuori regola, vincolo resta NOT VALID'; END $$;
