#!/usr/bin/env bash
# #7 Filtri posizione: scarta salti implausibili e spostamenti sotto soglia (rumore).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"; EXT="FILT-$RUN"
JA=$(mktemp); curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
# assicura config default filtri
A -X POST "$BASE/api/modules/traccar/tools/alarm_map" -d '{"config":{"sos_as":"sos","max_jump_kmh":300,"min_move_m":5}}' >/dev/null
DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"$EXT\",\"source\":\"traccar\",\"label\":\"Filt\"}" | jget 'd["id"]')
pos(){ curl -s -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
  -d "{\"device\":{\"uniqueId\":\"$EXT\"},\"position\":{\"id\":\"$1\",\"latitude\":$2,\"longitude\":$3,\"fixTime\":\"$4\"}}"; }

# 1a posizione: accettata
pos "f1" 45.4641 9.1919 "2026-11-10T10:00:00Z" | grep -q '"ok":true' && ok "prima posizione accettata" || no "first" "-"
# salto: 100+ km in 1 secondo -> scartato
pos "f2" 46.4641 10.1919 "2026-11-10T10:00:01Z" | grep -q '"filtered":"salto' && ok "salto implausibile scartato" || no "jump" "$(pos f2b 46.5 10.2 2026-11-10T10:00:01Z)"
# rumore: ~1 m dopo 30 s -> scartato
pos "f3" 45.46411 9.19190 "2026-11-10T10:00:30Z" | grep -q '"filtered":"spostamento' && ok "spostamento sotto soglia scartato" || no "noise" "$(pos f3b 45.46411 9.19190 2026-11-10T10:00:35Z)"
# movimento legittimo (tanto tempo) -> accettato
pos "f4" 45.5200 9.2500 "2026-11-10T12:00:00Z" | grep -q '"ok":true' && ok "movimento legittimo accettato" || no "legit" "-"
# ultima posizione resta quella legittima (non il salto)
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');assert abs(x['last_lat']-45.52)<1e-6,x;print('ok')" >/dev/null 2>&1 \
  && ok "ultima posizione = movimento valido (no salto)" || no "last pos" "-"

A -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
