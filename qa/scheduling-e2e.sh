#!/usr/bin/env bash
# W2 — motore regole turni: overlap bloccato (hard), avvisi riposo/ore, preflight validate, force.
# Richiede core :8080 con seed demo + TEST_CLOCK. Abilita workforce su acme.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }
RUN=$(date +%s)

echo "== Motore regole turni (W2) =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"workforce","state":"attivo"}' -o /dev/null

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
MARIO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='mario@acme.loneworker.eu'),''))")
mk(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@" -X POST $BASE/api/shift-plans; }
code(){ curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@" -X POST $BASE/api/shift-plans; }

# base: mar 2031-05-06 08-16
B0="\"user_id\":\"$MARIO\",\"starts_at\":\"2031-05-06T08:00:00Z\",\"ends_at\":\"2031-05-06T16:00:00Z\""
say "1 crea turno pulito -> 200, 0 avvisi"
R=$(mk -d "{$B0}"); echo "$R" | grep -q '"id"' && echo "$R" | grep -q '"warnings":\[\]' && ok || no "$R"

# overlap 12-18 stesso giorno
OV="\"user_id\":\"$MARIO\",\"starts_at\":\"2031-05-06T12:00:00Z\",\"ends_at\":\"2031-05-06T18:00:00Z\""
say "2 turno sovrapposto -> 409 (conflitto hard)"
c=$(code -d "{$OV}"); [ "$c" = "409" ] && ok || no "got $c"

say "3 preflight validate segnala overlap (ok:false)"
V=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' -X POST $BASE/api/shift-plans/validate -d "{$OV}")
echo "$V" | grep -q '"ok":false' && echo "$V" | grep -q '"code":"overlap"' && ok || no "$V"

say "4 Forza (x-force) crea comunque -> 200"
c=$(code -H 'x-force: 1' -d "{$OV}"); [ "$c" = "200" ] && ok || no "got $c"

# riposo corto: turno il giorno dopo che inizia 6h dopo la fine (16:00 -> 22:00 stesso giorno? no)
# fine base 16:00; nuovo turno 2031-05-06 20:00-23:00 (riposo 4h) -> warning min_rest, non blocca
RS="\"user_id\":\"$MARIO\",\"starts_at\":\"2031-05-06T20:00:00Z\",\"ends_at\":\"2031-05-06T23:00:00Z\""
say "5 riposo corto -> 200 con avviso min_rest (non blocca)"
R=$(mk -d "{$RS}"); echo "$R" | grep -q '"id"' && echo "$R" | grep -q '"code":"min_rest"' && ok || no "$R"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
