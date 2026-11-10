-- L'AI come ATTORE dentro YouWorkforce (non cosmetica): provider scelto da YouWorkforce
-- (youworker.ai o esterno) + report generati e SALVATI su DB. Le scritture operative
-- dell'agente (nota diario, riga consuntivo, nota incidente, bozza ordine, regola sensore)
-- passano dagli stessi servizi/permessi/RLS della UI; qui si aggiunge solo la persistenza
-- dei report e la scelta del provider.

-- Provider AI selezionabile: youworker | openai | deepseek | custom (OpenAI-compatibili)
-- oppure anthropic (adattatore nativo /v1/messages).
ALTER TABLE app.ai_connections ADD COLUMN IF NOT EXISTS provider text NOT NULL DEFAULT 'youworker';

-- Report prodotti dall'agente AI, salvati e consultabili (tenant-scoped, RLS).
CREATE TABLE IF NOT EXISTS app.ai_reports (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    kind       text NOT NULL DEFAULT 'generico',   -- incidenti | consuntivi | sensori | turni | generico
    title      text NOT NULL,
    body       text NOT NULL,
    params     jsonb,                               -- parametri/filtri usati per generarlo
    created_by text NOT NULL,                       -- email dell'autore (operatore/admin) o 'ai-agent'
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS ai_reports_tenant ON app.ai_reports(tenant_id, created_at DESC);
ALTER TABLE app.ai_reports ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.ai_reports
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.ai_reports TO app_rt;
