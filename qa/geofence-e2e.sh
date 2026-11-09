#!/usr/bin/env bash
# #3 Geofence + presenza: CRUD, calcolo dentro/fuori sulle posizioni, presenza corrente.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"; EXT="GEO-$RUN"

JA=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); CM=$(csrfh "$JM")
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
pos(){ curl -s -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
  -d "{\"device\":{\"uniqueId\":\"$EXT\"},\"position\":{\"id\":\"$1\",\"latitude\":$2,\"longitude\":$3,\"fixTime\":\"$4\"}}"; }
presin(){ A "$BASE/api/presence" | python -c "import sys,json;print(any(p['geofence_id']=='$1' and p['external_id']=='$EXT' for p in json.load(sys.stdin)))"; }

DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"$EXT\",\"source\":\"traccar\",\"label\":\"Geo badge\"}" | jget 'd["id"]')
# cerchio attorno al Duomo di Milano (raggio 500 m)
GID=$(A -X POST "$BASE/api/geofences" -d "{\"name\":\"Cantiere $RUN\",\"kind\":\"circle\",\"geom\":{\"lat\":45.4641,\"lon\":9.1919,\"radius\":500}}" | jget 'd["id"]')
[ -n "$GID" ] && ok "geofence cerchio creata" || no "create circle" "$GID"
A "$BASE/api/geofences" | grep -q "$GID" && ok "geofence in lista" || no "list" "-"

# posizione DENTRO -> ingresso (>=1 cambio; possono esistere altre geofence sovrapposte)
R=$(pos "g1" 45.4642 9.1920 "2026-11-10T10:00:00Z"); echo "$R" | grep -q '"presence_changes":0' && no "enter" "$R" || ok "ingresso rilevato (cambio presenza)"
[ "$(presin "$GID")" = "True" ] && ok "presenza: device DENTRO il cantiere" || no "presence inside" "-"
# posizione FUORI (50 km) -> uscita
R=$(pos "g2" 46.00 10.00 "2026-11-10T11:00:00Z"); echo "$R" | grep -q '"presence_changes":0' && no "exit" "$R" || ok "uscita rilevata (cambio presenza)"
[ "$(presin "$GID")" = "False" ] && ok "presenza: device FUORI (non piu' elencato)" || no "presence outside" "-"
# resta fuori, nessun cambio
R=$(pos "g3" 46.001 10.001 "2026-11-10T12:00:00Z"); echo "$R" | grep -q '"presence_changes":0' && ok "nessun cambio presenza se resta fuori" || no "no change" "$R"

# poligono: quadrato attorno a (45.50,9.20)
GP=$(A -X POST "$BASE/api/geofences" -d "{\"name\":\"Area $RUN\",\"kind\":\"polygon\",\"geom\":{\"points\":[[45.49,9.19],[45.49,9.21],[45.51,9.21],[45.51,9.19]]}}" | jget 'd["id"]')
R=$(pos "g4" 45.50 9.20 "2026-11-10T13:00:00Z"); [ "$(presin "$GP")" = "True" ] && ok "poligono: punto interno -> dentro" || no "polygon inside" "$R"

# validazioni + RBAC
C=$(A -o /dev/null -w '%{http_code}' -X POST "$BASE/api/geofences" -d '{"name":"x","kind":"polygon","geom":{"points":[[1,1],[2,2]]}}')
[ "$C" = "400" ] && ok "poligono <3 punti -> 400" || no "poly validation" "$C"
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JM" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CM" -H 'content-type: application/json' -X POST "$BASE/api/geofences" -d '{"name":"y","kind":"circle","geom":{"lat":1,"lon":1,"radius":10}}')
[ "$C" = "403" ] && ok "lavoratore non crea geofence (403)" || no "rbac" "$C"

# cleanup
A -X DELETE "$BASE/api/geofences/$GID" >/dev/null; A -X DELETE "$BASE/api/geofences/$GP" >/dev/null; A -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
