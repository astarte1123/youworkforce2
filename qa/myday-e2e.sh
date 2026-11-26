#!/usr/bin/env bash
# App dell'operaio — «La mia giornata» (GET /api/my/day): interventi propri e della propria squadra
# in ordine, esclusi quelli di altre squadre; spostamenti dal deposito; mezzi da ritirare; squadra con
# gli assenti; meteo per sede e fascia con l'avviso del tipo di attività; riepilogo per zona; giro di
# navigazione; domani; limiti della data; ognuno vede solo la sua giornata. Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sqlq(){ docker exec -i ywdb psql -U postgres -d youworkforce -qc "$1" >/dev/null; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="md$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Md $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"interventi\",\"mezzi\",\"meteo\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" "$BASE$2"; }
login A "a@$SLUG.test" DirectPass9
call A PUT /api/settings/timezone '{"timezone":"Europe/Rome"}' >/dev/null
mk(){ call A POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
W1=$(mk luca); W2=$(mk sara); W3=$(mk piero); W4=$(mk ugo)
login L "luca@$SLUG.test" WorkPass9; login U "ugo@$SLUG.test" WorkPass9
G1=$(call A POST /api/user-groups '{"name":"Squadra Nord","kind":"squadra"}' | py "d['id']")
G2=$(call A POST /api/user-groups '{"name":"Squadra Sud","kind":"squadra"}' | py "d['id']")
for u in $W1 $W2 $W3; do call A POST "/api/user-groups/$G1/members" "{\"user_id\":\"$u\"}" >/dev/null; done
call A POST "/api/user-groups/$G2/members" "{\"user_id\":\"$W4\"}" >/dev/null
DEP=$(call A POST /api/sites '{"name":"Deposito","lat":45.503,"lon":9.129}' | py "d['id']")
S1=$(call A POST /api/sites '{"name":"Parco Nord","address":"Via Clerici 1, Milano","lat":45.53,"lon":9.20,"access_notes":"Cancello lato parcheggio, chiave in portineria"}' | py "d['id']")
S2=$(call A POST /api/sites '{"name":"Villa Monza","lat":45.58,"lon":9.27}' | py "d['id']")
sqlq "INSERT INTO app.planning_settings(tenant_id,depot_site_id) VALUES('$TID','$DEP')"
AT=$(call A POST /api/activity-types '{"name":"Taglio erba","unit":"m2","base_minutes":90,"weather":{"rain_max_mm":2}}' | py "d['id']")
FG=$(call A POST /api/assets '{"name":"Furgone FV 105","kind":"furgone","identifier":"FV105XX"}' | py "d['id']")
read TD TM <<<"$(python -c "import datetime as t;d=t.date.today();print(d, d+t.timedelta(days=1))")"
# assenza di sara oggi
sqlq "INSERT INTO app.worker_absences(tenant_id,user_id,starts_at,ends_at,kind) VALUES('$TID','$W2','${TD}T00:00:00+01:00','${TD}T23:59:00+01:00','ferie')" 2>/dev/null || \
  sqlq "INSERT INTO app.worker_absences(tenant_id,user_id,starts_at,ends_at) VALUES('$TID','$W2','${TD}T00:00:00+01:00','${TD}T23:59:00+01:00')"
O(){ call A POST /api/orders "{\"title\":\"$1\",\"site_id\":\"$2\",\"activity_type_id\":\"$AT\",\"quantity\":800,\"scheduled_start\":\"$3\",\"scheduled_end\":\"$4\"${5:+,$5}}" | py "d['id']"; }
O1=$(O "Taglio Parco Nord" $S1 "${TD}T08:00:00+01:00" "${TD}T09:30:00+01:00" "\"assigned_group\":\"$G1\"")
O2=$(O "Taglio Villa Monza" $S2 "${TD}T11:00:00+01:00" "${TD}T12:30:00+01:00" "\"assigned_user\":\"$W1\"")
O3=$(O "Taglio squadra Sud" $S2 "${TD}T09:00:00+01:00" "${TD}T10:00:00+01:00" "\"assigned_group\":\"$G2\"")
O4=$(O "Taglio domani" $S1 "${TM}T08:00:00+01:00" "${TM}T09:00:00+01:00" "\"assigned_group\":\"$G1\"")
sqlq "INSERT INTO app.asset_bookings(tenant_id,asset_id,work_order_id,starts_at,ends_at) VALUES('$TID','$FG','$O1','${TD}T08:00:00+01:00','${TD}T09:30:00+01:00')"
call A PUT /api/integration/weather/zones/MI '{"name":"Milano Nord","lat":45.53,"lon":9.20}' >/dev/null
call A POST /api/integration/weather/slots "{\"zone\":\"MI\",\"slots\":[{\"from\":\"${TD}T07:00:00+01:00\",\"to\":\"${TD}T10:00:00+01:00\",\"rain_mm\":6,\"rain_prob\":80,\"temp_c\":9,\"wind_kmh\":12,\"condition\":\"pioggia\"},{\"from\":\"${TD}T10:00:00+01:00\",\"to\":\"${TD}T19:00:00+01:00\",\"rain_mm\":0,\"temp_c\":12,\"condition\":\"sereno\"}]}" >/dev/null

echo "== la giornata di luca =="
D=$(call L GET "/api/my/day")
echo "$D" | py "[o['title'] for o in d['orders']]" | grep -q "\['Taglio Parco Nord', 'Taglio Villa Monza'\]" && ok "suoi e della sua squadra, in ordine di orario; non quelli di altre squadre né di domani" || no "interventi" "$(echo "$D" | py "[o['title'] for o in d['orders']]")"
echo "$D" | py "(d['orders'][0]['seq'], d['orders'][0]['travel_minutes']>0, d['orders'][1]['travel_minutes']>0, d['totals']['travel_minutes']>d['orders'][0]['travel_minutes']+d['orders'][1]['travel_minutes'])" | grep -q "(1, True, True, True)" \
  && ok "spostamenti: dal deposito, tra le tappe e rientro" || no "spostamenti" "$(echo "$D" | py "d['totals']")"
echo "$D" | py "([v['name'] for v in d['vehicles']], d['depot']['name'])" | grep -q "(\['Furgone FV 105'\], 'Deposito')" && ok "mezzi da ritirare al deposito" || no "mezzi" "-"
echo "$D" | py "[(m['email'].split('@')[0], m['absent']) for m in d['teams'][0]['members']]" | grep -q "('sara', True)" && ok "squadra con gli assenti del giorno" || no "squadra" "$(echo "$D" | py "d['teams']")"
echo "$D" | py "(d['orders'][0]['weather']['condition'], d['orders'][0]['weather']['rain_mm'])" | grep -q "('pioggia', 3.0)" && ok "meteo della sede nella fascia dell'intervento (pioggia in proporzione)" || no "meteo" "$(echo "$D" | py "d['orders'][0]['weather']")"
echo "$D" | py "[w['code'] for w in d['orders'][0]['weather_warnings']]" | grep -q "weather_rain" && ok "avviso: pioggia oltre il limite del taglio erba" || no "avviso" "-"
echo "$D" | py "d['weather'][0]['zone']" | grep -q "Milano Nord" && ok "riepilogo meteo della giornata per zona" || no "riepilogo" "-"
echo "$D" | py "(d['orders'][0]['access_notes'], d['orders'][0]['site_address'])" | grep -q "Cancello lato parcheggio" && ok "note di accesso e indirizzo della sede" || no "sede" "-"
echo "$D" | py "d['route_url']" | grep -q "origin=45.503,9.129&destination=45.503,9.129&waypoints=45.53,9.2%7C45.58,9.27" && ok "giro in Maps: deposito → tappe → deposito" || no "giro" "$(echo "$D" | py "d['route_url']")"
call L GET "/api/my/day?date=$TM" | py "([o['title'] for o in d['orders']], d['is_today'])" | grep -q "(\['Taglio domani'\], False)" && ok "domani" || no "domani" "-"
call L GET "/api/my/day?next=1" | py "(d['date'], [o['title'] for o in d['orders']])" | grep -q "('$TM', \['Taglio domani'\])" && ok "prossima giornata con interventi" || no "prossima" "$(call L GET '/api/my/day?next=1' | head -c 200)"
C=$(code L "/api/my/day?date=2020-01-01"); [ "$C" = "400" ] && ok "data troppo lontana: 400" || no "data" "$C"

echo "== ognuno la sua =="
call U GET "/api/my/day" | py "([o['title'] for o in d['orders']], d['teams'][0]['name'])" | grep -q "(\['Taglio squadra Sud'\], 'Squadra Sud')" && ok "ugo (altra squadra) vede solo la sua giornata" || no "isolamento" "$(call U GET /api/my/day | head -c 300)"
C=$(curl -s -o /dev/null -w "%{http_code}" -H "x-tenant-slug: $SLUG" "$BASE/api/my/day"); [ "$C" = "401" ] && ok "senza accesso: 401" || no "anonimo" "$C"

echo "== risultato: $PASS passati, $FAIL falliti =="
[ "$FAIL" -eq 0 ]
