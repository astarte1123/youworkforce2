-- 0094 — Cantieri C2: cronoprogramma. Date e dipendenze sulle lavorazioni (WBS). Additiva.
-- Ogni lavorazione può avere una finestra pianificata e dipendere da un'altra (finish-to-start):
-- "questa inizia dopo che è finita quella".
ALTER TABLE app.commessa_lavorazioni ADD COLUMN IF NOT EXISTS start_date  date;
ALTER TABLE app.commessa_lavorazioni ADD COLUMN IF NOT EXISTS end_date    date;
ALTER TABLE app.commessa_lavorazioni ADD COLUMN IF NOT EXISTS depends_on  uuid REFERENCES app.commessa_lavorazioni(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS commessa_lav_dates ON app.commessa_lavorazioni(commessa_id, start_date);
