-- ============================================================================
-- Fase 1 — Lone Worker + Dispositivi: superfici di gestione (anagrafiche + ops)
--   * devices: anagrafica completa (etichetta, abilitazione, config, aggiornamento)
--   * alarm_groups + members + routing: gruppi di allarme ed escalation per tenant
--   * incidents.assigned_user: assegnazione operatore in centrale
-- Note/annotazioni centrale riusano incident_timeline (kind='nota').
-- ============================================================================

-- --- Dispositivi: arricchimento anagrafico ---
ALTER TABLE devices ADD COLUMN IF NOT EXISTS label      text;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS enabled    boolean NOT NULL DEFAULT true;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS config     jsonb   NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE devices ADD COLUMN IF NOT EXISTS updated_at timestamptz NOT NULL DEFAULT now();

-- --- Centrale: assegnazione operatore ---
ALTER TABLE incidents ADD COLUMN IF NOT EXISTS assigned_user uuid REFERENCES users(id);

-- --- Gruppi di allarme (destinatari/escalation) ---
CREATE TABLE alarm_groups (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name        text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, name)
);

CREATE TABLE alarm_group_members (
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  group_id    uuid NOT NULL REFERENCES alarm_groups(id) ON DELETE CASCADE,
  user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  position    int  NOT NULL DEFAULT 1,   -- ordine di escalation
  PRIMARY KEY (group_id, user_id)
);

-- Routing: quale gruppo gestisce un dato tipo evento (per tenant).
CREATE TABLE alarm_routing (
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  etype       text NOT NULL,
  group_id    uuid NOT NULL REFERENCES alarm_groups(id) ON DELETE CASCADE,
  PRIMARY KEY (tenant_id, etype)
);

ALTER TABLE alarm_groups        ENABLE ROW LEVEL SECURITY;
ALTER TABLE alarm_group_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE alarm_routing       ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['alarm_groups','alarm_group_members','alarm_routing']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;
