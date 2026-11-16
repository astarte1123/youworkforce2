#!/usr/bin/env bash
# W4 — proposta sostituti: il motore scarta conflitti/assenze/competenza e propone candidati idonei.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Proposta sostituti (W4) =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"workforce","state":"attivo"}' -o /dev/null

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
uid(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='$1'),''))"; }
MARIO=$(uid mario@acme.loneworker.eu); OPER=$(uid operatore@acme.loneworker.eu)
P(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }

D=$(python -c "import datetime,random;print((datetime.date(2042,1,1)+datetime.timedelta(days=random.randint(0,3000))).isoformat())")
SID=$(P -X POST $BASE/api/shift-plans -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"${D}T08:00:00Z\",\"ends_at\":\"${D}T16:00:00Z\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
say "1 turno di mario creato"
[ -n "$SID" ] && ok || no "no id"

subs(){ P "$BASE/api/shift-plans/$SID/substitutes"; }
say "2 operatore proposto come sostituto, mario escluso"
R=$(subs); echo "$R" | grep -q "operatore@acme" && ! (echo "$R" | grep -q "\"user_id\":\"$MARIO\"") && ok || no "$R"

say "3 operatore assente in quel giorno -> NON più proposto"
P -X POST $BASE/api/absences -d "{\"user_id\":\"$OPER\",\"kind\":\"ferie\",\"starts_at\":\"${D}T00:00:00Z\",\"ends_at\":\"${D}T23:59:00Z\"}" -o /dev/null
R=$(subs); echo "$R" | grep -q "operatore@acme" && no "operatore ancora proposto" || ok

say "4 risposta ha summary e provider"
R=$(subs); echo "$R" | grep -q '"summary"' && echo "$R" | grep -q '"provider"' && ok || no "$R"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
