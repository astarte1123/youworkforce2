-- Strada nuova (motore nativo) resa corretta e self-sufficient.
-- 1) Ogni tenant deve avere una riga esplicita in tenant_oncall_cfg: niente più fallback implicito
--    (una riga mancante faceva cadere il tenant sul default di prodotto 'native' SENZA config,
--    rendendo il motore nativo autoritativo ma non instradabile). Backfill su 'native' = strada nuova.
INSERT INTO app.tenant_oncall_cfg (tenant_id, escalation_engine)
SELECT t.id, 'native'
  FROM app.tenants t
 WHERE NOT EXISTS (SELECT 1 FROM app.tenant_oncall_cfg c WHERE c.tenant_id = t.id);

-- 2) Gruppo di default (catch-all): quando un incidente non risolve un gruppo né da profilo di
--    protezione né da routing per-etype, il motore nativo lo instrada QUI. Evita che un incidente
--    critico resti senza destinatari (requisito single-path: "mai perso").
ALTER TABLE app.tenant_oncall_cfg
  ADD COLUMN IF NOT EXISTS default_group_id uuid REFERENCES app.alarm_groups(id) ON DELETE SET NULL;
