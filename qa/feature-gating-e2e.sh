#!/usr/bin/env bash
# SL-20 / CA-21 / FE-12 (25/11/2026): matrice unica funzione -> moduli (modules::FEATURE_MODULES).
#  - SOS dall'app accettato solo se un modulo porta Centrale ed escalation (altrimenti 403 chiaro)
#  - squadre con turni O cantieri; piani dei turni solo con workforce
#  - /api/login-info pubblico: dice solo se l'ambiente è demo
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
sa(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' "$@"; }
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="fg$R"
TID=$(sa -X POST "$BASE/api/superadmin/tenants" -d "{\"slug\":\"$SLUG\",\"name\":\"Fg $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\"]}" | py "d.get('id','')")
trap 'sa -o /dev/null -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
call A POST /api/users "{\"email\":\"tec@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" >/dev/null
login T "tec@$SLUG.test" WorkPass9
setmod(){ sa -o /dev/null -X POST "$BASE/api/superadmin/tenants/$TID/modules" -d "{\"module_code\":\"$1\",\"state\":\"$2\"}"; }

echo "== SL-20 SOS dall'app =="
C=$(code T POST /api/sos '{"client_key":"fg-1"}'); [ "$C" = "403" ] && ok "solo Interventi: SOS dall'app rifiutato (nessuno lo vedrebbe)" || no "sos senza moduli" "$C"
call T POST /api/sos '{"client_key":"fg-1"}' | grep -q "chiama subito i soccorsi" && ok "il rifiuto dice di chiamare i soccorsi" || no "msg" "-"
setmod gps attivo
C=$(code T POST /api/sos '{"client_key":"fg-2"}'); [ "$C" = "200" ] && ok "con GPS (Centrale disponibile): SOS accettato" || no "sos gps" "$C"
setmod gps non_abilitato
C=$(code T POST /api/worker/heartbeat '{}'); [ "$C" = "403" ] && ok "battito dell'app solo con Lone Worker" || no "heartbeat" "$C"
setmod loneworker attivo
C=$(code T POST /api/shifts/start '{}'); [ "$C" = "403" ] && ok "turno nell'app solo con Gestione turni (anche con Lone Worker attivo)" || no "app shift" "$C"
setmod loneworker non_abilitato

echo "== CA-21 squadre e turni =="
C=$(code A POST /api/teams '{"name":"Squadra"}'); [ "$C" = "403" ] && ok "squadre senza turni né cantieri: 403" || no "teams off" "$C"
setmod cantieri attivo
C=$(code A POST /api/teams "{\"name\":\"Squadra $R\"}"); [ "$C" = "200" ] && ok "squadre con Cantieri: create" || no "teams cantieri" "$C"
C=$(code A GET /api/teams); [ "$C" = "200" ] && ok "elenco squadre con solo Cantieri: 200 (prima 403)" || no "list teams" "$C"
C=$(code A POST /api/shift-plans '{"user_id":"00000000-0000-0000-0000-000000000000","starts_at":"2026-12-01T08:00:00Z","ends_at":"2026-12-01T16:00:00Z"}')
[ "$C" = "403" ] && ok "piano turni senza Workforce: 403" || no "shift off" "$C"

echo "== FE-12 login =="
curl -s "$BASE/api/login-info" | py "isinstance(d.get('demo'),bool) and len(d)==1" | grep -q True && ok "login-info pubblico: solo il flag demo" || no "login-info" "-"

echo "feature-gating: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
