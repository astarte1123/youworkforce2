#!/usr/bin/env bash
# IoT Fase 5 — catalogo sensori REALI agganciati ai blueprint + attivazione per modello.
# Tenant: iot (SOLO modulo iot). "Quale sensore per quale lavoro": scelto il modello, tutto si configura.
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
R=$RANDOM; EXT="E2E-CAT-$R"

echo "== IoT Fase 5 — catalogo sensori reali =="

# 1) catalogo: esistono sensori IoT (category sensore) con blueprint agganciato + protocollo non-GPS
CAT=$(api "$BASE/api/catalog/devices?category=sensore")
NSENS=$(echo "$CAT" | jq1 "d=json.load(sys.stdin);print(sum(1 for m in d if m.get('blueprint')))")
[ "${NSENS:-0}" -ge 5 ] && ok "catalogo espone sensori IoT con blueprint ($NSENS)" || no "sensori IoT assenti" "$NSENS"
PROTO=$(echo "$CAT" | jq1 "d=json.load(sys.stdin);print(1 if any(m.get('protocol') in ('lorawan','mqtt','http') and m.get('blueprint') for m in d) else 0)")
[ "$PROTO" = 1 ] && ok "sensori con protocolli IoT (lorawan/mqtt/http)" || no "protocolli IoT assenti" "-"
# un modello noto con capability = metrica canonica
CAP=$(echo "$CAT" | jq1 "d=json.load(sys.stdin);m=next((x for x in d if x['model_code']=='dragino-lht65'),{});print(1 if 'temperature' in (m.get('capabilities') or []) and m.get('blueprint')=='cold-chain' else 0)")
[ "$CAP" = 1 ] && ok "modello reale mappa capability canoniche (LHT65→temperature, cold-chain)" || no "capability/blueprint modello errati" "$CAP"

# 1b) SEPARAZIONE PER PERIMETRO (divisione netta IoT vs Sicurezza/Campo)
cats(){ api "$1" | jq1 "import sys,json;print(','.join(sorted({m['category'] for m in json.load(sys.stdin)})))"; }
SIOT=$(cats "$BASE/api/catalog/devices?scope=iot")
[ "$SIOT" = "sensore" ] && ok "scope=iot → SOLO sensori" || no "scope iot non pulito" "$SIOT"
SSAF=$(cats "$BASE/api/catalog/devices?scope=safety")
echo "$SSAF" | grep -qv "sensore" && [ -n "$SSAF" ] && ! echo "$SSAF" | grep -q "sensore\|altro" && ok "scope=safety → niente sensori né dump GPS ($SSAF)" || no "scope safety non pulito" "$SSAF"
# il dump GPS 'altro' NON compare di default, ma emerge cercando
DEF=$(api "$BASE/api/catalog/devices" | jq1 "print(sum(1 for m in json.load(sys.stdin) if m['category']=='altro'))")
[ "${DEF:-x}" = 0 ] && ok "default: dump GPS 'altro' nascosto" || no "altro non nascosto" "$DEF"
SRCH=$(api "$BASE/api/catalog/devices?q=teltonika" | jq1 "print(len(json.load(sys.stdin)))")
[ "${SRCH:-0}" -ge 10 ] && ok "ricerca testuale include tutto il catalogo ($SRCH)" || no "ricerca non trova nulla" "$SRCH"
# facets coerenti col perimetro
FS=$(api "$BASE/api/catalog/facets?scope=iot" | jq1 "print(json.load(sys.stdin)['categories'])")
[ "$FS" = "['sensore']" ] && ok "facets scope=iot → solo 'sensore'" || no "facets iot errate" "$FS"

# 2) blueprint elenca i sensori consigliati
BP=$(api "$BASE/api/iot/blueprints")
MODS=$(echo "$BP" | jq1 "d=json.load(sys.stdin);b=next((x for x in d if x['code']=='air-quality'),{});print(len(b.get('models',[])))")
[ "${MODS:-0}" -ge 1 ] && ok "blueprint air-quality elenca sensori consigliati ($MODS)" || no "nessun modello sul blueprint" "$MODS"

# 3) attivazione PER MODELLO (senza blueprint esplicito): deriva il lavoro dal sensore scelto
ACT=$(api -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"model_code\":\"dragino-lht65\",\"external_id\":\"$EXT\",\"label\":\"Sonda e2e\"}")
DID=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('device_id',''))")
BPR=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('blueprint',''))")
MC=$(echo "$ACT" | jq1 "print(json.load(sys.stdin).get('model_code',''))")
[ -n "$DID" ] && [ "$BPR" = "cold-chain" ] && [ "$MC" = "dragino-lht65" ] && ok "attivazione per modello deriva il blueprint (cold-chain)" || no "attivazione per modello ko" "bp=$BPR mc=$MC"
# device salva model_code + blueprint in config + offline_after_s dal blueprint
ROW=$(sql "SELECT model_code||'|'||COALESCE(config->>'blueprint','')||'|'||COALESCE(offline_after_s::text,'') FROM app.devices WHERE id='$DID'")
[ "$ROW" = "dragino-lht65|cold-chain|900" ] && ok "device: model_code + blueprint + watchdog configurati" || no "config device errata" "$ROW"
# normalizzazione del blueprint applicata (temperature)
NM=$(sql "SELECT count(*) FROM app.device_metric_map WHERE device_id='$DID' AND metric='temperature'")
[ "${NM:-0}" -ge 1 ] && ok "normalizzazione applicata (temperature)" || no "mappa non applicata" "$NM"

# 4) errore onesto: modello generico senza blueprint e senza blueprint esplicito -> 400
C=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/iot/activate" -d "{\"model_code\":\"generic-mqtt-sensor\",\"external_id\":\"$EXT-GEN\"}")
[ "$C" = 400 ] && ok "modello generico senza lavoro -> 400 onesto" || no "atteso 400" "$C"

# ---- teardown ----
sql "DELETE FROM app.ingest_tokens WHERE device_id='$DID'" >/dev/null
api -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
sql "DELETE FROM app.devices d USING app.tenants t WHERE t.id=d.tenant_id AND t.slug='$SLUG' AND d.external_id LIKE '$EXT%'" >/dev/null
sql "DELETE FROM app.sensor_rules sr USING app.tenants t WHERE t.id=sr.tenant_id AND t.slug='$SLUG' AND sr.label IN ('Temperatura cella troppo alta','Batteria sensore scarica')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
