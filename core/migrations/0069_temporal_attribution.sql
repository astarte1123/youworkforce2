-- Gap #1 — Attribuzione temporale degli eventi.
-- Un evento tardivo (es. SOS offline rigiocato alla riconnessione) deve essere attribuito al
-- lavoratore valido ALL'ORARIO dell'evento, non all'assegnatario corrente del device.
-- Per farlo serve lo STORICO delle assegnazioni device->worker con validità temporale, e uno
-- SNAPSHOT del lavoratore sull'incidente (+ marcatura di incertezza quando l'orario non è
-- affidabile o l'assegnazione è ambigua).

CREATE TABLE IF NOT EXISTS app.device_assignments (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    device_id     uuid NOT NULL REFERENCES app.devices(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    assigned_at   timestamptz NOT NULL DEFAULT now(),
    unassigned_at timestamptz
);
CREATE INDEX IF NOT EXISTS device_assignments_dev_idx ON app.device_assignments(tenant_id, device_id, assigned_at);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.device_assignments TO app_rt;

ALTER TABLE app.device_assignments ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.device_assignments
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

-- Backfill: le assegnazioni correnti diventano storia "aperta" (idempotente).
INSERT INTO app.device_assignments(tenant_id, device_id, user_id, assigned_at)
  SELECT d.tenant_id, d.id, d.assigned_user, COALESCE(d.updated_at, now())
  FROM app.devices d
  WHERE d.assigned_user IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM app.device_assignments da WHERE da.device_id = d.id AND da.unassigned_at IS NULL);

-- Incidenti: lavoratore snapshot all'evento + incertezza di attribuzione.
ALTER TABLE app.incidents ADD COLUMN IF NOT EXISTS worker_user_id uuid REFERENCES app.users(id) ON DELETE SET NULL;
ALTER TABLE app.incidents ADD COLUMN IF NOT EXISTS attribution_uncertain boolean NOT NULL DEFAULT false;
ALTER TABLE app.incidents ADD COLUMN IF NOT EXISTS attribution_note text;
