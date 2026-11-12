-- F1+F2 — Escalation nativa self-sufficient e disaccoppiata.
-- Obiettivo: il motore nativo (strada nuova) deve configurarsi, instradare, risolvere on-call,
-- gestire override/assenze, verificare recapiti e sospendere (maintenance) SENZA dipendere da
-- GoAlert, e deve restare disponibile per qualsiasi modulo che produce incidenti (IoT/TB/Traccar)
-- anche con "Sicurezza lavoratore" (loneworker) spento.

-- 1) MAINTENANCE nativo: finestra di sospensione consegne, sorgente di verità nel tenant_oncall_cfg
--    (il runner nativo la rispetta; prima era solo su GoAlert = no-op per tenant nativi).
ALTER TABLE app.tenant_oncall_cfg
  ADD COLUMN IF NOT EXISTS maintenance_until timestamptz;

-- 2) OVERRIDE/ASSENZE native (prima vivevano solo in GoAlert). Replace/Remove/Add su un gruppo
--    in una finestra temporale. add_user = sostituto (aggiunto), remove_user = assente (rimosso).
CREATE TABLE IF NOT EXISTS app.esc_overrides (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    group_id    uuid NOT NULL REFERENCES app.alarm_groups(id) ON DELETE CASCADE,
    start_at    timestamptz NOT NULL,
    end_at      timestamptz NOT NULL,
    add_user    uuid REFERENCES app.users(id) ON DELETE CASCADE,
    remove_user uuid REFERENCES app.users(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (add_user IS NOT NULL OR remove_user IS NOT NULL),
    CHECK (end_at > start_at)
);
CREATE INDEX IF NOT EXISTS esc_overrides_group_idx ON app.esc_overrides(tenant_id, group_id, start_at, end_at);

ALTER TABLE app.esc_overrides ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
  CREATE POLICY tenant_isolation ON app.esc_overrides
    USING (tenant_id = current_setting('app.current_tenant', true)::uuid)
    WITH CHECK (tenant_id = current_setting('app.current_tenant', true)::uuid);
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON app.esc_overrides TO app_rt;

-- 3) VERIFICA RECAPITI nativa (prima solo via GoAlert contactMethods). Codice OTP sul telefono
--    dell'utente, con scadenza. phone_verified_at = recapito confermato.
ALTER TABLE app.users
  ADD COLUMN IF NOT EXISTS phone_verified_at   timestamptz,
  ADD COLUMN IF NOT EXISTS phone_verify_code   text,
  ADD COLUMN IF NOT EXISTS phone_verify_expires timestamptz;
