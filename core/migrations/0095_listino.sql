-- 0095 — Interventi A2: listino prezzi + valorizzazione della visita. Additiva. Modulo interventi.
-- Il listino è il catalogo delle voci fatturabili (manodopera, materiali, trasferta, forfait).
-- Le righe valorizzano il singolo intervento (work_order) attingendo dal listino o a mano.
CREATE TABLE IF NOT EXISTS app.price_list_items (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    code       text,                               -- codice articolo (opz.)
    name       text NOT NULL,
    category   text NOT NULL DEFAULT 'altro',      -- manodopera | materiale | trasferta | forfait | altro
    unit       text NOT NULL DEFAULT 'pezzo',      -- ora | pezzo | km | giorno | forfait
    unit_price numeric(12,2) NOT NULL DEFAULT 0,
    active     boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, code)
);
CREATE TABLE IF NOT EXISTS app.work_order_lines (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    work_order_id uuid NOT NULL REFERENCES app.work_orders(id) ON DELETE CASCADE,
    item_id       uuid REFERENCES app.price_list_items(id) ON DELETE SET NULL,
    description   text NOT NULL,
    category      text NOT NULL DEFAULT 'altro',
    unit          text NOT NULL DEFAULT 'pezzo',
    qty           numeric(12,2) NOT NULL DEFAULT 1,
    unit_price    numeric(12,2) NOT NULL DEFAULT 0,
    created_by    uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS price_items_idx ON app.price_list_items(tenant_id, active);
CREATE INDEX IF NOT EXISTS wo_lines_idx ON app.work_order_lines(tenant_id, work_order_id);

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['price_list_items','work_order_lines'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
