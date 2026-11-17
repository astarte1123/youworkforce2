-- 0093 — Nomi dei moduli del campo allineati al vocabolario di prodotto: due moduli distinti
-- "Interventi" e "Cantieri" (prima il catalogo mostrava ancora "Interventi e cantieri").
UPDATE app.modules_catalog SET name='Interventi' WHERE code='interventi';
UPDATE app.modules_catalog SET name='Cantieri'   WHERE code='cantieri';
