-- #4 escalation engine nativo — FASE 1 (shadow). Log del confronto on-call nativo vs GoAlert,
-- per gruppo, senza consegnare nulla. Obiettivo: 0 divergenze prima di qualsiasi cutover.
CREATE TABLE IF NOT EXISTS app.esc_shadow_log (
    id             bigserial PRIMARY KEY,
    tenant_id      uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id       uuid,
    at             timestamptz NOT NULL DEFAULT now(),
    native_oncall  text[] NOT NULL DEFAULT '{}',   -- email calcolate dal motore nativo
    goalert_oncall text[] NOT NULL DEFAULT '{}',   -- email on-call correnti in GoAlert
    match          boolean NOT NULL,
    detail         jsonb
);
CREATE INDEX IF NOT EXISTS esc_shadow_log_tenant_at ON app.esc_shadow_log(tenant_id, at DESC);
