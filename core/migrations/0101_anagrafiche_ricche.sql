-- 0101 — Anagrafiche più ricche (R1). Additiva.
-- Cliente: dati per fatturazione elettronica IT + termini di pagamento.
ALTER TABLE app.clients ADD COLUMN IF NOT EXISTS pec           text;
ALTER TABLE app.clients ADD COLUMN IF NOT EXISTS sdi           text;   -- codice destinatario SDI
ALTER TABLE app.clients ADD COLUMN IF NOT EXISTS payment_terms text;   -- es. "30 gg data fattura"
-- Commessa: legame al Cliente vero (oltre al testo libero) + codici appalto pubblico.
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS client_id uuid REFERENCES app.clients(id) ON DELETE SET NULL;
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS cig text;   -- Codice Identificativo Gara
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS cup text;   -- Codice Unico Progetto
CREATE INDEX IF NOT EXISTS commesse_client ON app.commesse(client_id) WHERE client_id IS NOT NULL;
