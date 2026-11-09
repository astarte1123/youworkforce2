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
A "$BASE/api/connectors" | python -c "import sys,json;d=json.load(sys.stdin);t=next(c for c in d if c['code']=='traccar');assert t['state']=='attivo',t;assert t['tokens']>=1,t;print('ok')" >/dev/null 2>&1 \
  && ok "connettori: traccar attivo con token" || no "connectors" "$(A "$BASE/api/connectors"|head -c 160)"

# 2) crea ingest-token per thingsboard
TOK=$(A -X POST "$BASE/api/ingest-tokens" -d '{"source":"thingsboard"}' | jget 'd["token"]')
[ -n "$TOK" ] && ok "ingest-token thingsboard creato" || no "create token" "$TOK"
A "$BASE/api/ingest-tokens" | grep -q "$TOK" && ok "token in lista" || no "list tokens" "-"

# 3) il nuovo token risolve il tenant: ingest thingsboard critico -> incidente
INC=$(curl -s -X POST "$BASE/api/ingress/thingsboard?token=$TOK&device=SENSOR-$RUN" -H 'content-type: application/json' -d '{"temperature":120}' | jget 'd.get("incident_id")')
[ -n "$INC" ] && [ "$INC" != "None" ] && ok "ingest col nuovo token -> incidente ($INC)" || no "ingest new token" "$INC"

# 4) stato dispositivo: registra device + ingest -> ultimo contatto
DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"SENSOR-LS-$RUN\",\"source\":\"thingsboard\",\"label\":\"Sensore LS\"}" | jget 'd["id"]')
curl -s -o /dev/null -X POST "$BASE/api/ingress/thingsboard?token=$TOK&device=SENSOR-LS-$RUN" -H 'content-type: application/json' -d '{"temperature":20}'
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);dev=next(x for x in d if x['id']=='$DID');assert dev['last_seen'],dev;print('ok')" >/dev/null 2>&1 \
  && ok "dispositivo: ultimo contatto registrato" || no "device last_seen" "-"

# 5) elimina token
A -X DELETE "$BASE/api/ingest-tokens/$TOK" | grep -q '"ok":true' && ok "token eliminato" || no "delete token" "-"
# il token eliminato non autentica piu'
C=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/ingress/thingsboard?token=$TOK" -H 'content-type: application/json' -d '{"temperature":10}')
[ "$C" = "401" ] && ok "token revocato -> 401" || no "revoked token" "$C"

# 6) RBAC: operatore non gestisce i token
C=$(O -o /dev/null -w '%{http_code}' "$BASE/api/ingest-tokens")
[ "$C" = "403" ] && ok "operatore non vede i token (403)" || no "rbac tokens" "$C"

# cleanup device
A -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
