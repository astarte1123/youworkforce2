-- Watchdog infrastrutturale (superadmin): storico alert di sistema + dedup/notifica.
-- Control-plane (owner_pool), nessun tenant: riguarda il server/DB/worker, non un tenant.
CREATE TABLE IF NOT EXISTS app.system_alerts (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind       text NOT NULL,            -- db | disk | mem | cpu | worker | db_conns | outbox
    severity   text NOT NULL,            -- warn | bad
    detail     text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    notified   boolean NOT NULL DEFAULT false,
    resolved_at timestamptz
);
CREATE INDEX IF NOT EXISTS ix_system_alerts_created ON app.system_alerts(created_at DESC);
CREATE INDEX IF NOT EXISTS ix_system_alerts_open ON app.system_alerts(kind) WHERE resolved_at IS NULL;
