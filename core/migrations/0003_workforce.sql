-- Prompt 9: Forza lavoro, ordini di lavoro, consuntivi.
-- Money as integer minor units (cents) + currency => deterministic, no float.
SET search_path TO app, public;

CREATE TABLE work_orders (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  site_id       uuid REFERENCES sites(id),
  title         text NOT NULL,
  instructions  text,
  assigned_user uuid REFERENCES users(id),
  state         text NOT NULL DEFAULT 'bozza', -- bozza|assegnato|in_corso|completato|annullato
  outcome       text,
  cancel_reason text,
  version       int  NOT NULL DEFAULT 1,
  created_by    uuid REFERENCES users(id),
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE timesheets (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  work_order_id uuid REFERENCES work_orders(id),
  user_id       uuid NOT NULL REFERENCES users(id),
  state         text NOT NULL DEFAULT 'bozza', -- bozza|in_revisione|da_correggere|approvato
  note          text,
  approved_by   uuid REFERENCES users(id),
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE timesheet_lines (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  timesheet_id  uuid NOT NULL REFERENCES timesheets(id) ON DELETE CASCADE,
  activity      text NOT NULL,
  work_date     date NOT NULL,
  minutes       int  NOT NULL CHECK (minutes >= 0),
  rate_cents    bigint,            -- per hour; NULL => no cost attributed (rule missing)
  currency      text NOT NULL DEFAULT 'EUR'
);

CREATE TABLE expenses (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  timesheet_id  uuid NOT NULL REFERENCES timesheets(id) ON DELETE CASCADE,
  description   text NOT NULL,
  amount_cents  bigint NOT NULL,
  currency      text NOT NULL DEFAULT 'EUR',
  has_receipt   boolean NOT NULL DEFAULT false
);

CREATE TABLE timesheet_events (  -- correction/approval history
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  timesheet_id  uuid NOT NULL REFERENCES timesheets(id) ON DELETE CASCADE,
  kind          text NOT NULL,
  detail        jsonb,
  at            timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE work_orders      ENABLE ROW LEVEL SECURITY;
ALTER TABLE timesheets       ENABLE ROW LEVEL SECURITY;
ALTER TABLE timesheet_lines  ENABLE ROW LEVEL SECURITY;
ALTER TABLE expenses         ENABLE ROW LEVEL SECURITY;
ALTER TABLE timesheet_events ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['work_orders','timesheets','timesheet_lines','expenses','timesheet_events']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;
