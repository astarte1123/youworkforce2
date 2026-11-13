-- Campo Slice 2 — Ronde e punzonature (modulo "giri"). Indipendente da interventi/workforce/
-- consuntivi/loneworker. Additivo e non distruttivo. Checkpoint QR/NFC/beacon/GPS.
INSERT INTO app.modules_catalog(code,name,kind,available)
VALUES ('giri','Ronde e punzonature','product',true)
ON CONFLICT (code) DO NOTHING;
CREATE TABLE IF NOT EXISTS app.patrol_routes (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id      uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name           text NOT NULL,
    site_id        uuid REFERENCES app.sites(id) ON DELETE SET NULL,
    window_minutes integer,                 -- finestra max per completare il giro (opz.)
    active         boolean NOT NULL DEFAULT true,
    created_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE TABLE IF NOT EXISTS app.patrol_checkpoints (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    route_id    uuid NOT NULL REFERENCES app.patrol_routes(id) ON DELETE CASCADE,
    name        text NOT NULL,
    position    integer NOT NULL DEFAULT 1,
    kind        text NOT NULL,              -- qr | nfc | beacon | gps
    code        text,                       -- payload QR/NFC/beacon atteso
    lat         double precision,
    lon         double precision,
    radius_m    integer NOT NULL DEFAULT 50,-- tolleranza GPS/beacon (m)
    geofence_id uuid REFERENCES app.geofences(id) ON DELETE SET NULL
);
CREATE TABLE IF NOT EXISTS app.patrol_rounds (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    route_id   uuid NOT NULL REFERENCES app.patrol_routes(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    started_at timestamptz NOT NULL DEFAULT now(),
    ended_at   timestamptz,
    status     text NOT NULL DEFAULT 'in_corso'  -- in_corso | completato | incompleto
);
CREATE TABLE IF NOT EXISTS app.patrol_punches (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    round_id      uuid NOT NULL REFERENCES app.patrol_rounds(id) ON DELETE CASCADE,
    checkpoint_id uuid NOT NULL REFERENCES app.patrol_checkpoints(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    punched_at    timestamptz NOT NULL DEFAULT now(),
    method        text NOT NULL,            -- qr | nfc | beacon | gps | manual
    lat           double precision,
    lon           double precision,
    ok            boolean NOT NULL DEFAULT true,
    note          text,
    UNIQUE (round_id, checkpoint_id)
);
CREATE INDEX IF NOT EXISTS patrol_checkpoints_route_idx ON app.patrol_checkpoints(tenant_id, route_id, position);
CREATE INDEX IF NOT EXISTS patrol_rounds_route_idx ON app.patrol_rounds(tenant_id, route_id);
CREATE INDEX IF NOT EXISTS patrol_punches_round_idx ON app.patrol_punches(tenant_id, round_id);

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['patrol_routes','patrol_checkpoints','patrol_rounds','patrol_punches'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
