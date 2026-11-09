-- ============================================================================
-- #2 Comandi al device: connessione Traccar per-tenant (base URL + credenziali)
-- per inoltrare comandi (es. richiesta posizione) via API Traccar.
-- NOTA sicurezza: password in chiaro per la demo -> in produzione usare un vault /
-- un token API Traccar dedicato. Scritta dal superadmin via owner connection.
-- ============================================================================
CREATE TABLE traccar_connections (
  tenant_id  uuid PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
  base_url   text NOT NULL,
  username   text NOT NULL,
  password   text NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);
