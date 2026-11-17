#!/usr/bin/env bash
# F2: dipendenze MULTIPLE tra lavorazioni (cronoprogramma).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== F2 — dipendenze multiple =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"
CID=$(curl -s $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"Deps test\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
L1=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"A"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
L2=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"B"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
L3=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"C"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
getl(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | python -c "import sys,json;print(json.dumps(json.load(sys.stdin)['lavorazioni']))"; }

say "1 C dipende da A e B"
curl -s $H -X POST $BASE/api/lavorazioni/$L3 -d "{\"deps\":[\"$L1\",\"$L2\"]}" -o /dev/null
N=$(getl | jq1 "print(len(next(l['deps'] for l in json.load(sys.stdin) if l['id']=='$L3')))")
[ "$N" = "2" ] && ok || no "deps=$N"

say "2 rimozione: C dipende solo da A"
curl -s $H -X POST $BASE/api/lavorazioni/$L3 -d "{\"deps\":[\"$L1\"]}" -o /dev/null
D=$(getl | jq1 "print(next(l['deps'] for l in json.load(sys.stdin) if l['id']=='$L3'))")
echo "$D" | grep -q "$L1" && [ "$(getl | jq1 "print(len(next(l['deps'] for l in json.load(sys.stdin) if l['id']=='$L3')))")" = "1" ] && ok || no "$D"

say "3 ciclo diretto (A dopo C, con C dopo A) -> 400"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/lavorazioni/$L1 -d "{\"deps\":[\"$L3\"]}")
[ "$c" = "400" ] && ok || no "got $c"

say "4 auto-dipendenza ignorata (resta 1)"
curl -s $H -X POST $BASE/api/lavorazioni/$L3 -d "{\"deps\":[\"$L1\",\"$L3\"]}" -o /dev/null
N=$(getl | jq1 "print(len(next(l['deps'] for l in json.load(sys.stdin) if l['id']=='$L3')))")
[ "$N" = "1" ] && ok || no "deps=$N"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
