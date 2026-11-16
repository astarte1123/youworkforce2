#!/usr/bin/env bash
# W3b — competenze: un turno che richiede una qualifica va solo a chi la possiede (non scaduta).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Competenze/qualifiche (W3b) =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in workforce qualifiche; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
MARIO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='mario@acme.loneworker.eu'),''))")
P(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }
codeP(){ curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }

QN="Patentino-$(date +%s)"
QID=$(P -X POST $BASE/api/qualifications -d "{\"name\":\"$QN\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
say "1 qualifica creata"
[ -n "$QID" ] && ok || no "no id"

D=$(python -c "import datetime,random;print((datetime.date(2037,1,1)+datetime.timedelta(days=random.randint(0,3000))).isoformat())")
REQ="\"user_id\":\"$MARIO\",\"starts_at\":\"${D}T08:00:00Z\",\"ends_at\":\"${D}T16:00:00Z\",\"required_qualification\":\"$QID\""

say "2 turno che richiede la qualifica, mario non l'ha -> 409"
c=$(codeP -X POST $BASE/api/shift-plans -d "{$REQ}"); [ "$c" = "409" ] && ok || no "got $c"

say "3 validate segnala qualification (ok:false)"
V=$(P -X POST $BASE/api/shift-plans/validate -d "{$REQ}")
echo "$V" | grep -q '"ok":false' && echo "$V" | grep -q '"code":"qualification"' && ok || no "$V"

say "4 assegno la qualifica a mario -> turno passa 200"
P -X POST $BASE/api/users/$MARIO/qualifications -d "{\"qualification_id\":\"$QID\"}" -o /dev/null
c=$(codeP -X POST $BASE/api/shift-plans -d "{$REQ}"); [ "$c" = "200" ] && ok || no "got $c"

say "5 qualifica SCADUTA -> nuovo turno bloccato 409"
P -X POST $BASE/api/users/$MARIO/qualifications -d "{\"qualification_id\":\"$QID\",\"expires_at\":\"2020-01-01T00:00:00Z\"}" -o /dev/null
D2=$(python -c "import datetime,random;print((datetime.date(2038,1,1)+datetime.timedelta(days=random.randint(0,3000))).isoformat())")
REQ2="\"user_id\":\"$MARIO\",\"starts_at\":\"${D2}T08:00:00Z\",\"ends_at\":\"${D2}T16:00:00Z\",\"required_qualification\":\"$QID\""
c=$(codeP -X POST $BASE/api/shift-plans -d "{$REQ2}"); [ "$c" = "409" ] && ok || no "got $c"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
