#!/usr/bin/env bash
# Dashboard unica configurabile: layout per-tenant dei widget. Verifica che il catalogo sia filtrato
# per moduli attivi + ruolo, che il layout salvato (enabled/position/size) sia rispettato, che un
# widget di modulo spento non compaia MAI anche se presente nel layout, e che solo il tenant_admin
# (cap dashboard.manage) possa scrivere. Tenant throwaway, teardown finale.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1"|head -1|tr -d '[:space:]'; }
code(){ "$@" -o /dev/null -w '%{http_code}'; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)" 2>/dev/null; }

R=$RANDOM; SLUG="dashtest-$R"
HASH=$(one "SELECT pass_hash FROM app.users WHERE email='admin@globex.loneworker.eu' LIMIT 1")
TID=$(one "INSERT INTO app.tenants(slug,name) VALUES('$SLUG','DashTest $R') RETURNING id")
ADM="adm-$R@dt.eu"; PLN="pln-$R@dt.eu"
one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$ADM','$HASH','tenant_admin')" >/dev/null
one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$PLN','$HASH','pianificatore')" >/dev/null
# moduli attivi: loneworker (safety) + interventi (campo). NIENTE workforce/consuntivi/giri.
for m in loneworker interventi; do sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','$m','attivo')" >/dev/null; done

login(){ local em=$1 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADM"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
PJ=$(login "$PLN"); PCS=$(awk '$6=="yw_csrf"{print $7}' "$PJ"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
pl(){  curl -s -b "$PJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $PCS" "$@"; }

# 1) admin GET: can_edit true; contiene incidents+checkins (loneworker) + orders (interventi) + role (core); NON timesheets/shifts/rounds (moduli off)
G=$(adm "$BASE/api/dashboard/layout")
echo "$G" | jget "d['can_edit']==True and {w['key'] for w in d['widgets']}>={'incidents','checkins','orders','role'} and 'timesheets' not in {w['key'] for w in d['widgets']} and 'shifts' not in {w['key'] for w in d['widgets']} and 'rounds' not in {w['key'] for w in d['widgets']}" | grep -q True && ok "admin GET: catalogo filtrato per moduli attivi + can_edit" || no "admin get" "$G"

# 2) pianificatore GET: can_edit false; vede orders+role ma NON incidents/checkins (ruolo non abilitato)
GP=$(pl "$BASE/api/dashboard/layout")
echo "$GP" | jget "d['can_edit']==False and 'orders' in {w['key'] for w in d['widgets']} and 'role' in {w['key'] for w in d['widgets']} and 'incidents' not in {w['key'] for w in d['widgets']} and 'checkins' not in {w['key'] for w in d['widgets']}" | grep -q True && ok "pianificatore GET: filtrato per ruolo + can_edit false" || no "planner get" "$GP"

# 3) PUT admin 200: disabilita orders, incidents full+primo, include shifts (modulo OFF) e chiave ignota
PUT='{"widgets":[{"widget_key":"incidents","enabled":true,"position":0,"size":"full"},{"widget_key":"role","enabled":true,"position":1,"size":"half"},{"widget_key":"orders","enabled":false,"position":2,"size":"half"},{"widget_key":"shifts","enabled":true,"position":3,"size":"half"},{"widget_key":"bogus","enabled":true,"position":9,"size":"half"}]}'
[ "$(code adm -X PUT "$BASE/api/dashboard/layout" -H 'content-type: application/json' -d "$PUT")" = 200 ] && ok "PUT admin -> 200" || no "put admin" "-"

# 4) GET riflette: incidents size=full e position 0; orders enabled=false; shifts ASSENTE (modulo off); bogus ASSENTE (chiave ignota)
G2=$(adm "$BASE/api/dashboard/layout")
echo "$G2" | jget "[w for w in d['widgets'] if w['key']=='incidents'][0]['size']=='full' and [w for w in d['widgets'] if w['key']=='incidents'][0]['position']==0" | grep -q True && ok "layout salvato: incidents full+pos0" || no "saved incidents" "$G2"
echo "$G2" | jget "[w for w in d['widgets'] if w['key']=='orders'][0]['enabled']==False" | grep -q True && ok "layout salvato: orders disabilitato" || no "saved orders off" "$G2"
echo "$G2" | jget "'shifts' not in {w['key'] for w in d['widgets']}" | grep -q True && ok "widget di modulo spento (shifts) MAI esposto anche se nel layout" || no "shifts leak" "$G2"
echo "$G2" | jget "'bogus' not in {w['key'] for w in d['widgets']}" | grep -q True && ok "chiave widget ignota ignorata" || no "bogus leak" "$G2"

# 5) widget ordinati per position
echo "$G2" | jget "[w['position'] for w in d['widgets']]==sorted(w['position'] for w in d['widgets'])" | grep -q True && ok "widget ordinati per position" || no "order" "$G2"

# 6) PUT pianificatore -> 403 (manca dashboard.manage)
[ "$(code pl -X PUT "$BASE/api/dashboard/layout" -H 'content-type: application/json' -d "$PUT")" = 403 ] && ok "PUT pianificatore -> 403" || no "put planner 403" "-"

# 7) audit traccia dashboard_layout_set
AF=$(adm "$BASE/api/audit-log?action=dashboard_layout_set&limit=5")
echo "$AF" | jget "len(d['rows'])>=1" | grep -q True && ok "audit traccia dashboard_layout_set" || no "audit" "$AF"

sql "DELETE FROM app.tenants WHERE id='$TID'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
