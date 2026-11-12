-- Worker Safety Fase A — identità per-dispositivo delle credenziali di ingest.
-- Oggi il token è per-tenant+source (condiviso). Qui si aggiunge:
--  * device_id  : lega (opzionale) il token a UN dispositivo → il fix/evento è attribuito a quel device
--  * token_hash : hash a riposo (sha256 hex, calcolato in Rust) per i NUOVI token; i legacy restano in
--                 chiaro nella colonna token finché non vengono ruotati
--  * label      : etichetta leggibile
--  * expires_at / revoked_at / last_used_at : ciclo di vita (scadenza, revoca, ultimo uso)
SET search_path TO app, public;

ALTER TABLE app.ingest_tokens
  ADD COLUMN IF NOT EXISTS device_id    uuid REFERENCES app.devices(id) ON DELETE CASCADE,
  ADD COLUMN IF NOT EXISTS token_hash   text,
  ADD COLUMN IF NOT EXISTS label        text,
  ADD COLUMN IF NOT EXISTS expires_at   timestamptz,
  ADD COLUMN IF NOT EXISTS revoked_at   timestamptz,
  ADD COLUMN IF NOT EXISTS last_used_at timestamptz;

CREATE INDEX IF NOT EXISTS ingest_tokens_hash_idx   ON app.ingest_tokens(token_hash);
CREATE INDEX IF NOT EXISTS ingest_tokens_device_idx ON app.ingest_tokens(device_id);
