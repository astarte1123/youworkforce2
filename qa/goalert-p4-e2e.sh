#!/usr/bin/env bash
# (a) validazione numeri Twilio lato nostro  (c) ack/escalate alert da YouWorkforce
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
JS=$(mktemp); JM=$(mktemp); JO=$(mktemp)
curl -s -o /dev/null -c "$JS" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
CS=$(csrfh "$JS"); CM=$(csrfh "$JM"); CO=$(csrfh "$JO")
TID=$(curl -s -b "$JS" "$BASE/api/superadmin/tenants" | python -c "import sys,json
for t in json.load(sys.stdin):
 if t['slug']=='acme':print(t['id'])")
cfg(){ curl -s -o /dev/null -w '%{http_code}' -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/goalert/config" -d "$1"; }
# (a) validazione
[ "$(cfg '{"entries":[{"id":"Twilio.FromNumber","value":"abc"}]}')" = "400" ] && ok "(a) FromNumber non-E164 -> 400 (nostro)" || no "a invalid" "-"
[ "$(cfg '{"entries":[{"id":"Twilio.AccountSID","value":"xx"}]}')" = "400" ] && ok "(a) AccountSID formato errato -> 400" || no "a sid" "-"
[ "$(cfg '{"entries":[{"id":"Twilio.FromNumber","value":"+12128675309"}]}')" = "200" ] && ok "(a) numero valido +12128675309 accettato" || no "a valid" "$(cfg '{"entries":[{"id":"Twilio.FromNumber","value":"+12128675309"}]}')"
# (c) ack/escalate: crea SOS, attendi AlertID, ack, escalate
IID=$(curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' -X POST "$BASE/api/sos" -d '{"lat":45.46,"lon":9.19}' | jget 'd["incident_id"]')
sleep 7
O(){ curl -s -b "$JO" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CO" -H 'content-type: application/json' "$@"; }
O -X POST "$BASE/api/incidents/$IID/goalert-ack" | grep -q '"ok":true' && ok "(c) ack inviato" || no "c ack" "-"
sleep 2
O "$BASE/api/incidents/$IID/goalert-status" | grep -q 'StatusAcknowledged' && ok "(c) stato -> Acknowledged" || no "c ackstate" "$(O "$BASE/api/incidents/$IID/goalert-status")"
O -X POST "$BASE/api/incidents/$IID/goalert-escalate" | grep -q '"ok":true' && ok "(c) escalate forzato" || no "c esc" "-"
O "$BASE/api/incidents/$IID/timeline" | grep -q 'human_ack' && ok "(c) timeline human_ack" || no "c tl" "-"
# cleanup: chiudi incidente
O -X POST "$BASE/api/incidents/$IID/close" -d '{"outcome":"test"}' >/dev/null
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
