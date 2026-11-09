-- Oltre ai beacon, l'admin deve poter posizionare sulla piantina anche altri
-- dispositivi del tenant (gateway, sensori IoT fissi, telecamere, ecc.) in un
-- punto fisso. Stessa meccanica dei beacon: floorplan_id + coordinate pixel.
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS floorplan_id uuid REFERENCES app.floorplans(id) ON DELETE SET NULL;
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS fp_x double precision;
ALTER TABLE app.devices ADD COLUMN IF NOT EXISTS fp_y double precision;
