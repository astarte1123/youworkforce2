-- Razionalizzazione Worker Safety (Fase D) — il motore di escalation NATIVO diventa il default.
-- Da ora i NUOVI tenant (nuova riga tenant_oncall_cfg) nascono con escalation_engine='native'.
-- I tenant ESISTENTI conservano il valore già scritto ('goalert', backfill della 0056): NON vengono
-- toccati qui. Il passaggio a 'native' degli esistenti è OPT-IN, via scripts/flip-escalation-native.sql.
-- GoAlert resta provisionato come rete di sicurezza; rollback per-tenant = escalation_engine='goalert'.
ALTER TABLE app.tenant_oncall_cfg
  ALTER COLUMN escalation_engine SET DEFAULT 'native';
