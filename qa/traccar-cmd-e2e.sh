#!/usr/bin/env bash
# #2 Comandi al device via API Traccar: connessione per-tenant, risoluzione uniqueId->id,
# invio comando (relay esito Traccar). Richiede Traccar reale su :8083 con BADGE-001.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
TR_BASE="${TR_BASE:-http://localhost:8083}"; TR_USER="${TR_USER:-op@acme.test}"; TR_PASS="${TR_PASS:-Passw0rd!}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"

JS=$(mktemp); JA=$(mktemp); JO=$(mktemp)
curl -s -o /dev/null -c "$JS" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
CS=$(csrfh "$JS"); CA=$(csrfh "$JA"); CO=$(csrfh "$JO")
TID=$(curl -s -b "$JS" "$BASE/api/superadmin/tenants" | python -c "import sys,json
for t in json.load(sys.stdin):
 if t['slug']=='acme':print(t['id'])")

# superadmin configura la connessione Traccar del tenant
curl -s -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/traccar" \
  -d "{\"base_url\":\"$TR_BASE\",\"username\":\"$TR_USER\",\"password\":\"$TR_PASS\"}" | grep -q '"ok":true' \
  && ok "connessione Traccar configurata (superadmin)" || no "set conn" "-"

# registra il device BADGE-001 lato nostro (source traccar)
DID=$(curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' -X POST "$BASE/api/devices" \
  -d '{"external_id":"BADGE-001","source":"traccar","label":"Badge Mario"}' | python -c 'import sys,json;d=json.load(sys.stdin);print(d.get("id",""))')
if [ -z "$DID" ]; then # gia presente: recuperane l'id
  DID=$(curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/devices" | python -c 'import sys,json;print(next(v["id"] for v in json.load(sys.stdin) if v["external_id"]=="BADGE-001" and v["source"]=="traccar"))')
fi
[ -n "$DID" ] && ok "device BADGE-001 presente ($DID)" || no "device" "-"

# operatore invia comando: risolve uniqueId->id numerico Traccar
R=$(curl -s -b "$JO" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CO" -H 'content-type: application/json' -X POST "$BASE/api/devices/$DID/command" -d '{"type":"positionSingle"}')
echo "$R" | python -c 'import sys,json;d=json.load(sys.stdin);assert d.get("resolved_device_id")==1,d;print("ok")' >/dev/null 2>&1 \
  && ok "comando: uniqueId risolto a id Traccar 1 + esito relayato" || no "command" "$R"
echo "   (traccar_status=$(echo "$R"|python -c 'import sys,json;print(json.load(sys.stdin).get("traccar_status"))'))"

# RBAC: lavoratore non comanda
JM=$(mktemp); curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CM=$(csrfh "$JM")
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' -X POST "$BASE/api/devices/$DID/command" -d '{"type":"positionSingle"}')
[ "$C" = "403" ] && ok "lavoratore non invia comandi (403)" || no "rbac" "$C"

echo "== $PASS passed, $FAIL failed =="; exit $FAIL
