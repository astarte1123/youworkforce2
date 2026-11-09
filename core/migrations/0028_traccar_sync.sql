-- ============================================================================
-- Traccar P4 — sync device (import + stato online/offline da Traccar) e log
-- comandi con esito tracciato. Il poller mantiene allineato il nostro inventario
-- a quello di Traccar; i comandi inviati al device sono storicizzati.
-- ============================================================================

ALTER TABLE devices
  ADD COLUMN IF NOT EXISTS traccar_status    text,    -- online | offline | unknown
  ADD COLUMN IF NOT EXISTS traccar_device_id bigint;  -- id numerico su Traccar

-- Storico comandi inviati al device (richiesta posizione, engineStop, ...).
CREATE TABLE IF NOT EXISTS device_commands (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  device_id     uuid NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  ctype         text NOT NULL,          -- tipo comando Traccar
  actor         text NOT NULL,          -- email operatore
  traccar_status integer,               -- HTTP status dalla send Traccar
  result        text,                   -- esito sintetico / errore
  created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS device_commands_dev_idx ON device_commands(device_id, created_at DESC);
