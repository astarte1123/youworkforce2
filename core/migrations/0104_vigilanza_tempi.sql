-- 0104 — Ronde/Vigilanza V1: tempi per tratta + anomalie. Additiva. Modulo "giri".
-- Il modulo giri diventa il motore anche della vigilanza (guardie giurate): oltre ai checkpoint
-- QR/NFC/beacon/GPS, ora ogni tappa ha tempi attesi/massimi; alla chiusura del giro si rilevano
-- le anomalie (tappa saltata, tratta in ritardo, giro non completato).
ALTER TABLE app.patrol_checkpoints ADD COLUMN IF NOT EXISTS expected_min  integer; -- tempo atteso dalla tappa precedente
ALTER TABLE app.patrol_checkpoints ADD COLUMN IF NOT EXISTS max_min       integer; -- tempo MAX dalla precedente (oltre = ritardo)
ALTER TABLE app.patrol_checkpoints ADD COLUMN IF NOT EXISTS dwell_max_min integer; -- permanenza max in zona (uso futuro)
ALTER TABLE app.patrol_routes ADD COLUMN IF NOT EXISTS mode      text NOT NULL DEFAULT 'piedi'; -- piedi | mezzo
ALTER TABLE app.patrol_routes ADD COLUMN IF NOT EXISTS grace_min integer NOT NULL DEFAULT 0;     -- tolleranza globale

CREATE TABLE IF NOT EXISTS app.patrol_anomalies (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    round_id      uuid NOT NULL REFERENCES app.patrol_rounds(id) ON DELETE CASCADE,
    checkpoint_id uuid REFERENCES app.patrol_checkpoints(id) ON DELETE SET NULL,
    kind          text NOT NULL,            -- saltato | in_ritardo | non_completato | fuori_sequenza | permanenza
    detail        text,
    incident_id   uuid,                     -- se Lone Worker attivo, incidente collegato (fase successiva)
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS patrol_anomalies_idx ON app.patrol_anomalies(tenant_id, round_id);

-- il modulo giri copre ora anche la vigilanza
UPDATE app.modules_catalog SET name='Ronde e vigilanza' WHERE code='giri';

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.patrol_anomalies TO app_rt';
  EXECUTE 'ALTER TABLE app.patrol_anomalies ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.patrol_anomalies USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
