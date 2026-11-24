-- 0134 — Integrità delle punzonature di vigilanza (CA-2, CA-3).
-- Prima: il codice del checkpoint arrivava all'app della guardia e si poteva digitare a mano, ma
-- veniva registrato come «qr»; una punzonatura fallita sovrascriveva quella riuscita.
-- Ora: il codice resta al pianificatore, ogni punzonatura dice COME è stata fatta (scansione,
-- tag NFC, inserimento manuale, automatica da dispositivo), i tentativi falliti restano registrati
-- senza declassare un esito positivo, e la distanza dal punto (se nota) è conservata come prova.

ALTER TABLE app.patrol_punches
  ADD COLUMN IF NOT EXISTS entry            text,            -- scansione | nfc | manuale | gps | auto
  ADD COLUMN IF NOT EXISTS attempts         integer NOT NULL DEFAULT 1,
  ADD COLUMN IF NOT EXISTS last_fail_at     timestamptz,
  ADD COLUMN IF NOT EXISTS last_fail_reason text,
  ADD COLUMN IF NOT EXISTS distance_m       integer;

-- Per giro: chi configura decide se la guardia può digitare il codice quando la scansione non
-- funziona. Default true (compatibile con i giri esistenti); l'inserimento manuale resta comunque
-- registrato come tale e segnalato come anomalia a fine ronda.
ALTER TABLE app.patrol_routes
  ADD COLUMN IF NOT EXISTS manual_code_allowed boolean NOT NULL DEFAULT true;
