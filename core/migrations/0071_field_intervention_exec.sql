-- Lavoro sul campo — Slice 1: esecuzione intervento completa + luogo arricchito.
-- Tutto ADDITIVO e opzionale: i record esistenti (sedi con solo nome/ID, ordini già aperti)
-- continuano a funzionare. Nessuna migrazione distruttiva. Proprietari canonici invariati
-- (sites = anagrafica luogo; work_orders = intervento).

-- Luogo (sites): indirizzo, coordinate, indicazioni d'accesso (tutti opzionali).
-- Funziona con: solo nome, indirizzo senza coordinate, punto all'aperto senza indirizzo.
ALTER TABLE app.sites ADD COLUMN IF NOT EXISTS address      text;
ALTER TABLE app.sites ADD COLUMN IF NOT EXISTS lat          double precision;
ALTER TABLE app.sites ADD COLUMN IF NOT EXISTS lon          double precision;
ALTER TABLE app.sites ADD COLUMN IF NOT EXISTS access_notes text;  -- visibile solo agli autorizzati

-- Esecuzione intervento (work_orders): arrivo e avvio separati, checklist, note/ore/materiali,
-- esito strutturato. report/signed_by/signed_at già aggiunti in 0070.
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS arrived_at      timestamptz;      -- arrivo sul posto
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS work_started_at timestamptz;      -- inizio lavoro
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS work_minutes    int;              -- ore (minuti) lavorate
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS materials       jsonb NOT NULL DEFAULT '[]'::jsonb;  -- [{descr,qta,unita}]
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS checklist       jsonb NOT NULL DEFAULT '[]'::jsonb;  -- [{label,done}]
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS outcome_kind    text;             -- completato|parziale|cliente_assente|accesso_impedito|ritorno
