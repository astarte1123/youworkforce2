#!/usr/bin/env bash
# End-to-end proof of the REAL ThingsBoard IoT connector (UPSTREAM REALE):
#   device telemetry -> ThingsBoard 3.x Rule Engine (REST API Call node)
#     -> core /api/ingress/thingsboard -> severity gate -> incident (if critical)
#       -> outbox -> GoAlert (real if GOALERT_BASE set) -> timeline.
#
# Prereqs: qa/run-thingsboard.sh has been run (TB up, rule chain wired, device SENSOR-DEF);
# core on :8080. Requires `python` on PATH.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
TB="${TB_URL:-http://localhost:8084}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1"; FAIL=$((FAIL+1)); }

# Resolve the device access token from ThingsBoard (self-contained).
TOK=$(curl -s -X POST "$TB/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"tenant@thingsboard.org","password":"tenant"}' | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
DID=$(curl -s "$TB/api/tenant/devices?deviceName=SENSOR-DEF" -H "X-Authorization: Bearer $TOK" | sed -n 's/.*"id":{"entityType":"DEVICE","id":"\([^"]*\)".*/\1/p')
AT=$(curl -s "$TB/api/device/$DID/credentials" -H "X-Authorization: Bearer $TOK" | sed -n 's/.*"credentialsId":"\([^"]*\)".*/\1/p')
[ -n "$AT" ] && ok "resolved SENSOR-DEF access token" || { no "no device token (run qa/run-thingsboard.sh)"; echo "== $PASS passed, $FAIL failed =="; exit 1; }

# core session
J=$(mktemp)
curl -s -c "$J" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}' >/dev/null
incs(){ curl -s -b "$J" -H 'x-tenant-slug: acme' "$BASE/api/incidents"; }
corrset(){ incs | python -c "import sys,json;print(' '.join(i['correlation'] for i in json.load(sys.stdin) if i['correlation'].startswith('tb-')))"; }

echo "== 1) CRITICAL telemetry (temperature=96) through the real Rule Engine =="
BEFORE=$(corrset)
code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$TB/api/v1/$AT/telemetry" -H 'content-type: application/json' -d '{"temperature":96}')
[ "$code" = "200" ] && ok "ThingsBoard accepted telemetry (200)" || no "telemetry http=$code"

NEW=""
for i in $(seq 1 10); do
  sleep 2
  for c in $(corrset); do case " $BEFORE " in *" $c "*) : ;; *) NEW="$c";; esac; done
  [ -n "$NEW" ] && break
done
[ -n "$NEW" ] && ok "Rule Engine forwarded -> new incident ($NEW)" || { no "no incident from rule engine"; echo "== $PASS passed, $FAIL failed =="; exit 1; }

IID=$(incs | python -c "import sys,json;print([i['id'] for i in json.load(sys.stdin) if i['correlation']=='$NEW'][0])")
ET=$(incs | python -c "import sys,json;print([i['etype'] for i in json.load(sys.stdin) if i['correlation']=='$NEW'][0])")
[ "$ET" = "sensore_critico" ] && ok "mapped to etype=sensore_critico (critical)" || no "wrong etype: $ET"

echo "== 2) timeline: notify from source=thingsboard + GoAlert ack =="
TL=""
for i in $(seq 1 6); do
  TL=$(curl -s -b "$J" -H 'x-tenant-slug: acme' "$BASE/api/incidents/$IID/timeline")
  echo "$TL" | grep -q goalert_sent && break; sleep 2
done
echo "$TL" | grep -q '"thingsboard"' && ok "notify carries source=thingsboard" || no "no source=thingsboard"
echo "$TL" | grep -q goalert_sent && ok "goalert_sent present" || no "no goalert_sent"
if echo "$TL" | grep -q 'UPSTREAM REALE'; then ok "GoAlert step profile: UPSTREAM REALE";
else echo "INFO  GoAlert step is SIMULATO (set GOALERT_BASE for UPSTREAM REALE)"; fi

echo "== 3) severity gate: non-critical telemetry (temperature=20) opens NO incident =="
BEFORE2=$(corrset)
curl -s -o /dev/null -X POST "$TB/api/v1/$AT/telemetry" -H 'content-type: application/json' -d '{"temperature":20}'
sleep 6
NEW2=""
for c in $(corrset); do case " $BEFORE2 " in *" $c "*) : ;; *) NEW2="$c";; esac; done
[ -z "$NEW2" ] && ok "no new incident for non-critical reading" || no "unexpected incident: $NEW2"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
