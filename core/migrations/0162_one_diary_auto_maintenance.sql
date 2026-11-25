-- CA-20 (decisioni PO 26/11/2026)
-- 1) UN SOLO DIARIO. Il giornale di cantiere (app.commessa_diario) confluisce in app.diary_entries:
--    una nota può essere legata a un intervento (work_order_id) o a un cantiere (commessa_id), con i
--    campi facoltativi del giornale (data, meteo, presenti). Le note esistenti vengono copiate con
--    source='cantiere' e la tabella vecchia viene eliminata. Così privacy (export / anonimizzazione)
--    e assistente vedono anche le note di cantiere.
-- 2) MANUTENZIONI GENERATE DA SOLE. Ogni piano attivo crea il suo intervento N giorni prima della
--    scadenza (lead_days, default 7) senza che nessuno prema «Genera»; auto_generate=false lo
--    lascia manuale. Lo fa il worker (field::maintenance_autogen).
ALTER TABLE app.diary_entries ALTER COLUMN user_id DROP NOT NULL;  -- note di cantiere senza autore (utente eliminato)
ALTER TABLE app.diary_entries ADD COLUMN IF NOT EXISTS commessa_id uuid REFERENCES app.commesse(id) ON DELETE CASCADE;
ALTER TABLE app.diary_entries ADD COLUMN IF NOT EXISTS diary_date date;
ALTER TABLE app.diary_entries ADD COLUMN IF NOT EXISTS weather text;
ALTER TABLE app.diary_entries ADD COLUMN IF NOT EXISTS present_count int CHECK (present_count IS NULL OR present_count >= 0);
UPDATE app.diary_entries SET diary_date = created_at::date WHERE diary_date IS NULL;
ALTER TABLE app.diary_entries ALTER COLUMN diary_date SET DEFAULT CURRENT_DATE;
ALTER TABLE app.diary_entries ALTER COLUMN diary_date SET NOT NULL;
CREATE INDEX IF NOT EXISTS diary_entries_commessa_idx ON app.diary_entries(commessa_id, diary_date DESC) WHERE commessa_id IS NOT NULL;

DO $$ BEGIN
  IF to_regclass('app.commessa_diario') IS NOT NULL THEN
    INSERT INTO app.diary_entries(id,tenant_id,user_id,commessa_id,diary_date,weather,present_count,body,source,idem_key,created_at)
    SELECT g.id, g.tenant_id, g.created_by, g.commessa_id, g.diary_date, g.weather, g.present_count, g.body,
           'cantiere', 'cantiere:' || g.id, g.created_at
      FROM app.commessa_diario g
    ON CONFLICT DO NOTHING;
    DROP TABLE app.commessa_diario;
  END IF;
END $$;

-- stesso tenant anche sulla nuova colonna (come 0144/0147/0160)
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
       AND ca.attname <> 'tenant_id' AND ch.relname = 'diary_entries'
       AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = pa.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped)
     GROUP BY ch.relname
  LOOP
    EXECUTE format('DROP TRIGGER IF EXISTS yw_same_tenant ON app.%I', r.child);
    EXECUTE format('CREATE TRIGGER yw_same_tenant BEFORE INSERT OR UPDATE OF tenant_id, %s ON app.%I FOR EACH ROW EXECUTE FUNCTION app.yw_same_tenant(%s)', r.cols, r.child, r.args);
  END LOOP;
END $$;

ALTER TABLE app.maintenance_plans ADD COLUMN IF NOT EXISTS auto_generate boolean NOT NULL DEFAULT true;
ALTER TABLE app.maintenance_plans ADD COLUMN IF NOT EXISTS lead_days int NOT NULL DEFAULT 7 CHECK (lead_days BETWEEN 0 AND 90);
ALTER TABLE app.maintenance_plans ADD COLUMN IF NOT EXISTS last_generated_at timestamptz;
