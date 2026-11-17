#!/usr/bin/env bash
# Fase 4 — IoT/connettori dal pannello: ingest-token (crea/usa/elimina), stato dispositivi
# (ultimo contatto), riepilogo connettori. Il tenant deriva SEMPRE dal token di ingest.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"

JA=$(mktemp); JO=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); CO=$(csrfh "$JO")
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
O(){ curl -s -b "$JO" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CO" -H 'content-type: application/json' "$@"; }

# 1) riepilogo connettori
A "$BASE/api/connectors" | python -c "import sys,json;d=json.load(sys.stdin);t=next(c for c in d if c['code']=='iot');assert t['state']=='attivo',t;print('ok')" >/dev/null 2>&1 \
  && ok "connettori: IoT nativo attivo" || no "connectors" "$(A "$BASE/api/connectors"|head -c 160)"

# 2) crea ingest-token IoT nativo (secret mostrato una volta; handle non segreto in lista)
CREATED=$(A -X POST "$BASE/api/ingest-tokens" -d '{"source":"iot"}')
TOK=$(echo "$CREATED" | jget 'd["token"]')      # secret: autentica l'ingest
HANDLE=$(echo "$CREATED" | jget 'd["handle"]')   # handle: compare in lista e serve per delete/revoke
[ -n "$TOK" ] && ok "ingest-token IoT nativo creato" || no "create token" "$TOK"
A "$BASE/api/ingest-tokens" | grep -q "$HANDLE" && ok "token (handle) in lista" || no "list tokens" "-"

# 3) regola soglia + device registrato + nuovo token: telemetria critica -> incidente
RID=$(A -X POST "$BASE/api/sensor-rules" -d '{"metric":"temperature","op":">=","threshold":100,"etype":"sensore_critico","label":"Temp E2E"}' | jget 'd.get("id","")')
A -X POST "$BASE/api/devices" -d "{\"external_id\":\"SENSOR-$RUN\",\"source\":\"iot\",\"label\":\"Sensore\"}" >/dev/null
INC=$(curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK&id=SENSOR-$RUN" -H 'content-type: application/json' -d '{"values":{"temperature":120}}' | jget 'json.dumps(d.get("incident"))')
[ -n "$INC" ] && [ "$INC" != "null" ] && ok "ingest col nuovo token -> incidente" || no "ingest new token" "$INC"

# 4) stato dispositivo: registra device + ingest -> ultimo stato (last_attributes)
DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"SENSOR-LS-$RUN\",\"source\":\"iot\",\"label\":\"Sensore LS\"}" | jget 'd["id"]')
curl -s -o /dev/null -X POST "$BASE/api/ingress/telemetry?token=$TOK&id=SENSOR-LS-$RUN" -H 'content-type: application/json' -d '{"values":{"temperature":20}}'
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);dev=next(x for x in d if x['id']=='$DID');assert dev.get('last_attributes'),dev;print('ok')" >/dev/null 2>&1 \
  && ok "dispositivo: ultimo stato registrato (last_attributes)" || no "device last_attributes" "-"

# 5) elimina token
A -X DELETE "$BASE/api/ingest-tokens/$HANDLE" | grep -q '"ok":true' && ok "token eliminato" || no "delete token" "-"
# il token eliminato non autentica piu'
C=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/ingress/telemetry?token=$TOK&id=SENSOR-$RUN" -H 'content-type: application/json' -d '{"values":{"temperature":10}}')
[ "$C" = "401" ] && ok "token revocato -> 401" || no "revoked token" "$C"

# 6) RBAC: operatore non gestisce i token
C=$(O -o /dev/null -w '%{http_code}' "$BASE/api/ingest-tokens")
[ "$C" = "403" ] && ok "operatore non vede i token (403)" || no "rbac tokens" "$C"

# cleanup device + regola
A -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
[ -n "${RID:-}" ] && A -X DELETE "$BASE/api/sensor-rules/$RID" >/dev/null 2>&1 || true

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
