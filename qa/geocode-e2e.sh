#!/usr/bin/env bash
# #4 Geocoding inverso (Nominatim/OSM) con cache. Richiede internet per il primo lookup.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
# coordinate stabili (Duomo di Milano), arrotondate a 4 decimali per la cache
LAT=45.4641; LON=9.1919
JO=$(mktemp); curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
O(){ curl -s -b "$JO" -H 'x-tenant-slug: acme' "$@"; }

# auth richiesta
C=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/geocode/reverse?lat=$LAT&lon=$LON")
[ "$C" = "401" ] && ok "senza sessione -> 401" || no "auth" "$C"
# coordinate invalide -> 400
C=$(O -o /dev/null -w '%{http_code}' "$BASE/api/geocode/reverse?lat=999&lon=9")
[ "$C" = "400" ] && ok "coordinate fuori range -> 400" || no "range" "$C"
# primo lookup
R1=$(O "$BASE/api/geocode/reverse?lat=$LAT&lon=$LON")
echo "$R1" | python -c 'import sys,json;d=json.load(sys.stdin);assert d.get("address");print("ok")' >/dev/null 2>&1 \
  && ok "indirizzo risolto ($(echo "$R1"|python -c 'import sys,json;print(json.load(sys.stdin)["address"][:40])'))" || no "lookup (serve internet?)" "$R1"
# secondo lookup -> dalla cache
O "$BASE/api/geocode/reverse?lat=$LAT&lon=$LON" | grep -q '"cached":true' && ok "secondo lookup servito dalla cache" || no "cache" "-"

echo "== $PASS passed, $FAIL failed =="; exit $FAIL
