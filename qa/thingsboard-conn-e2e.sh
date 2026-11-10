#!/usr/bin/env bash
# THINGSBOARD — connessione gestita DA YouWorkforce (superadmin): set/get mascherato,
# password cifrata a riposo, verifica collegamento reachable-aware, update senza pw.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$SJ" | tail -1)
api(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
TID=$(api "$BASE/api/superadmin/tenants" | jv '[t["id"] for t in d if t["slug"]=="acme"][0]')
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
# salva stato preesistente per ripristino
HAD=$(sql "SELECT count(*) FROM app.thingsboard_connections WHERE tenant_id=$TIDQ")

api -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/thingsboard" -d '{"base_url":"http://localhost:9090/","username":"e2e@tb.test","password":"e2esecret"}' | grep -q '"ok":true' && ok "set connessione TB" || no "set fallito" "-"
G=$(api "$BASE/api/superadmin/tenants/$TID/thingsboard")
echo "$G" | grep -q '"configured":true' && ok "get: configurata" || no "get non configurata" "$G"
echo "$G" | grep -q '"has_password":true' && ok "get: password mascherata (has_password, nessun valore)" || no "password non mascherata" "$G"
echo "$G" | grep -q 'e2esecret' && no "password in chiaro nel get!" "$G" || ok "password NON esposta nel get"
[ "$(sql "SELECT left(password,7) FROM app.thingsboard_connections WHERE tenant_id=$TIDQ")" = "enc:v1:" ] && ok "password cifrata a riposo (enc:v1)" || no "password non cifrata" "-"
# trailing slash normalizzato
[ "$(sql "SELECT base_url FROM app.thingsboard_connections WHERE tenant_id=$TIDQ")" = "http://localhost:9090" ] && ok "base URL normalizzato (no slash finale)" || no "slash non rimosso" "-"
# verify reachable-aware (nessun TB attivo qui)
V=$(api -X POST "$BASE/api/superadmin/tenants/$TID/thingsboard/verify")
echo "$V" | grep -q '"reachable"' && ok "verify risponde reachable-aware ($(echo "$V" | jv 'd["reachable"]'))" || no "verify senza reachable" "$V"
# update senza password -> mantiene la password
api -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/thingsboard" -d '{"base_url":"http://localhost:9091","username":"e2e@tb.test"}' >/dev/null
[ "$(api "$BASE/api/superadmin/tenants/$TID/thingsboard" | jv 'd["has_password"]')" = "True" ] && ok "update senza pw mantiene la password" || no "password persa su update" "-"

# ripristino: se non c'era connessione prima, rimuovi quella di test
[ "$HAD" = "0" ] && sql "DELETE FROM app.thingsboard_connections WHERE tenant_id=$TIDQ" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
