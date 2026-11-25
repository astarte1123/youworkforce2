-- SA-12 (decisione PO 25/11/2026): account superadmin gestiti dalla UI con MFA (TOTP) obbligatorio.
-- Le variabili d'ambiente servono solo a creare il primo account all'installazione.
ALTER TABLE app.superadmins ADD COLUMN IF NOT EXISTS disabled boolean NOT NULL DEFAULT false;
ALTER TABLE app.superadmins ADD COLUMN IF NOT EXISTS last_login_at timestamptz;
ALTER TABLE app.superadmins ADD COLUMN IF NOT EXISTS created_by text;
-- segreto TOTP cifrato con la chiave dei segreti (enc:v3); pending = in attivazione, non ancora confermato
ALTER TABLE app.superadmins ADD COLUMN IF NOT EXISTS totp_secret text;
ALTER TABLE app.superadmins ADD COLUMN IF NOT EXISTS totp_pending text;
ALTER TABLE app.superadmins ADD COLUMN IF NOT EXISTS totp_enabled_at timestamptz;
-- ultimo intervallo di 30 s usato: lo stesso codice non vale due volte (anti-replay)
ALTER TABLE app.superadmins ADD COLUMN IF NOT EXISTS totp_last_step bigint;
