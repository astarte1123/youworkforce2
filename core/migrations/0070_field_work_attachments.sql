-- Lavoro sul campo — Slice 1: intervento end-to-end (report + allegati + firma).
-- Allegati (foto/documenti/firma) di un ordine di lavoro, salvati in DB (stesso pattern delle
-- piantine: bytea + content_type). La firma è un allegato kind='signature'. Report e firmatario
-- stanno sull'ordine. Perimetro modulo 'interventi', indipendente da Sicurezza Lavoratore.

CREATE TABLE IF NOT EXISTS app.work_order_attachments (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    work_order_id uuid NOT NULL REFERENCES app.work_orders(id) ON DELETE CASCADE,
    kind          text NOT NULL DEFAULT 'photo',   -- photo | doc | signature
    filename      text,
    content_type  text NOT NULL DEFAULT 'application/octet-stream',
    caption       text,
    data          bytea NOT NULL,
    created_by    text,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS work_order_attachments_wo_idx ON app.work_order_attachments(tenant_id, work_order_id);
GRANT SELECT, INSERT, UPDATE, DELETE ON app.work_order_attachments TO app_rt;

ALTER TABLE app.work_order_attachments ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.work_order_attachments
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

-- Report di esecuzione + firma sull'ordine.
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS report    text;
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS signed_by text;
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS signed_at timestamptz;
