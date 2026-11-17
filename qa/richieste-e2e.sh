#!/usr/bin/env bash
# Interventi A5: richieste/ticket (creazione, conversione in intervento).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Interventi A5 — richieste =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
T="Richiesta-$(python -c 'import random;print(random.randint(100000,999999))')"

say "1 crea richiesta"
RQ=$(curl -s $H -X POST $BASE/api/requests -d "{\"title\":\"$T\",\"priority\":3,\"requester\":\"Mario\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$RQ" ] && ok || no "no id"

say "2 richiesta è 'nuova'"
ST=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/requests | jq1 "import sys,json;print(next(r['status'] for r in json.load(sys.stdin) if r['id']=='$RQ'))")
[ "$ST" = "nuova" ] && ok || no "status=$ST"

say "3 converti in intervento"
OID=$(curl -s $H -X POST $BASE/api/requests/$RQ/convert -d '{}' | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$OID" ] && ok || no "no order"

say "4 richiesta ora 'convertita' con order_id"
R=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/requests | jq1 "r=next(r for r in json.load(sys.stdin) if r['id']=='$RQ');print(r['status'], r.get('order_id')=='$OID')")
echo "$R" | grep -q "convertita True" && ok || no "$R"

say "5 riconversione -> 409"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/requests/$RQ/convert -d '{}')
[ "$c" = "409" ] && ok || no "got $c"

say "6 intervento creato esiste"
GOT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders | jq1 "print(any(o['id']=='$OID' for o in json.load(sys.stdin)))")
[ "$GOT" = "True" ] && ok || no "order mancante"

say "7 modulo interventi OFF -> 403"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/requests)
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
