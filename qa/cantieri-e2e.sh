#!/usr/bin/env bash
# Modulo Cantieri: commessa -> lavorazioni (WBS, media pesata) -> SAL + legame turno↔commessa.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Modulo Cantieri =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in workforce cantieri; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
MARIO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='mario@acme.loneworker.eu'),''))")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"

say "1 crea commessa"
CID=$(curl -s $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"Ristrutturazione\",\"client_name\":\"Globex\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$CID" ] && ok || no "no id"

say "2 codice duplicato -> 409"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"X\"}")
[ "$c" = "409" ] && ok || no "got $c"

say "3 aggiungi 2 lavorazioni (pesi 3 e 1)"
L1=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"Fondazioni","weight":3}' | jq1 "print(json.load(sys.stdin).get('id',''))")
L2=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"Finiture","weight":1}' | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$L1" ] && [ -n "$L2" ] && ok || no "L1=$L1 L2=$L2"

say "4 avanzamento pesato: L1=100,L2=0 -> 75%"
curl -s $H -X POST $BASE/api/lavorazioni/$L1 -d '{"progress_pct":100,"status":"fatto"}' -o /dev/null
curl -s $H -X POST $BASE/api/lavorazioni/$L2 -d '{"progress_pct":0}' -o /dev/null
PG=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(json.load(sys.stdin).get('progress'))")
[ "$PG" = "75" ] && ok || no "progress=$PG (atteso 75)"

say "5 registra SAL 60%"
curl -s $H -X POST $BASE/api/commesse/$CID/sal -d '{"pct":60,"amount":12000,"note":"primo SAL"}' -o /dev/null
NS=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(len(json.load(sys.stdin).get('sal',[])))")
[ "$NS" = "1" ] && ok || no "sal count=$NS"

say "6 turno legato alla commessa"
D=$(python -c "import random;print(2100+random.randint(0,899))")  # anno unico per evitare collisioni overlap su acme ri-usato
curl -s $H -X POST $BASE/api/shift-plans -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"$D-05-04T08:00:00Z\",\"ends_at\":\"$D-05-04T16:00:00Z\",\"commessa_id\":\"$CID\"}" -o /dev/null
NSH=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(len(json.load(sys.stdin).get('shifts',[])))")
[ "$NSH" = "1" ] && ok || no "shifts=$NSH"

say "7 lista turni espone commessa_code"
LT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/shift-plans)
echo "$LT" | grep -q "\"commessa_code\":\"$CODE\"" && ok || no "no commessa in shift list"

say "8 calendario: titolo turno include la commessa"
EV=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' "$BASE/api/calendar/events?from=$D-05-01T00:00:00Z&to=$D-05-10T00:00:00Z")
echo "$EV" | grep -q "$CODE" && ok || no "$EV"

say "9 modulo cantieri OFF -> 403"
# disattiva e verifica gating
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse)
[ "$c" = "403" ] && ok || no "got $c"
# riattiva
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
