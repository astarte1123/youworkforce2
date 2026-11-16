#!/usr/bin/env bash
# Lavoro sul campo — modalità (cantieri/assistenza) + legame turno↔intervento.
# Richiede core :8080 con seed demo. Abilita workforce+interventi su acme.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Lavoro sul campo — modalità + turno↔intervento =="
# superadmin: abilita workforce + interventi su acme
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in workforce interventi; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done

# tenant_admin (per field-modes set) + pianificatore (per turni/ordini)
curl -s -c "$J/ad" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CA=$(csrfh "$J/ad"); CP=$(csrfh "$J/pl")
MARIO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='mario@acme.loneworker.eu'),''))")

say "1 field-modes default = entrambe attive"
FM=$(curl -s -b "$J/ad" -H 'x-tenant-slug: acme' $BASE/api/field-modes)
echo "$FM" | grep -q '"cantieri":true' && echo "$FM" | grep -q '"assistenza":true' && ok || no "$FM"

say "2 set solo cantieri -> assistenza=false"
curl -s -b "$J/ad" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' -X POST $BASE/api/field-modes -d '{"cantieri":true,"assistenza":false}' -o /dev/null
FM=$(curl -s -b "$J/ad" -H 'x-tenant-slug: acme' $BASE/api/field-modes)
echo "$FM" | grep -q '"assistenza":false' && ok || no "$FM"

say "3 set nessuna modalità -> 400"
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/ad" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' -X POST $BASE/api/field-modes -d '{"cantieri":false,"assistenza":false}')
[ "$c" = "400" ] && ok || no "got $c"
# ripristina entrambe
curl -s -b "$J/ad" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' -X POST $BASE/api/field-modes -d '{"cantieri":true,"assistenza":true}' -o /dev/null

# ordine + turno legato
D=$(python -c "import random;print(2040+random.randint(0,50))")
OID=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' -X POST $BASE/api/orders \
  -d "{\"title\":\"FM-Order\",\"site_name\":\"Milano\",\"scheduled_start\":\"$D-03-04T10:00:00Z\",\"scheduled_end\":\"$D-03-04T11:00:00Z\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
say "4 ordine creato"
[ -n "$OID" ] && ok || no "no order id"

say "5 turno con work_order_id"
SR=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' -X POST $BASE/api/shift-plans \
  -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"$D-03-04T08:00:00Z\",\"ends_at\":\"$D-03-04T16:00:00Z\",\"work_order_id\":\"$OID\"}")
echo "$SR" | grep -q '"id"' && ok || no "$SR"

say "6 lista turni espone order_title"
LT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/shift-plans)
echo "$LT" | grep -q '"order_title":"FM-Order"' && ok || no "$LT"

say "7 calendario: titolo turno include l'intervento"
W="from=$D-03-01T00:00:00Z&to=$D-03-10T00:00:00Z"
EV=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' "$BASE/api/calendar/events?$W")
echo "$EV" | grep -q 'FM-Order' && ok || no "$EV"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
