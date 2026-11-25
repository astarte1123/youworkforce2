-- CA-4 (decisione PO 25/11/2026): REGISTRO UNICO DELLE ORE. Una sola vista su tutte le fonti del
-- tempo lavorato, sempre allineata (nessuna copia da sincronizzare):
--   intervento (priorità 1, fatturabile) · ronda (2) · presenza di cantiere (3) · turno svolto (4)
-- Decisioni: le ronde entrano nel registro ma non nel consuntivo; il turno svolto conta come
-- presenza in servizio, non fatturabile; in caso di sovrapposizione vince la fonte più specifica
-- (numero più basso) e la sovrapposizione resta visibile.
-- security_invoker: la vista applica l'RLS di chi la interroga (isolamento per tenant).
CREATE OR REPLACE VIEW app.work_hours WITH (security_invoker = true) AS
  SELECT 'intervento'::text AS kind, 1 AS prio, o.id AS source_id, o.tenant_id, o.assigned_user AS user_id,
         COALESCE(o.work_started_at, o.arrived_at) AS started_at,
         COALESCE(o.work_started_at, o.arrived_at) + make_interval(mins => o.work_minutes) AS ended_at,
         true AS billable, o.id AS work_order_id, NULL::uuid AS commessa_id, o.title AS label
    FROM app.work_orders o
   WHERE o.state = 'completato' AND o.assigned_user IS NOT NULL AND COALESCE(o.work_minutes, 0) > 0
     AND COALESCE(o.work_started_at, o.arrived_at) IS NOT NULL
  UNION ALL
  SELECT 'ronda', 2, r.id, r.tenant_id, r.user_id, r.started_at, r.ended_at,
         false, NULL, NULL, rt.name
    FROM app.patrol_rounds r JOIN app.patrol_routes rt ON rt.id = r.route_id
   WHERE r.ended_at IS NOT NULL AND r.user_id IS NOT NULL AND COALESCE(r.closed_by, '') <> 'auto'
  UNION ALL
  SELECT 'presenza', 3, p.id, p.tenant_id, p.user_id, p.clock_in, p.clock_out,
         false, NULL, p.commessa_id, c.name
    FROM app.commessa_presenze p JOIN app.commesse c ON c.id = p.commessa_id
   WHERE p.clock_out IS NOT NULL AND NOT p.auto_closed AND p.clock_out > p.clock_in
  UNION ALL
  SELECT 'turno', 4, s.id, s.tenant_id, s.user_id, s.started_at, s.ended_at,
         false, NULL, NULL, COALESCE(s.note, 'turno')
    FROM app.shifts s
   WHERE s.ended_at IS NOT NULL AND s.state <> 'in_corso' AND s.ended_at > s.started_at;

GRANT SELECT ON app.work_hours TO app_rt;
