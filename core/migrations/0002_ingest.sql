-- Machine ingress credentials: tenant is derived from the credential, never the payload.
SET search_path TO app, public;

CREATE TABLE ingest_tokens (
  token       text PRIMARY KEY,
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  source      text NOT NULL,          -- traccar|iot|blues
  created_at  timestamptz NOT NULL DEFAULT now()
);
-- Lookup is done by the API on the owner connection before setting tenant context.
GRANT SELECT ON app.ingest_tokens TO app_rt;
