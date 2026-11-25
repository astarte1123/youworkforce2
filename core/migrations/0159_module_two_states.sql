-- AR-5 (decisione PO 26/11/2026): i moduli hanno DUE stati, attivo e non attivo. Gli stati
-- intermedi (da_configurare, degradato, in_disattivazione) erano accettati ma mai gestiti: per il
-- prodotto valevano già come «non attivo» (solo 'attivo' abilita le funzioni).
UPDATE app.tenant_modules SET state = 'non_abilitato', updated_at = now() WHERE state NOT IN ('attivo', 'non_abilitato');
ALTER TABLE app.tenant_modules DROP CONSTRAINT IF EXISTS tenant_modules_state_two;
ALTER TABLE app.tenant_modules ADD CONSTRAINT tenant_modules_state_two CHECK (state IN ('attivo', 'non_abilitato'));
