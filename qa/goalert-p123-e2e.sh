#!/usr/bin/env bash
# GoAlert P1+P2+P3: provisioning (EP+service+key+heartbeat), open con AlertID+details,
# status read-back, close-loop (action=close), on-call. Richiede GoAlert reale su :8082.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; GA="${GA:-http://localhost:8082}"
GU="${GA_USER:-admin}"; GP="${GA_PASS:-goalertadmin1}"
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

# P3 provisioning
R=$(curl -s -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/goalert/provision" \
  -d "{\"user\":\"$GU\",\"pass\":\"$GP\",\"name\":\"Acme P123\"}")
echo "$R" | python -c 'import sys,json;d=json.load(sys.stdin);assert d.get("ok");assert d["service_id"];assert d["integration_key"];print("ok")' >/dev/null 2>&1 \
  && ok "P3 provisioning: EP+service+key creati" || no "provision" "$R"
echo "$R" | grep -q '"heartbeat_url"' && [ "$(echo "$R"|jget 'bool(d.get("heartbeat_url"))')" = "True" ] && ok "P3 heartbeat monitor creato" || no "heartbeat" "$(echo "$R"|head -c 160)"

# P1 open: SOS -> alert reale con AlertID
IID=$(curl -s -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' -X POST "$BASE/api/sos" -d '{"lat":45.46,"lon":9.19}' | jget 'd["incident_id"]')
[ -n "$IID" ] && ok "SOS creato ($IID)" || no "sos" "-"
sleep 7  # lascia drenare l'outbox verso GoAlert
ST=$(curl -s -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/incidents/$IID/goalert-status")
echo "$ST" | python -c 'import sys,json;d=json.load(sys.stdin);assert d["available"],d;assert "Status" in (d.get("status") or ""),d;print("ok")' >/dev/null 2>&1 \
  && ok "P2 status: alert GoAlert leggibile ($(echo "$ST"|jget 'd.get("status")'))" || no "status open" "$ST"

# P1 close-loop: chiudo da noi -> GoAlert deve chiudere
curl -s -o /dev/null -b "$JO" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CO" -H 'content-type: application/json' -X POST "$BASE/api/incidents/$IID/close" -d '{"outcome":"risolto"}'
sleep 7
ST=$(curl -s -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/incidents/$IID/goalert-status")
echo "$ST" | grep -q 'StatusClosed' && ok "P1 close-loop: alert GoAlert CHIUSO dopo chiusura incidente" || no "close loop" "$ST"
# timeline deve avere goalert_sent + goalert_closed
TL=$(curl -s -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/incidents/$IID/timeline")
echo "$TL" | grep -q 'goalert_sent' && ok "timeline: goalert_sent (inviato, non ack umano)" || no "sent" "-"
echo "$TL" | grep -q 'goalert_closed' && ok "timeline: goalert_closed" || no "closed tl" "-"

# P2 on-call
OC=$(curl -s -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/oncall-now")
echo "$OC" | python -c 'import sys,json;d=json.load(sys.stdin);assert d["available"],d;print("ok")' >/dev/null 2>&1 \
  && ok "P2 on-call: endpoint reperibilita' GoAlert ok" || no "oncall" "$OC"

echo "== $PASS passed, $FAIL failed =="; exit $FAIL
