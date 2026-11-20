-- Onboarding sicuro dell'admin del tenant: invito monouso invece di una password nota al superadmin.
-- Alla creazione del tenant (senza password) si genera un token; l'admin apre il link e imposta la
-- PROPRIA password. Il token è salvato solo come hash (sha256), monouso e con scadenza.
-- Control-plane (owner_pool): nessuna RLS, accesso solo dal core superadmin / endpoint invito pubblico.
CREATE TABLE IF NOT EXISTS app.user_invites (
    token_hash text PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES app.users(id)   ON DELETE CASCADE,
    email      text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    used_at    timestamptz
);
CREATE INDEX IF NOT EXISTS ix_user_invites_tenant ON app.user_invites(tenant_id);
