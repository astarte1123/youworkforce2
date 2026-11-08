-- Hardening: sessioni persistenti (sopravvivono al riavvio del core) con scadenza.
-- Tabella di sistema (solo owner_pool, nessun RLS): il token è opaco; il tenant è una colonna.
SET search_path TO app, public;

CREATE TABLE sessions (
  token       text PRIMARY KEY,
  user_id     uuid NOT NULL,
  tenant_id   uuid,
  email       text NOT NULL,
  role        text NOT NULL,
  is_super    boolean NOT NULL DEFAULT false,
  created_at  timestamptz NOT NULL DEFAULT now(),
  expires_at  timestamptz NOT NULL
);
CREATE INDEX sessions_expires ON sessions(expires_at);
