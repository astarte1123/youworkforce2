-- TA-15 (decisione PO 25/11/2026): il geocoding inverso non passa più dal server (il browser
-- dell'operatore lo chiede a OpenStreetMap su richiesta). La cache degli indirizzi delle posizioni
-- non serve più e conteneva indirizzi ricavati dalle posizioni dei lavoratori: si elimina.
DROP TABLE IF EXISTS app.geocode_cache;
