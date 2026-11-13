-- Fase 3 — Ruoli personalizzati per tenant. Un ruolo custom = bundle NOMINATO di capability,
-- assegnabile a utenti e/o ai gruppi di persone condivisi (Fase 1). SOLO additivo: aggiunge
-- capability sopra il ruolo base, non le toglie. Additivo e non distruttivo: senza assegnazioni
-- il comportamento resta identico (ruoli fissi + capability layer Fase 2).
CREATE TABLE IF NOT EXISTS app.custom_roles (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name        text NOT NULL,
    description text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE TABLE IF NOT EXISTS app.custom_role_capabilities (
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    role_id    uuid NOT NULL REFERENCES app.custom_roles(id) ON DELETE CASCADE,
    capability text NOT NULL,
    PRIMARY KEY (role_id, capability)
);
CREATE TABLE IF NOT EXISTS app.user_custom_roles (
    tenant_id uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id   uuid NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    role_id   uuid NOT NULL REFERENCES app.custom_roles(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, role_id)
);
CREATE TABLE IF NOT EXISTS app.user_group_custom_roles (
    tenant_id uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id  uuid NOT NULL REFERENCES app.user_groups(id) ON DELETE CASCADE,
    role_id   uuid NOT NULL REFERENCES app.custom_roles(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, role_id)
);
CREATE INDEX IF NOT EXISTS user_custom_roles_u_idx ON app.user_custom_roles(tenant_id, user_id);
CREATE INDEX IF NOT EXISTS ugcr_g_idx ON app.user_group_custom_roles(tenant_id, group_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON app.custom_roles             TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.custom_role_capabilities TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.user_custom_roles        TO app_rt;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.user_group_custom_roles  TO app_rt;

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['custom_roles','custom_role_capabilities','user_custom_roles','user_group_custom_roles'] LOOP
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
