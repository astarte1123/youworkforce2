-- IO-1 (25/11/2026): regole sensori con AMBITO e valutazione di tutte le regole.
-- Prima una regola valeva per tutti i sensori del tenant e contava solo la PRIMA che scattava:
-- una regola «telemetria» poteva nascondere una «critica» sulla stessa metrica.
-- Ora: ogni regola vale per tutto il tenant (default, come prima) oppure per un dispositivo, un
-- modello di dispositivo, una sede o uno scenario; il motore valuta TUTTE le regole applicabili e
-- tiene la più grave (a parità, la prima per ordinamento).
ALTER TABLE app.sensor_rules
  ADD COLUMN IF NOT EXISTS scope_kind text NOT NULL DEFAULT 'tenant',
  ADD COLUMN IF NOT EXISTS scope_ref  text;     -- id dispositivo / codice modello / id sede / codice scenario
DO $$ BEGIN
  ALTER TABLE app.sensor_rules ADD CONSTRAINT sensor_rules_scope_chk CHECK (
    scope_kind IN ('tenant','device','model','site','scenario')
    AND ((scope_kind = 'tenant') = (scope_ref IS NULL)));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
