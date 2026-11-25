-- Piani di ronda sospendibili: `active` esisteva ma non c'era modo di cambiarlo. Alla riattivazione
-- le ronde «dovute» si contano da quel momento (active_since): gli orari del periodo sospeso non
-- diventano ronde mancate né allarmi arretrati.
ALTER TABLE app.patrol_schedules ADD COLUMN IF NOT EXISTS active_since timestamptz;
UPDATE app.patrol_schedules SET active_since = created_at WHERE active_since IS NULL;
ALTER TABLE app.patrol_schedules ALTER COLUMN active_since SET DEFAULT now();
ALTER TABLE app.patrol_schedules ALTER COLUMN active_since SET NOT NULL;
COMMENT ON COLUMN app.patrol_checkpoints.dwell_max_min IS
  'Riservato, non usato: con una sola punzonatura per punto non si misura la permanenza (manca l''uscita).';
