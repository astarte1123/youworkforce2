-- ============================================================================
-- #8 ACL per-sede: un operatore/lavoratore puo' essere ristretto a sedi specifiche.
-- Se un utente ha almeno una riga qui, vede SOLO dispositivi/incidenti di quelle sedi.
-- Nessuna riga = vede tutto il tenant (compatibilita').
-- ============================================================================
CREATE TABLE operator_sites (
  tenant_id uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  site_id   uuid NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  PRIMARY KEY (user_id, site_id)
);
ALTER TABLE operator_sites ENABLE ROW LEVEL SECURITY;
DO $$
BEGIN
  EXECUTE 'CREATE POLICY tenant_isolation ON app.operator_sites
     USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid)
     WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.operator_sites TO app_rt';
END$$;
