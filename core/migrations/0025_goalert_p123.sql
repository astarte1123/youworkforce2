-- ============================================================================
-- GoAlert P1+P2+P3:
--  P1 close-loop: incidents.goalert_alert_id (AlertID restituito dalla create)
--  P2 bidirezionale/on-call: goalert_services + credenziali GraphQL + service_id
--  P3 provisioning/heartbeat/routing: heartbeat_url + routing per tipo evento
-- Credenziali in chiaro per la demo -> in produzione vault / API key GoAlert.
-- ============================================================================
ALTER TABLE goalert_services ADD COLUMN IF NOT EXISTS service_id    text;
ALTER TABLE goalert_services ADD COLUMN IF NOT EXISTS gql_user      text;
ALTER TABLE goalert_services ADD COLUMN IF NOT EXISTS gql_pass      text;
ALTER TABLE goalert_services ADD COLUMN IF NOT EXISTS heartbeat_url text;

ALTER TABLE incidents ADD COLUMN IF NOT EXISTS goalert_alert_id int;

-- Routing per tipo evento -> service/key GoAlert diversi (opzionale).
CREATE TABLE goalert_routing (
  tenant_id       uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  etype           text NOT NULL,
  integration_key text NOT NULL,
  service_id      text,
  PRIMARY KEY (tenant_id, etype)
);
