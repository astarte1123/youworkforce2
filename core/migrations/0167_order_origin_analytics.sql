-- Fase 3 «Analisi e stime» (piano Idealverde). Per misurare durate reali, ricorrenze e volume degli
-- extra serve sapere da dove nasce un intervento e se è un lavoro extra (fuori dal programmato).
--  origin: manuale (creato in console) | piano (generato da un piano di manutenzione) |
--          richiesta (convertito da una richiesta) | integrazione (arrivato dal gestionale)
--  is_extra: lavoro estemporaneo, fuori contratto o programma (richieste, urgenze, «intervento extra»)

ALTER TABLE app.work_orders
    ADD COLUMN IF NOT EXISTS origin text NOT NULL DEFAULT 'manuale' CHECK (origin IN ('manuale','piano','richiesta','integrazione')),
    ADD COLUMN IF NOT EXISTS is_extra boolean NOT NULL DEFAULT false;

-- origine dello storico (regole del prodotto, non dati di un cliente)
UPDATE app.work_orders SET origin='integrazione' WHERE external_ref IS NOT NULL;
UPDATE app.work_orders o SET origin='richiesta', is_extra=true
  FROM app.service_requests r WHERE r.order_id = o.id AND r.tenant_id = o.tenant_id;
UPDATE app.work_orders SET origin='piano'
 WHERE origin='manuale' AND (title LIKE '%(manutenzione)'
        OR id IN (SELECT last_order FROM app.maintenance_plans WHERE last_order IS NOT NULL));

CREATE INDEX IF NOT EXISTS work_orders_analytics_idx ON app.work_orders(tenant_id, activity_type_id, state) WHERE work_minutes IS NOT NULL;

-- nuovo modulo «Analisi e stime» (spento finché il superadmin non lo attiva)
INSERT INTO app.modules_catalog(code, name, kind, available)
VALUES ('analisi', 'Analisi e stime', 'product', true)
ON CONFLICT (code) DO NOTHING;
