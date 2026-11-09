-- ============================================================================
-- #4 Geocoding inverso: cache condivisa indirizzo<-coordinate (Nominatim/OSM).
-- Gli indirizzi sono dati pubblici: cache globale per rispettare la usage policy
-- di Nominatim (<=1 req/s) ed evitare chiamate ripetute sulle stesse coordinate.
-- ============================================================================
CREATE TABLE geocode_cache (
  lat_key      numeric(8,4) NOT NULL,   -- ~11 m di risoluzione
  lon_key      numeric(8,4) NOT NULL,
  display_name text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (lat_key, lon_key)
);
-- Letta/scritta dal worker di sistema (owner connection); nessuna RLS.
