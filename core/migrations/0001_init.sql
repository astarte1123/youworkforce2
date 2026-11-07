-- YouWorkforce core schema — multitenancy + RLS
-- Evidence profile: SIMULATO (local demo). Isolation enforced in DB, not only UI.

CREATE SCHEMA IF NOT EXISTS app;
SET search_path TO app, public;

-- Runtime role: non-owner, NO BYPASSRLS. The API connects as this role.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_rt') THEN
    CREATE ROLE app_rt LOGIN PASSWORD 'app_rt_pw' NOBYPASSRLS;
  END IF;
END$$;

GRANT USAGE ON SCHEMA app TO app_rt;

-- ============================ Control plane ============================
CREATE TABLE tenants (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  slug        text UNIQUE NOT NULL,
  name        text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now()
);

-- Superadmins live in the control plane, never scoped to a tenant.
CREATE TABLE superadmins (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  email       text UNIQUE NOT NULL,
  pass_hash   text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now()
);

-- Product modules catalog (code is stable id).
CREATE TABLE modules_catalog (
  code        text PRIMARY KEY,
  name        text NOT NULL,
  kind        text NOT NULL,            -- product | connector | core
  available   boolean NOT NULL DEFAULT true  -- false => "non disponibile in questa release"
);

-- Commercial enablement per tenant (set by superadmin, no deploy).
CREATE TABLE tenant_modules (
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  module_code text NOT NULL REFERENCES modules_catalog(code),
  state       text NOT NULL DEFAULT 'non_abilitato', -- non_abilitato|da_configurare|attivo|degradato|in_disattivazione
  updated_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, module_code)
);

-- ============================ Tenant data ============================
CREATE TABLE users (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  email       text NOT NULL,
  pass_hash   text NOT NULL,
  role        text NOT NULL,  -- tenant_admin|operatore|lavoratore|reperibile|auditor
  created_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, email)
);

CREATE TABLE sites (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id   uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name        text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE devices (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  site_id       uuid REFERENCES sites(id),
  external_id   text NOT NULL,            -- id from source (badge / sensor)
  source        text NOT NULL,           -- traccar|blues|iot|manual
  assigned_user uuid REFERENCES users(id),
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, source, external_id)
);

-- Normalized event v1 (see contracts/events.schema.json)
CREATE TABLE events (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  source        text NOT NULL,
  source_event  text NOT NULL,           -- repeatable id from source for idempotency
  idem_key      text NOT NULL,           -- tenant/source/stable key
  site_id       uuid REFERENCES sites(id),
  device_id     uuid REFERENCES devices(id),
  etype         text NOT NULL,           -- sos|man_down|immobilita|mancato_checkin|device_offline|batteria|soglia_ambientale|recupero_sensore
  origin_ts     timestamptz NOT NULL,
  received_ts   timestamptz NOT NULL DEFAULT now(),
  lat           double precision,
  lon           double precision,
  accuracy_m    double precision,
  payload_ref   jsonb,
  correlation   text NOT NULL,
  UNIQUE (tenant_id, source, idem_key)   -- idempotency per tenant/source/stable key
);

CREATE TABLE incidents (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  event_id      uuid REFERENCES events(id),
  site_id       uuid REFERENCES sites(id),
  etype         text NOT NULL,
  state         text NOT NULL DEFAULT 'aperto', -- aperto|preso_in_carico|intervento|chiuso
  priority      int  NOT NULL DEFAULT 1,
  correlation   text NOT NULL,
  version       int  NOT NULL DEFAULT 1,
  opened_at     timestamptz NOT NULL DEFAULT now(),
  closed_at     timestamptz,
  outcome       text
);

CREATE TABLE incident_timeline (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  incident_id   uuid NOT NULL REFERENCES incidents(id) ON DELETE CASCADE,
  kind          text NOT NULL,  -- notify_requested|goalert_ack|delivery|human_ack|transition
  detail        jsonb,
  at            timestamptz NOT NULL DEFAULT now()
);

-- Durable check-in timers (core-owned, independent of badge data).
CREATE TABLE checkins (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id       uuid NOT NULL REFERENCES users(id),
  site_id       uuid REFERENCES sites(id),
  due_at        timestamptz NOT NULL,
  state         text NOT NULL DEFAULT 'attivo', -- attivo|confermato|scaduto
  created_at    timestamptz NOT NULL DEFAULT now()
);

-- Transactional outbox: incident + audit + notify request in one tx.
CREATE TABLE outbox (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  kind          text NOT NULL,  -- goalert_notify
  payload       jsonb NOT NULL,
  state         text NOT NULL DEFAULT 'pending', -- pending|in_flight|done|error
  attempts      int NOT NULL DEFAULT 0,
  claimed_until timestamptz,
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE audit_log (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id     uuid,            -- null = control plane (superadmin) action
  actor         text NOT NULL,
  action        text NOT NULL,
  detail        jsonb,
  at            timestamptz NOT NULL DEFAULT now()
);

-- ============================ RLS ============================
-- Bind tenant context to the transaction: current_setting('app.current_tenant').
ALTER TABLE users            ENABLE ROW LEVEL SECURITY;
ALTER TABLE sites            ENABLE ROW LEVEL SECURITY;
ALTER TABLE devices          ENABLE ROW LEVEL SECURITY;
ALTER TABLE events           ENABLE ROW LEVEL SECURITY;
ALTER TABLE incidents        ENABLE ROW LEVEL SECURITY;
ALTER TABLE incident_timeline ENABLE ROW LEVEL SECURITY;
ALTER TABLE checkins         ENABLE ROW LEVEL SECURITY;
ALTER TABLE outbox           ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_modules   ENABLE ROW LEVEL SECURITY;

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['users','sites','devices','events','incidents',
      'incident_timeline','checkins','outbox','tenant_modules']
  LOOP
    EXECUTE format($f$
      CREATE POLICY tenant_isolation ON app.%I
        USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
        WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
    $f$, t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt;', t);
  END LOOP;
END$$;

-- Control-plane tables: readable by runtime for tenant/module resolution.
GRANT SELECT ON app.tenants, app.modules_catalog TO app_rt;
GRANT SELECT, INSERT ON app.audit_log TO app_rt;
-- Superadmin writes to control plane go through the owner connection (not app_rt).

-- Seed module catalog (v1 + evolutivo markers).
INSERT INTO modules_catalog(code,name,kind,available) VALUES
  ('workforce','Forza lavoro e turni','product',true),
  ('interventi','Interventi e cantieri','product',true),
  ('iot','Dispositivi e IoT','product',true),
  ('loneworker','Lone Worker e centrale allarmi','product',true),
  ('assistant','Assistente YouWorker.ai e diario vocale','product',true),
  ('consuntivi','Ore, spese e consuntivi','product',true),
  ('qualifiche','Qualifiche e scadenze','product',false),
  ('prevenzione','Prevenzione e checklist','product',false),
  ('goalert','GoAlert (escalation)','connector',true),
  ('traccar','Traccar (badge)','connector',true),
  ('thingsboard','ThingsBoard (IoT industriale)','connector',true),
  ('blues','Blues (sperimentale)','connector',true);
