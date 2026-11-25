-- CA-15 (25/11/2026): presenze di cantiere verificate.
--  - una sola presenza aperta per persona (prima si poteva essere «in cantiere» su più commesse
--    contemporaneamente e contare due volte le stesse ore);
--  - distanza dalla sede della commessa al momento dell'entrata: se è oltre il raggio la presenza
--    è registrata ma segnata «fuori area», da verificare (un GPS impreciso non blocca il lavoratore).
ALTER TABLE app.commessa_presenze ADD COLUMN IF NOT EXISTS distance_m double precision;
ALTER TABLE app.commessa_presenze ADD COLUMN IF NOT EXISTS outside_area boolean;

-- presenze aperte in parallelo già presenti: resta aperta la più recente, le altre si chiudono
-- d'ufficio all'entrata successiva (auto_closed = da verificare, come CA-17)
WITH ranked AS (
  SELECT id,
         first_value(clock_in) OVER (PARTITION BY tenant_id, user_id ORDER BY clock_in DESC) AS newest,
         row_number() OVER (PARTITION BY tenant_id, user_id ORDER BY clock_in DESC) AS rn
    FROM app.commessa_presenze WHERE clock_out IS NULL
)
UPDATE app.commessa_presenze p SET clock_out = GREATEST(r.newest, p.clock_in), auto_closed = true
  FROM ranked r WHERE r.id = p.id AND r.rn > 1;

CREATE UNIQUE INDEX IF NOT EXISTS commessa_presenze_one_open
  ON app.commessa_presenze(tenant_id, user_id) WHERE clock_out IS NULL;
