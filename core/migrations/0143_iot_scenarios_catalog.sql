-- IO-4 (25/11/2026): catalogo degli scenari IoT nel DATABASE, gestibile dal superadmin.
-- Prima il catalogo era scritto nel codice (scenario_catalog) e la spec dei widget veniva letta da
-- file con percorso relativo (spariva se il core partiva da un'altra cartella). Ora ogni scenario
-- è una riga: moduli, regole sensori, dispositivi demo, spec dei widget e indicazione per l'AI.
-- Gli scenari predefiniti sono inseriti all'avvio (builtin, mai sovrascritti se modificati).
-- Tabella di sistema (non per tenant): la legge e la scrive il core (owner_pool).
CREATE TABLE IF NOT EXISTS app.iot_scenarios (
  code       text PRIMARY KEY CHECK (code ~ '^[a-z0-9_]{2,40}$'),
  name       text NOT NULL,
  sector     text NOT NULL DEFAULT '',
  descr      text NOT NULL DEFAULT '',
  modules    text[] NOT NULL DEFAULT '{iot}',
  rules      jsonb NOT NULL DEFAULT '[]'::jsonb,   -- [{metric, op, threshold, etype}]
  devices    jsonb NOT NULL DEFAULT '[]'::jsonb,   -- [{suffix, label}] dispositivi demo
  spec       jsonb,                                -- widget/narrativa (facoltativa)
  ai_focus   text,
  builtin    boolean NOT NULL DEFAULT false,
  enabled    boolean NOT NULL DEFAULT true,
  updated_at timestamptz NOT NULL DEFAULT now()
);
REVOKE ALL ON app.iot_scenarios FROM PUBLIC;
GRANT SELECT ON app.iot_scenarios TO app_rt;
