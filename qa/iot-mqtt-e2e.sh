#!/usr/bin/env bash
# IoT Fase 2b — broker MQTT nativo (TCP 1883). Tenant: iot (SOLO modulo iot -> autonomia).
# Prova: attivazione blueprint MQTT, ingest via PUBLISH (normalizzazione+soglia), isolamento
# multi-tenant dal topic, credenziale non valida rifiutata, downlink comandi su SUBSCRIBE.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
MQ_HOST="${MQ_HOST:-127.0.0.1}"; MQ_PORT="${MQ_PORT:-1883}"
SLUG="iot"; EMAIL="admin@iot.loneworker.eu"; PW="demo"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }
HERE="$(cd "$(dirname "$0")" && pwd)"
MC(){ python "$HERE/mqtt_client.py" "$@"; }
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"$PW\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
R=$RANDOM; EXT="E2E-MQTT-$R"
TID=$(sql "SELECT id FROM app.tenants WHERE slug='$SLUG'")

echo "== IoT Fase 2b — broker MQTT nativo =="

# 0) il broker è in ascolto
(exec 3<>/dev/tcp/$MQ_HOST/$MQ_PORT) 2>/dev/null && ok "broker MQTT in ascolto ($MQ_HOST:$MQ_PORT)" || { no "broker non in ascolto" "$MQ_PORT"; echo "== $PASS passed, $FAIL failed =="; exit 1; }

# 1) attivazione blueprint MQTT (air-quality) -> device + token + istruzioni mqtt
ACT=$(api -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"blueprint\":\"air-quality\",\"external_id\":\"$EXT\",\"label\":\"Aria e2e\"}")
DID=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('device_id',''))")
TOK=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('token',''))")
PROTO=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('connect',{}).get('protocol',''))")
CT=$(echo "$ACT" | jq1 "d=json.load(sys.stdin);print(1 if d.get('connect',{}).get('command_topic','').endswith('/command') and d.get('connect',{}).get('port')==1883 else 0)")
[ -n "$DID" ] && [ -n "$TOK" ] && [ "$PROTO" = "mqtt" ] && ok "attivazione MQTT: device+token+protocollo" || no "attivazione mqtt ko" "proto=$PROTO"
[ "$CT" = 1 ] && ok "istruzioni MQTT: command_topic + porta 1883" || no "istruzioni mqtt incomplete" "-"
TOPIC="yw/$TID/$EXT/telemetry"

# 2) credenziale non valida -> CONNACK 5 (not authorized)
RC=$(MC connrc "$MQ_HOST" "$MQ_PORT" "token-inesistente-xyz" | awk '/CONNACK/{print $2}')
[ "$RC" = 5 ] && ok "credenziale non valida rifiutata (CONNACK 5)" || no "token invalido non rifiutato" "rc=$RC"

# 3) ingest via PUBLISH: co2=1200 (soglia >=1000) + t=21 (->temperature)
RC=$(MC pub "$MQ_HOST" "$MQ_PORT" "$TOK" "$TOPIC" '{"values":{"co2":1200,"t":21}}' | awk '/CONNACK/{print $2}')
[ "$RC" = 0 ] && ok "CONNACK accettata con token valido" || no "connect valido rifiutato" "rc=$RC"
for i in $(seq 1 20); do N=$(sql "SELECT count(*) FROM app.device_telemetry WHERE device_id='$DID'"); [ "${N:-0}" -ge 2 ] && break; sleep 0.3; done
NC=$(sql "SELECT metric||'|'||num FROM app.device_telemetry WHERE device_id='$DID' AND key='co2' ORDER BY ts DESC LIMIT 1")
[ "$NC" = "co2|1200" ] && ok "telemetria MQTT salvata + normalizzata (co2)" || no "ingest mqtt co2 ko" "$NC"
NTemp=$(sql "SELECT metric FROM app.device_telemetry WHERE device_id='$DID' AND key='t' ORDER BY ts DESC LIMIT 1")
[ "$NTemp" = "temperature" ] && ok "chiave grezza 't' normalizzata a temperature" || no "normalizzazione temperature ko" "$NTemp"
# soglia co2>=1000 -> incidente nativo
for i in $(seq 1 20); do INC=$(sql "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DID' AND i.etype='sensore_critico'"); [ "${INC:-0}" -ge 1 ] && break; sleep 0.3; done
[ "${INC:-0}" -ge 1 ] && ok "soglia MQTT scatta incidente (co2>=1000)" || no "soglia mqtt non scatta" "$INC"

# 4) ISOLAMENTO: PUBLISH su topic di un ALTRO tenant -> scartato (nessuna telemetria col marcatore)
FAKE="00000000-0000-0000-0000-000000000000"
MC pub "$MQ_HOST" "$MQ_PORT" "$TOK" "yw/$FAKE/$EXT/telemetry" '{"values":{"co2":4242}}' >/dev/null
sleep 0.8
LEAK=$(sql "SELECT count(*) FROM app.device_telemetry WHERE device_id='$DID' AND num=4242")
[ "${LEAK:-0}" = 0 ] && ok "isolamento: publish su topic di altro tenant scartata" || no "ISOLAMENTO VIOLATO" "leak=$LEAK"

# 5) downlink comandi: inserisco un comando pending, SUBSCRIBE -> ricevo la PUBLISH
sql "INSERT INTO app.iot_commands(tenant_id,device_id,method,params,status) VALUES('$TID','$DID','reboot','{\"delay\":5}'::jsonb,'pending')" >/dev/null
CMDTOPIC="yw/$TID/$EXT/command"
MSG=$(MC sub "$MQ_HOST" "$MQ_PORT" "$TOK" "$CMDTOPIC" 2 | awk '/^MSG/{print;exit}')
echo "$MSG" | grep -q 'reboot' && ok "downlink: comando pending consegnato su SUBSCRIBE" || no "downlink comando ko" "$MSG"
SENT=$(sql "SELECT status FROM app.iot_commands WHERE device_id='$DID' AND method='reboot' ORDER BY created_at DESC LIMIT 1")
[ "$SENT" = "sent" ] && ok "comando marcato 'sent' dopo la consegna" || no "comando non marcato sent" "$SENT"

# ---- teardown COMPLETO ----
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e iot-mqtt' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.event_id IN (SELECT id FROM app.events WHERE device_id='$DID')" >/dev/null
sql "DELETE FROM app.iot_commands WHERE device_id='$DID'" >/dev/null
sql "DELETE FROM app.ingest_tokens WHERE device_id='$DID'" >/dev/null
api -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices d USING app.tenants t WHERE t.id=d.tenant_id AND t.slug='$SLUG' AND d.external_id='$EXT'" >/dev/null
sql "DELETE FROM app.sensor_rules sr USING app.tenants t WHERE t.id=sr.tenant_id AND t.slug='$SLUG' AND sr.label='CO₂ elevata (areare)'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
