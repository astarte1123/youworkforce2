-- 0085 — Colore calendario per utente (W5). Additiva.
-- Ogni lavoratore può avere un colore distinto nel calendario turni (scelto o assegnato); serve per
-- distinguere/mostrare/nascondere i turni per persona (come i calendari di Google).
ALTER TABLE app.users ADD COLUMN IF NOT EXISTS cal_color text;
