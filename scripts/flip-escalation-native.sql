-- OPT-IN: porta i tenant ESISTENTI al motore di escalation nativo.
-- La migrazione 0057 cambia solo il default (nuovi tenant). Questo script, da eseguire a mano,
-- fa il cutover dei tenant già esistenti ancora su 'goalert'.
--
-- USO (tutti i tenant):
--   psql "$DATABASE_URL" -f scripts/flip-escalation-native.sql
-- USO (singolo tenant):   passare -v tid='<uuid>' e togliere il commento alla WHERE mirata.
--
-- SICUREZZA: GoAlert resta acceso come rete di sicurezza. La soppressione del doppio-path scatta
-- solo se il tenant native ha un percorso di consegna nativo reale (Twilio live) o YW_ESC_SUPPRESS_GOALERT=1
-- (vedi esc_suppress_goalert). Rollback: UPDATE ... SET escalation_engine='goalert'.

BEGIN;

-- tutti i tenant ancora su goalert -> native
UPDATE app.tenant_oncall_cfg
   SET escalation_engine = 'native', updated_at = now()
 WHERE escalation_engine = 'goalert';
--  AND tenant_id = :'tid';   -- scommenta per un singolo tenant

-- riepilogo post-flip
SELECT escalation_engine, count(*) AS tenants
  FROM app.tenant_oncall_cfg
 GROUP BY escalation_engine
 ORDER BY escalation_engine;

COMMIT;
