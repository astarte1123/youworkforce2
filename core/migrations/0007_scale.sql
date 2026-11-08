-- Scalabilità multi-istanza: throttle login su store condiviso (DB), non in memoria.
-- (Gli eventi live SSE usano invece Postgres LISTEN/NOTIFY, senza tabella.)
SET search_path TO app, public;

CREATE TABLE login_throttle (
  key           text PRIMARY KEY,
  count         int  NOT NULL DEFAULT 0,
  reset_at      timestamptz NOT NULL,
  blocked_until timestamptz,
  updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX login_throttle_blocked ON login_throttle(blocked_until);
