-- Ambienti / casi d'uso (ispirati a thingsboard.io/use-cases): scenari attivabili dal
-- superadmin su qualsiasi tenant. Il catalogo e' definito nel codice (parametrico);
-- qui si traccia solo quali scenari sono stati applicati a ciascun tenant, per attivarli
-- e rimuoverli in modo pulito. L'applicazione provisiona moduli + regole sensori + (opz.)
-- device/sito demo, riusando le primitive esistenti.
CREATE TABLE IF NOT EXISTS app.tenant_scenarios (
    tenant_id     uuid NOT NULL REFERENCES app.tenants(id) ON DELETE CASCADE,
    scenario_code text NOT NULL,
    with_demo     boolean NOT NULL DEFAULT false,
    applied_by    text NOT NULL,
    applied_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, scenario_code)
);
