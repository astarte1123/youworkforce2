-- Prompt 5: per-tenant GoAlert routing. Integration key is per tenant (tenant-aware),
-- no client access to GoAlert's internal console. Set by superadmin (control plane).
SET search_path TO app, public;

CREATE TABLE goalert_services (
  tenant_id       uuid PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
  integration_key text NOT NULL,
  updated_at      timestamptz NOT NULL DEFAULT now()
);
-- Read by the system worker on the owner connection; no app_rt access needed.
