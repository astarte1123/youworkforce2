-- ThingsBoard: completamento del percorso PULL (spina dorsale come Traccar), oltre al
-- push via Rule Engine. YouWorkforce fa login JWT, specchia i device nel proprio
-- inventario e recupera l'ultima telemetria, applicando il motore regole sensori per
-- alzare incidenti. Tutto configurato da YouWorkforce (superadmin).
ALTER TABLE app.thingsboard_connections ADD COLUMN IF NOT EXISTS poll_enabled boolean NOT NULL DEFAULT false;
ALTER TABLE app.thingsboard_connections ADD COLUMN IF NOT EXISTS poll_interval_s int NOT NULL DEFAULT 60;

-- Stato salute del poller TB (system-scope, letto da operatore/admin/auditor).
CREATE TABLE IF NOT EXISTS app.thingsboard_poll_state (
    tenant_id    uuid PRIMARY KEY REFERENCES app.tenants(id) ON DELETE CASCADE,
    last_poll_at timestamptz,
    last_ok_at   timestamptz,
    reachable    boolean,
    last_error   text,
    device_count int,
    alarm_count  int,
    updated_at   timestamptz NOT NULL DEFAULT now()
);
