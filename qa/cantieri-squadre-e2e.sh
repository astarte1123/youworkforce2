#!/usr/bin/env bash
# Cantieri C1: squadre assegnate alla commessa + presenze (timbrature).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Cantieri C1 — squadre + presenze =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
curl -s -c "$J/ad" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl"); CA=$(csrfh "$J/ad")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
HA="-b $J/ad -H x-tenant-slug:acme -H x-csrf-token:$CA -H content-type:application/json"
MARIO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='mario@acme.loneworker.eu'),''))")
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"
TN="Squadra-$(python -c 'import random;print(random.randint(1000,9999))')"

CID=$(curl -s $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"Cantiere test\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
TID=$(curl -s $HA -X POST $BASE/api/teams -d "{\"name\":\"$TN\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
curl -s $HA -X POST $BASE/api/teams/$TID/members -d "{\"user_id\":\"$MARIO\"}" -o /dev/null
say "1 commessa + squadra create"
[ -n "$CID" ] && [ -n "$TID" ] && ok || no "CID=$CID TID=$TID"

say "2 assegna squadra alla commessa"
curl -s $H -X POST $BASE/api/commesse/$CID/teams -d "{\"team_id\":\"$TID\"}" -o /dev/null
NT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(len(json.load(sys.stdin).get('teams',[])))")
[ "$NT" = "1" ] && ok || no "teams=$NT"

say "3 squadra mostra n. persone"
NM=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(json.load(sys.stdin)['teams'][0]['members'])")
[ "$NM" -ge 1 ] && ok || no "members=$NM"

say "4 timbra entrata -> my_open true"
curl -s $H -X POST $BASE/api/commesse/$CID/presenze/in -d '{}' -o /dev/null
MO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(json.load(sys.stdin).get('my_open'))")
[ "$MO" = "True" ] && ok || no "my_open=$MO"

say "5 doppia entrata -> 409"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/commesse/$CID/presenze/in -d '{}')
[ "$c" = "409" ] && ok || no "got $c"

say "6 timbra uscita -> my_open false"
curl -s $H -X POST $BASE/api/commesse/$CID/presenze/out -d '{}' -o /dev/null
MO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(json.load(sys.stdin).get('my_open'))")
[ "$MO" = "False" ] && ok || no "my_open=$MO"

say "7 uscita senza entrata aperta -> 400"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/commesse/$CID/presenze/out -d '{}')
[ "$c" = "400" ] && ok || no "got $c"

say "8 presenze storicizzate (>=1)"
NP=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(len(json.load(sys.stdin).get('presenze',[])))")
[ "$NP" -ge 1 ] && ok || no "presenze=$NP"

say "9 rimuovi squadra"
curl -s $H -X DELETE $BASE/api/commesse/$CID/teams/$TID -o /dev/null
NT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(len(json.load(sys.stdin).get('teams',[])))")
[ "$NT" = "0" ] && ok || no "teams=$NT"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
