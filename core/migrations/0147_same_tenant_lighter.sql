-- Review 25/11/2026 su 0144: il trigger «stesso tenant» serializzava l'INTERA riga in JSON
-- (to_jsonb(NEW) e to_jsonb(OLD)) a ogni INSERT/UPDATE. Costoso su tabelle con allegati/immagini
-- (work_order_attachments.data, floorplans.image) e su app.devices, aggiornata a ogni posizione GPS.
-- Ora:
--  - si leggono solo le colonne che servono (accesso per nome al record, niente JSON della riga);
--  - in UPDATE il trigger scatta solo se cambia tenant_id o una delle colonne di riferimento
--    (UPDATE OF ...): gli aggiornamenti di posizione/batteria/ultimo contatto non lo eseguono più.
-- Stessa regola e stesso errore (YW_TENANT_MISMATCH) di 0144.

CREATE OR REPLACE FUNCTION app.yw_same_tenant() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, public AS $$
DECLARE
  i int := 0; col text; reft text; refc text; typ text; v text; ov text; t uuid; ntid uuid; otid uuid;
BEGIN
  ntid := NEW.tenant_id;
  IF TG_OP = 'UPDATE' THEN otid := OLD.tenant_id; END IF;
  WHILE i < TG_NARGS LOOP
    col := TG_ARGV[i]; reft := TG_ARGV[i+1]; refc := TG_ARGV[i+2]; typ := TG_ARGV[i+3]; i := i + 4;
    EXECUTE format('SELECT ($1).%I::text', col) INTO v USING NEW;
    CONTINUE WHEN v IS NULL;
    IF TG_OP = 'UPDATE' THEN
      EXECUTE format('SELECT ($1).%I::text', col) INTO ov USING OLD;
      CONTINUE WHEN ov IS NOT DISTINCT FROM v AND otid IS NOT DISTINCT FROM ntid;
    END IF;
    EXECUTE format('SELECT tenant_id FROM app.%I WHERE %I = $1::%s', reft, refc, typ) INTO t USING v;
    IF t IS NOT NULL AND t IS DISTINCT FROM ntid THEN
      RAISE EXCEPTION 'YW_TENANT_MISMATCH: %.% fa riferimento a un dato di un''altra azienda', TG_TABLE_NAME, col
        USING ERRCODE = '23514';
    END IF;
  END LOOP;
  RETURN NEW;
END $$;

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
