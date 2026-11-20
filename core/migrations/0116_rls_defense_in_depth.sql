-- 0116 — Difesa in profondità: RLS + policy tenant_isolation su TUTTE le restanti tabelle con
-- tenant_id che finora ne erano prive. Oggi queste tabelle sono accedute solo via owner_pool
-- (ruolo `postgres`, owner) con filtro tenant_id/PK/token espliciti, quindi NON c'è leak; questa
-- migrazione aggiunge una rete di sicurezza: se un domani una query girasse su `app_rt` (rt_pool)
-- senza filtro, l'isolamento per-tenant resta garantito dal DB.
--
-- RLS NON forzata (niente FORCE): l'owner (`postgres`/owner_pool) continua a bypassare, così i
-- flussi control-plane e i lookup by-token (sessioni, iCal, ingest) restano invariati.
-- Idempotente: enable ripetuto è no-op; la policy si crea solo se assente.

DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY[
      'ai_connections','ai_features','audit_log','calendar_ical_tokens',
      'esc_alert_state','esc_delivery_log','esc_group_ical_tokens','esc_rotation_state',
      'ingest_tokens','oncall_ical_tokens','sessions','tenant_scenarios',
      'thingsboard_connections','thingsboard_poll_state','traccar_connections','traccar_poll_state'
  ]
  LOOP
    -- la tabella potrebbe non esistere su DB molto vecchi: salta in quel caso
    IF to_regclass('app.'||t) IS NULL THEN CONTINUE; END IF;
    EXECUTE format('ALTER TABLE app.%I ENABLE ROW LEVEL SECURITY;', t);
    IF NOT EXISTS (
      SELECT 1 FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid
      JOIN pg_namespace n ON n.oid=c.relnamespace
      WHERE n.nspname='app' AND c.relname=t AND p.polname='tenant_isolation'
    ) THEN
      EXECUTE format($f$
        CREATE POLICY tenant_isolation ON app.%I
          USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
          WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
      $f$, t);
    END IF;
  END LOOP;
END$$;
