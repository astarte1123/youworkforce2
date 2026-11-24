-- 0130 — Sospensione del tenant: programmabile e con effetto su TUTTO il servizio (errore SA-5).
-- Decisione PO (24/11/2026): un tenant sospeso NON riceve né scala allarmi; l'amministratore del
-- tenant va avvisato PRIMA, con la data e le limitazioni.
--   suspend_at   data programmata: finché è nel futuro il tenant resta 'attivo' e l'amministratore
--                vede l'avviso; alla data il worker porta status='sospeso'. NULL = nessuna programmata.
--   suspend_note motivo/limitazioni mostrati nell'avviso (testo del superadmin).
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS suspend_at   timestamptz;
ALTER TABLE app.tenants ADD COLUMN IF NOT EXISTS suspend_note text;

-- Tenant già sospesi prima della 0130: le sessioni aperte restavano valide. Si chiudono ora
-- (l'impersonazione del superadmin per assistenza resta ammessa).
DELETE FROM app.sessions s USING app.tenants t
 WHERE s.tenant_id = t.id AND t.status = 'sospeso' AND s.impersonator IS NULL;
