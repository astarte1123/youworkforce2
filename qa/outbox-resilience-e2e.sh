#!/usr/bin/env bash
# SICUREZZA (#6) — un'escalation critica NON deve essere abbandonata se GoAlert e'
# irraggiungibile a lungo. Prima: i goalert_notify falliti andavano in 'error' dopo 6
# tentativi (~12s con tick 2s), cioe' un blip di GoAlert faceva sparire un SOS.
# Ora: backoff esponenziale + ritentativo a oltranza per gli invii di sicurezza; gli
# invii non critici invece continuano a capitolare in 'error' dopo 6 tentativi.
# Strategia: routing override con token GoAlert invalido -> il notify fallisce in modo
# deterministico; verifichiamo non-abbandono + backoff; poi rimuoviamo l'override
# (fallback alla key reale del tenant) e verifichiamo la consegna (recovery).
set -uo pipefail
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"

cleanup(){
  sql "DELETE FROM app.outbox WHERE tenant_id=$TIDQ AND payload->>'etype' IN ('_test_fail_6','_test_noncrit_6')" >/dev/null
  sql "DELETE FROM app.goalert_routing WHERE tenant_id=$TIDQ AND etype IN ('_test_fail_6','_test_noncrit_6')" >/dev/null
}
trap cleanup EXIT
cleanup

# routing override: etype di test -> token GoAlert invalido (fallimento deterministico)
sql "INSERT INTO app.goalert_routing(tenant_id,etype,integration_key) VALUES($TIDQ,'_test_fail_6','token-invalido-per-test-6')
     ON CONFLICT (tenant_id,etype) DO UPDATE SET integration_key=EXCLUDED.integration_key" >/dev/null
sql "INSERT INTO app.goalert_routing(tenant_id,etype,integration_key) VALUES($TIDQ,'_test_noncrit_6','token-invalido-per-test-6')
     ON CONFLICT (tenant_id,etype) DO UPDATE SET integration_key=EXCLUDED.integration_key" >/dev/null

# --- 1) NON-ABBANDONO di un invio critico oltre la vecchia soglia di 6 ---
# seminiamo gia' a 9 tentativi: col vecchio codice sarebbe 'error'; ora deve restare in coda.
RID=$(sql "INSERT INTO app.outbox(tenant_id,kind,payload,state,attempts,next_attempt_at)
  VALUES($TIDQ,'goalert_notify',
    '{\"etype\":\"_test_fail_6\",\"summary\":\"SOS test #6\",\"dedup\":\"test6-crit\",\"details\":\"x\"}',
    'pending',9,now()-interval '1 second') RETURNING id" | head -1)
echo "seminato notify critico a 9 tentativi ($RID); attendo ritentativi..."
sleep 6
ST=$(sql "SELECT state FROM app.outbox WHERE id='$RID'")
AT=$(sql "SELECT attempts FROM app.outbox WHERE id='$RID'")
[ "$ST" = "pending" ] && ok "invio critico oltre 6 tentativi NON abbandonato (state=pending, attempts=$AT)" || no "invio critico abbandonato" "state=$ST attempts=$AT"
LE=$(sql "SELECT coalesce(last_error,'') FROM app.outbox WHERE id='$RID'")
[ -n "$LE" ] && ok "last_error valorizzato per visibilita' admin ($LE)" || no "last_error vuoto" "nessun errore registrato"

# --- 2) BACKOFF: il prossimo tentativo e' schedulato nel futuro (non raffica a 2s) ---
FUT=$(sql "SELECT (next_attempt_at > now()+interval '2 seconds') FROM app.outbox WHERE id='$RID'")
[ "$FUT" = "t" ] && ok "backoff attivo: next_attempt_at schedulato nel futuro (no raffica)" || no "nessun backoff" "next_attempt_at non nel futuro"

# --- 3) invio NON critico: capitola in 'error' dopo 6 (comportamento invariato) ---
RID2=$(sql "INSERT INTO app.outbox(tenant_id,kind,payload,state,attempts,next_attempt_at)
  VALUES($TIDQ,'heartbeat_test',
    '{\"etype\":\"_test_noncrit_6\",\"summary\":\"nc\",\"dedup\":\"test6-nc\",\"details\":\"x\"}',
    'pending',9,now()-interval '1 second') RETURNING id" | head -1)
sleep 4
ST2=$(sql "SELECT state FROM app.outbox WHERE id='$RID2'")
[ "$ST2" = "error" ] && ok "invio NON critico oltre 6 -> error (cap invariato)" || no "non critico non capitola" "state=$ST2"

# --- 4) RECOVERY: rimosso l'override (fallback alla key reale) l'invio critico passa ---
sql "DELETE FROM app.goalert_routing WHERE tenant_id=$TIDQ AND etype='_test_fail_6'" >/dev/null
sql "UPDATE app.outbox SET next_attempt_at=now()-interval '1 second' WHERE id='$RID'" >/dev/null
echo "rimosso override -> key reale; attendo consegna..."
sleep 6
ST3=$(sql "SELECT state FROM app.outbox WHERE id='$RID'")
[ "$ST3" = "done" ] && ok "al ripristino l'escalation critica viene consegnata (state=done)" || no "non consegnata al ripristino" "state=$ST3"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
