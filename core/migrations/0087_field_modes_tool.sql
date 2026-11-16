-- 0087 — Registra il tool 'modes' del modulo interventi. Additiva.
-- tenant_module_config ha FK (module_code,tool_code) -> module_tools: senza questa riga
-- l'impostazione delle modalità (cantieri/assistenza) viola il vincolo.
INSERT INTO app.module_tools(module_code,tool_code,name,description,default_config,sort) VALUES
 ('interventi','modes','Modalità di lavoro','Attiva Cantieri (turni+copertura) e/o Assistenza (appuntamenti+listino).',
  '{"cantieri":true,"assistenza":true}'::jsonb, 90)
ON CONFLICT (module_code,tool_code) DO NOTHING;
