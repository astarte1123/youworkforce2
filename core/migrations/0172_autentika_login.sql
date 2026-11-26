-- Login con Autentika (IdP OpenID Connect di YouCo), fase A1 di docs/autentika-integrazione.md.
-- Affianca il login con email e password (entrambi attivi durante la prova). Per ogni tenant YW:
-- issuer del tenant Autentika, client confidenziale (segreto cifrato con la chiave dei segreti),
-- redirect registrato. YW resta il BFF: la sessione è quella di YW (app.sessions), legata al `sid`
-- di Autentika per il back-channel logout.

CREATE TABLE IF NOT EXISTS app.auth_providers (
    tenant_id     uuid PRIMARY KEY REFERENCES app.tenants(id) ON DELETE CASCADE,
    issuer        text NOT NULL CHECK (issuer ~ '^https?://[^/?#]+$'),
    client_id     text NOT NULL CHECK (length(btrim(client_id)) BETWEEN 1 AND 200),
    client_secret text NOT NULL,                       -- cifrato (st.secrets)
    redirect_uri  text NOT NULL CHECK (redirect_uri ~ '^https?://'),
    scope         text NOT NULL DEFAULT 'openid profile email groups',
    idp_hint      text,
    resource      text,                                -- risorsa (aud) per l'access token con i ruoli
    enabled       boolean NOT NULL DEFAULT false,
    jit           boolean NOT NULL DEFAULT true,       -- primo accesso: crea l'utente YW
    link_by_email boolean NOT NULL DEFAULT true,       -- primo accesso: collega un utente YW con la stessa email verificata
    default_role  text NOT NULL DEFAULT 'lavoratore',
    role_map      jsonb NOT NULL DEFAULT '{"admin":"tenant_admin","pianificatore":"pianificatore","responsabile":"responsabile","operatore":"operatore","reperibile":"reperibile","auditor":"auditor","lavoratore":"lavoratore"}',
    updated_at    timestamptz NOT NULL DEFAULT now()
);

-- login in corso: state, PKCE e nonce restano sul server (10 minuti)
CREATE TABLE IF NOT EXISTS app.oidc_pending (
    state         text PRIMARY KEY,
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    code_verifier text NOT NULL,
    nonce         text NOT NULL,
    return_to     text NOT NULL DEFAULT '/',
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS oidc_pending_created_idx ON app.oidc_pending(created_at);

-- l'utente YW collegato al soggetto di Autentika: chiave stabile (tenant, sub), mai l'email
ALTER TABLE app.users ADD COLUMN IF NOT EXISTS auth_sub text;
CREATE UNIQUE INDEX IF NOT EXISTS users_auth_sub_uq ON app.users(tenant_id, auth_sub) WHERE auth_sub IS NOT NULL;

-- sessione YW nata da Autentika: sid per il back-channel logout, id_token per il logout verso Autentika
ALTER TABLE app.sessions
    ADD COLUMN IF NOT EXISTS auth_sid text,
    ADD COLUMN IF NOT EXISTS auth_id_token text;
CREATE INDEX IF NOT EXISTS sessions_auth_sid_idx ON app.sessions(tenant_id, auth_sid) WHERE auth_sid IS NOT NULL;
