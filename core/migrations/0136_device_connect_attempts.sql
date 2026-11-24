-- Badge che tentano di collegarsi ma vengono rifiutati (24-25/11/2026).
-- Il bridge Teltonika rifiuta in silenzio un IMEI sconosciuto, conteso tra tenant o di un tenant
-- senza modulo dispositivi: il badge riprova all'infinito e nessuno lo sa. Qui si registra il
-- tentativo (uno per identità, con contatore) così il superadmin vede chi chiama e perché, e
-- l'admin del tenant vede se il SUO badge censito è rifiutato.
-- Tabella di sistema, senza tenant: la legge solo il superadmin (owner_pool), mai app_rt.
-- Righe: al massimo ~2000 (limite applicato dall'inserimento), ripulite dopo 30 giorni dal worker;
-- la riga di un IMEI sparisce quando quell'IMEI viene accettato.
CREATE TABLE IF NOT EXISTS app.device_connect_attempts (
  protocol  text        NOT NULL,              -- 'teltonika'
  identity  text        NOT NULL,              -- IMEI dichiarato dal dispositivo (troncato a 32)
  reason    text        NOT NULL,              -- sconosciuto | conteso | modulo_spento
  peer      text,                              -- indirizzo IP:porta dell'ultimo tentativo
  first_at  timestamptz NOT NULL,
  last_at   timestamptz NOT NULL,
  attempts  integer     NOT NULL DEFAULT 1,
  PRIMARY KEY (protocol, identity),
  CONSTRAINT device_connect_attempts_reason_chk CHECK (reason IN ('sconosciuto','conteso','modulo_spento'))
);
CREATE INDEX IF NOT EXISTS device_connect_attempts_last_idx ON app.device_connect_attempts(last_at DESC);
REVOKE ALL ON app.device_connect_attempts FROM PUBLIC;
