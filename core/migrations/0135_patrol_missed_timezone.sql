-- 0135 — Ronde mancate come incidente (CA-12) + orari nel fuso locale (CA-16).
-- Decisioni PO (24/11/2026):
--  - una ronda pianificata non svolta apre un incidente «Ronda mancata» con escalation normale
--    (non salva-vita: la manutenzione lo sospende);
--  - il modulo Ronde porta con sé Centrale ed escalation anche senza Sicurezza Lavoratore;
--  - gli orari delle ronde sono nel fuso del tenant (default Europe/Rome), che la sede può cambiare.

ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS timezone text NOT NULL DEFAULT 'Europe/Rome';
ALTER TABLE app.sites   ADD COLUMN IF NOT EXISTS timezone text;   -- NULL = come il tenant

-- Una riga per orario di ronda mancato: la presa dello slot è atomica (UNIQUE), quindi un solo
-- incidente per slot anche con più istanze del worker o riavvii.
CREATE TABLE IF NOT EXISTS app.patrol_missed (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    schedule_id  uuid NOT NULL REFERENCES app.patrol_schedules(id) ON DELETE CASCADE,
    route_id     uuid NOT NULL REFERENCES app.patrol_routes(id) ON DELETE CASCADE,
    slot_at      timestamptz NOT NULL,          -- orario previsto della ronda
    deadline_at  timestamptz NOT NULL,          -- oltre questo orario la ronda è mancata
    incident_id  uuid REFERENCES app.incidents(id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (schedule_id, slot_at)
);
CREATE INDEX IF NOT EXISTS patrol_missed_tenant_idx ON app.patrol_missed(tenant_id, slot_at);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.patrol_missed TO app_rt';
  EXECUTE 'ALTER TABLE app.patrol_missed ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.patrol_missed USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
