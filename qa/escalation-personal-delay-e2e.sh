#!/usr/bin/env bash
# Regole di notifica personali con ritardo (come GoAlert): chi è stato avvisato da uno step riceve
# anche gli avvisi ritardati (SMS subito, chiamata dopo 5 min) quando l'escalation è già passata allo
# step successivo. Prima la chiamata veniva saltata se lo step aspettava meno di 5 min.
# Tenant usa-e-getta; orologio di test (TEST_CLOCK).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -Atc "$1"; }
J=$(mktemp -d); R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="ep$R"
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Ep $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"loneworker\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
adv(){ curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$1}"; }
run(){ call POST /api/escalation/delivery/run '{}' >/dev/null; }
P1="+3939$R""1"; P2="+3939$R""2"
U1=$(call POST /api/users "{\"email\":\"r1@$SLUG.test\",\"role\":\"reperibile\",\"password\":\"WorkPass9\"}" | py "d['id']")
U2=$(call POST /api/users "{\"email\":\"r2@$SLUG.test\",\"role\":\"reperibile\",\"password\":\"WorkPass9\"}" | py "d['id']")
sql "UPDATE app.users SET phone='$P1' WHERE id='$U1'; UPDATE app.users SET phone='$P2' WHERE id='$U2'" >/dev/null
G=$(call POST /api/groups '{"name":"Notte"}' | py "d['id']")
call POST "/api/groups/$G/members" "{\"user_id\":\"$U1\"}" >/dev/null
# U1: SMS subito, chiamata dopo 5 minuti
SMS=$(call POST "/api/oncall-contacts/$U1/methods" "{\"kind\":\"sms\",\"value\":\"$P1\",\"label\":\"cell\"}" | py "d['id']")
VOI=$(call POST "/api/oncall-contacts/$U1/methods" "{\"kind\":\"voice\",\"value\":\"$P1\",\"label\":\"cell\"}" | py "d['id']")
call POST "/api/oncall-contacts/$U1/rules" "{\"cm_id\":\"$SMS\",\"delay_min\":0}" >/dev/null
call POST "/api/oncall-contacts/$U1/rules" "{\"cm_id\":\"$VOI\",\"delay_min\":5}" >/dev/null
# policy: step 0 al reperibile del gruppo, dopo 2 minuti step 1 a U2 (aspetta 30 min)
call POST "/api/groups/$G/policy" "{\"steps\":[{\"delay_min\":2,\"channel\":\"sms\",\"target_kind\":\"oncall\"},{\"delay_min\":30,\"channel\":\"sms\",\"target_kind\":\"user\",\"target_user_id\":\"$U2\"}],\"repeat\":0}" >/dev/null
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,forced_group_id) VALUES('$TID','sos','nuovo',1,'ep-$R',now(),'$G')" >/dev/null
IID=$(sql "SELECT id FROM app.incidents WHERE tenant_id='$TID' AND correlation='ep-$R'")
log(){ sql "SELECT step||':'||channel||':'||to_value FROM app.esc_delivery_log WHERE incident_id='$IID' ORDER BY id" | tr '\n' ' '; }
run
log | grep -q "0:sms:$P1" && ! log | grep -q "voice" && ok "step 0: SMS subito al reperibile, la chiamata non ancora" || no "step 0" "$(log)"
adv 180; run
log | grep -q "1:sms:$P2" && ok "dopo 2 minuti l'escalation passa allo step 1 (altra persona)" || no "step 1" "$(log)"
! log | grep -q "voice" && ok "a 3 minuti la chiamata del primo reperibile non è ancora dovuta" || no "chiamata in anticipo" "$(log)"
adv 150; run
log | grep -q "0:voice:$P1" && ok "a 5 minuti parte la chiamata del primo reperibile anche se lo step è cambiato" || no "chiamata saltata" "$(log)"
N=$(sql "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$IID' AND channel='voice'"); run
[ "$(sql "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$IID' AND channel='voice'")" = "$N" ] && ok "nessun doppio invio alle passate successive" || no "doppioni" "$(log)"
# presa in carico: niente più avvisi ritardati
sql "UPDATE app.incidents SET state='preso_in_carico' WHERE id='$IID'" >/dev/null
C0=$(sql "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$IID'"); adv 600; run
[ "$(sql "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$IID'")" = "$C0" ] && ok "dopo la presa in carico nessun altro avviso" || no "avvisi dopo presa in carico" "$(log)"
sql "UPDATE app.incidents SET state='chiuso',closed_at=now() WHERE id='$IID'" >/dev/null
echo "escalation-personal-delay: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
