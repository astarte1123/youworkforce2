#!/usr/bin/env bash
# Hardening: protezione CSRF (double-submit) sulle mutazioni cookie-autenticate.
# Il cookie yw_csrf (leggibile da JS) deve combaciare con l'header X-CSRF-Token.
# Gli endpoint macchina (ingest token, nessun cookie) e login/logout sono esenti.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }

J=$(mktemp)
curl -s -o /dev/null -c "$J" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' \
  -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
TOK=$(csrfh "$J")
[ -n "$TOK" ] && ok "login imposta il cookie CSRF (yw_csrf)" || no "csrf cookie" "assente"

post(){ # extra-headers... -> http code for a cookie-auth mutation
  curl -s -o /dev/null -w "%{http_code}" -b "$J" -H 'x-tenant-slug: acme' -H 'content-type: application/json' "$@" \
    -X POST "$BASE/api/shifts/end" -d '{}'; }

# pulizia: termina eventuale turno aperto (con token valido)
curl -s -o /dev/null -b "$J" -H 'x-tenant-slug: acme' -H "x-csrf-token: $TOK" -X POST "$BASE/api/shifts/start" -d '{}'

NOCSRF=$(post)
[ "$NOCSRF" = "403" ] && ok "mutazione cookie-auth SENZA header CSRF -> 403" || no "missing csrf" "$NOCSRF"

BAD=$(post -H 'x-csrf-token: valore-sbagliato')
[ "$BAD" = "403" ] && ok "header CSRF errato -> 403" || no "wrong csrf" "$BAD"

# con token corretto: deve passare il gate (200 termina, o 409 se già terminato — comunque NON 403)
OKC=$(post -H "x-csrf-token: $TOK")
[ "$OKC" != "403" ] && ok "header CSRF corretto -> passa il gate ($OKC)" || no "valid csrf" "$OKC"

# endpoint macchina (ingest token, nessun cookie): CSRF non applicabile
CORR="csrf-ing-$(date +%s)"
ING=$(curl -s -X POST "$BASE/api/events" -H 'content-type: application/json' -H 'x-ingest-token: acme-ingest-token' \
  -d "{\"etype\":\"sos\",\"source_event\":\"$CORR\",\"idem_key\":\"$CORR\",\"correlation\":\"$CORR\"}")
echo "$ING" | grep -q incident_id && ok "ingress macchina non impattato dal CSRF" || no "machine ingress" "$ING"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
