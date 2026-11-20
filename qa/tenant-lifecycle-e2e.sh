#!/usr/bin/env bash
# Ondata 3 superadmin: ciclo di vita tenant (sospendi/riattiva = login bloccato/sbloccato) + elimina
# (cascade, conferma slug) + readiness in lista.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }
code(){ curl -s -o /dev/null -w '%{http_code}' "$@"; }

echo "== Superadmin — ciclo di vita tenant =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
S(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }
R=$(python -c 'import random;print(random.randint(100000,999999))')
SLUG="life$R"; EMAIL="admin@$SLUG.test"; LOGIN(){ code -X POST $BASE/api/login -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"DirectPass9\"}"; }

say "1 crea tenant (password diretta) + login ok"
TID=$(S -X POST $BASE/api/superadmin/tenants -d "{\"slug\":\"$SLUG\",\"name\":\"Life $R\",\"admin_email\":\"$EMAIL\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$TID" ] && [ "$(LOGIN)" = "200" ] && ok || no "id=$TID login=$(LOGIN)"

say "2 sospendi -> login 403"
S -X POST $BASE/api/superadmin/tenants/$TID/status -d '{"status":"sospeso"}' -o /dev/null
[ "$(LOGIN)" = "403" ] && ok || no "login=$(LOGIN)"

say "3 lista espone status=sospeso"
ST=$(S $BASE/api/superadmin/tenants | jq1 "t=next(t for t in json.load(sys.stdin) if t['id']=='$TID');print(t.get('status'))")
[ "$ST" = "sospeso" ] && ok || no "$ST"

say "4 riattiva -> login 200"
S -X POST $BASE/api/superadmin/tenants/$TID/status -d '{"status":"attivo"}' -o /dev/null
[ "$(LOGIN)" = "200" ] && ok || no "login=$(LOGIN)"

say "5 stato non valido -> 400"
[ "$(code -X POST $BASE/api/superadmin/tenants/$TID/status -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -d '{"status":"boh"}')" = "400" ] && ok || no "atteso 400"

say "6 readiness: operativo (sicurezza attivo + admin)"
RD=$(S $BASE/api/superadmin/tenants | jq1 "t=next(t for t in json.load(sys.stdin) if t['id']=='$TID');print(t.get('mods_active'),t.get('users_tot'))")
echo "$RD" | grep -qE "^[1-9][0-9]* [1-9]" && ok || no "$RD"

say "7 elimina con slug errato -> 400"
[ "$(code -X POST $BASE/api/superadmin/tenants/$TID -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X DELETE -d '{"slug":"sbagliato"}')" = "400" ] && ok || no "atteso 400"

say "8 elimina con slug corretto -> 200"
[ "$(code -X DELETE $BASE/api/superadmin/tenants/$TID -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -d "{\"slug\":\"$SLUG\"}")" = "200" ] && ok || no "delete fallita"

say "9 dopo l'eliminazione: tenant sparito (login 404)"
[ "$(LOGIN)" = "404" ] && ok || no "login=$(LOGIN)"

say "10 audit set_module registrato"
S -X POST $BASE/api/superadmin/tenants/$(S $BASE/api/superadmin/tenants | jq1 "print(next(t['id'] for t in json.load(sys.stdin) if t['slug']=='acme'))")/modules -d '{"module_code":"loneworker","state":"attivo"}' -o /dev/null
N=$(docker exec ywdb psql -U postgres -d youworkforce -tAc "SELECT count(*) FROM app.audit_log WHERE action='set_module'" 2>/dev/null)
[ "${N:-0}" -ge 1 ] && ok || no "audit set_module assente ($N)"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
