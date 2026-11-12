-- Parità pagina Profile GoAlert — opt-out/DNC, verifica recapiti, status-update per-utente,
-- note di passaggio turno, playbook. Tutto sulla strada nativa; gating a livello applicativo
-- (ESCALATION_MODS): la configurazione resta del tenant admin, il modulo lo accende il superadmin.

-- G1) DO-NOT-CALL / opt-out. Registro GLOBALE per valore (numero/email): un opt-out via STOP vale
--     ovunque quel recapito compaia (il carrier comunque non consegna) e PERSISTE oltre lo stato del
--     modulo (compliance). Nessun tenant_id -> nessuna RLS; letto dal runner via owner_pool.
CREATE TABLE IF NOT EXISTS app.contact_optout (
    value       text PRIMARY KEY,                 -- E.164 o email normalizzato
    kind        text NOT NULL DEFAULT 'sms',       -- sms|voice|email
    at          timestamptz NOT NULL DEFAULT now(),
    reason      text NOT NULL DEFAULT 'carrier_stop'
);
GRANT SELECT, INSERT, DELETE ON app.contact_optout TO app_rt;

-- stato "disabilitato" del singolo recapito (per opt-out o scelta admin)
ALTER TABLE app.user_contact_methods
  ADD COLUMN IF NOT EXISTS disabled_at timestamptz;
-- telefono PRIMARIO opt-out (users.phone)
ALTER TABLE app.users
  ADD COLUMN IF NOT EXISTS phone_opt_out_at timestamptz;

-- G2) verifica OTP anche sui recapiti aggiuntivi (verified_at già presente in 0063)
ALTER TABLE app.user_contact_methods
  ADD COLUMN IF NOT EXISTS verify_code    text,
  ADD COLUMN IF NOT EXISTS verify_expires timestamptz;

-- G3) Alert Status Updates per-utente (override del default tenant; NULL = eredita)
ALTER TABLE app.users
  ADD COLUMN IF NOT EXISTS status_updates_on boolean;

-- G4) note di passaggio turno (handoff) + playbook per gruppo
CREATE TABLE IF NOT EXISTS app.esc_handoff_notes (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id   uuid NOT NULL REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
    author     text NOT NULL DEFAULT '',
    body       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS esc_handoff_notes_group_idx ON app.esc_handoff_notes(tenant_id, group_id, created_at DESC);
ALTER TABLE app.esc_handoff_notes ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.esc_handoff_notes
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.esc_handoff_notes TO app_rt;

-- playbook "cosa fare" mostrato al reperibile: per gruppo e/o per tipo evento
ALTER TABLE app.alarm_groups
  ADD COLUMN IF NOT EXISTS playbook text NOT NULL DEFAULT '';
