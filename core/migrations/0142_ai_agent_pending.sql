-- IO-2 / IO-5 (25/11/2026): azioni sensibili proposte dall'agente AI salvate dal SERVER.
-- Prima la conferma eseguiva qualunque azione sensibile (nome + argomenti) inviata dal browser:
-- bastava una richiesta costruita a mano per far eseguire all'«agente» ciò che non aveva proposto.
-- Ora l'agente salva la proposta qui; la conferma indica solo l'id e vale una volta sola, per
-- lo stesso utente, entro 30 minuti.
CREATE TABLE IF NOT EXISTS app.ai_agent_pending (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
  user_id    uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
  name       text NOT NULL,
  args       jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at timestamptz NOT NULL DEFAULT now(),
  used_at    timestamptz
);
CREATE INDEX IF NOT EXISTS ai_agent_pending_user ON app.ai_agent_pending(tenant_id, user_id, created_at DESC);
ALTER TABLE app.ai_agent_pending ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.ai_agent_pending
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.ai_agent_pending TO app_rt;
