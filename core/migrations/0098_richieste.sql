-- 0098 — Interventi A5: richieste/ticket. Additiva. Canale d'ingresso prima dell'ordine.
CREATE TABLE IF NOT EXISTS app.service_requests (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    client_id   uuid REFERENCES app.clients(id) ON DELETE SET NULL,
    title       text NOT NULL,
    description text,
    priority    integer NOT NULL DEFAULT 2,         -- 1 bassa | 2 media | 3 alta
    status      text NOT NULL DEFAULT 'nuova',      -- nuova | presa | convertita | chiusa
    requester   text,                               -- chi ha segnalato (nome/contatto)
    order_id    uuid REFERENCES app.work_orders(id) ON DELETE SET NULL,
    created_by  uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS requests_idx ON app.service_requests(tenant_id, status, created_at);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.service_requests TO app_rt';
  EXECUTE 'ALTER TABLE app.service_requests ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.service_requests USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
