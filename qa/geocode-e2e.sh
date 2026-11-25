#!/usr/bin/env bash
# TA-15 (decisione PO 25/11/2026): il server NON fa geocoding (nessuna connessione esterna, nessuna
# posizione inviata a terzi). L'indirizzo lo chiede il browser dell'operatore su richiesta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
JO=$(mktemp); curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/geocode/reverse?lat=45.4641&lon=9.1919")
[ "$C" = "404" ] && ok "il server non espone più il geocoding (404)" || no "endpoint ancora attivo" "$C"
[ "$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "SELECT to_regclass('app.geocode_cache') IS NULL" | tr -d '[:space:]')" = "t" ] \
  && ok "cache degli indirizzi eliminata dal database" || no "cache" "-"
grep -q "nominatim.openstreetmap.org/reverse" console/static/app.html && grep -q "Mostra indirizzo" console/static/app.html \
  && ok "la console chiede l'indirizzo dal browser, su richiesta" || no "console" "-"
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
