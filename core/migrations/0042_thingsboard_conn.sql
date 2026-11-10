-- Connessione al server ThingsBoard del tenant, gestita DA YouWorkforce (superadmin),
-- come per Traccar: base URL + credenziali (password cifrata a riposo). Abilita la
-- verifica del collegamento e lo specchio dei device. Il push via Rule Engine resta il
-- percorso di ingestione (vedi /api/ingress/thingsboard).
CREATE TABLE IF NOT EXISTS app.thingsboard_connections (
    tenant_id  uuid PRIMARY KEY REFERENCES app.tenants(id) ON DELETE CASCADE,
    base_url   text NOT NULL,
    username   text NOT NULL,
    password   text NOT NULL,            -- enc:v1:... (cifrata)
    updated_at timestamptz NOT NULL DEFAULT now()
);
