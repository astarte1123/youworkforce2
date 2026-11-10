-- Motore di regole sensori PARAMETRICO, configurabile da YouWorkforce: sostituisce
-- la soglia temperatura cablata. Ogni regola: metrica (chiave nel payload telemetria)
-- + operatore + soglia -> etype (es. sensore_critico / batteria_critica). Gli etype
-- critici entrano nella catena allarmi esistente (incidente + GoAlert + routing).
-- Si applica sia a ThingsBoard sia all'IoT generico.
CREATE TABLE IF NOT EXISTS app.sensor_rules (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    metric     text NOT NULL,                 -- es. temperature, gas, co, smoke, door, motion, vibration, battery, water, humidity
    op         text NOT NULL,                 -- '>=','<=','>','<','==','is_true','is_false'
    threshold  double precision,              -- per operatori numerici
    etype      text NOT NULL,                 -- es. sensore_critico, batteria_critica, telemetria
    label      text,
    enabled    boolean NOT NULL DEFAULT true,
    sort       int NOT NULL DEFAULT 100,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sensor_rules_tenant ON app.sensor_rules(tenant_id, enabled, sort);
ALTER TABLE app.sensor_rules ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.sensor_rules
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
