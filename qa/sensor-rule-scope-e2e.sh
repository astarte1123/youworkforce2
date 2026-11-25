#!/usr/bin/env bash
# IO-1 (mig 0138): regole sensori con ambito, valutazione di tutte le regole, vince la più grave.
#  - una regola minore («telemetria») prima nell'ordine non nasconde più una critica sulla stessa metrica
#  - una regola di un dispositivo vale solo per quel dispositivo; idem per la sede
#  - l'allarme riporta la soglia della regola che ha deciso
#  - ambito validato (dispositivo/sede inesistente, scenario sconosciuto -> 400); elenco leggibile
# Tenant usa-e-getta, solo modulo IoT.
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
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="sr$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Sr $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"iot\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
act(){ call POST /api/iot/activate "{\"blueprint\":\"air-quality\",\"external_id\":\"$1\",\"label\":\"$2\"}"; }
A=$(act "SRA-$R" "Sensore A"); DA=$(echo "$A" | py "d['device_id']"); TA=$(echo "$A" | py "d['token']")
B=$(act "SRB-$R" "Sensore B"); DB=$(echo "$B" | py "d['device_id']"); TB=$(echo "$B" | py "d['token']")
[ -n "$DA" ] && [ -n "$DB" ] && ok "due sensori attivati" || { no "attivazione" "$A"; exit 1; }
tele(){ curl -s -X POST "$BASE/api/ingress/telemetry?token=$1" -H 'content-type: application/json' -d "{\"values\":$2}"; }
nal(){ q "SELECT count(*) FROM app.iot_alerts WHERE tenant_id='$TID' AND device_id='$1' AND kind='soglia' AND metric='$2'"; }

echo "== una regola minore non nasconde la critica =="
call POST /api/sensor-rules '{"metric":"gas","op":">=","threshold":10,"etype":"telemetria","label":"gas registrato","sort":10}' >/dev/null
call POST /api/sensor-rules '{"metric":"gas","op":">=","threshold":50,"etype":"sensore_critico","label":"gas pericoloso","sort":100}' >/dev/null
tele "$TA" '{"gas":20}' >/dev/null
[ "$(nal "$DA" gas)" = "0" ] && ok "gas 20: solo la regola minore, nessun allarme" || no "gas basso" "$(nal "$DA" gas)"
RESP=$(tele "$TA" '{"gas":80}')
echo "$RESP" | py "d['alert']['severity']=='critical' and d['alert']['threshold']==50" | grep -q True \
  && ok "gas 80: vale la regola critica (soglia 50) anche se la minore viene prima" || no "critica nascosta" "$RESP"

echo "== ambito dispositivo e sede =="
C=$(code POST /api/sensor-rules "{\"metric\":\"temp\",\"op\":\">\",\"threshold\":30,\"etype\":\"sensore_critico\",\"scope_kind\":\"device\",\"scope_ref\":\"$DB\"}")
[ "$C" = "200" ] && ok "regola per il solo sensore B" || no "crea regola device" "$C"
tele "$TA" '{"temp":40}' >/dev/null; tele "$TB" '{"temp":40}' >/dev/null
[ "$(nal "$DA" temp)" = "0" ] && ok "sensore A: la regola di B non si applica" || no "ambito A" "$(nal "$DA" temp)"
[ "$(nal "$DB" temp)" = "1" ] && ok "sensore B: allarme" || no "ambito B" "$(nal "$DB" temp)"
S=$(call POST /api/sites '{"name":"Magazzino"}' | py "d['id']")
docker exec -i ywdb psql -U postgres -d youworkforce -qc "UPDATE app.devices SET site_id='$S' WHERE id='$DA'" >/dev/null
call POST /api/sensor-rules "{\"metric\":\"hum\",\"op\":\">\",\"threshold\":80,\"etype\":\"sensore_critico\",\"scope_kind\":\"site\",\"scope_ref\":\"$S\"}" >/dev/null
tele "$TA" '{"hum":90}' >/dev/null; tele "$TB" '{"hum":90}' >/dev/null
[ "$(nal "$DA" hum)" = "1" ] && [ "$(nal "$DB" hum)" = "0" ] && ok "regola di sede: solo i sensori di quella sede" || no "sede" "$(nal "$DA" hum)/$(nal "$DB" hum)"

echo "== validazione ed elenco =="
C=$(code POST /api/sensor-rules "{\"metric\":\"x\",\"op\":\">\",\"threshold\":1,\"etype\":\"sensore_critico\",\"scope_kind\":\"device\",\"scope_ref\":\"$(python -c 'import uuid;print(uuid.uuid4())')\"}")
[ "$C" = "400" ] && ok "dispositivo inesistente rifiutato" || no "val device" "$C"
C=$(code POST /api/sensor-rules '{"metric":"x","op":">","threshold":1,"etype":"sensore_critico","scope_kind":"scenario","scope_ref":"nessuno"}')
[ "$C" = "400" ] && ok "scenario sconosciuto rifiutato" || no "val scenario" "$C"
C=$(code POST /api/sensor-rules '{"metric":"x","op":">","threshold":1,"etype":"sensore_critico","scope_kind":"device"}')
[ "$C" = "400" ] && ok "ambito senza riferimento rifiutato" || no "val vuoto" "$C"
call GET /api/sensor-rules | py "sorted(r['scope_label'] for r in d if r['metric'] in ('temp','hum'))" | grep -q "Dispositivo: Sensore B" \
  && ok "elenco: ambito leggibile («Dispositivo: Sensore B»)" || no "elenco" "-"
RID=$(call GET /api/sensor-rules | py "[r['id'] for r in d if r['metric']=='temp'][0]")
call POST "/api/sensor-rules/$RID" '{"metric":"temp","op":">","threshold":30,"etype":"sensore_critico"}' >/dev/null
[ "$(q "SELECT scope_kind FROM app.sensor_rules WHERE id='$RID'")" = "device" ] && ok "modifica senza ambito: l'ambito resta" || no "update keep" "-"

echo "sensor-rule-scope: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
