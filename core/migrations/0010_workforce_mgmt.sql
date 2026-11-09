-- ============================================================================
-- Fase 2 — Forza lavoro + turni:
--   * teams + team_members: squadre operative del tenant
--   * shift_plans: turni PIANIFICATI (distinti dai turni reali in `shifts`)
--   * oncall: finestre di reperibilita'
-- `shifts` resta il turno REALE timbrato dal lavoratore; qui si pianifica.
-- ============================================================================

CREATE TABLE teams (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name        text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, name)
);
CREATE TABLE team_members (
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  team_id     uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  PRIMARY KEY (team_id, user_id)
);

-- Turni pianificati (dal pianificatore/admin). Stato: pianificato|confermato|annullato.
CREATE TABLE shift_plans (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  site_id     uuid REFERENCES sites(id),
  starts_at   timestamptz NOT NULL,
  ends_at     timestamptz NOT NULL,
  note        text,
  state       text NOT NULL DEFAULT 'pianificato',
  created_by  uuid REFERENCES users(id),
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX shift_plans_window ON shift_plans(tenant_id, starts_at);

-- Reperibilita' (on-call): finestra temporale per un utente.
CREATE TABLE oncall (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  starts_at   timestamptz NOT NULL,
  ends_at     timestamptz NOT NULL,
  note        text,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX oncall_window ON oncall(tenant_id, starts_at);

ALTER TABLE teams        ENABLE ROW LEVEL SECURITY;
ALTER TABLE team_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE shift_plans  ENABLE ROW LEVEL SECURITY;
ALTER TABLE oncall       ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['teams','team_members','shift_plans','oncall']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;
