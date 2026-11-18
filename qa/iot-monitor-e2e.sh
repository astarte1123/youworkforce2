#!/usr/bin/env bash
# IoT Fase 4 — monitoraggio flotta IoT: stato online/offline, ultimi valori canonici, trend, alert.
# Tenant: iot (SOLO modulo iot -> autonomia).
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
R=$RANDOM; EXT="E2E-MON-$R"
mon(){ api "$BASE/api/iot/monitor"; }
devj(){ mon | jq1 "d=json.load(sys.stdin);print(next((json.dumps(x) for x in d['devices'] if x['external_id']=='$EXT'),''))"; }

echo "== IoT Fase 4 — monitoraggio IoT =="

# attivazione air-quality (watchdog 900s, soglia co2>=1000)
ACT=$(api -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"blueprint\":\"air-quality\",\"external_id\":\"$EXT\",\"label\":\"Mon e2e\"}")
DID=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('device_id',''))")
TOK=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('token',''))")
[ -n "$DID" ] && [ -n "$TOK" ] && ok "attivazione device" || { no "attivazione ko" "-"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }

# serie di telemetria (trend) con metriche canoniche
for val in 410 430 455 480; do curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d "{\"values\":{\"co2\":$val,\"t\":21}}" -o /dev/null; done

# 1) device compare nel monitor, online, con ultimi valori canonici
D=$(devj)
ONLINE=$(echo "$D" | jq1 "print(1 if json.load(sys.stdin).get('online') else 0)")
[ "$ONLINE" = 1 ] && ok "device presente e online" || no "device non online" "$D"
CO2=$(echo "$D" | jq1 "d=json.load(sys.stdin);print(next((m.get('value') for m in d.get('metrics',[]) if m['metric']=='co2'),''))")
[ "$CO2" = "480.0" ] || [ "$CO2" = "480" ] && ok "ultimo valore canonico co2=480 (con unità)" || no "ultimo valore co2 errato" "$CO2"
UNIT=$(echo "$D" | jq1 "d=json.load(sys.stdin);print(next((m.get('unit') for m in d.get('metrics',[]) if m['metric']=='co2'),''))")
[ "$UNIT" = "ppm" ] && ok "unità di misura presente (ppm)" || no "unità mancante" "$UNIT"

# 2) trend (spark) della metrica primaria popolato
SPK=$(echo "$D" | jq1 "print(len(json.load(sys.stdin).get('spark',[])))")
[ "${SPK:-0}" -ge 2 ] && ok "trend (spark) popolato ($SPK punti)" || no "spark vuoto" "$SPK"

# 3) summary coerente
SUMOK=$(mon | jq1 "d=json.load(sys.stdin)['summary'];print(1 if d.get('total',0)>=1 and d.get('online',0)>=1 else 0)")
[ "$SUMOK" = 1 ] && ok "summary flotta (totali/online)" || no "summary ko" "-"

# 4) soglia -> alert aperto riflesso nel monitor
curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"co2":1500}}' -o /dev/null
AL=$(devj | jq1 "print(json.load(sys.stdin).get('open_alerts',0))")
[ "${AL:-0}" -ge 1 ] && ok "alert aperti riflessi nel monitor ($AL)" || no "open_alerts non riflesso" "$AL"

# 5) OFFLINE: avanza orologio oltre watchdog + sweep -> device offline nel monitor
adv 1000
api -X POST "$BASE/api/_test/iot-watchdog" -o /dev/null
for i in $(seq 1 10); do OFF=$(devj | jq1 "print(0 if json.load(sys.stdin).get('online') else 1)"); [ "$OFF" = 1 ] && break; sleep 0.3; done
[ "$OFF" = 1 ] && ok "device risulta offline nel monitor dopo il silenzio" || no "device ancora online" "$OFF"

# ---- teardown ----
[ "$ADVANCED" -ne 0 ] && adv $((-ADVANCED))
sql "DELETE FROM app.iot_alerts WHERE device_id='$DID'" >/dev/null
sql "DELETE FROM app.ingest_tokens WHERE device_id='$DID'" >/dev/null
api -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices d USING app.tenants t WHERE t.id=d.tenant_id AND t.slug='$SLUG' AND d.external_id='$EXT'" >/dev/null
sql "DELETE FROM app.sensor_rules sr USING app.tenants t WHERE t.id=sr.tenant_id AND t.slug='$SLUG' AND sr.label='CO₂ elevata (areare)'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
