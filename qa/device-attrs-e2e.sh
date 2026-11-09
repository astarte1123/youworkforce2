#!/usr/bin/env bash
# #1 Attributi device esposti: la posizione porta telemetria (ignition, motion, heartRate,
# steps, temp) -> ultima nota sul device + nello storico.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"; EXT="ATTR-$RUN"

JA=$(mktemp); curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"$EXT\",\"source\":\"traccar\",\"label\":\"Wearable\"}" | jget 'd["id"]')
[ -n "$DID" ] && ok "device creato" || no "create" "$DID"

curl -s -o /dev/null -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
  -d "{\"device\":{\"uniqueId\":\"$EXT\"},\"position\":{\"id\":\"a1\",\"latitude\":45.47,\"longitude\":9.19,\"fixTime\":\"2026-11-10T10:00:00Z\",\"attributes\":{\"batteryLevel\":77,\"ignition\":true,\"motion\":true,\"heartRate\":82,\"steps\":1234,\"deviceTemp\":36}}}"

A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');a=x['last_attributes'] or {};assert a.get('ignition')==True,a;assert a.get('heartRate')==82,a;assert a.get('steps')==1234,a;print('ok')" >/dev/null 2>&1 \
  && ok "ultima telemetria sul device (ignition/heartRate/steps)" || no "last_attributes" "$(A "$BASE/api/devices" | python -c "import sys,json;print(next(v['last_attributes'] for v in json.load(sys.stdin) if v['id']=='$DID'))")"

A "$BASE/api/devices/$DID/positions" | python -c "import sys,json;d=json.load(sys.stdin);a=d[0].get('attributes') or {};assert a.get('deviceTemp')==36,a;assert a.get('motion')==True,a;print('ok')" >/dev/null 2>&1 \
  && ok "storico espone attributi (temp/motion)" || no "positions attributes" "-"

A -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
