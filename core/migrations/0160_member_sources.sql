-- TA-7 (decisione PO 26/11/2026): gruppi di escalation e profili di protezione prendono i membri da
-- uno o più GRUPPI DI PERSONE, sempre allineati; restano possibili le persone aggiunte a mano.
-- - alarm_group_sources / protection_profile_sources: quali gruppi di persone alimentano chi;
-- - i membri arrivati da un gruppo hanno via_user_group = quel gruppo (quelli a mano: NULL, e
--   non vengono mai toccati dalla sincronizzazione);
-- - entrare/uscire da un gruppo di persone, o collegare/scollegare un gruppo, aggiorna subito i
--   membri (trigger). Nei gruppi di escalation i nuovi membri vanno in coda alla rotazione.
-- - profili: una persona ha un solo profilo; se ne ha già un altro, resta quello.
ALTER TABLE app.alarm_group_members ADD COLUMN IF NOT EXISTS via_user_group uuid REFERENCES app.user_groups(id) ON DELETE CASCADE;
ALTER TABLE app.protection_profile_members ADD COLUMN IF NOT EXISTS via_user_group uuid REFERENCES app.user_groups(id) ON DELETE CASCADE;

CREATE TABLE IF NOT EXISTS app.alarm_group_sources (
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id      uuid NOT NULL REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
    user_group_id uuid NOT NULL REFERENCES app.user_groups(id) ON DELETE CASCADE,
    added_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, user_group_id)
);
CREATE TABLE IF NOT EXISTS app.protection_profile_sources (
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    profile_id    uuid NOT NULL REFERENCES app.protection_profiles(id) ON DELETE CASCADE,
    user_group_id uuid NOT NULL REFERENCES app.user_groups(id) ON DELETE CASCADE,
    added_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (profile_id, user_group_id)
);
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['alarm_group_sources','protection_profile_sources'] LOOP
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE app.%I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON app.%I', t);
    EXECUTE format('CREATE POLICY tenant_isolation ON app.%I USING (tenant_id = current_setting(''app.current_tenant'', true)::uuid) WITH CHECK (tenant_id = current_setting(''app.current_tenant'', true)::uuid)', t);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON app.%I TO app_rt', t);
  END LOOP;
END $$;

-- (ri)calcola i membri portati dai gruppi di persone per UN gruppo di escalation / profilo
CREATE OR REPLACE FUNCTION app.yw_sync_alarm_group(p_group uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, public AS $$
BEGIN
  -- tolti quelli il cui gruppo di persone non è più collegato o che non ne fanno più parte
  DELETE FROM app.alarm_group_members am
   WHERE am.group_id = p_group AND am.via_user_group IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM app.alarm_group_sources s JOIN app.user_group_members m ON m.group_id = s.user_group_id
                      WHERE s.group_id = p_group AND m.user_id = am.user_id);
  -- aggiunti (in coda) quelli nuovi
  INSERT INTO app.alarm_group_members(tenant_id, group_id, user_id, position, via_user_group)
  SELECT x.tenant_id, p_group, x.user_id,
         COALESCE((SELECT max(position) FROM app.alarm_group_members WHERE group_id = p_group), 0) + row_number() OVER (ORDER BY x.added_at, x.user_id),
         x.user_group_id
    FROM (SELECT DISTINCT ON (m.user_id) s.tenant_id, m.user_id, s.user_group_id, m.added_at
            FROM app.alarm_group_sources s JOIN app.user_group_members m ON m.group_id = s.user_group_id
           WHERE s.group_id = p_group ORDER BY m.user_id, s.added_at) x
   WHERE NOT EXISTS (SELECT 1 FROM app.alarm_group_members am WHERE am.group_id = p_group AND am.user_id = x.user_id)
  ON CONFLICT DO NOTHING;
END $$;
CREATE OR REPLACE FUNCTION app.yw_sync_profile(p_profile uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, public AS $$
BEGIN
  DELETE FROM app.protection_profile_members pm
   WHERE pm.profile_id = p_profile AND pm.via_user_group IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM app.protection_profile_sources s JOIN app.user_group_members m ON m.group_id = s.user_group_id
                      WHERE s.profile_id = p_profile AND m.user_id = pm.user_id);
  INSERT INTO app.protection_profile_members(tenant_id, profile_id, user_id, via_user_group)
  SELECT DISTINCT ON (m.user_id) s.tenant_id, p_profile, m.user_id, s.user_group_id
    FROM app.protection_profile_sources s JOIN app.user_group_members m ON m.group_id = s.user_group_id
   WHERE s.profile_id = p_profile
   ORDER BY m.user_id, s.added_at
  ON CONFLICT DO NOTHING;  -- una persona ha un solo profilo: se ne ha già uno, resta quello
END $$;

CREATE OR REPLACE FUNCTION app.yw_sources_changed() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, public AS $$
DECLARE r record;
BEGIN
  IF TG_TABLE_NAME = 'alarm_group_sources' THEN
    PERFORM app.yw_sync_alarm_group(COALESCE(NEW.group_id, OLD.group_id));
  ELSIF TG_TABLE_NAME = 'protection_profile_sources' THEN
    PERFORM app.yw_sync_profile(COALESCE(NEW.profile_id, OLD.profile_id));
  ELSE -- user_group_members: tutti i gruppi di escalation e profili alimentati da quel gruppo
    FOR r IN SELECT group_id FROM app.alarm_group_sources WHERE user_group_id = COALESCE(NEW.group_id, OLD.group_id) LOOP
      PERFORM app.yw_sync_alarm_group(r.group_id);
    END LOOP;
    FOR r IN SELECT profile_id FROM app.protection_profile_sources WHERE user_group_id = COALESCE(NEW.group_id, OLD.group_id) LOOP
      PERFORM app.yw_sync_profile(r.profile_id);
    END LOOP;
  END IF;
  RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS yw_sources_sync ON app.alarm_group_sources;
CREATE TRIGGER yw_sources_sync AFTER INSERT OR DELETE ON app.alarm_group_sources FOR EACH ROW EXECUTE FUNCTION app.yw_sources_changed();
DROP TRIGGER IF EXISTS yw_sources_sync ON app.protection_profile_sources;
CREATE TRIGGER yw_sources_sync AFTER INSERT OR DELETE ON app.protection_profile_sources FOR EACH ROW EXECUTE FUNCTION app.yw_sources_changed();
DROP TRIGGER IF EXISTS yw_sources_sync ON app.user_group_members;
CREATE TRIGGER yw_sources_sync AFTER INSERT OR DELETE ON app.user_group_members FOR EACH ROW EXECUTE FUNCTION app.yw_sources_changed();

-- controllo «stesso tenant» sulle nuove tabelle (come 0144/0147)
DO $$
DECLARE r record;
BEGIN
  FOR r IN
    SELECT ch.relname AS child,
           string_agg(format('%L, %L, %L, %L', ca.attname, pa.relname, pc.attname, format_type(pc.atttypid, pc.atttypmod)), ', ' ORDER BY ca.attname) AS args,
           string_agg(DISTINCT quote_ident(ca.attname), ', ') AS cols
      FROM pg_constraint c
      JOIN pg_class ch ON ch.oid = c.conrelid
      JOIN pg_namespace n ON n.oid = ch.relnamespace AND n.nspname = 'app'
      JOIN pg_class pa ON pa.oid = c.confrelid
      JOIN pg_attribute ca ON ca.attrelid = ch.oid AND ca.attnum = c.conkey[1]
      JOIN pg_attribute pc ON pc.attrelid = pa.oid AND pc.attnum = c.confkey[1]
     WHERE c.contype = 'f' AND array_length(c.conkey, 1) = 1 AND pa.relname <> 'tenants'
       AND ca.attname <> 'tenant_id'
       AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = ch.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped)
       AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = pa.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped)
     GROUP BY ch.relname
  LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS yw_same_tenant ON app.%I', r.child);
    EXECUTE format('CREATE TRIGGER yw_same_tenant BEFORE INSERT OR UPDATE OF tenant_id, %s ON app.%I FOR EACH ROW EXECUTE FUNCTION app.yw_same_tenant(%s)', r.cols, r.child, r.args);
  END LOOP;
END $$;
