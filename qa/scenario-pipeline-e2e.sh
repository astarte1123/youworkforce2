#!/usr/bin/env bash
# IO-7 (decisione PO 26/11/2026): demo e aerazione degli scenari passano dalla pipeline vera.
# - la demo è telemetria (device_telemetry + regole + alert IoT); senza Lone Worker nessun incidente
#   di sicurezza; senza modulo IoT la demo è rifiutata;
# - l'aerazione è un comando (iot_commands set_aeration), eseguito dal dispositivo simulato che
#   risponde con la telemetria; il cruscotto lo mostra.
# Tenant usa-e-getta (solo IoT).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -Atc "$1"; }
J=$(mktemp -d); R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="sp$R"; SCN=smart_farming
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
sa(){ curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
sac(){ curl -s -o /dev/null -w "%{http_code}" -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
TID=$(sa POST /api/superadmin/tenants "{\"slug\":\"$SLUG\",\"name\":\"Sp $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"iot\"]}" | py "d.get('id','')")
trap 'sa DELETE "/api/superadmin/tenants/$TID" "{\"slug\":\"$SLUG\"}" >/dev/null; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }

C=$(sac POST "/api/superadmin/tenants/$TID/scenarios/$SCN/demo"); [ "$C" = "409" ] && ok "demo senza dispositivi demo: rifiutata con spiegazione" || no "demo senza device" "$C"
sa POST "/api/superadmin/tenants/$TID/scenarios" "{\"code\":\"$SCN\",\"with_demo\":true}" >/dev/null
D=$(sa POST "/api/superadmin/tenants/$TID/scenarios/$SCN/demo")
[ "$(echo "$D" | py "d['events']")" -ge 3 ] && ok "demo: campioni inviati come telemetria ($(echo "$D" | py "d['events']"))" || no "demo eventi" "$D"
[ "$(echo "$D" | py "d['alerts']")" -ge 1 ] && ok "demo: le regole alzano alert IoT" || no "demo alert" "$D"
[ "$(echo "$D" | py "d['incidents']")" = "0" ] && ok "demo senza Lone Worker: nessun incidente di sicurezza" || no "demo incidenti" "$D"
TEL=$(sql "SELECT count(*) FROM app.device_telemetry WHERE tenant_id='$TID'")
[ "$TEL" -ge 10 ] && ok "telemetria salvata nella serie storica ($TEL letture)" || no "telemetria" "$TEL"
[ "$(sql "SELECT count(*) FROM app.incidents WHERE tenant_id='$TID'")" = "0" ] && ok "Centrale vuota (modulo Lone Worker spento)" || no "incidenti su DB" "-"

curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
DASH=$(call GET "/api/scenarios/$SCN/dashboard")
[ "$(echo "$DASH" | py "len(d['incidents'])")" -ge 1 ] && ok "cruscotto: allarmi recenti dagli alert IoT" || no "cruscotto allarmi" "$(echo "$DASH" | head -c 300)"
DEV=$(sql "SELECT id FROM app.devices WHERE tenant_id='$TID' AND external_id='scn-$SCN-silo-a'")
A=$(call POST "/api/scenarios/$SCN/aeration" "{\"device_id\":\"$DEV\",\"on\":true}")
[ "$(echo "$A" | py "d.get('status')")" = "done" ] && ok "aerazione: comando eseguito dal silo simulato" || no "aerazione" "$A"
[ "$(sql "SELECT method||':'||(params->>'on')||':'||status FROM app.iot_commands WHERE tenant_id='$TID' AND device_id='$DEV' ORDER BY created_at DESC LIMIT 1")" = "set_aeration:true:done" ] \
  && ok "aerazione registrata nella coda comandi del dispositivo" || no "coda comandi" "-"
[ "$(sql "SELECT str FROM app.device_telemetry WHERE device_id='$DEV' AND key='aeration' ORDER BY ts DESC LIMIT 1")" = "true" ] && ok "il silo risponde con la telemetria «aerazione accesa»" || no "telemetria aerazione" "-"
[ "$(sql "SELECT COALESCE(config->>'aeration','-') FROM app.devices WHERE id='$DEV'")" = "-" ] && ok "nessuna scrittura diretta dello stato del dispositivo" || no "config scritta" "-"
call GET "/api/scenarios/$SCN/dashboard" | py "[s['aeration'] for s in d['silos'] if s['external_id']=='scn-$SCN-silo-a'][0]" | grep -q True && ok "cruscotto: aerazione accesa sul silo A" || no "cruscotto aerazione" "-"

# senza modulo IoT la demo è rifiutata
sac POST "/api/superadmin/tenants/$TID/modules" '{"module_code":"iot","state":"non_abilitato"}' >/dev/null
C=$(sac POST "/api/superadmin/tenants/$TID/scenarios/$SCN/demo"); [ "$C" = "409" ] && ok "modulo IoT spento: demo rifiutata" || no "demo senza IoT" "$C"
echo "scenario-pipeline: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
