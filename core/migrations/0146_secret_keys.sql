-- Chiave dei segreti gestita da YouWorkforce (decisione PO 25/11/2026): niente più chiave in un file
-- di testo / variabile d'ambiente. La chiave che cifra i segreti (chiavi dei provider AI) è nel DB,
-- ma MAI in chiaro:
--  - sealed_server: sigillata dal server su cui gira il core (Windows: DPAPI di macchina; Linux:
--    chiave di macchina generata dal core, file protetto). Un dump del DB da solo non basta.
--  - recovery_*: sigillata con una passphrase di recupero scelta dal superadmin (Argon2id), per
--    sbloccare la chiave su un altro server (ripristino, migrazione).
-- Una sola chiave attiva; alla rotazione i segreti vengono ricifrati e la vecchia chiave eliminata.
CREATE TABLE IF NOT EXISTS app.secret_keys (
  key_id           integer PRIMARY KEY,
  sealed_server    bytea,
  server_name      text,
  recovery_salt    bytea,
  recovery_wrapped bytea,
  active           boolean NOT NULL DEFAULT true,
  created_at       timestamptz NOT NULL DEFAULT now(),
  created_by       text NOT NULL DEFAULT 'sistema',
  recovery_set_at  timestamptz
);
CREATE UNIQUE INDEX IF NOT EXISTS secret_keys_one_active ON app.secret_keys(active) WHERE active;
REVOKE ALL ON app.secret_keys FROM PUBLIC;
