#!/usr/bin/env bash
# SL-2 (mig 0133): rilevamento nativo dell'immobilità, parametri dal profilo di protezione.
#  - parametri salvati e validati (soglia, preavviso, spostamento minimo, solo in turno)
#  - fuori turno (con "solo durante il turno") nessun preavviso né allarme
#  - in turno, fermo: preavviso «Stai bene?» nell'app a soglia - preavviso
#  - «Sto bene» azzera il conteggio
#  - fermo oltre la soglia: un incidente critico `immobilita`, uno solo
#  - uno spostamento oltre la soglia del profilo riarma il conteggio
# Tenant usa-e-getta (pacchetto completo: serve il modulo turni). Richiede TEST_CLOCK=1.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | tr -d '[:space:]'; }
[ "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":0}')" = "200" ] \
  || { echo "SKIP: core senza TEST_CLOCK=1"; exit 0; }
ADV(){ curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$1}"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' \
  -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="imm$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Imm $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"plan\":\"full\"}" \
  | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
login(){ curl -s -o /dev/null -c "$J/$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$2\",\"password\":\"$3\"}"; }
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
callc(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/$1" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/$1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$2" "$BASE$3" ${4:+-d "$4"}; }
login A "a@$SLUG.test" DirectPass9
W=$(call A POST /api/users "{\"email\":\"w@$SLUG.test\",\"role\":\"lavoratore\",\"password\":\"WorkPass9\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
P=$(call A POST /api/safety/profiles '{"name":"Isolati"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
call A POST "/api/safety/profiles/$P/members" "{\"user_id\":\"$W\"}" >/dev/null

echo "== parametri =="
C=$(callc A PUT "/api/safety/profiles/$P" '{"name":"Isolati","immobility_enabled":true,"immobility_after_s":300,"immobility_prewarn_s":400}')
[ "$C" = "400" ] && ok "preavviso maggiore della soglia rifiutato" || no "validazione" "$C"
C=$(callc A PUT "/api/safety/profiles/$P" '{"name":"Isolati","immobility_enabled":true,"immobility_after_s":180,"immobility_prewarn_s":60,"immobility_min_move_m":20,"immobility_shift_only":true}')
[ "$C" = "200" ] && ok "immobilità attivata: 3 min, preavviso 60 s, 20 m, solo in turno" || no "salva" "$C"
IM=$(call A GET /api/safety/profiles | python -c "import sys,json;p=[x for x in json.load(sys.stdin)['profiles'] if x['id']=='$P'][0]['immobility'];print(p['enabled'],p['after_s'],p['prewarn_s'],p['min_move_m'],p['shift_only'])")
[ "$IM" = "True 180 60 20 True" ] && ok "parametri riletti dal profilo" || no "rilettura" "$IM"

login W "w@$SLUG.test" WorkPass9
pos(){ call W POST /api/worker/position "{\"lat\":$1,\"lon\":$2}" >/dev/null; }
st(){ call W GET /api/worker/immobility | python -c "import sys,json;print(json.load(sys.stdin)['state'])"; }
nimm(){ q "SELECT count(*) FROM app.incidents WHERE tenant_id='$TID' AND etype='immobilita'"; }
tick(){ for i in $(seq 1 12); do sleep 1; done; }   # il controllo gira ogni ~10 s

echo "== fuori turno =="
pos 45.10000 9.10000; ADV 200; pos 45.10001 9.10001; tick
[ "$(nimm)" = "0" ] && [ "$(st)" = "ok" ] && ok "fuori turno: nessun preavviso né allarme" || no "fuori turno" "$(nimm)/$(st)"

echo "== in turno =="
call W POST /api/shifts/start '{}' >/dev/null
pos 45.10002 9.10002             # spostamento < 20 m: non conta come movimento
ADV 200; pos 45.10003 9.10003    # oltre 180 s fermo ma il conteggio era già partito: allarme subito sotto
tick
[ "$(nimm)" = "1" ] && ok "fermo oltre la soglia in turno: un incidente 'immobilita'" || no "allarme" "$(nimm)"
tick
[ "$(nimm)" = "1" ] && ok "un solo allarme per lo stesso periodo di immobilità" || no "doppio" "$(nimm)"
[ "$(st)" = "allarme" ] && ok "l'app vede l'allarme aperto" || no "stato allarme" "$(st)"

echo "== nuovo periodo: preavviso e Sto bene =="
pos 45.10100 9.10100             # ~140 m: movimento reale, conteggio riarmato
ADV 125; pos 45.10101 9.10101; tick
[ "$(st)" = "preavviso" ] && ok "a soglia - 60 s: preavviso «Stai bene?»" || no "preavviso" "$(st)"
C=$(callc W POST /api/worker/immobility/ok); [ "$C" = "200" ] || no "ok" "$C"
[ "$(st)" = "ok" ] && ok "«Sto bene»: conteggio azzerato" || no "sto bene" "$(st)"
ADV 100; pos 45.10101 9.10101; tick
[ "$(nimm)" = "1" ] && ok "dopo «Sto bene» nessun nuovo allarme prima della soglia" || no "dopo ok" "$(nimm)"

echo "immobility: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
