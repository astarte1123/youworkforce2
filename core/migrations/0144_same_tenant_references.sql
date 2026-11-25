-- TA-11 / CA-19 (25/11/2026): un riferimento deve puntare a un dato della STESSA azienda.
-- Le chiavi esterne verificano solo che il dato esista: con un id di un'altra azienda si poteva
-- aggiungere a un gruppo un utente altrui, assegnare un ordine a un tecnico altrui, legare un
-- giro, una squadra, un SAL o una sede di un altro tenant. RLS controlla solo il tenant_id della
-- riga scritta, non quello del dato a cui punta.
-- Soluzione unica: per OGNI chiave esterna a colonna singola tra due tabelle che hanno entrambe
-- tenant_id, un trigger verifica che il dato puntato sia dello stesso tenant della riga scritta
-- (in UPDATE solo se il riferimento o il tenant cambiano). Il controllo vale per tutte le vie di
-- scrittura (API, agente AI, worker), anche quelle future: i trigger si rigenerano rilanciando il
-- blocco DO in una migrazione successiva quando si aggiungono tabelle.

CREATE OR REPLACE FUNCTION app.yw_same_tenant() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = app, public AS $$
DECLARE
  i int := 0; col text; reft text; refc text; typ text; v text; ov text; t uuid;
  newj jsonb := to_jsonb(NEW); oldj jsonb;
BEGIN
  IF TG_OP = 'UPDATE' THEN oldj := to_jsonb(OLD); END IF;
  WHILE i < TG_NARGS LOOP
    col := TG_ARGV[i]; reft := TG_ARGV[i+1]; refc := TG_ARGV[i+2]; typ := TG_ARGV[i+3]; i := i + 4;
    v := newj ->> col;
    CONTINUE WHEN v IS NULL;
    IF TG_OP = 'UPDATE' THEN
      ov := oldj ->> col;
      CONTINUE WHEN ov IS NOT DISTINCT FROM v AND (oldj ->> 'tenant_id') IS NOT DISTINCT FROM (newj ->> 'tenant_id');
    END IF;
    EXECUTE format('SELECT tenant_id FROM app.%I WHERE %I = $1::%s', reft, refc, typ) INTO t USING v;
    IF t IS NOT NULL AND t IS DISTINCT FROM (newj ->> 'tenant_id')::uuid THEN
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
                      ORDER BY ca.attname) AS args
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
    EXECUTE format('CREATE TRIGGER yw_same_tenant BEFORE INSERT OR UPDATE ON app.%I FOR EACH ROW EXECUTE FUNCTION app.yw_same_tenant(%s)',
                   r.child, r.args);
  END LOOP;
END $$;
