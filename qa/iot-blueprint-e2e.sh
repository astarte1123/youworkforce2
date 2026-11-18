#!/usr/bin/env bash
# IoT Fase 2a — Blueprint (profilo-lavoro) + attivazione zero-touch + webhook LoRaWAN/TTN.
# Tenant: iot (SOLO modulo iot -> autonomia). L'admin sceglie il lavoro, tutto si configura da solo.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="iot"; EMAIL="admin@iot.loneworker.eu"; PW="demo"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"$PW\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
R=$RANDOM; EH="E2E-BPH-$R"; EL="E2E-BPL-$R"

echo "== IoT Fase 2a — blueprint + attivazione zero-touch =="

# catalogo
BP=$(api "$BASE/api/iot/blueprints")
echo "$BP" | jq1 "d=json.load(sys.stdin);print(1 if any(x['code']=='cold-chain' and x['protocol']=='http' for x in d) else 0)" | grep -q 1 && ok "catalogo blueprint (cold-chain http)" || no "blueprint assente" "-"

# ---- attivazione HTTP (cold-chain) ----
ACT=$(api -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"blueprint\":\"cold-chain\",\"external_id\":\"$EH\",\"label\":\"Cella e2e\"}")
DID=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('device_id',''))")
TOK=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('token',''))")
URLOK=$(echo "$ACT" | jq1 "d=json.load(sys.stdin);print(1 if d.get('connect',{}).get('url','').startswith('/api/ingress/telemetry') else 0)")
[ -n "$DID" ] && [ -n "$TOK" ] && ok "attivazione crea device + token" || no "attivazione ko" "did=$DID"
[ "$URLOK" = 1 ] && ok "istruzioni di collegamento HTTP presenti" || no "connect.url http assente" "-"

# device con source iot + blueprint in config
SRC=$(sql "SELECT source||'|'||COALESCE(config->>'blueprint','') FROM app.devices WHERE id='$DID'")
[ "$SRC" = "iot|cold-chain" ] && ok "device source=iot + blueprint in config" || no "device config errata" "$SRC"
# mappa di normalizzazione applicata (temperature, >=1 riga)
NM=$(sql "SELECT count(*) FROM app.device_metric_map WHERE device_id='$DID' AND metric='temperature'")
[ "${NM:-0}" -ge 1 ] && ok "mappa normalizzazione applicata dal blueprint" || no "mappa non applicata" "$NM"
# soglia di default creata (temperature >= 8)
SR=$(sql "SELECT count(*) FROM app.sensor_rules sr JOIN app.tenants t ON t.id=sr.tenant_id WHERE t.slug='$SLUG' AND sr.metric='temperature' AND sr.op='>=' AND sr.threshold=8")
[ "${SR:-0}" -ge 1 ] && ok "soglia di default creata dal blueprint" || no "soglia non creata" "$SR"

# dato grezzo 't' arriva normalizzato e la soglia scatta
INC=$(curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"t":12}}' | jq1 "d=json.load(sys.stdin);a=d.get('alert');print(1 if isinstance(a,dict) and a.get('id') else 0)")
[ "$INC" = 1 ] && ok "telemetria HTTP normalizzata + alert IoT scatta (t=12 -> temperature>=8)" || no "flusso http end-to-end ko" "$INC"
NT=$(sql "SELECT metric||'|'||num FROM app.device_telemetry WHERE device_id='$DID' AND key='t' ORDER BY ts DESC LIMIT 1")
[ "$NT" = "temperature|12" ] && ok "valore grezzo salvato come metrica canonica" || no "normalizzazione http ko" "$NT"

# idempotenza: ri-attivare lo stesso external_id -> 409
C2=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"blueprint\":\"cold-chain\",\"external_id\":\"$EH\"}")
[ "$C2" = 409 ] && ok "doppione device rifiutato (409)" || no "doppione non rifiutato" "$C2"

# ---- attivazione LoRaWAN (env-lorawan) + webhook TTN ----
ACTL=$(api -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"blueprint\":\"env-lorawan\",\"external_id\":\"$EL\"}")
DIDL=$(echo "$ACTL" | jq1 "print(json.load(sys.stdin).get('device_id',''))")
TOKL=$(echo "$ACTL" | jq1 "print(json.load(sys.stdin).get('token',''))")
WH=$(echo "$ACTL" | jq1 "d=json.load(sys.stdin);print(1 if d.get('connect',{}).get('webhook_url','').startswith('/api/ingress/lorawan') else 0)")
[ "$WH" = 1 ] && ok "attivazione LoRaWAN: webhook_url presente" || no "webhook lorawan assente" "-"
# uplink TTN v3 (payload decodificato) -> telemetria normalizzata
STORED=$(curl -s -X POST "$BASE/api/ingress/lorawan?token=$TOKL" -H 'content-type: application/json' \
  -d '{"end_device_ids":{"device_id":"'"$EL"'"},"uplink_message":{"decoded_payload":{"temperature":21.5,"humidity":60,"battery":90}}}' \
  | jq1 "print(json.load(sys.stdin).get('stored',0))")
[ "${STORED:-0}" -ge 3 ] && ok "webhook LoRaWAN/TTN ingerisce il payload decodificato ($STORED)" || no "lorawan ingest ko" "$STORED"
NTL=$(sql "SELECT metric||'|'||unit FROM app.device_telemetry WHERE device_id='$DIDL' AND key='temperature' ORDER BY ts DESC LIMIT 1")
[ "$NTL" = "temperature|°C" ] && ok "telemetria LoRaWAN normalizzata (metrica+unità)" || no "normalizzazione lorawan ko" "$NTL"

# ---- teardown COMPLETO ----
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e iot-blueprint' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'iot-telem-%'" >/dev/null
sql "DELETE FROM app.ingest_tokens it USING app.tenants t WHERE t.id=it.tenant_id AND t.slug='$SLUG' AND it.device_id IN ('$DID','$DIDL')" >/dev/null
api -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
api -X DELETE "$BASE/api/devices/$DIDL" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices d USING app.tenants t WHERE t.id=d.tenant_id AND t.slug='$SLUG' AND d.external_id IN ('$EH','$EL')" >/dev/null
# le soglie di default del blueprint sono per-tenant: le rimuovo per non lasciare residui
sql "DELETE FROM app.sensor_rules sr USING app.tenants t WHERE t.id=sr.tenant_id AND t.slug='$SLUG' AND sr.label IN ('Temperatura cella troppo alta','Batteria sensore scarica')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
