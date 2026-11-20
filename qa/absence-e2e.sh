#!/usr/bin/env bash
# W3a — assenze: un'assenza impedisce di pianificare turni sulla persona in quel periodo.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Assenze (W3a) =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"workforce","state":"attivo"}' -o /dev/null

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
MARIO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='mario@acme.loneworker.eu'),''))")
P(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }
codeP(){ curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }

# finestra assenza UNICA per run (DB persistente)
A1=$(python -c "import datetime,random;print((datetime.date(2036,1,1)+datetime.timedelta(days=random.randint(0,3000))).isoformat())")
A0="${A1}T00:00:00Z"; AE=$(python -c "import datetime;print((datetime.date.fromisoformat('$A1')+datetime.timedelta(days=9)).isoformat())")T00:00:00Z
MID=$(python -c "import datetime;print((datetime.date.fromisoformat('$A1')+datetime.timedelta(days=4)).isoformat())")
OUTD=$(python -c "import datetime;print((datetime.date.fromisoformat('$A1')+datetime.timedelta(days=20)).isoformat())")
AB=$(P -X POST $BASE/api/absences -d "{\"user_id\":\"$MARIO\",\"kind\":\"ferie\",\"starts_at\":\"$A0\",\"ends_at\":\"$AE\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
say "1 assenza creata"
[ -n "$AB" ] && ok || no "no id"

say "1b modifica assenza (ferie -> malattia)"
P -X POST $BASE/api/absences/$AB -d "{\"user_id\":\"$MARIO\",\"kind\":\"malattia\",\"starts_at\":\"$A0\",\"ends_at\":\"$AE\"}" -o /dev/null
K=$(P $BASE/api/absences | jq1 "import json,sys;print(next((a['kind'] for a in json.load(sys.stdin) if a['id']=='$AB'),''))")
[ "$K" = "malattia" ] && ok || no "kind=$K"

IN="\"user_id\":\"$MARIO\",\"starts_at\":\"${MID}T08:00:00Z\",\"ends_at\":\"${MID}T16:00:00Z\""
say "2 validate turno in assenza -> ok:false, code absence"
V=$(P -X POST $BASE/api/shift-plans/validate -d "{$IN}")
echo "$V" | grep -q '"ok":false' && echo "$V" | grep -q '"code":"absence"' && ok || no "$V"

say "3 crea turno in assenza -> 409"
c=$(codeP -X POST $BASE/api/shift-plans -d "{$IN}"); [ "$c" = "409" ] && ok || no "got $c"

OUT="\"user_id\":\"$MARIO\",\"starts_at\":\"${OUTD}T08:00:00Z\",\"ends_at\":\"${OUTD}T16:00:00Z\""
say "4 turno fuori assenza -> 200"
c=$(codeP -X POST $BASE/api/shift-plans -d "{$OUT}"); [ "$c" = "200" ] && ok || no "got $c"

say "5 rimossa l'assenza, il turno in quel periodo passa -> 200"
P -X DELETE $BASE/api/absences/$AB -o /dev/null
c=$(codeP -X POST $BASE/api/shift-plans -d "{$IN}"); [ "$c" = "200" ] && ok || no "got $c"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
