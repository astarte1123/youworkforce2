#!/usr/bin/env bash
# TA-13 audit uniforme · IO-12 comandi IoT · IO-13 token LoRaWAN fuori dall'URL · SL-17 batteria.
#  - TA-13: membri dei gruppi, sedi, asset nel registro attività con l'utente reale
#  - IO-12: metodo del comando validato; ogni comando nel registro attività
#  - IO-13: l'attivazione LoRaWAN dà URL senza segreto + intestazione con il token
#  - SL-17: batteria sotto soglia su un normale record di posizione (OsmAnd) -> una segnalazione per discesa
# Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
q(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1" | head -1 | tr -d '[:space:]'; }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="hd$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Hd $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"iot\",\"gps\",\"loneworker\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
aud(){ q "SELECT count(*) FROM app.audit_log WHERE tenant_id='$TID' AND action='$1' AND actor='a@$SLUG.test'"; }

echo "== TA-13 audit =="
U=$(call POST /api/users '{"email":"w@x.test","role":"lavoratore","password":"WorkPass9"}' | py "d['id']")
G=$(call POST /api/user-groups '{"name":"Squadra"}' | py "d['id']")
call POST "/api/user-groups/$G/members" "{\"user_id\":\"$U\"}" >/dev/null
call DELETE "/api/user-groups/$G/members/$U" >/dev/null
S=$(call POST /api/sites '{"name":"Magazzino"}' | py "d['id']")
call POST "/api/sites/$S" '{"name":"Magazzino nord"}' >/dev/null
[ "$(aud user_group_member_add)" = "1" ] && [ "$(aud user_group_member_del)" = "1" ] && ok "membri del gruppo nel registro, con l'utente reale" || no "audit gruppo" "$(aud user_group_member_add)/$(aud user_group_member_del)"
[ "$(aud update_site)" = "1" ] && ok "modifica sede nel registro" || no "audit sede" "-"

echo "== IO-12 comandi IoT =="
ACT=$(call POST /api/iot/activate "{\"blueprint\":\"air-quality\",\"external_id\":\"HD-$R\",\"label\":\"Sonda\"}")
DID=$(echo "$ACT" | py "d['device_id']")
C=$(code POST "/api/iot/devices/$DID/command" '{"method":"rm -rf /; reboot"}'); [ "$C" = "400" ] && ok "metodo non valido rifiutato" || no "metodo" "$C"
C=$(code POST "/api/iot/devices/$DID/command" '{"method":"set_interval","params":{"s":60}}'); [ "$C" = "200" ] && ok "comando con nome valido accettato" || no "comando" "$C"
[ "$(aud iot_command)" = "1" ] && ok "comando nel registro attività" || no "audit comando" "$(aud iot_command)"

echo "== IO-13 LoRaWAN =="
L=$(call POST /api/iot/activate "{\"blueprint\":\"env-lorawan\",\"external_id\":\"LW-$R\",\"label\":\"Suolo\"}")
if echo "$L" | grep -q '"protocol":"lorawan"'; then
  echo "$L" | py "('token' not in d['connect']['webhook_url']) and d['connect']['header_name']=='x-ingest-token' and d['connect']['header_value'].startswith('iot-')" | grep -q True \
    && ok "LoRaWAN: URL senza segreto, token in intestazione" || no "lorawan" "$(echo "$L" | head -c 200)"
else
  BP=$(call GET /api/iot/blueprints | py "[b['code'] for b in d if (b.get('protocol') or '')=='lorawan'][:1]")
  echo "SKIP  nessun blueprint LoRaWAN con quel codice ($BP)"
fi

echo "== SL-17 batteria sotto soglia (OsmAnd) =="
IMEI="35$(python -c 'import random;print(random.randint(10**12,10**13-1))')"
DG=$(docker exec -i ywdb psql -U postgres -d youworkforce -tAc "INSERT INTO app.devices(tenant_id,external_id,label,source) VALUES('$TID','$IMEI','Badge','gps') RETURNING id" | head -1 | tr -d '[:space:]')
TOK="e2e-batt-$R"; docker exec -i ywdb psql -U postgres -d youworkforce -qc "INSERT INTO app.ingest_tokens(token,token_hash,tenant_id,source,device_id) VALUES('$TOK',NULL,'$TID','gps','$DG')" >/dev/null
osm(){ curl -s -o /dev/null "$BASE/api/ingress/osmand?token=$TOK&id=$IMEI&lat=45.1&lon=9.1&batt=$1&timestamp=$(( $(date +%s) + $2 ))"; }
NB(){ q "SELECT count(*) FROM app.events WHERE tenant_id='$TID' AND device_id='$DG' AND etype IN ('batteria_bassa','batteria_critica')"; }
osm 60 0; [ "$(NB)" = "0" ] && ok "batteria al 60%: nessuna segnalazione" || no "60" "$(NB)"
osm 10 1; [ "$(NB)" = "1" ] && ok "scende al 10% su un normale record: una segnalazione" || no "10" "$(NB)"
osm 9 2; [ "$(NB)" = "1" ] && ok "resta sotto soglia: nessun doppione" || no "9" "$(NB)"
osm 80 3; osm 8 4; [ "$(NB)" = "2" ] && ok "ricarica e nuova discesa: nuova segnalazione" || no "ridiscesa" "$(NB)"

echo "hardening-audit-devices: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
