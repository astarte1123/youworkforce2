#!/usr/bin/env bash
# Modulo spento dal superadmin -> la gestione sparisce anche via API (403).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
JS=$(mktemp); JA=$(mktemp)
curl -s -o /dev/null -c "$JS" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CS=$(csrfh "$JS")
TID=$(curl -s -b "$JS" "$BASE/api/superadmin/tenants" | python -c "import sys,json
for t in json.load(sys.stdin):
 if t['slug']=='acme':print(t['id'])")
setmod(){ curl -s -o /dev/null -b "$JS" -H "x-csrf-token: $CS" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/modules" -d "{\"module_code\":\"$1\",\"state\":\"$2\"}"; }
code(){ curl -s -o /dev/null -w '%{http_code}' -b "$JA" -H 'x-tenant-slug: acme' "$BASE$1"; }

# WORKFORCE
setmod workforce non_abilitato
[ "$(code /api/teams)" = "403" ] && [ "$(code /api/shift-plans)" = "403" ] && [ "$(code /api/oncall)" = "403" ] \
  && ok "workforce spento -> teams/turni/reperibilita API 403" || no "workforce off" "teams=$(code /api/teams) plans=$(code /api/shift-plans)"
setmod workforce attivo
[ "$(code /api/teams)" = "200" ] && ok "workforce riacceso -> teams API 200" || no "workforce on" "$(code /api/teams)"

# INTERVENTI (modulo singolo: contratto on/off pulito; i connettori esterni sono ritirati)
setmod interventi non_abilitato
[ "$(code /api/orders)" = "403" ] \
  && ok "interventi spento -> ordini API 403" || no "interventi off" "ord=$(code /api/orders)"
setmod interventi attivo
[ "$(code /api/orders)" = "200" ] && ok "interventi riacceso -> ordini API 200" || no "interventi on" "$(code /api/orders)"

# CONSUNTIVI (catena già gated)
setmod consuntivi non_abilitato
[ "$(code /api/timesheets)" = "403" ] && ok "consuntivi spento -> consuntivi API 403" || no "consuntivi off" "$(code /api/timesheets)"
setmod consuntivi attivo
[ "$(code /api/timesheets)" = "200" ] && ok "consuntivi riacceso -> API 200" || no "consuntivi on" "$(code /api/timesheets)"

echo "== $PASS passed, $FAIL failed =="; exit $FAIL
