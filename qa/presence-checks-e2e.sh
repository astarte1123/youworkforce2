#!/usr/bin/env bash
# CA-15 (25/11/2026): presenze di cantiere verificate.
#  - una sola presenza aperta per persona (non su due commesse insieme)
#  - con squadre assegnate timbra solo chi ne fa parte (chi pianifica sempre)
#  - posizione lontana dalla sede: presenza valida ma «fuori area»
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
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="pc$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Pc $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"cantieri\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
UT=$(call A POST /api/users "{\"email\":\"tec@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | py "d['id']")
login T "tec@$SLUG.test" WorkPass9
S1=$(call A POST /api/sites '{"name":"Cantiere Nord","lat":45.4642,"lon":9.1900}' | py "d['id']")
C1=$(call A POST /api/commesse "{\"code\":\"N-$R\",\"name\":\"Nord $R\",\"site_id\":\"$S1\"}" | py "d['id']")
C2=$(call A POST /api/commesse "{\"code\":\"S-$R\",\"name\":\"Sud $R\"}" | py "d['id']")

V=$(call T POST "/api/commesse/$C1/presenze/in" '{"lat":45.4643,"lon":9.1901}')
echo "$V" | py "d['outside_area'] is False" | grep -q True && ok "entrata vicino alla sede: in area" || no "in area" "$V"
C=$(code T POST "/api/commesse/$C2/presenze/in" '{}'); [ "$C" = "409" ] && ok "seconda presenza aperta su un'altra commessa: 409" || no "parallela" "$C"
call T POST "/api/commesse/$C2/presenze/in" '{}' | grep -q "Nord $R" && ok "il messaggio indica dove la presenza è aperta" || no "msg" "-"
C=$(code T POST "/api/commesse/$C1/presenze/in" '{}'); [ "$C" = "409" ] && ok "doppia entrata sulla stessa commessa: 409" || no "doppia" "$C"
call T POST "/api/commesse/$C1/presenze/out" '{}' >/dev/null

V=$(call T POST "/api/commesse/$C1/presenze/in" '{"lat":45.50,"lon":9.30}')
echo "$V" | py "d['outside_area'] is True and d['distance_m']>1000" | grep -q True && ok "entrata a chilometri dalla sede: presenza valida ma fuori area" || no "fuori area" "$V"
call A GET "/api/commesse/$C1" | py "[p for p in d['presenze'] if p['clock_out'] is None][0]['outside_area']" | grep -q True && ok "nel dettaglio commessa la presenza è segnata fuori area" || no "dettaglio" "-"
call T POST "/api/commesse/$C1/presenze/out" '{}' >/dev/null

TM=$(call A POST /api/teams "{\"name\":\"Squadra $R\"}" | py "d['id']")
call A GET /api/user-groups | py "[g['kind'] for g in d if g['id']=='$TM'][0]" | grep -q squadra && ok "CA-5: la squadra è un gruppo di persone (tipo «squadra»)" || no "squadra=gruppo" "-"
call A POST "/api/commesse/$C2/teams" "{\"team_id\":\"$TM\"}" >/dev/null
C=$(code T POST "/api/commesse/$C2/presenze/in" '{}'); [ "$C" = "403" ] && ok "commessa con squadre: chi non ne fa parte non timbra (403)" || no "squadra" "$C"
call A POST "/api/teams/$TM/members" "{\"user_id\":\"$UT\"}" >/dev/null
V=$(call T POST "/api/commesse/$C2/presenze/in" '{}')
echo "$V" | py "d['outside_area'] is None" | grep -q True && ok "membro della squadra timbra; sede senza coordinate: non verificabile" || no "membro" "$V"
call T POST "/api/commesse/$C2/presenze/out" '{}' >/dev/null
C=$(code A POST "/api/commesse/$C2/presenze/in" '{}'); [ "$C" = "200" ] && ok "chi pianifica timbra anche senza squadra" || no "planner" "$C"

echo "presence-checks: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
