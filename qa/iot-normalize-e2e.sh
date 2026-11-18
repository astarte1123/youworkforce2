#!/usr/bin/env bash
# IoT Fase 1 — normalizzazione: metriche canoniche + mappa per-device (chiave grezza -> metrica+unità).
# Tenant: iot (SOLO modulo iot attivo -> prova l'AUTONOMIA del modulo). Idempotente, teardown completo.
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
TID="(SELECT id FROM app.tenants WHERE slug='$SLUG')"
R=$RANDOM; EXT="E2E-NORM-$R"

echo "== IoT Fase 1 — normalizzazione (tenant iot, standalone) =="

say_metrics() {
  M=$(api "$BASE/api/iot/metrics")
  echo "$M" | jq1 "d=json.load(sys.stdin);print(1 if any(x['metric']=='temperature' and x['unit'] for x in d) else 0)"
}
[ "$(say_metrics)" = 1 ] && ok "catalogo metriche canoniche (temperature con unità)" || no "metriche assenti" "-"

DID=$(api -H 'content-type: application/json' -X POST "$BASE/api/devices" -d "{\"external_id\":\"$EXT\",\"source\":\"iot\",\"label\":\"Sensore e2e\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$DID" ] && ok "device IoT creato" || no "device non creato" "-"
TOK=$(api -H 'content-type: application/json' -X POST "$BASE/api/ingest-tokens" -d "{\"source\":\"iot\",\"device_id\":\"$DID\"}" | jq1 "print(json.load(sys.stdin).get('token',''))")
[ -n "$TOK" ] && ok "token device-bound creato" || no "token non creato" "-"

# mappa: t -> temperature (1:1); tr -> temperature con scala 0.1 (255 -> 25.5); foo NON mappata
MR=$(api -H 'content-type: application/json' -X PUT "$BASE/api/iot/devices/$DID/map" -d '{"rows":[{"raw_key":"t","metric":"temperature"},{"raw_key":"tr","metric":"temperature","scale":0.1}]}' | jq1 "print(json.load(sys.stdin).get('n',0))")
[ "$MR" = 2 ] && ok "mappa di normalizzazione salvata (2 righe)" || no "mappa non salvata" "$MR"
# metrica sconosciuta -> 400
BADC=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X PUT "$BASE/api/iot/devices/$DID/map" -d '{"rows":[{"raw_key":"x","metric":"inesistente"}]}')
[ "$BADC" = 400 ] && ok "metrica sconosciuta rifiutata (400)" || no "validazione metrica ko" "$BADC"
# ripristina la mappa buona (il 400 ha fatto rollback, ma riallineo per chiarezza)
api -H 'content-type: application/json' -X PUT "$BASE/api/iot/devices/$DID/map" -d '{"rows":[{"raw_key":"t","metric":"temperature"},{"raw_key":"tr","metric":"temperature","scale":0.1}]}' >/dev/null

# ingest telemetria grezza
curl -s -o /dev/null -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"t":7,"tr":255,"foo":1}}'

# assert normalizzazione nel time-series
NT=$(sql "SELECT metric||'|'||unit||'|'||num FROM app.device_telemetry WHERE device_id='$DID' AND key='t' ORDER BY ts DESC LIMIT 1")
[ "$NT" = "temperature|°C|7" ] && ok "chiave grezza 't' -> temperature °C num 7" || no "normalizzazione t errata" "$NT"
NTR=$(sql "SELECT metric||'|'||num FROM app.device_telemetry WHERE device_id='$DID' AND key='tr' ORDER BY ts DESC LIMIT 1")
[ "$NTR" = "temperature|25.5" ] && ok "scala lineare 'tr' 255 -> 25.5 temperature" || no "trasformazione lineare errata" "$NTR"
NFOO=$(sql "SELECT COALESCE(metric,'<null>') FROM app.device_telemetry WHERE device_id='$DID' AND key='foo' ORDER BY ts DESC LIMIT 1")
[ "$NFOO" = "<null>" ] && ok "chiave non mappata resta grezza (metric NULL)" || no "foo non doveva avere metrica" "$NFOO"

# latest espone metric+unit
LAT=$(api "$BASE/api/iot/devices/$DID/telemetry/latest" | jq1 "d=json.load(sys.stdin);print(1 if any(x.get('metric')=='temperature' and x.get('unit') for x in d) else 0)")
[ "$LAT" = 1 ] && ok "latest espone metrica+unità" || no "latest senza metrica" "$LAT"

# serie filtrabile per metrica canonica (non solo chiave grezza)
SER=$(api "$BASE/api/iot/devices/$DID/telemetry?key=temperature" | jq1 "print(len(json.load(sys.stdin)))")
[ "${SER:-0}" -ge 2 ] && ok "serie filtrabile per metrica canonica (temperature)" || no "serie per metrica ko" "$SER"

# regola su metrica CANONICA scatta anche con chiave grezza
api -H 'content-type: application/json' -X POST "$BASE/api/sensor-rules" -d '{"metric":"temperature","op":">=","threshold":5,"etype":"sensore_critico","label":"e2e temp alta"}' >/dev/null
INC=$(curl -s -X POST "$BASE/api/ingress/telemetry?token=$TOK" -H 'content-type: application/json' -d '{"values":{"t":8}}' | jq1 "d=json.load(sys.stdin);a=d.get('alert');print(1 if isinstance(a,dict) and a.get('id') else 0)")
[ "$INC" = 1 ] && ok "regola su metrica canonica scatta da chiave grezza 't'" || no "regola canonica non scattata" "$INC"

# ---- teardown COMPLETO ----
sql "DELETE FROM app.sensor_rules sr USING app.tenants t WHERE t.id=sr.tenant_id AND t.slug='$SLUG' AND sr.label='e2e temp alta'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e iot-normalize' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'iot-telem-%'" >/dev/null
sql "DELETE FROM app.ingest_tokens it USING app.tenants t WHERE t.id=it.tenant_id AND t.slug='$SLUG' AND it.device_id='$DID'" >/dev/null
api -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices d USING app.tenants t WHERE t.id=d.tenant_id AND t.slug='$SLUG' AND d.external_id='$EXT'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
