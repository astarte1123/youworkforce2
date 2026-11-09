-- ============================================================================
-- Per-module tool configuration (schema-driven).
-- Two levels:
--   * superadmin decides WHICH tools a tenant may use (tenant_tool_enabled)
--   * tenant_admin sets the VALUES for enabled tools  (tenant_module_config)
-- Catalog (module_tools) is product-defined, global, with a JSON schema that
-- drives both UI rendering and server-side validation.
-- ============================================================================

-- Catalog of configurable "tools" each module exposes. Global, no RLS.
CREATE TABLE module_tools (
  module_code    text NOT NULL REFERENCES modules_catalog(code),
  tool_code      text NOT NULL,
  name           text NOT NULL,
  description    text NOT NULL DEFAULT '',
  config_schema  jsonb NOT NULL DEFAULT '{}'::jsonb,   -- {"fields":[{key,label,type,min,max,options}]}
  default_config jsonb NOT NULL DEFAULT '{}'::jsonb,
  sort           int  NOT NULL DEFAULT 0,
  PRIMARY KEY (module_code, tool_code)
);
GRANT SELECT ON app.module_tools TO app_rt;

-- Superadmin scope: a tool is AVAILABLE to a tenant unless a row says enabled=false.
-- (absence of a row => available; superadmin acts only to restrict)
CREATE TABLE tenant_tool_enabled (
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  module_code text NOT NULL,
  tool_code   text NOT NULL,
  enabled     boolean NOT NULL DEFAULT true,
  updated_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, module_code, tool_code),
  FOREIGN KEY (module_code, tool_code) REFERENCES module_tools(module_code, tool_code)
);

-- Tenant scope: the values tenant_admin sets for enabled tools.
CREATE TABLE tenant_module_config (
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  module_code text NOT NULL,
  tool_code   text NOT NULL,
  config      jsonb NOT NULL DEFAULT '{}'::jsonb,
  updated_by  uuid,
  updated_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, module_code, tool_code),
  FOREIGN KEY (module_code, tool_code) REFERENCES module_tools(module_code, tool_code)
);

ALTER TABLE tenant_tool_enabled  ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_module_config ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['tenant_tool_enabled','tenant_module_config']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;

-- Seed the tool catalog. Each tool carries a schema (for UI + validation) and
-- safe defaults. Adding a module tool later is data, not a deploy.
INSERT INTO module_tools(module_code,tool_code,name,description,config_schema,default_config,sort) VALUES
  ('loneworker','checkin_timer','Timer check-in',
     'Intervallo predefinito e massimo del check-in periodico.',
     '{"fields":[{"key":"default_minutes","label":"Minuti predefiniti","type":"int","min":1,"max":720},{"key":"max_minutes","label":"Minuti massimi","type":"int","min":1,"max":1440}]}',
     '{"default_minutes":30,"max_minutes":120}', 10),
  ('loneworker','sos_escalation','Escalation SOS',
     'Priorita e ripetizione della notifica SOS.',
     '{"fields":[{"key":"priority","label":"Priorita","type":"enum","options":["alta","media"]},{"key":"repeat_seconds","label":"Ripeti ogni (s)","type":"int","min":0,"max":3600}]}',
     '{"priority":"alta","repeat_seconds":0}', 20),
  ('thingsboard','severity_gate','Soglia severita IoT',
     'Soglia temperatura oltre la quale apre un incidente.',
     '{"fields":[{"key":"temperature_max","label":"Temp. max (C)","type":"int","min":0,"max":300}]}',
     '{"temperature_max":90}', 10),
  ('traccar','alarm_map','Mappatura allarmi badge',
     'Come mappare l''allarme sos del badge.',
     '{"fields":[{"key":"sos_as","label":"Mappa sos -> ","type":"enum","options":["sos","man_down"]}]}',
     '{"sos_as":"sos"}', 10),
  ('consuntivi','rates','Arrotondamenti e valuta',
     'Arrotondamento minuti e valuta del consuntivo.',
     '{"fields":[{"key":"round_minutes","label":"Arrotonda a (min)","type":"int","min":1,"max":60},{"key":"currency","label":"Valuta","type":"enum","options":["EUR","USD"]}]}',
     '{"round_minutes":15,"currency":"EUR"}', 10),
  ('workforce','shift','Turni',
     'Durata massima del turno.',
     '{"fields":[{"key":"max_hours","label":"Ore max turno","type":"int","min":1,"max":24}]}',
     '{"max_hours":12}', 10);
