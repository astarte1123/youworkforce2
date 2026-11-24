-- 0133 — Rilevamento NATIVO dell'immobilità (SL-2). Prima l'immobilità era promessa (onboarding,
-- documentazione) ma nessun canale nativo la rilevava.
-- Decisione PO (24/11/2026): parametri tutti configurabili da chi imposta il profilo di protezione,
-- soglia di default 5 minuti, solo durante il turno, preavviso «Stai bene?» nell'app.

-- Parametri per profilo di protezione (l'immobilità vale solo per i lavoratori di un profilo che la attiva)
ALTER TABLE app.protection_profiles
  ADD COLUMN IF NOT EXISTS immobility_enabled    boolean NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS immobility_after_s    integer NOT NULL DEFAULT 300,
  ADD COLUMN IF NOT EXISTS immobility_prewarn_s  integer NOT NULL DEFAULT 60,
  ADD COLUMN IF NOT EXISTS immobility_min_move_m integer NOT NULL DEFAULT 15,
  ADD COLUMN IF NOT EXISTS immobility_shift_only boolean NOT NULL DEFAULT true;
DO $$ BEGIN
  ALTER TABLE app.protection_profiles ADD CONSTRAINT protection_profiles_immobility_chk CHECK (
    immobility_after_s BETWEEN 60 AND 7200
    AND immobility_prewarn_s BETWEEN 0 AND 600 AND immobility_prewarn_s < immobility_after_s
    AND immobility_min_move_m BETWEEN 2 AND 500);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

-- Stato di movimento per dispositivo: ultimo spostamento reale (oltre la soglia del profilo, o
-- sensore di movimento del badge) e punto di riferimento da cui misurarlo.
ALTER TABLE app.devices
  ADD COLUMN IF NOT EXISTS moved_at         timestamptz,
  ADD COLUMN IF NOT EXISTS move_anchor_lat  double precision,
  ADD COLUMN IF NOT EXISTS move_anchor_lon  double precision,
  -- valore di moved_at per cui è già stato mandato il preavviso / aperto l'allarme (una volta sola
  -- per periodo di immobilità: un nuovo movimento cambia moved_at e riarma il controllo)
  ADD COLUMN IF NOT EXISTS imm_prewarn_for  timestamptz,
  ADD COLUMN IF NOT EXISTS imm_alarm_for    timestamptz;
