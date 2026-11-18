-- 0110 — IoT Fase 1: normalizzazione dati. Metriche canoniche + mappa per-device.
-- Obiettivo: i dati grezzi del device (chiavi/forme diverse per modello) arrivano NORMALIZZATI
-- (metrica canonica + unità) cosi' le regole/viste/dashboard parlano un linguaggio unico.
-- Additiva, retro-compatibile (map opzionale: senza map il valore resta grezzo).

-- Catalogo PRODOTTO delle metriche canoniche (globale, non per-tenant): chiave + etichetta IT + unità + tipo.
CREATE TABLE IF NOT EXISTS app.iot_metrics (
    metric    text PRIMARY KEY,                 -- chiave canonica (es. 'temperature')
    label     text NOT NULL,                    -- etichetta leggibile IT (es. 'Temperatura')
    unit      text NOT NULL DEFAULT '',         -- unità di misura (es. '°C'); '' = adimensionale/booleano
    kind      text NOT NULL DEFAULT 'num',      -- num | bool | str
    category  text NOT NULL DEFAULT 'generico', -- raggruppamento UI (ambiente/energia/processo/...)
    sort      int  NOT NULL DEFAULT 100
);

INSERT INTO app.iot_metrics(metric,label,unit,kind,category,sort) VALUES
  ('temperature','Temperatura','°C','num','ambiente',10),
  ('humidity','Umidità','%','num','ambiente',20),
  ('co2','Anidride carbonica (CO₂)','ppm','num','ambiente',30),
  ('pm25','Polveri sottili (PM2.5)','µg/m³','num','ambiente',40),
  ('noise','Rumore','dB','num','ambiente',50),
  ('pressure','Pressione','bar','num','processo',60),
  ('level','Livello','%','num','processo',70),
  ('distance','Distanza','m','num','processo',80),
  ('flow','Portata','L/min','num','processo',90),
  ('weight','Peso','kg','num','processo',100),
  ('energy','Energia','kWh','num','energia',110),
  ('power','Potenza','W','num','energia',120),
  ('current','Corrente','A','num','energia',130),
  ('voltage','Tensione','V','num','energia',140),
  ('count','Conteggio','n','num','processo',150),
  ('battery','Batteria','%','num','stato',160),
  ('rssi','Segnale radio','dBm','num','stato',170),
  ('door','Porta/Contatto','','bool','stato',180),
  ('presence','Presenza','','bool','stato',190),
  ('leak','Perdita/Allagamento','','bool','stato',200),
  ('motion','Movimento','','bool','stato',210)
ON CONFLICT (metric) DO UPDATE SET label=EXCLUDED.label, unit=EXCLUDED.unit, kind=EXCLUDED.kind, category=EXCLUDED.category, sort=EXCLUDED.sort;

GRANT SELECT ON app.iot_metrics TO app_rt;

-- Mappa per-device: chiave grezza del device -> metrica canonica, con trasformazione lineare opzionale
-- (num_canonico = scale * num_grezzo + offset) per conversioni di unità/fondo scala.
CREATE TABLE IF NOT EXISTS app.device_metric_map (
    tenant_id uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    device_id uuid NOT NULL REFERENCES app.devices(id) ON DELETE CASCADE,
    raw_key   text NOT NULL,
    metric    text NOT NULL REFERENCES app.iot_metrics(metric),
    scale     double precision NOT NULL DEFAULT 1,
    offset_v  double precision NOT NULL DEFAULT 0,
    PRIMARY KEY (device_id, raw_key)
);
CREATE INDEX IF NOT EXISTS device_metric_map_tenant_idx ON app.device_metric_map(tenant_id, device_id);

-- device_telemetry: oltre alla chiave grezza, salva metrica canonica + unità (per viste/regole uniformi).
ALTER TABLE app.device_telemetry ADD COLUMN IF NOT EXISTS metric text;
ALTER TABLE app.device_telemetry ADD COLUMN IF NOT EXISTS unit   text;
CREATE INDEX IF NOT EXISTS device_telemetry_metric_idx ON app.device_telemetry(tenant_id, device_id, metric, ts DESC);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.device_metric_map TO app_rt';
  EXECUTE 'ALTER TABLE app.device_metric_map ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.device_metric_map USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
