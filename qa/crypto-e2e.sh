#!/usr/bin/env bash
# P3 — encryption-at-rest dei segreti upstream (password Traccar, gql_pass GoAlert).
# Richiede core avviato con YW_SECRET_KEY. Verifica che i valori nel DB siano cifrati
# ("enc:v1:...") e che le integrazioni funzionino ancora (decifratura trasparente).
set -uo pipefail
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }

# 1) password Traccar cifrata a riposo
TR=$(sql "SELECT password FROM app.traccar_connections LIMIT 1")
case "$TR" in
  enc:v1:*) ok "password Traccar cifrata a riposo (enc:v1:)";;
  "")       no "nessuna connessione Traccar" "configura prima";;
  *)        no "password Traccar in chiaro" "$TR";;
esac

# 2) gql_pass GoAlert cifrata a riposo (se presente)
GA=$(sql "SELECT gql_pass FROM app.goalert_services WHERE gql_pass IS NOT NULL LIMIT 1")
case "$GA" in
  enc:v1:*) ok "gql_pass GoAlert cifrata a riposo (enc:v1:)";;
  "")       ok "nessun gql_pass salvato (niente da cifrare)";;
  *)        no "gql_pass in chiaro" "$GA";;
esac

# 3) decifratura trasparente: il poller Traccar fa login con la password decifrata
#    (se fosse decifrata male, reachable sarebbe false)
JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "http://localhost:8080/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
R=$(curl -s -b "$JA" -H 'x-tenant-slug: acme' "http://localhost:8080/api/traccar/status")
echo "$R" | python -c 'import sys,json;exit(0 if json.load(sys.stdin).get("reachable") else 1)' \
  && ok "login Traccar OK con password decifrata (decrypt trasparente)" || no "decrypt rotto" "$R"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
