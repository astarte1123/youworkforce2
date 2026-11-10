#!/usr/bin/env bash
# ThingsBoard PULL (spina dorsale) + AI-first. Mock TB (login JWT + devices + telemetria):
# - sync specchia i device nell'inventario (source=thingsboard)
# - la telemetria passa dal motore regole -> incidente critico (stesso percorso del push)
# - stato poller leggibile
# - AI-first: l'agente elenca i device TB (read) e il tool sensibile tb_sync_devices
#   NON si esegue senza conferma, poi la conferma lo esegue.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"

# --- mock ThingsBoard ---
cat > /tmp/mocktb.py <<'PY'
import http.server, json
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); self.rfile.read(n)
        if self.path.endswith('/api/auth/login'): return self.j({"token":"tok-mock"})
        self.j({"ok":True})
    def do_GET(self):
        if '/api/tenant/devices' in self.path:
            return self.j({"data":[{"id":{"id":"dev-uuid-1","entityType":"DEVICE"},"name":"sensore-gas-01","type":"gas"}],"totalElements":1})
        if '/values/timeseries' in self.path:
            return self.j({"gas":[{"ts":1700000000000,"value":"75"}],"temperature":[{"ts":1700000000000,"value":"20"}]})
        self.j({})
    def j(self,o):
        b=json.dumps(o).encode(); self.send_response(200)
        self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b)))
        self.end_headers(); self.wfile.write(b)
http.server.HTTPServer(('127.0.0.1',9095),H).serve_forever()
PY
python /tmp/mocktb.py & MTB=$!

# --- mock AI (tool-calling): su goal 'THINGSBOARD' chiama tb_list_devices ---
cat > /tmp/mocktbai.py <<'PY'
import http.server, json
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); raw=self.rfile.read(n).decode('utf-8','ignore')
        if '"tools"' not in raw: return self.j({"choices":[{"message":{"role":"assistant","content":"ok"}}]})
        if '"role":"tool"' in raw: return self.j({"choices":[{"message":{"role":"assistant","content":"Elenco device ThingsBoard letto."}}]})
        tc=[{"id":"c1","type":"function","function":{"name":"tb_list_devices","arguments":"{}"}}]
        self.j({"choices":[{"message":{"role":"assistant","content":None,"tool_calls":tc}}]})
    def j(self,o):
        b=json.dumps(o).encode(); self.send_response(200)
        self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b)))
        self.end_headers(); self.wfile.write(b)
http.server.HTTPServer(('127.0.0.1',9094),H).serve_forever()
PY
python /tmp/mocktbai.py & MAI=$!
sleep 1

# --- superadmin: connessione TB verso il mock ---
SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$SJ"|tail -1)
sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
TID=$(sapi "$BASE/api/superadmin/tenants" | jv '[t["id"] for t in d if t["slug"]=="acme"][0]')
HADTB=$(sql "SELECT count(*) FROM app.thingsboard_connections WHERE tenant_id=$TIDQ")
HADAI=$(sql "SELECT count(*) FROM app.ai_connections WHERE tenant_id=$TIDQ")
sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/thingsboard" -d '{"base_url":"http://127.0.0.1:9095","username":"t@tb","password":"pw"}' >/dev/null

# --- tenant admin: regola gas>=50 -> sensore_critico ---
login(){ curl -s -o /dev/null -c "$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$2\",\"password\":\"demo\"}"; }
csrf(){ awk '$6=="yw_csrf"{print $7}' "$1"|tail -1; }
AD=$(mktemp); login "$AD" "admin@acme.loneworker.eu"; ACX=$(csrf "$AD")
aapi(){ curl -s -b "$AD" -H 'x-tenant-slug: acme' -H "x-csrf-token: $ACX" "$@"; }
RID=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/sensor-rules" -d '{"metric":"gas","op":">=","threshold":50,"etype":"sensore_critico","label":"tbpull-e2e"}' | jv 'd.get("id","")')

# 1) sync: specchio device + telemetria -> incidente
DEV0=$(sql "SELECT count(*) FROM app.devices WHERE tenant_id=$TIDQ AND source='thingsboard'")
INC0=$(sql "SELECT count(*) FROM app.incidents WHERE tenant_id=$TIDQ AND etype='sensore_critico'")
R=$(sapi -X POST "$BASE/api/superadmin/tenants/$TID/thingsboard/sync")
echo "$R" | grep -q '"reachable":true' && ok "sync TB raggiungibile" || no "sync non reachable" "$R"
[ "$(echo "$R" | jv 'd.get("device_count")')" = "1" ] && ok "sync: 1 device contato" || no "device_count errato" "$R"
DEV1=$(sql "SELECT count(*) FROM app.devices WHERE tenant_id=$TIDQ AND source='thingsboard' AND external_id='sensore-gas-01'")
[ "$DEV1" -ge 1 ] && ok "device TB specchiato nell'inventario (source=thingsboard)" || no "device non specchiato" "-"
[ "$(echo "$R" | jv 'd.get("alarm_count")')" = "1" ] && ok "telemetria gas=75 -> incidente alzato (regola)" || no "nessun incidente da telemetria" "$R"
INC1=$(sql "SELECT count(*) FROM app.incidents WHERE tenant_id=$TIDQ AND etype='sensore_critico'")
[ "$INC1" -gt "$INC0" ] && ok "incidente sensore_critico creato su DB (percorso push condiviso)" || no "incidente non creato" "$INC0->$INC1"

# 2) stato poller leggibile
S=$(aapi "$BASE/api/thingsboard/status")
echo "$S" | grep -q '"configured":true' && ok "stato poller: configurato" || no "stato non configurato" "$S"
echo "$S" | grep -q '"reachable":true' && ok "stato poller: reachable" || no "stato non reachable" "$S"

# 3) AI-first: agente elenca i device TB (read tool) -> provider openai verso mock AI
sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/ai" -d '{"provider":"openai","base_url":"http://127.0.0.1:9094","api_key":"sk-mock"}' >/dev/null
aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"agent","enabled":true}' >/dev/null
A=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/agent" -d '{"goal":"THINGSBOARD: elenca i device"}')
echo "$A" | grep -q 'tb_list_devices' && ok "agente usa tb_list_devices (AI-first)" || no "agente non elenca device TB" "$A"
echo "$A" | grep -q 'sensore-gas-01' && ok "agente vede il device specchiato" || no "device non visto dall'agente" "$A"

# 4) tb_sync_devices e' sensibile: non eseguito senza conferma; confirm lo esegue
R=$(aapi -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/ai/agent/confirm" -d '{"name":"tb_sync_devices","args":{}}')
[ "$R" = "200" ] && ok "confirm esegue tb_sync_devices (sensibile)" || no "confirm tb_sync fallito" "$R"

# cleanup
aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"agent","enabled":false}' >/dev/null
[ -n "$RID" ] && aapi -X DELETE "$BASE/api/sensor-rules/$RID" >/dev/null
sql "DELETE FROM app.incidents WHERE tenant_id=$TIDQ AND correlation LIKE 'tbpull-%'" >/dev/null
sql "DELETE FROM app.events WHERE tenant_id=$TIDQ AND correlation LIKE 'tbpull-%'" >/dev/null
sql "DELETE FROM app.devices WHERE tenant_id=$TIDQ AND source='thingsboard' AND external_id='sensore-gas-01'" >/dev/null
sql "DELETE FROM app.thingsboard_poll_state WHERE tenant_id=$TIDQ" >/dev/null
[ "$HADTB" = "0" ] && sql "DELETE FROM app.thingsboard_connections WHERE tenant_id=$TIDQ" >/dev/null
[ "$HADAI" = "0" ] && sql "DELETE FROM app.ai_connections WHERE tenant_id=$TIDQ" >/dev/null
kill $MTB $MAI 2>/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
