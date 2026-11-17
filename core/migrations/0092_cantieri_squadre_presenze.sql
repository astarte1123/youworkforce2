-- 0092 — Cantieri C1: squadre assegnate alla commessa + presenze (timbrature). Additiva.
-- Riuso le squadre del core (app.teams/team_members); qui solo il legame commessa↔squadra
-- e le timbrature di presenza in cantiere.
CREATE TABLE IF NOT EXISTS app.commessa_teams (
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    team_id     uuid NOT NULL REFERENCES app.teams(id) ON DELETE CASCADE,
    PRIMARY KEY (commessa_id, team_id)
);
CREATE TABLE IF NOT EXISTS app.commessa_presenze (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    clock_in    timestamptz NOT NULL DEFAULT now(),
    clock_out   timestamptz,
    lat         double precision,
    lon         double precision,
    source      text NOT NULL DEFAULT 'manuale',  -- manuale | gps | pwa
    note        text
);
CREATE INDEX IF NOT EXISTS commessa_teams_idx ON app.commessa_teams(tenant_id, commessa_id);
CREATE INDEX IF NOT EXISTS commessa_presenze_idx ON app.commessa_presenze(tenant_id, commessa_id, clock_in);
CREATE INDEX IF NOT EXISTS commessa_presenze_open ON app.commessa_presenze(commessa_id, user_id) WHERE clock_out IS NULL;

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['commessa_teams','commessa_presenze'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
