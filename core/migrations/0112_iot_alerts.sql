-- 0112 — IoT Fase 3: alert IoT in VISTA SEPARATA (non "Centrale" sicurezza-lavoratore).
-- Gli alert di processo IoT (soglia superata, device offline, ripristino) hanno un ciclo di vita
-- PROPRIO, distinto dagli incidenti di sicurezza (SOS/man_down/escalation GoAlert). Un tenant
-- puramente IoT li vede e li gestisce senza toccare il mondo sicurezza. In coesistenza col modulo
-- Lone Worker, la soglia alza ANCHE un incidente (bridge gestito in codice), ma l'alert IoT resta.

CREATE TABLE IF NOT EXISTS app.iot_alerts (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    device_id   uuid NOT NULL REFERENCES app.devices(id) ON DELETE CASCADE,
    kind        text NOT NULL,                    -- soglia | offline | ripristino
    severity    text NOT NULL DEFAULT 'warning',  -- info | warning | critical
    state       text NOT NULL DEFAULT 'aperto',   -- aperto | preso_in_carico | chiuso
    metric      text,                             -- metrica canonica (per kind=soglia)
    last_value  double precision,                 -- ultimo valore osservato
    threshold   double precision,                 -- soglia violata (per kind=soglia)
    rule_etype  text,                             -- classificazione d'origine (regola sensori)
    message     text NOT NULL,                    -- testo leggibile
    occurrences int  NOT NULL DEFAULT 1,          -- quante volte ri-osservato mentre aperto (anti-spam)
    opened_at   timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    ack_at      timestamptz, ack_by text,
    closed_at   timestamptz, closed_by text
);
CREATE INDEX IF NOT EXISTS iot_alerts_tenant_state_idx ON app.iot_alerts(tenant_id, state, opened_at DESC);
CREATE INDEX IF NOT EXISTS iot_alerts_device_idx ON app.iot_alerts(device_id, kind, state);

-- Watchdog offline: ultimo istante di telemetria (orologio controllabile, coerente col watchdog)
-- e soglia di silenzio per-device (secondi; 0/NULL = watchdog disattivo per quel device).
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS last_telemetry_at timestamptz;
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS offline_after_s   int;

-- Blueprint: soglia di offline di default (il device attivato "per un lavoro" sa quando è muto).
ALTER TABLE app.iot_blueprints ADD COLUMN IF NOT EXISTS offline_after_s int;
UPDATE app.iot_blueprints SET offline_after_s=900  WHERE code IN ('cold-chain','air-quality','tank-level') AND offline_after_s IS NULL;
UPDATE app.iot_blueprints SET offline_after_s=3600 WHERE code IN ('energy-meter','env-lorawan')            AND offline_after_s IS NULL;

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.iot_alerts TO app_rt';
  EXECUTE 'ALTER TABLE app.iot_alerts ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.iot_alerts USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
