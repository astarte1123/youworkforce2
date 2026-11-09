-- ============================================================================
-- Traccar — watchdog della CONNESSIONE (sicurezza): se Traccar stesso e'
-- irraggiungibile oltre una soglia, il sistema di sicurezza e' cieco -> va
-- alzato un incidente operativo + escalation, con recovery automatico.
-- Inoltre: soglia configurabile per il down della connessione.
-- ============================================================================

ALTER TABLE traccar_connections
  ADD COLUMN IF NOT EXISTS conn_down_after_s integer NOT NULL DEFAULT 180;  -- grace prima dell'allarme connessione

-- stato dell'episodio di down nella poll_state (per dedup + recovery)
ALTER TABLE traccar_poll_state
  ADD COLUMN IF NOT EXISTS down_since         timestamptz,
  ADD COLUMN IF NOT EXISTS conn_incident_open boolean NOT NULL DEFAULT false,
  ADD COLUMN IF NOT EXISTS conn_incident_corr text;
