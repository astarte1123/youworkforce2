-- CA-6 (decisione PO 25/11/2026): un listino unico, in centesimi, con tariffe per QUALIFICA e
-- validità nel tempo. Interventi, consuntivi e costi di commessa leggono il prezzo da qui e ne
-- copiano il valore sulla riga (il prezzo storico non cambia se il listino cambia).

-- 1) listino: prezzo in centesimi (fonte unica); unit_price resta come colonna calcolata
ALTER TABLE app.price_list_items ADD COLUMN IF NOT EXISTS price_cents bigint;
UPDATE app.price_list_items SET price_cents = round(COALESCE(unit_price, 0) * 100)::bigint WHERE price_cents IS NULL;
ALTER TABLE app.price_list_items ALTER COLUMN price_cents SET NOT NULL, ALTER COLUMN price_cents SET DEFAULT 0;
ALTER TABLE app.price_list_items ADD CONSTRAINT price_list_items_price_nonneg CHECK (price_cents >= 0);
ALTER TABLE app.price_list_items DROP COLUMN unit_price;
ALTER TABLE app.price_list_items ADD COLUMN unit_price numeric GENERATED ALWAYS AS (price_cents / 100.0) STORED;
-- tariffe per qualifica (NULL = vale per tutti) e periodo di validità (estremi inclusi, NULL = aperto)
ALTER TABLE app.price_list_items ADD COLUMN IF NOT EXISTS qualification_id uuid REFERENCES app.qualifications(id) ON DELETE SET NULL;
ALTER TABLE app.price_list_items ADD COLUMN IF NOT EXISTS valid_from date;
ALTER TABLE app.price_list_items ADD COLUMN IF NOT EXISTS valid_to date;
ALTER TABLE app.price_list_items ADD CONSTRAINT price_list_items_validity CHECK (valid_to IS NULL OR valid_from IS NULL OR valid_to >= valid_from);

-- 2) righe dell'intervento: prezzo copiato in centesimi
ALTER TABLE app.work_order_lines ADD COLUMN IF NOT EXISTS unit_price_cents bigint;
UPDATE app.work_order_lines SET unit_price_cents = round(COALESCE(unit_price, 0) * 100)::bigint WHERE unit_price_cents IS NULL;
ALTER TABLE app.work_order_lines ALTER COLUMN unit_price_cents SET NOT NULL, ALTER COLUMN unit_price_cents SET DEFAULT 0;
ALTER TABLE app.work_order_lines DROP COLUMN unit_price;
ALTER TABLE app.work_order_lines ADD COLUMN unit_price numeric GENERATED ALWAYS AS (unit_price_cents / 100.0) STORED;

-- 3) tariffa oraria di manodopera di una persona in un giorno:
--    voce «manodopera» a ore, attiva e valida quel giorno; prima quelle per una qualifica che la
--    persona ha (non scaduta) — la più alta se sono più d'una —, poi quella per tutti.
CREATE OR REPLACE FUNCTION app.yw_labor_rate_cents(p_tenant uuid, p_user uuid, p_day date)
RETURNS bigint LANGUAGE sql STABLE AS $$
  SELECT i.price_cents
    FROM app.price_list_items i
   WHERE i.tenant_id = p_tenant AND i.active AND i.category = 'manodopera' AND i.unit = 'ora'
     AND (i.valid_from IS NULL OR i.valid_from <= p_day) AND (i.valid_to IS NULL OR i.valid_to >= p_day)
     AND (i.qualification_id IS NULL OR EXISTS (
            SELECT 1 FROM app.user_qualifications q
             WHERE q.tenant_id = p_tenant AND q.user_id = p_user AND q.qualification_id = i.qualification_id
               AND (q.expires_at IS NULL OR q.expires_at::date >= p_day)))
   ORDER BY (i.qualification_id IS NOT NULL) DESC, CASE WHEN i.qualification_id IS NOT NULL THEN i.price_cents END DESC NULLS LAST, i.created_at
   LIMIT 1
$$;
GRANT EXECUTE ON FUNCTION app.yw_labor_rate_cents(uuid, uuid, date) TO app_rt;

-- 4) controllo «stesso tenant» rigenerato (nuovo riferimento listino -> qualifica)
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
