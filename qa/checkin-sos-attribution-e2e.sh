#!/usr/bin/env bash
# SL-5 / SL-6 / SL-10 (lato server):
#  - check-in mancato -> incidente con LAVORATORE e SEDE (sede univoca del lavoratore)
#  - SOS dall'app -> incidente con lavoratore e sede; stessa client_key ripetuta = stesso SOS
#  - un operatore ristretto a un'altra sede vede comunque un incidente SENZA sede
# Tenant usa-e-getta (eliminato a fine suite). Richiede TEST_CLOCK=1.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}')" = "200" ] \
  || { echo "SKIP: core senza TEST_CLOCK=1"; exit 0; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="att$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Att $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"sicurezza\"}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
id(){ python -c "import sys,json;print(json.load(sys.stdin)['id'])"; }
login A "a@$SLUG.test" DirectPass9
S1=$(call A POST /api/sites '{"name":"Cantiere Nord"}' | id); S2=$(call A POST /api/sites '{"name":"Magazzino Sud"}' | id)
W=$(call A POST /api/users "{\"email\":\"w@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | id)
O=$(call A POST /api/users "{\"email\":\"op@$SLUG.test\",\"role\":\"operatore\",\"password\":\"OperPass9\"}" | id)
call A POST "/api/users/$W/sites" "{\"site_ids\":[\"$S1\"]}" >/dev/null
call A POST "/api/users/$O/sites" "{\"site_ids\":[\"$S2\"]}" >/dev/null
login W "w@$SLUG.test" WorkPass9; login O "op@$SLUG.test" OperPass9

echo "== SL-10 check-in attivo ripristinabile dall'app =="
CID=$(call W POST /api/checkins '{"minutes":1}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
M=$(call W GET /api/checkins/mine | python -c "import sys,json;print((json.load(sys.stdin) or {}).get('id'))")
[ "$M" = "$CID" ] && ok "l'app ritrova il check-in attivo dopo un ricaricamento" || no "checkins/mine" "$M"
M=$(call O GET /api/checkins/mine | python -c "import sys,json;print(json.load(sys.stdin))")
[ "$M" = "None" ] && ok "un altro utente non vede il check-in del lavoratore" || no "mine altrui" "$M"

echo "== SL-5/SL-6 check-in mancato =="
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":90}'
for i in $(seq 1 15); do [ "$(q "SELECT count(*) FROM app.incidents WHERE tenant_id='$TID' AND etype='mancato_checkin'")" = "1" ] && break; sleep 1; done
[ "$(q "SELECT count(*) FROM app.incidents WHERE tenant_id='$TID' AND etype='mancato_checkin'")" = "1" ] && ok "check-in scaduto -> un incidente" || no "incidente" "-"
[ "$(q "SELECT (worker_user_id='$W') FROM app.incidents WHERE tenant_id='$TID' AND etype='mancato_checkin'")" = "t" ] && ok "incidente con il lavoratore" || no "lavoratore" "-"
[ "$(q "SELECT (site_id='$S1') FROM app.incidents WHERE tenant_id='$TID' AND etype='mancato_checkin'")" = "t" ] && ok "incidente con la sede del lavoratore" || no "sede" "-"
[ "$(q "SELECT state FROM app.checkins WHERE tenant_id='$TID' AND user_id='$W'")" = "scaduto" ] && ok "check-in marcato scaduto insieme all'incidente" || no "stato" "-"

echo "== SOS dall'app =="
K="sos-$R-1"
I1=$(call W POST /api/sos "{\"lat\":45.1,\"lon\":9.1,\"client_key\":\"$K\"}" | python -c "import sys,json;print(json.load(sys.stdin)['incident_id'])")
I2=$(call W POST /api/sos "{\"lat\":45.1,\"lon\":9.1,\"client_key\":\"$K\"}" | python -c "import sys,json;print(json.load(sys.stdin)['incident_id'])")
[ -n "$I1" ] && [ "$I1" = "$I2" ] && ok "stessa chiave ripetuta dall'app: stesso SOS, nessun doppione" || no "client_key" "$I1 / $I2"
[ "$(q "SELECT (worker_user_id='$W' AND site_id='$S1') FROM app.incidents WHERE id='$I1'")" = "t" ] && ok "SOS dall'app con lavoratore e sede" || no "sos attr" "-"

echo "== incidente senza sede visibile anche a chi è ristretto per sede =="
call A POST "/api/users/$W/sites" '{"site_ids":[]}' >/dev/null
I3=$(call W POST /api/sos "{\"client_key\":\"sos-$R-2\"}" | python -c "import sys,json;print(json.load(sys.stdin)['incident_id'])")
V=$(call O GET /api/incidents | python -c "import sys,json;ids=[i['id'] for i in json.load(sys.stdin)];print('$I3' in ids, '$I1' in ids)")
[ "$V" = "True False" ] && ok "operatore di un'altra sede: vede il SOS senza sede, non quello della sede altrui" || no "visibilità" "$V"

echo "checkin-sos-attribution: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
