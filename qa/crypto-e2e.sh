#!/usr/bin/env bash
# Encryption-at-rest dei segreti. YouWorkforce non si collega a sistemi esterni: l'unico segreto
# esterno è la chiave del provider AI. Verifica su un tenant usa-e-getta che la chiave:
#   1) sia salvata cifrata (enc:v1:...) e mai in chiaro nel DB;
#   2) non venga mai restituita dall'API (solo il flag has_api_key);
#   3) venga rimossa insieme al tenant.
# Richiede core avviato con YW_SECRET_KEY.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$SJ" | tail -1)
sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }

R=$RANDOM$RANDOM; SLUG="crypto$R"; SECRET="sk-e2e-crypto-$R"
TID=$(sapi -X POST "$BASE/api/superadmin/tenants" -d "{\"slug\":\"$SLUG\",\"name\":\"Crypto $R\",\"admin_email\":\"admin@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" | jv "d.get('id','')")
[ -n "$TID" ] && ok "tenant usa-e-getta creato" || { no "creazione tenant" "-"; echo "== $PASS passed, $FAIL failed =="; exit 1; }

sapi -X POST "$BASE/api/superadmin/tenants/$TID/ai" -d "{\"base_url\":\"http://localhost:9099/\",\"api_key\":\"$SECRET\",\"model\":\"youworker-1\"}" | grep -q '"ok":true' \
  && ok "chiave AI impostata" || no "set chiave AI" "-"

STORED=$(sql "SELECT api_key FROM app.ai_connections WHERE tenant_id='$TID'")
case "$STORED" in
  enc:v1:*) ok "chiave AI cifrata a riposo (enc:v1:)";;
  *)        no "chiave AI NON cifrata" "${STORED:0:12}";;
esac
[ "$(sql "SELECT count(*) FROM app.ai_connections WHERE api_key LIKE '%$SECRET%'")" = "0" ] \
  && ok "nessuna copia in chiaro nel DB" || no "chiave in chiaro presente" "-"

G=$(sapi "$BASE/api/superadmin/tenants/$TID/ai")
echo "$G" | grep -q "$SECRET" && no "API espone la chiave" "$G" || ok "API non espone mai la chiave"
echo "$G" | grep -q '"has_api_key":true' && ok "API espone solo il flag has_api_key" || no "flag has_api_key assente" "$G"

C=$(curl -s -o /dev/null -w '%{http_code}' -b "$SJ" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}")
[ "$(sql "SELECT count(*) FROM app.ai_connections WHERE tenant_id='$TID'")" = "0" ] \
  && ok "chiave rimossa con il tenant (HTTP $C)" || no "chiave orfana dopo eliminazione" "$C"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
