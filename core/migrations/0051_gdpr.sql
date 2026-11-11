-- #7 GDPR minimo: anonimizzazione utente (diritto all'oblio preservando l'integrità
-- storica degli incidenti) + ritenzione dati. Niente hard-delete dell'utente: si scrubba
-- la PII e si marca anonymized_at.
ALTER TABLE app.users ADD COLUMN IF NOT EXISTS anonymized_at timestamptz;
