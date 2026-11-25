-- SA-13 (25/11/2026): la sede demo di uno scenario è legata ESPLICITAMENTE allo scenario.
-- Prima la rimozione di uno scenario cancellava tutte le sedi vuote il cui nome finiva per
-- «(demo)», anche quelle di altri scenari o create a mano con quel nome.
ALTER TABLE app.sites ADD COLUMN IF NOT EXISTS scenario_code text;
-- sedi demo già create: si riconoscono dal nome esatto «<nome scenario> (demo)»
UPDATE app.sites s SET scenario_code = sc.code
  FROM app.iot_scenarios sc
 WHERE s.scenario_code IS NULL AND s.name = sc.name || ' (demo)'
   AND EXISTS (SELECT 1 FROM app.tenant_scenarios t WHERE t.tenant_id = s.tenant_id AND t.scenario_code = sc.code);
