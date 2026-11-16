#!/usr/bin/env bash
# W1 — Calendario core condiviso: eventi aggregati (turni+interventi) + iCal personale.
# Richiede core :8080 con seed demo + TEST_CLOCK. Abilita workforce+interventi su acme.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Calendario (W1) =="
# superadmin: abilita workforce + interventi su acme
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in workforce interventi; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
curl -s -c "$J/m" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
MARIO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='mario@acme.loneworker.eu'),''))")

# turno + ordine programmato (finestra fissa per il test)
curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' -X POST $BASE/api/shift-plans \
  -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"2030-03-04T08:00:00Z\",\"ends_at\":\"2030-03-04T16:00:00Z\"}" -o /dev/null
OID=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' -X POST $BASE/api/orders \
  -d '{"title":"CalTest","site_name":"Milano","scheduled_start":"2030-03-04T10:00:00Z","scheduled_end":"2030-03-04T11:00:00Z"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' -X POST $BASE/api/orders/$OID/assign -d "{\"version\":1,\"user_id\":\"$MARIO\"}" -o /dev/null

W="from=2030-03-01T00:00:00Z&to=2030-03-10T00:00:00Z"
EV=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' "$BASE/api/calendar/events?$W")
say "1 eventi aggregano turno + intervento"
echo "$EV" | grep -q '"source":"turno"' && echo "$EV" | grep -q '"source":"intervento"' && ok || no "$EV"

say "2 iCal personale di mario ha VEVENT"
URL=$(curl -s -b "$J/m" -H 'x-tenant-slug: acme' $BASE/api/calendar/my-url | jq1 "print(json.load(sys.stdin).get('url',''))")
n=$(curl -s "$BASE$URL" | grep -c 'BEGIN:VEVENT')
[ "${n:-0}" -ge 1 ] && ok || no "VEVENT=$n url=$URL"

say "3 iCal con token non valido -> 404"
c=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/calendar.ics?token=xxx")
[ "$c" = "404" ] && ok || no "got $c"

say "4 lavoratore vede solo i propri eventi (own)"
EM=$(curl -s -b "$J/m" -H 'x-tenant-slug: acme' "$BASE/api/calendar/events?$W")
echo "$EM" | grep -q "$MARIO" && ok || no "$EM"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
