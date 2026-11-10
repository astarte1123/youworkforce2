-- Abilitazione per-processo dell'AI youworker.ai, pilotata dall'admin del tenant.
-- Ogni processo (assistente, sintesi incidente, suggerimento regole sensori, anomalie
-- check-in, assistenza interventi/consuntivi, diario vocale) e' attivabile/disattivabile
-- indipendentemente. Spegnere l'AI NON toglie l'inserimento manuale ne' la catena SOS.
-- Accesso via owner_pool con filtro tenant_id esplicito (come app.*_connections): niente RLS.
CREATE TABLE IF NOT EXISTS app.ai_features (
    tenant_id  uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    feature    text NOT NULL,       -- assistant | incident_summary | sensor_suggest | checkin_anomaly | interventi_assist | consuntivi_summary | voice_diary
    enabled    boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, feature)
);
