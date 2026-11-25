#!/usr/bin/env bash
# IO-4 (mig 0143): catalogo scenari IoT nel DB, gestibile dal superadmin, con sensori reali.
#  - scenari predefiniti caricati all'avvio, con la spec dei widget (prima letta da file)
#  - creazione/modifica/validazione; disattivato = non attivabile; predefinito/attivo non eliminabile
#  - IO-9: due regole stessa metrica e operatore con soglie diverse vengono inserite entrambe
#  - sensore REALE collegato allo scenario: il cruscotto ne mostra la telemetria
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
sa(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
sac(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="sx$R"; CODE="e2e_$R"
TID=$(sa POST /api/superadmin/tenants "{\"slug\":\"$SLUG\",\"name\":\"Sx $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"iot\"]}" | py "d.get('id','')")
trap 'sac DELETE "/api/superadmin/tenants/$TID/scenarios/$CODE" >/dev/null; sac DELETE "/api/superadmin/tenants/$TID" "{\"slug\":\"$SLUG\"}" >/dev/null; sac DELETE "/api/superadmin/scenario-catalog/$CODE" >/dev/null; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }

echo "== catalogo predefinito =="
N=$(sa GET /api/superadmin/scenarios | py "len([s for s in d if s['builtin']])")
[ "$N" -ge 16 ] && ok "scenari predefiniti nel DB ($N)" || no "seed" "$N"
sa GET /api/superadmin/scenario-catalog/smart_energy | py "d['spec'] is not None and len(d['rules'])>0" | grep -q True \
  && ok "spec dei widget incorporata (non più letta da file)" || no "spec" "-"
C=$(sac DELETE /api/superadmin/scenario-catalog/smart_energy); [ "$C" = "409" ] && ok "predefinito: non si elimina (409)" || no "del builtin" "$C"

echo "== creazione e validazione =="
C=$(sac POST /api/superadmin/scenario-catalog "{\"code\":\"$CODE\",\"name\":\"X\",\"rules\":[{\"metric\":\"t\",\"op\":\"~\",\"threshold\":1,\"etype\":\"sensore_critico\"}]}")
[ "$C" = "400" ] && ok "operatore non valido: 400" || no "val op" "$C"
C=$(sac POST /api/superadmin/scenario-catalog "{\"code\":\"$CODE\",\"name\":\"X\",\"modules\":[\"inesistente\"]}")
[ "$C" = "400" ] && ok "modulo sconosciuto: 400" || no "val mod" "$C"
BODY="{\"code\":\"$CODE\",\"name\":\"Cella frigo $R\",\"sector\":\"Logistica\",\"descr\":\"prova\",\"modules\":[\"iot\"],
 \"rules\":[{\"metric\":\"temperature\",\"op\":\">=\",\"threshold\":8,\"etype\":\"telemetria\"},{\"metric\":\"temperature\",\"op\":\">=\",\"threshold\":12,\"etype\":\"sensore_critico\"}],
 \"devices\":[{\"suffix\":\"cella-1\",\"label\":\"Cella 1\"}]}"
C=$(sac POST /api/superadmin/scenario-catalog "$BODY"); [ "$C" = "200" ] && ok "scenario creato" || no "crea" "$C"
C=$(sac POST /api/superadmin/scenario-catalog "$BODY"); [ "$C" = "409" ] && ok "codice già usato: 409" || no "dup" "$C"

echo "== attivazione sul tenant =="
sa POST "/api/superadmin/tenants/$TID/scenarios" "{\"code\":\"$CODE\",\"with_demo\":true}" | py "d['rules_added']" | grep -q "^2$" \
  && ok "due regole stessa metrica/operatore, soglie diverse: inserite entrambe (IO-9)" || no "IO-9" "-"
[ "$(q "SELECT count(*) FROM app.devices WHERE tenant_id='$TID' AND source='scenario' AND external_id='scn-$CODE-cella-1'")" = "1" ] && ok "dispositivo demo creato" || no "demo" "-"
C=$(sac DELETE /api/superadmin/scenario-catalog/$CODE); [ "$C" = "409" ] && ok "attivo su un tenant: non si elimina (409)" || no "del attivo" "$C"

echo "== sensore reale nello scenario =="
ACT=$(call POST /api/iot/activate "{\"blueprint\":\"air-quality\",\"external_id\":\"REAL-$R\",\"label\":\"Sonda reale\"}")
DID=$(echo "$ACT" | py "d['device_id']"); TOK=$(echo "$ACT" | py "d['token']")
C=$(code POST "/api/devices/$DID/scenario" '{"code":"nessuno"}'); [ "$C" = "400" ] && ok "scenario non attivo: 400" || no "val dev scn" "$C"
C=$(code POST "/api/devices/$DID/scenario" "{\"code\":\"$CODE\"}"); [ "$C" = "200" ] && ok "sensore reale collegato allo scenario" || no "link" "$C"
curl -s -o /dev/null -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"temperature":13.5}}'
D=$(call GET "/api/scenarios/$CODE/dashboard")
echo "$D" | py "d['real_device_count']==1 and any(dv.get('real') for dv in d['devices'])" | grep -q True && ok "cruscotto: il sensore reale compare" || no "real dev" "$(echo "$D" | head -c 200)"
echo "$D" | py "[w['value'] for w in d['widgets'] if w['metric']=='temperature'][0]" | grep -q "13.5" && ok "cruscotto: valore dalla telemetria reale" || no "valore" "-"
C=$(code POST "/api/devices/$DID/scenario" '{"code":null}'); [ "$C" = "200" ] && ok "sensore scollegato" || no "unlink" "$C"

echo "== disattivazione =="
sa POST "/api/superadmin/tenants/$TID/scenarios/$CODE" >/dev/null 2>&1
sac DELETE "/api/superadmin/tenants/$TID/scenarios/$CODE" >/dev/null
UPD=$(echo "$BODY" | python -c "import sys,json;d=json.load(sys.stdin);d.pop('code');d['enabled']=False;print(json.dumps(d))")
C=$(sac POST /api/superadmin/scenario-catalog/$CODE "$UPD"); [ "$C" = "200" ] && ok "scenario disattivato" || no "disattiva" "$C"
C=$(sac POST "/api/superadmin/tenants/$TID/scenarios" "{\"code\":\"$CODE\"}"); [ "$C" = "409" ] && ok "disattivato: non si attiva (409)" || no "apply off" "$C"
C=$(sac DELETE /api/superadmin/scenario-catalog/$CODE); [ "$C" = "200" ] && ok "scenario eliminato dal catalogo" || no "del" "$C"

echo "scenario-catalog: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
