#!/usr/bin/env bash
# Autonomia e coesistenza moduli. Verifica che "Sicurezza lavoratore" (loneworker) e "Lavoro sul
# campo" (interventi/consuntivi/workforce/giri) funzionino in modo indipendente E insieme sullo
# stesso tenant. Il gating è per-richiesta (module_active) → basta cambiare tenant_modules fra le
# chiamate, nessun re-login. Tenant throwaway, teardown finale (non tocca globex).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (got $2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1"|head -1|tr -d '[:space:]'; }
code(){ "$@" -o /dev/null -w '%{http_code}'; }

R=$RANDOM
SLUG="modtest-$R"
HASH=$(one "SELECT pass_hash FROM app.users WHERE email='admin@globex.loneworker.eu' LIMIT 1")
TID=$(one "INSERT INTO app.tenants(slug,name) VALUES('$SLUG','ModTest $R') RETURNING id")
ADM="admin-$R@mt.eu"; WRK="wrk-$R@mt.eu"
one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$ADM','$HASH','tenant_admin')" >/dev/null
one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$WRK','$HASH','lavoratore')" >/dev/null

# attiva SOLO i moduli passati (azzera prima)
setmods(){ sql "DELETE FROM app.tenant_modules WHERE tenant_id='$TID'" >/dev/null
  for m in "$@"; do sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','$m','attivo')" >/dev/null; done; }

login(){ local em=$1 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADM"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
WJ=$(login "$WRK"); WCS=$(awk '$6=="yw_csrf"{print $7}' "$WJ"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
wrk(){ curl -s -b "$WJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $WCS" "$@"; }
CK='{"minutes":5}'; DN='{"body":"nota test"}'

expect(){ local label=$1 want=$2 got=$3; [ "$got" = "$want" ] && ok "$label" || no "$label" "$got want $want"; }

echo "== Scenario A: SOLO Sicurezza lavoratore (loneworker) =="
setmods loneworker
expect "A check-in creato (safety attivo)" 200 "$(code wrk -X POST "$BASE/api/checkins" -H 'content-type: application/json' -d "$CK")"
expect "A incidenti visibili (loneworker in ESCALATION_MODS)" 200 "$(code adm "$BASE/api/incidents")"
expect "A ordini bloccati (interventi off)" 403 "$(code adm "$BASE/api/orders")"
expect "A consuntivi bloccati (consuntivi off)" 403 "$(code adm "$BASE/api/timesheets")"
expect "A squadre bloccate (workforce off)" 403 "$(code adm "$BASE/api/teams")"
expect "A diario bloccato (nessun modulo Campo)" 403 "$(code wrk -X POST "$BASE/api/diary" -H 'content-type: application/json' -d "$DN")"

echo "== Scenario B: SOLO Lavoro sul campo (interventi,consuntivi,workforce,giri) =="
setmods interventi consuntivi workforce giri
expect "B check-in bloccato (loneworker off)" 403 "$(code wrk -X POST "$BASE/api/checkins" -H 'content-type: application/json' -d "$CK")"
expect "B incidenti bloccati (nessun modulo safety)" 403 "$(code adm "$BASE/api/incidents")"
expect "B ordini visibili (interventi on)" 200 "$(code adm "$BASE/api/orders")"
expect "B consuntivi visibili (consuntivi on)" 200 "$(code adm "$BASE/api/timesheets")"
expect "B squadre visibili (workforce on)" 200 "$(code adm "$BASE/api/teams")"
expect "B diario creato (modulo Campo attivo)" 200 "$(code wrk -X POST "$BASE/api/diary" -H 'content-type: application/json' -d "$DN")"

echo "== Scenario C: ENTRAMBI (coesistenza) =="
setmods loneworker interventi consuntivi workforce giri
expect "C check-in creato" 200 "$(code wrk -X POST "$BASE/api/checkins" -H 'content-type: application/json' -d "$CK")"
expect "C incidenti visibili" 200 "$(code adm "$BASE/api/incidents")"
expect "C ordini visibili" 200 "$(code adm "$BASE/api/orders")"
expect "C consuntivi visibili" 200 "$(code adm "$BASE/api/timesheets")"
expect "C squadre visibili" 200 "$(code adm "$BASE/api/teams")"
expect "C diario creato" 200 "$(code wrk -X POST "$BASE/api/diary" -H 'content-type: application/json' -d "$DN")"

# teardown
sql "DELETE FROM app.tenants WHERE id='$TID'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
