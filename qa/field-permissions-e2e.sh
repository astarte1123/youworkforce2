#!/usr/bin/env bash
# Permessi Lavoro sul campo (CA-9, CA-10, CA-11).
#  CA-10 ordini: un ruolo che non pianifica e non ha `orders.view_all` vede solo i propri ordini
#        (prima bastava non chiamarsi «lavoratore»); chi ha view_all vede solo le proprie sedi;
#        lettura di righe/allegati con la stessa regola
#  CA-9  consuntivi: approvazione e rimando solo nelle proprie sedi; modulo spento -> 403 anche sul
#        dettaglio (l'assistente AI usa la stessa regola di lettura della UI)
#  CA-11 costi: budget/costi/fatture nascosti a chi non pianifica e non ha `costs.view`; il tecnico
#        non fissa il prezzo dei materiali (vale il listino); posizioni dei colleghi e cruscotto
#        vigilanza solo a chi supervisiona
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="fp$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Fp $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"consuntivi\",\"cantieri\",\"giri\",\"gps\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"$2\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
UT=$(mk tec lavoratore); UO=$(mk ope operatore); UR=$(mk resp responsabile); UP=$(mk plan pianificatore)
login T "tec@$SLUG.test" WorkPass9; login O "ope@$SLUG.test" WorkPass9; login RS "resp@$SLUG.test" WorkPass9; login P "plan@$SLUG.test" WorkPass9
S1=$(call A POST /api/sites '{"name":"Nord"}' | py "d['id']"); S2=$(call A POST /api/sites '{"name":"Sud"}' | py "d['id']")
O1=$(call P POST /api/orders "{\"title\":\"Nord $R\",\"site_id\":\"$S1\",\"assigned_user\":\"$UT\"}" | py "d['id']")
O2=$(call P POST /api/orders "{\"title\":\"Sud $R\",\"site_id\":\"$S2\"}" | py "d['id']")
n_orders(){ call "$1" GET /api/orders | py "len([o for o in d if o['title'].endswith('$R')])"; }

echo "== CA-10 ordini =="
[ "$(n_orders T)" = "1" ] && ok "tecnico: vede solo il proprio ordine" || no "tecnico" "$(n_orders T)"
[ "$(n_orders O)" = "0" ] && ok "operatore (non pianifica, senza «vedere tutti»): nessun ordine altrui" || no "operatore" "$(n_orders O)"
[ "$(n_orders P)" = "2" ] && ok "pianificatore: vede tutti" || no "pianificatore" "$(n_orders P)"
[ "$(n_orders RS)" = "2" ] && ok "responsabile (orders.view_all): vede tutti" || no "responsabile" "$(n_orders RS)"
call A POST "/api/users/$UR/sites" "{\"site_ids\":[\"$S1\"]}" >/dev/null
login RS "resp@$SLUG.test" WorkPass9
[ "$(n_orders RS)" = "1" ] && ok "responsabile con sede Nord: vede solo gli ordini di Nord" || no "sedi" "$(n_orders RS)"
C=$(code O GET "/api/orders/$O2/lines"); [ "$C" = "403" ] && ok "righe di un ordine non visibile: 403" || no "righe" "$C"
C=$(code RS GET "/api/orders/$O2/attachments"); [ "$C" = "403" ] && ok "allegati di un ordine fuori sede: 403" || no "allegati" "$C"
C=$(code RS GET "/api/orders/$O1/lines"); [ "$C" = "200" ] && ok "righe di un ordine della propria sede: 200" || no "righe ok" "$C"

echo "== CA-11 prezzi dei materiali =="
IT=$(call A POST /api/price-items '{"name":"Valvola","category":"materiale","unit":"pezzo","unit_price":42.5}' | py "d['id']")
call T POST "/api/orders/$O1/lines" "{\"description\":\"Valvola\",\"item_id\":\"$IT\",\"qty\":1,\"unit_price\":1}" >/dev/null
[ "$(q "SELECT unit_price FROM app.work_order_lines WHERE work_order_id='$O1'")" = "42.50" ] \
  && ok "il tecnico non fissa il prezzo: vale il listino" || no "prezzo" "$(q "SELECT unit_price FROM app.work_order_lines WHERE work_order_id='$O1'")"
call T GET "/api/orders/$O1/lines" | py "d['prices_hidden'] and d['lines'][0]['unit_price'] is None" | grep -q True \
  && ok "il tecnico vede i materiali, non i prezzi" || no "prezzi nascosti" "-"
call P GET "/api/orders/$O1/lines" | py "d['lines'][0]['unit_price']" | grep -q "42.5" && ok "il pianificatore vede i prezzi" || no "prezzi visibili" "-"

echo "== CA-11 costi della commessa =="
CM=$(call P POST /api/commesse "{\"code\":\"C$R\",\"name\":\"Cantiere $R\",\"budget_amount\":10000}" | py "d['id']")
call T GET "/api/commesse/$CM" | py "('budget' not in d) and ('fatture' not in d) and d.get('costs_hidden') is True" | grep -q True \
  && ok "tecnico: budget, costi e fatture nascosti" || no "costi tecnico" "-"
call T GET /api/commesse | py "[c for c in d if c['id']=='$CM'][0]['budget'] is None" | grep -q True && ok "elenco commesse: budget nascosto al tecnico" || no "elenco" "-"
call RS GET "/api/commesse/$CM" | py "d['budget']" | grep -q "10000" && ok "responsabile (costs.view): vede il budget" || no "costi resp" "-"

echo "== CA-11 posizioni =="
docker exec -i ywdb psql -U postgres -d youworkforce -qc "INSERT INTO app.devices(tenant_id,external_id,label,source,assigned_user,last_lat,last_lon) VALUES('$TID','fp-a-$R','Badge altro','gps','$UP',45.1,9.1),('$TID','fp-b-$R','Badge mio','gps','$UT',45.2,9.2)" >/dev/null
call T GET /api/devices | py "sorted(x['label'] for x in d)" | grep -q "^\['Badge mio'\]$" && ok "tecnico: vede solo il proprio dispositivo (niente posizioni dei colleghi)" || no "posizioni" "$(call T GET /api/devices | head -c 200)"
call O GET /api/devices | py "len(d)" | grep -q "^2$" && ok "operatore di centrale: vede tutti i dispositivi" || no "operatore posizioni" "-"
C=$(code T GET /api/patrol-dashboard); [ "$C" = "403" ] && ok "cruscotto vigilanza: 403 a chi non supervisiona" || no "cruscotto" "$C"
C=$(code P GET /api/patrol-dashboard); [ "$C" = "200" ] && ok "cruscotto vigilanza: 200 al pianificatore" || no "cruscotto P" "$C"

echo "== CA-9 consuntivi =="
call T POST "/api/orders/$O1/report" '{"work_minutes":60}' >/dev/null
TS1=$(call T POST "/api/orders/$O1/timesheet" '{}' | py "d['timesheet_id']")
call T POST "/api/timesheets/$TS1/submit" '{}' >/dev/null
call A POST "/api/users/$UR/sites" "{\"site_ids\":[\"$S2\"]}" >/dev/null; login RS "resp@$SLUG.test" WorkPass9
C=$(code RS POST "/api/timesheets/$TS1/approve" '{}'); [ "$C" = "403" ] && ok "approvazione fuori dalle proprie sedi: 403" || no "approva fuori sede" "$C"
C=$(code RS POST "/api/timesheets/$TS1/request-changes" '{"note":"x"}'); [ "$C" = "403" ] && ok "rimando fuori dalle proprie sedi: 403" || no "rimanda fuori sede" "$C"
call A POST "/api/users/$UR/sites" "{\"site_ids\":[\"$S1\"]}" >/dev/null; login RS "resp@$SLUG.test" WorkPass9
C=$(code RS POST "/api/timesheets/$TS1/approve" '{}'); [ "$C" = "200" ] && ok "approvazione nella propria sede: 200" || no "approva" "$C"
# l'assistente AI (get_consuntivo, riassunto) usa la stessa guard_timesheet_access della UI: non è
# richiamabile qui senza un provider AI configurato (lo strumento non è «confermabile»).
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.tenant_modules SET state='non_abilitato' WHERE tenant_id='$TID' AND module_code='consuntivi'" >/dev/null
C=$(code T GET "/api/timesheets/$TS1"); [ "$C" = "403" ] && ok "modulo consuntivi spento: dettaglio 403" || no "modulo" "$C"

echo "field-permissions: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
