-- 0090 — Ritiro della scorciatoia "modalità Lavoro sul campo" (interventi.modes).
-- La separazione cantiere/assistenza ora è a livello di MODULO (interventi=Assistenza,
-- cantieri=Cantieri), non un flag di etichetta. Rimuovo config e tool. Additivo e sicuro.
DELETE FROM app.tenant_module_config WHERE module_code='interventi' AND tool_code='modes';
DELETE FROM app.module_tools        WHERE module_code='interventi' AND tool_code='modes';
