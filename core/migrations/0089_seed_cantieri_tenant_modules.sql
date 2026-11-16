-- 0089 — Backfill righe tenant_modules per il nuovo modulo 'cantieri'.
-- I tenant creati PRIMA dell'esistenza del modulo non hanno la riga: il set-module del
-- superadmin fa UPDATE (0 righe) e lo stato resta 'non_abilitato'. Seminiamo la riga
-- (stato spento) così l'attivazione funziona. I tenant nuovi la ricevono dal seed al create.
INSERT INTO app.tenant_modules(tenant_id,module_code,state)
SELECT t.id,'cantieri','non_abilitato' FROM app.tenants t
ON CONFLICT (tenant_id,module_code) DO NOTHING;
