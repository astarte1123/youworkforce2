-- Colore dell'area geofence, scelto dall'admin (mappa outdoor e piantine indoor).
ALTER TABLE app.geofences ADD COLUMN IF NOT EXISTS color text NOT NULL DEFAULT '#16a34a';
