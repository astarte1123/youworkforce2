-- 0132 — FE-1: «I miei allarmi» indica per ogni incidente se l'utente è stato avvisato, leggendo i
-- log di consegna nella transazione del tenant (ruolo applicativo app_rt). La tabella ha già
-- l'isolamento per tenant (RLS, mig 0116): si concede la SOLA lettura; le scritture restano al motore
-- di escalation (owner).
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='app_rt') THEN
    GRANT SELECT ON app.esc_delivery_log TO app_rt;
  END IF;
END $$;
