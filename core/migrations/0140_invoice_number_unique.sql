-- CA-13 (25/11/2026): numero di fattura unico nel tenant. L'API rifiuta già i duplicati; il vincolo
-- DB si crea solo se i dati esistenti lo permettono (altrimenti resta il controllo applicativo e
-- `qa/sql`-style: elencare i doppioni con
--   SELECT tenant_id, number, count(*) FROM app.commessa_fatture GROUP BY 1,2 HAVING count(*)>1).
DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM app.commessa_fatture GROUP BY tenant_id, number HAVING count(*) > 1) THEN
    CREATE UNIQUE INDEX IF NOT EXISTS commessa_fatture_number_uq ON app.commessa_fatture(tenant_id, number);
  ELSE
    RAISE NOTICE 'commessa_fatture: numeri duplicati esistenti, indice unico non creato';
  END IF;
END $$;
