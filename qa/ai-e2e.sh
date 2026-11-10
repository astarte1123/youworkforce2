#!/usr/bin/env bash
# youworker.ai (piattaforma AI di YouCo) — integrazione DA YouWorkforce:
# connessione cifrata/mascherata/verify (superadmin), feature per-processo (tenant_admin),
# gating, assistente con grounding deterministico, endpoint per-processo reachable-aware.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

# --- superadmin ---
SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$SJ" | tail -1)
sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
TID=$(sapi "$BASE/api/superadmin/tenants" | jv '[t["id"] for t in d if t["slug"]=="acme"][0]')
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
HAD=$(sql "SELECT count(*) FROM app.ai_connections WHERE tenant_id=$TIDQ")

# --- tenant sessions ---
login(){ local jar=$1 email=$2; curl -s -o /dev/null -c "$jar" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$email\",\"password\":\"demo\"}"; }
csrf(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
AD=$(mktemp); login "$AD" "admin@acme.loneworker.eu"; AC=$(csrf "$AD")
aapi(){ curl -s -b "$AD" -H 'x-tenant-slug: acme' -H "x-csrf-token: $AC" "$@"; }
OP=$(mktemp); login "$OP" "operatore@acme.loneworker.eu"; OC=$(csrf "$OP")
oapi(){ curl -s -b "$OP" -H 'x-tenant-slug: acme' -H "x-csrf-token: $OC" "$@"; }

# 1) connessione: set
sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/ai" \
  -d '{"base_url":"http://localhost:9099/","api_key":"sk-e2e-secret","model":"youworker-1"}' | grep -q '"ok":true' && ok "set connessione youworker.ai" || no "set fallito" "-"
# 2) get mascherato
G=$(sapi "$BASE/api/superadmin/tenants/$TID/ai")
echo "$G" | grep -q '"configured":true' && ok "get: configurata" || no "get non configurata" "$G"
echo "$G" | grep -q '"has_api_key":true' && ok "get: api key mascherata (flag, nessun valore)" || no "api key non mascherata" "$G"
echo "$G" | grep -q 'sk-e2e-secret' && no "api key in chiaro nel get!" "$G" || ok "api key NON esposta nel get"
# 3) cifrata a riposo
[ "$(sql "SELECT left(api_key,7) FROM app.ai_connections WHERE tenant_id=$TIDQ")" = "enc:v1:" ] && ok "api key cifrata a riposo (enc:v1)" || no "api key non cifrata" "-"
# 4) base URL normalizzato
[ "$(sql "SELECT base_url FROM app.ai_connections WHERE tenant_id=$TIDQ")" = "http://localhost:9099" ] && ok "base URL normalizzato (no slash finale)" || no "slash non rimosso" "-"
# 5) verify reachable-aware (nessun server AI qui)
V=$(sapi -X POST "$BASE/api/superadmin/tenants/$TID/ai/verify")
echo "$V" | grep -q '"reachable"' && ok "verify reachable-aware ($(echo "$V" | jv 'd["reachable"]'))" || no "verify senza reachable" "$V"
# 6) update senza api_key mantiene la chiave
sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/ai" -d '{"base_url":"http://localhost:9099","model":"youworker-2"}' >/dev/null
[ "$(sapi "$BASE/api/superadmin/tenants/$TID/ai" | jv 'd["has_api_key"]')" = "True" ] && ok "update senza api_key mantiene la chiave" || no "chiave persa su update" "-"

# 7) feature: lettura (tenant_admin)
F=$(aapi "$BASE/api/ai/features")
echo "$F" | grep -q '"connection_configured":true' && ok "features: connessione vista configurata" || no "connection_configured errato" "$F"
# 8) feature sconosciuta -> 400
R=$(aapi -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"pippo","enabled":true}')
[ "$R" = "400" ] && ok "feature sconosciuta -> 400" || no "feature sconosciuta accettata" "$R"
# 9) operatore non puo' impostare feature -> 403
R=$(oapi -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"assistant","enabled":true}')
[ "$R" = "403" ] && ok "operatore non imposta feature -> 403" || no "operatore ha impostato feature" "$R"

# 10) processo con feature OFF -> 403 (sintesi incidente). Con provider configurato le feature
# sono ON di default: qui disattivo ESPLICITAMENTE incident_summary per testare il gating OFF.
aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"incident_summary","enabled":false}' >/dev/null
IID=$(sql "SELECT id FROM app.incidents WHERE tenant_id=$TIDQ ORDER BY opened_at DESC LIMIT 1")
if [ -n "$IID" ]; then
  R=$(oapi -o /dev/null -w '%{http_code}' "$BASE/api/incidents/$IID/ai-summary")
  [ "$R" = "403" ] && ok "sintesi incidente feature OFF -> 403" || no "sintesi non gated" "$R"
fi

# 11) abilita assistant + incident_summary
aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"assistant","enabled":true}' | grep -q '"ok":true' && ok "abilita assistant" || no "abilita assistant fallito" "-"
aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"incident_summary","enabled":true}' >/dev/null

# 12) assistente: grounding resta (server AI giu' -> fallback deterministico, mai errore)
A=$(oapi -H 'content-type: application/json' -X POST "$BASE/api/assistant" -d '{"q":"check-in scaduti"}')
echo "$A" | grep -q '"provider"' && ok "assistente risponde (grounding deterministico anche con AI giu')" || no "assistente non risponde" "$A"
echo "$A" | grep -q '"sources"' && ok "assistente cita le fonti" || no "assistente senza fonti" "$A"
# 13) assistente rifiuta azione critica anche con AI on
A2=$(oapi -H 'content-type: application/json' -X POST "$BASE/api/assistant" -d '{"q":"chiudi l allarme SOS"}')
echo "$A2" | grep -q 'critical_refused' && ok "assistente rifiuta azione critica (AI on)" || no "azione critica non rifiutata" "$A2"

# 14) processo con feature ON ma server AI giu' -> 502 (onesto, non finto)
if [ -n "$IID" ]; then
  R=$(oapi -o /dev/null -w '%{http_code}' "$BASE/api/incidents/$IID/ai-summary")
  [ "$R" = "502" ] && ok "sintesi incidente feature ON, AI giu' -> 502 (onesto)" || no "sintesi non 502" "$R"
fi

# ripristino: cancella le righe feature create dal test (così tornano al default-ON del prodotto)
sql "DELETE FROM app.ai_features WHERE tenant_id=$TIDQ AND feature IN ('assistant','incident_summary')" >/dev/null
[ "$HAD" = "0" ] && sql "DELETE FROM app.ai_connections WHERE tenant_id=$TIDQ" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
