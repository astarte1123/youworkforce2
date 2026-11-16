-- 0091 — Assistenza A1: anagrafica Cliente. Additiva. Appartiene al modulo interventi (Assistenza).
-- Finora l'ordine aveva solo una "sede". Il field-service ha bisogno del CLIENTE (committente)
-- con i suoi referenti; la sede resta il luogo dell'intervento.
CREATE TABLE IF NOT EXISTS app.clients (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    name       text NOT NULL,
    vat        text,                 -- P.IVA / codice fiscale
    address    text,
    phone      text,
    email      text,
    notes      text,
    active     boolean NOT NULL DEFAULT true,
    created_by uuid REFERENCES app.users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS app.client_contacts (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    client_id  uuid NOT NULL REFERENCES app.clients(id) ON DELETE CASCADE,
    name       text NOT NULL,
    role       text,                 -- ruolo/mansione del referente
    phone      text,
    email      text
);
ALTER TABLE app.work_orders ADD COLUMN IF NOT EXISTS client_id uuid REFERENCES app.clients(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS clients_tenant_idx ON app.clients(tenant_id, active);
CREATE INDEX IF NOT EXISTS client_contacts_idx ON app.client_contacts(tenant_id, client_id);
CREATE INDEX IF NOT EXISTS work_orders_client ON app.work_orders(client_id) WHERE client_id IS NOT NULL;

DO $$ DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['clients','client_contacts'] LOOP
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    BEGIN
      EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXCEPTION WHEN duplicate_object THEN NULL; END;
  END LOOP;
END $$;
