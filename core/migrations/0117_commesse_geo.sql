-- Geolocalizzazione dei cantieri (commesse): un cantiere è un luogo specifico, spesso NON una
-- "sede" riusabile. Diamo alla commessa indirizzo + coordinate proprie, così appare sulla
-- Mappa cantieri anche senza sede collegata. La mappa usa queste coordinate con fallback alla sede.
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS address text;
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS lat double precision;
ALTER TABLE app.commesse ADD COLUMN IF NOT EXISTS lon double precision;
