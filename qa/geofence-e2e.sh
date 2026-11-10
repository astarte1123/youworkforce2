#!/usr/bin/env bash
# Geofence: crea, rinomina, sposta/ridimensiona, associa dispositivi+eventi, elimina.
# (tenant_admin; modulo traccar). Verifica il fix elimina/modifica + le associazioni.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
AD=$(mktemp); curl -s -o /dev/null -c "$AD" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
ACX=$(awk '$6=="yw_csrf"{print $7}' "$AD"|tail -1)
aapi(){ curl -s -b "$AD" -H 'x-tenant-slug: acme' -H "x-csrf-token: $ACX" "$@"; }

GID=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/geofences" -d '{"name":"QA Area","kind":"circle","geom":{"lat":45.46,"lon":9.19,"radius":300}}' | jv 'd.get("id","")')
[ -n "$GID" ] && ok "crea geofence" || no "create fallita" "-"
DEV=$(aapi "$BASE/api/devices" | jv '(d[0]["id"] if d else "")')

aapi -H 'content-type: application/json' -X PUT "$BASE/api/geofences/$GID" -d '{"name":"QA Area Rinominata"}' | grep -q '"ok":true' && ok "rinomina (PUT name)" || no "rinomina fallita" "-"
aapi -H 'content-type: application/json' -X PUT "$BASE/api/geofences/$GID" -d '{"geom":{"lat":45.47,"lon":9.20,"radius":500}}' | grep -q '"ok":true' && ok "sposta/ridimensiona (PUT geom)" || no "move fallita" "-"
R=$(aapi "$BASE/api/geofences" | jv '[g["geom"]["radius"] for g in d if g["id"]=="'"$GID"'"][0]')
[ "$R" = "500" ] && ok "geometria aggiornata (radius 500)" || no "geom non aggiornata" "$R"

if [ -n "$DEV" ]; then
  aapi -H 'content-type: application/json' -X PUT "$BASE/api/geofences/$GID/devices" -d "{\"device_ids\":[\"$DEV\"]}" | grep -q '"ok":true' && ok "associa device" || no "assoc device fallita" "-"
fi
aapi -H 'content-type: application/json' -X PUT "$BASE/api/geofences/$GID/events" -d '{"etypes":["sos","man_down","device_offline"]}' | grep -q '"count":3' && ok "associa 3 eventi" || no "assoc eventi fallita" "-"
# etype non ammesso -> 400
C=$(aapi -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X PUT "$BASE/api/geofences/$GID/events" -d '{"etypes":["pippo"]}')
[ "$C" = "400" ] && ok "etype non ammesso -> 400" || no "etype non validato" "$C"
# list riflette le associazioni
L=$(aapi "$BASE/api/geofences" | jv '[{"d":len(g["device_ids"]),"e":len(g["etypes"])} for g in d if g["id"]=="'"$GID"'"][0]')
echo "$L" | grep -q "'e': 3" && ok "list mostra associazioni" || no "associazioni non in list" "$L"

# elimina (il bug era confirm() lato UI; qui l'API deve rispondere 200 e sparire)
aapi -o /dev/null -w '%{http_code}\n' -X DELETE "$BASE/api/geofences/$GID" | grep -q 200 && ok "elimina geofence (API)" || no "delete fallita" "-"
[ "$(aapi "$BASE/api/geofences" | jv 'len([g for g in d if g["id"]=="'"$GID"'"])')" = "0" ] && ok "geofence rimossa (+ associazioni via cascade)" || no "ancora presente" "-"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
