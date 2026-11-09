#!/usr/bin/env bash
# Traccar P4 — sync device (stato online/offline + id Traccar nel nostro inventario)
# e comandi con esito tracciato. UPSTREAM REALE (Traccar 8083).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }

JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
CS=$(awk '$6=="yw_csrf"{print $7}' "$JA" | tail -1)
hdr=(-b "$JA" -H 'x-tenant-slug: acme')

# forza un poll (sync) via verify (serve admin -> login separato)
JB=$(mktemp)
curl -s -o /dev/null -c "$JB" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CB=$(awk '$6=="yw_csrf"{print $7}' "$JB" | tail -1)
curl -s -o /dev/null -b "$JB" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CB" -X POST "$BASE/api/traccar/verify"
sleep 2

# 1) sync: il device Traccar risulta nel nostro inventario con stato + id numerico
ST=$(sql "SELECT traccar_status FROM app.devices WHERE source='traccar' AND external_id='BADGE-001'")
TID=$(sql "SELECT traccar_device_id FROM app.devices WHERE source='traccar' AND external_id='BADGE-001'")
[ -n "$ST" ] && [ "$ST" != "" ] && ok "sync device: traccar_status='$ST'" || no "sync status" "vuoto"
[ -n "$TID" ] && ok "sync device: id Traccar mappato ($TID)" || no "sync id" "vuoto"

# 2) il device appare in /api/devices con traccar_status
DID=$(curl -s "${hdr[@]}" "$BASE/api/devices" | python -c "import sys,json;d=[x for x in json.load(sys.stdin) if x['external_id']=='BADGE-001'];print(d[0]['id'] if d else '')")
curl -s "${hdr[@]}" "$BASE/api/devices" | python -c "import sys,json;d=[x for x in json.load(sys.stdin) if x['external_id']=='BADGE-001'][0];exit(0 if d.get('traccar_status') else 1)" \
  && ok "list_devices espone traccar_status" || no "traccar_status in list" "-"

# 3) comando + esito tracciato
R=$(curl -s "${hdr[@]}" -H "x-csrf-token: $CS" -H 'content-type: application/json' -X POST "$BASE/api/devices/$DID/command" -d '{"type":"positionSingle"}')
echo "$R" | python -c 'import sys,json;d=json.load(sys.stdin);exit(0 if 200<=d.get("traccar_status",0)<300 else 1)' \
  && ok "comando positionSingle inviato (202 Traccar)" || no "comando" "$R"

# 4) storico comandi contiene l'invio con esito
H=$(curl -s "${hdr[@]}" "$BASE/api/devices/$DID/commands")
echo "$H" | python -c 'import sys,json;d=json.load(sys.stdin);exit(0 if any(c["ctype"]=="positionSingle" and c.get("result") for c in d) else 1)' \
  && ok "storico comandi: esito tracciato (result)" || no "storico" "$H"

# 5) tipi comando disponibili dal device (via Traccar)
T=$(curl -s "${hdr[@]}" "$BASE/api/devices/$DID/command-types")
echo "$T" | python -c 'import sys,json;json.load(sys.stdin)' >/dev/null 2>&1 \
  && ok "command-types raggiungibile (array da Traccar)" || no "command-types" "$T"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
