-- CA-5 (decisione PO 25/11/2026): squadre e gruppi di persone diventano UN SOLO concetto, i gruppi
-- di persone (app.user_groups, nel nucleo). Prima chi gestiva le persone le inseriva due volte e le
-- squadre dipendevano dal modulo Forza lavoro.
-- Ogni squadra diventa un gruppo di tipo «squadra» con LO STESSO id (i riferimenti restano validi);
-- se esiste già un gruppo con lo stesso nome, il nome riceve « (squadra)» (mai unire i membri a un
-- gruppo esistente: i gruppi danno anche permessi).
INSERT INTO app.user_groups(id, tenant_id, name, kind, created_at)
SELECT t.id, t.tenant_id,
       CASE WHEN EXISTS (SELECT 1 FROM app.user_groups g WHERE g.tenant_id = t.tenant_id AND g.name = t.name)
            THEN t.name || ' (squadra)' ELSE t.name END,
       'squadra', t.created_at
  FROM app.teams t
ON CONFLICT (id) DO NOTHING;

INSERT INTO app.user_group_members(tenant_id, group_id, user_id)
SELECT m.tenant_id, m.team_id, m.user_id FROM app.team_members m
ON CONFLICT DO NOTHING;

-- le commesse si assegnano a un gruppo di persone
ALTER TABLE app.commessa_teams DROP CONSTRAINT IF EXISTS commessa_teams_team_id_fkey;
ALTER TABLE app.commessa_teams RENAME COLUMN team_id TO group_id;
ALTER TABLE app.commessa_teams RENAME TO commessa_groups;
ALTER TABLE app.commessa_groups ADD CONSTRAINT commessa_groups_group_id_fkey
  FOREIGN KEY (group_id) REFERENCES app.user_groups(id) ON DELETE CASCADE;

DROP TABLE app.team_members;
DROP TABLE app.teams;

-- controllo «stesso tenant» (0144/0147) rigenerato sui riferimenti attuali
DO $$
DECLARE r record;
BEGIN
  FOR r IN
    SELECT ch.relname AS child,
           string_agg(format('%L, %L, %L, %L', ca.attname, pa.relname, pc.attname, format_type(pc.atttypid, pc.atttypmod)), ', '
                      ORDER BY ca.attname) AS args,
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
    EXECUTE format('CREATE TRIGGER yw_same_tenant BEFORE INSERT OR UPDATE OF tenant_id, %s ON app.%I FOR EACH ROW EXECUTE FUNCTION app.yw_same_tenant(%s)',
                   r.cols, r.child, r.args);
  END LOOP;
END $$;
