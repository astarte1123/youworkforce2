#!/usr/bin/env bash
# IoT Fase 3 — alert IoT in VISTA SEPARATA (soglia/offline/ripristino) + watchdog offline.
# Tenant: iot (SOLO modulo iot -> gli alert NON diventano incidenti di sicurezza: separazione).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="iot"; EMAIL="admin@iot.loneworker.eu"; PW="demo"
PASS=0; FAIL=0; ADVANCED=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }
adv(){ curl -s -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d "{\"seconds\":$1}" -o /dev/null; ADVANCED=$((ADVANCED+$1)); }
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"$PW\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
R=$RANDOM; EXT="E2E-ALERT-$R"
TID=$(sql "SELECT id FROM app.tenants WHERE slug='$SLUG'")

echo "== IoT Fase 3 — alert IoT in vista separata =="

# attivazione air-quality (soglia co2>=1000 'sensore_critico' + offline_after_s=900)
ACT=$(api -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"blueprint\":\"air-quality\",\"external_id\":\"$EXT\",\"label\":\"Alert e2e\"}")
DID=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('device_id',''))")
TOK=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('token',''))")
[ -n "$DID" ] && [ -n "$TOK" ] && ok "attivazione (device+token)" || { no "attivazione ko" "-"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }
OAS=$(sql "SELECT offline_after_s FROM app.devices WHERE id='$DID'")
[ "$OAS" = 900 ] && ok "watchdog offline configurato dal blueprint (900s)" || no "offline_after_s non impostato" "$OAS"

# 1) SOGLIA -> alert IoT (severità critical), nessun incidente di sicurezza (separazione)
RESP=$(curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"co2":1500}}')
AOK=$(echo "$RESP" | jq1 "d=json.load(sys.stdin);a=d.get('alert');print(1 if isinstance(a,dict) and a.get('id') and a.get('severity')=='critical' else 0)")
[ "$AOK" = 1 ] && ok "soglia -> alert IoT (severity critical) nella risposta" || no "alert non alzato" "$RESP"
INCOK=$(echo "$RESP" | jq1 "print(1 if json.load(sys.stdin).get('incident') is None else 0)")
[ "$INCOK" = 1 ] && ok "nessun incidente nella risposta (tenant IoT puro)" || no "incidente presente su tenant iot" "$RESP"
NINC=$(sql "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE e.device_id='$DID'")
[ "${NINC:-0}" = 0 ] && ok "SEPARAZIONE: nessun incidente di sicurezza creato" || no "incidente creato (separazione violata)" "$NINC"
AID=$(sql "SELECT id FROM app.iot_alerts WHERE device_id='$DID' AND kind='soglia' AND metric='co2' AND state='aperto' ORDER BY opened_at DESC LIMIT 1")
[ -n "$AID" ] && ok "alert 'soglia' persistito (aperto)" || no "alert non persistito" "-"

# 2) DEDUP anti-spam: nuova lettura oltre soglia -> stessa riga, occorrenze++
curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"co2":1600}}' -o /dev/null
NROW=$(sql "SELECT count(*) FROM app.iot_alerts WHERE device_id='$DID' AND kind='soglia'")
OCC=$(sql "SELECT occurrences FROM app.iot_alerts WHERE id='$AID'")
[ "${NROW:-0}" = 1 ] && [ "${OCC:-0}" -ge 2 ] && ok "dedup: 1 alert, occorrenze=$OCC (anti-spam)" || no "dedup ko" "rows=$NROW occ=$OCC"

# 3) summary + list
SUM=$(api "$BASE/api/iot/alerts/summary" | jq1 "d=json.load(sys.stdin);print(1 if d.get('open',0)>=1 and d.get('critical',0)>=1 else 0)")
[ "$SUM" = 1 ] && ok "summary conteggia aperti/critical" || no "summary ko" "-"
INLIST=$(api "$BASE/api/iot/alerts?state=aperto" | jq1 "print(1 if any(str(a.get('id'))=='$AID' for a in json.load(sys.stdin)) else 0)")
[ "$INLIST" = 1 ] && ok "alert compare nella vista 'Alert IoT'" || no "alert non in lista" "-"

# 4) ack + close
ACKED=$(api -X POST "$BASE/api/iot/alerts/$AID/ack" | jq1 "print(json.load(sys.stdin).get('ok'))")
ST=$(sql "SELECT state FROM app.iot_alerts WHERE id='$AID'")
[ "$ACKED" = True ] && [ "$ST" = "preso_in_carico" ] && ok "ack -> preso_in_carico" || no "ack ko" "$ST"
api -X POST "$BASE/api/iot/alerts/$AID/close" -o /dev/null
ST=$(sql "SELECT state FROM app.iot_alerts WHERE id='$AID'")
[ "$ST" = "chiuso" ] && ok "close -> chiuso" || no "close ko" "$ST"

# 5) WATCHDOG OFFLINE: avanza l'orologio oltre la soglia -> alert offline
adv 1000
api -X POST "$BASE/api/_test/iot-watchdog" -o /dev/null
for i in $(seq 1 10); do OFF=$(sql "SELECT count(*) FROM app.iot_alerts WHERE device_id='$DID' AND kind='offline' AND state='aperto'"); [ "${OFF:-0}" -ge 1 ] && break; sleep 0.3; done
[ "${OFF:-0}" -ge 1 ] && ok "watchdog: device muto -> alert offline" || no "offline non alzato" "$OFF"

# 6) RIPRISTINO: il device riparla -> chiude offline + alert ripristino
curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"co2":500}}' -o /dev/null
OFFC=$(sql "SELECT count(*) FROM app.iot_alerts WHERE device_id='$DID' AND kind='offline' AND state='chiuso'")
RIP=$(sql "SELECT count(*) FROM app.iot_alerts WHERE device_id='$DID' AND kind='ripristino'")
[ "${OFFC:-0}" -ge 1 ] && [ "${RIP:-0}" -ge 1 ] && ok "ripristino: offline chiuso + alert ripristino" || no "ripristino ko" "offc=$OFFC rip=$RIP"

# 7) ISOLAMENTO: un altro tenant (acme, con modulo iot) NON vede gli alert di iot
AJ=$(mktemp)
curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ" | tail -1)
LEAK=$(curl -s -b "$AJ" -H 'x-tenant-slug: acme' -H "x-csrf-token: $ACS" "$BASE/api/iot/alerts" | jq1 "print(sum(1 for a in json.load(sys.stdin) if str(a.get('device_id'))=='$DID'))")
[ "${LEAK:-0}" = 0 ] && ok "isolamento: un altro tenant non vede gli alert IoT di iot" || no "ISOLAMENTO VIOLATO" "$LEAK"

# ---- teardown ----
[ "$ADVANCED" -ne 0 ] && adv $((-ADVANCED))   # ripristina l'orologio
sql "DELETE FROM app.iot_alerts WHERE device_id='$DID'" >/dev/null
sql "DELETE FROM app.ingest_tokens WHERE device_id='$DID'" >/dev/null
api -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices d USING app.tenants t WHERE t.id=d.tenant_id AND t.slug='$SLUG' AND d.external_id='$EXT'" >/dev/null
sql "DELETE FROM app.sensor_rules sr USING app.tenants t WHERE t.id=sr.tenant_id AND t.slug='$SLUG' AND sr.label='CO₂ elevata (areare)'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
