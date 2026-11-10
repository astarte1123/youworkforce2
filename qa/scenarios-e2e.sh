#!/usr/bin/env bash
# Ambienti / casi d'uso (thingsboard.io/use-cases) attivabili dal superadmin su qualsiasi
# tenant: catalogo, applicazione (moduli + regole + demo), idempotenza, rimozione pulita.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
SCN="air_quality"

# mock AI (OpenAI-compatibile, testo) per l'analisi AI dello scenario
cat > /tmp/mockscnai.py <<'PY'
import http.server, json
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); self.rfile.read(n)
        b=json.dumps({"choices":[{"message":{"role":"assistant","content":"Analisi mock: PM2.5 oltre soglia, consigliata verifica ventilazione."}}]}).encode()
        self.send_response(200); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b))); self.end_headers(); self.wfile.write(b)
http.server.HTTPServer(('127.0.0.1',9093),H).serve_forever()
PY
python /tmp/mockscnai.py & MSCNAI=$!

SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$SJ"|tail -1)
sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
TID=$(sapi "$BASE/api/superadmin/tenants" | jv '[t["id"] for t in d if t["slug"]=="acme"][0]')

# pulizia preventiva di eventuali residui dello scenario di test
sql "DELETE FROM app.sensor_rules WHERE tenant_id=$TIDQ AND label LIKE 'scn:$SCN:%'" >/dev/null
sql "DELETE FROM app.devices WHERE tenant_id=$TIDQ AND source='scenario' AND external_id LIKE 'scn-$SCN-%' AND id NOT IN (SELECT device_id FROM app.events WHERE device_id IS NOT NULL)" >/dev/null
sql "DELETE FROM app.tenant_scenarios WHERE tenant_id=$TIDQ AND scenario_code='$SCN'" >/dev/null

# 1) catalogo: 16 scenari
N=$(sapi "$BASE/api/superadmin/scenarios" | jv 'len(d)')
[ "$N" = "16" ] && ok "catalogo: 16 scenari" || no "catalogo count" "$N"
sapi "$BASE/api/superadmin/scenarios" | grep -q 'scada_oil_gas' && ok "catalogo include SCADA Oil & Gas" || no "scenario mancante" "-"

# 2) applica air_quality con demo
R=$(sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/scenarios" -d "{\"code\":\"$SCN\",\"with_demo\":true}")
echo "$R" | grep -q '"ok":true' && ok "scenario applicato" || no "apply fallito" "$R"
RA=$(echo "$R" | jv 'd["rules_added"]'); [ "$RA" -ge 1 ] && ok "regole inserite ($RA)" || no "nessuna regola" "$R"
[ "$(echo "$R" | jv 'd["devices_added"]')" -ge 1 ] && ok "device demo creati" || no "nessun device demo" "$R"

# 3) verifica DB: moduli, regole, sito/device demo, stato
[ "$(sql "SELECT state FROM app.tenant_modules WHERE tenant_id=$TIDQ AND module_code='iot'")" = "attivo" ] && ok "modulo iot attivo" || no "iot non attivo" "-"
RC=$(sql "SELECT count(*) FROM app.sensor_rules WHERE tenant_id=$TIDQ AND label LIKE 'scn:$SCN:%'")
[ "$RC" = "4" ] && ok "4 regole bundle air_quality (pm25/co2/humidity/temperature)" || no "regole bundle count" "$RC"
[ "$(sql "SELECT count(*) FROM app.sites WHERE tenant_id=$TIDQ AND name='Air Quality Monitoring (demo)'")" = "1" ] && ok "sito demo creato" || no "sito demo assente" "-"
[ "$(sql "SELECT count(*) FROM app.devices WHERE tenant_id=$TIDQ AND source='scenario' AND external_id='scn-$SCN-air-1'")" = "1" ] && ok "device demo creato" || no "device demo assente" "-"
[ "$(sql "SELECT count(*) FROM app.tenant_scenarios WHERE tenant_id=$TIDQ AND scenario_code='$SCN'")" = "1" ] && ok "stato scenario registrato" || no "stato non registrato" "-"

# 4) elenco scenari del tenant
sapi "$BASE/api/superadmin/tenants/$TID/scenarios" | grep -q "$SCN" && ok "scenario in elenco tenant" || no "non in elenco" "-"

# 5) idempotenza: riapplicare non duplica le regole
R2=$(sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/scenarios" -d "{\"code\":\"$SCN\",\"with_demo\":true}")
[ "$(echo "$R2" | jv 'd["rules_added"]')" = "0" ] && ok "riapplicazione idempotente (0 regole nuove)" || no "duplica regole" "$R2"

# 5b) telemetria demo: inietta eventi normali + allarmi -> incidenti reali
D=$(sapi -X POST "$BASE/api/superadmin/tenants/$TID/scenarios/$SCN/demo")
[ "$(echo "$D" | jv 'd["events"]')" -ge 2 ] && ok "demo: eventi iniettati ($(echo "$D"|jv 'd["events"]'))" || no "demo senza eventi" "$D"
[ "$(echo "$D" | jv 'd["incidents"]')" -ge 1 ] && ok "demo: incidenti alzati dalle regole ($(echo "$D"|jv 'd["incidents"]'))" || no "demo senza incidenti" "$D"
INCD=$(sql "SELECT count(*) FROM app.incidents WHERE tenant_id=$TIDQ AND correlation LIKE 'scndemo-$SCN-%'")
[ "$INCD" -ge 1 ] && ok "demo: incidenti presenti su DB (centrale)" || no "incidenti demo non su DB" "$INCD"

# 5c) cruscotto parametrico (tenant): widget derivati + valori dalla telemetria demo
AD=$(mktemp); curl -s -o /dev/null -c "$AD" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
ACX=$(awk '$6=="yw_csrf"{print $7}' "$AD"|tail -1)
aapi(){ curl -s -b "$AD" -H 'x-tenant-slug: acme' -H "x-csrf-token: $ACX" "$@"; }
aapi "$BASE/api/scenarios" | grep -q "$SCN" && ok "cruscotto: scenario attivo elencato al tenant" || no "scenario non elencato al tenant" "-"
DASH=$(aapi "$BASE/api/scenarios/$SCN/dashboard")
[ "$(echo "$DASH" | jv 'len(d["widgets"])')" -ge 3 ] && ok "cruscotto: widget derivati dalle regole" || no "nessun widget" "$DASH"
echo "$DASH" | jv '[w for w in d["widgets"] if w["metric"]=="pm25"][0]["value"]' | grep -qiE '[0-9]' && ok "cruscotto: valore pm25 dalla telemetria demo" || no "valore pm25 assente" "$DASH"
[ "$(echo "$DASH" | jv 'len(d["incidents"])')" -ge 1 ] && ok "cruscotto: allarmi recenti presenti" || no "nessun allarme nel cruscotto" "$DASH"
# KPI aggregati + device con coordinate (mappa) + preset AI focus
[ "$(echo "$DASH" | jv 'd["kpis"]["device_count"]')" -ge 1 ] && ok "cruscotto: KPI aggregati" || no "KPI assenti" "$DASH"
echo "$DASH" | jv 'd["devices"][0]["lat"]' | grep -qE '[0-9]' && ok "cruscotto: device con coordinate (mappa)" || no "coord device assenti" "$DASH"
echo "$DASH" | grep -q '"ai_focus"' && ok "cruscotto: preset AI focus presente" || no "ai_focus assente" "$DASH"

# 5d) analisi AI dello scenario (mock provider) -> salva report
HADAI=$(sql "SELECT count(*) FROM app.ai_connections WHERE tenant_id=$TIDQ")
sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/ai" -d '{"provider":"openai","base_url":"http://127.0.0.1:9093","api_key":"sk-mock"}' >/dev/null
REP0=$(sql "SELECT count(*) FROM app.ai_reports WHERE tenant_id=$TIDQ AND kind='$SCN'")
AN=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/scenarios/$SCN/ai-analysis" -d '{"save":true}')
echo "$AN" | grep -q '"analysis"' && ok "analisi AI scenario (grounding sui dati)" || no "analisi AI fallita" "$AN"
echo "$AN" | grep -q '"report_id"' && [ "$(sql "SELECT count(*) FROM app.ai_reports WHERE tenant_id=$TIDQ AND kind='$SCN'")" -gt "$REP0" ] && ok "analisi AI salvata come report" || no "report analisi non salvato" "$AN"
sql "DELETE FROM app.ai_reports WHERE tenant_id=$TIDQ AND kind='$SCN'" >/dev/null
[ "$HADAI" = "0" ] && sql "DELETE FROM app.ai_connections WHERE tenant_id=$TIDQ" >/dev/null

# 6) rimozione pulita
R3=$(sapi -X DELETE "$BASE/api/superadmin/tenants/$TID/scenarios/$SCN")
echo "$R3" | grep -q '"ok":true' && ok "scenario rimosso" || no "remove fallito" "$R3"
[ "$(sql "SELECT count(*) FROM app.sensor_rules WHERE tenant_id=$TIDQ AND label LIKE 'scn:$SCN:%'")" = "0" ] && ok "regole bundle rimosse" || no "regole non rimosse" "-"
[ "$(sql "SELECT count(*) FROM app.tenant_scenarios WHERE tenant_id=$TIDQ AND scenario_code='$SCN'")" = "0" ] && ok "stato scenario rimosso" || no "stato non rimosso" "-"

# cleanup demo residuo (prima incidenti/eventi demo, poi device/sito FK-safe)
sql "DELETE FROM app.incidents WHERE tenant_id=$TIDQ AND correlation LIKE 'scndemo-$SCN-%'" >/dev/null
sql "DELETE FROM app.events WHERE tenant_id=$TIDQ AND correlation LIKE 'scndemo-$SCN-%'" >/dev/null
sql "DELETE FROM app.devices WHERE tenant_id=$TIDQ AND source='scenario' AND external_id LIKE 'scn-$SCN-%' AND id NOT IN (SELECT device_id FROM app.events WHERE device_id IS NOT NULL)" >/dev/null
sql "DELETE FROM app.sites WHERE tenant_id=$TIDQ AND name='Air Quality Monitoring (demo)'" >/dev/null
kill $MSCNAI 2>/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
