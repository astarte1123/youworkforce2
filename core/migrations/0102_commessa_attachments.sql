-- 0102 — Cantieri R3: allegati di commessa (foto/documenti, incl. SAL/fatture/giornale). Additiva.
-- Pattern bytea come work_order_attachments/floorplans (self-contained, niente storage esterno).
CREATE TABLE IF NOT EXISTS app.commessa_attachments (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    commessa_id  uuid NOT NULL REFERENCES app.commesse(id) ON DELETE CASCADE,
    entity_kind  text NOT NULL DEFAULT 'commessa',  -- commessa | diario | fattura | sal
    kind         text NOT NULL DEFAULT 'photo',     -- photo | doc
    filename     text,
    content_type text NOT NULL DEFAULT 'application/octet-stream',
    caption      text,
    data         bytea NOT NULL,
    created_by   text,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS commessa_att_idx ON app.commessa_attachments(tenant_id, commessa_id);

DO $$ BEGIN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON app.commessa_attachments TO app_rt';
  EXECUTE 'ALTER TABLE app.commessa_attachments ENABLE ROW LEVEL SECURITY';
  BEGIN
    EXECUTE 'CREATE POLICY tenant_isolation ON app.commessa_attachments USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)';
  EXCEPTION WHEN duplicate_object THEN NULL; END;
END $$;
