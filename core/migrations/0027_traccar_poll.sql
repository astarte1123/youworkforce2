-- ============================================================================
-- Traccar P1 — polling via API con cursore + watchdog del silenzio.
-- La spina dorsale diventa il PULL (teniamo noi le credenziali, interroghiamo noi):
--   * recupero eventi/posizioni persi se il core era giu' (backfill da cursore),
--   * rilevamento "silenzio" indipendente dal fatto che Traccar invii deviceOffline,
--   * stato salute della connessione visibile nel pannello.
-- Il push forwarding resta come fast-path a bassa latenza; i due convergono
-- idempotentemente (stesso idem_key evento / stesso source_id posizione).
-- ============================================================================

-- Impostazioni poll per-tenant, accanto alla connessione Traccar gia' esistente.
ALTER TABLE traccar_connections
  ADD COLUMN IF NOT EXISTS poll_enabled   boolean NOT NULL DEFAULT true,
  ADD COLUMN IF NOT EXISTS poll_interval_s integer NOT NULL DEFAULT 30,   -- cadenza poll
  ADD COLUMN IF NOT EXISTS silence_after_s integer NOT NULL DEFAULT 600;  -- soglia watchdog (0 = off)

-- Stato del poller + salute connessione per-tenant (gestito dal sistema, owner pool).
CREATE TABLE IF NOT EXISTS traccar_poll_state (
  tenant_id     uuid PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
  last_event_id bigint      NOT NULL DEFAULT 0,   -- cursore: max event id visto
  last_poll_at  timestamptz,                      -- ultimo tentativo
  last_ok_at    timestamptz,                      -- ultimo poll riuscito
  reachable     boolean     NOT NULL DEFAULT false,
  last_error    text,
  device_count  integer     NOT NULL DEFAULT 0,
  online_count  integer     NOT NULL DEFAULT 0,
  updated_at    timestamptz NOT NULL DEFAULT now()
);
