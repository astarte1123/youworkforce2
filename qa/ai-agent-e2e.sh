#!/usr/bin/env bash
# Agente AI (tool-calling che ESEGUE su DB). Prova con mock OpenAI-compatibile:
# - scrittura a basso rischio eseguita subito (nota diario salvata)
# - azione sensibile (create_sensor_rule) NON eseguita -> pending -> confirm la esegue
# - difesa: tool sulla catena di sicurezza rifiutato
# - report AI generato e SALVATO su DB
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
jv(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"

# --- mock provider OpenAI-compatibile con tool-calling ---
cat > /tmp/mockagent.py <<'PY'
import http.server, json
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); raw=self.rfile.read(n).decode('utf-8','ignore')
        if self.path.endswith('/transcriptions'):
            return self.j({"text":"trascrizione mock"})
        # chat semplice (nessun tool offerto) -> testo: usato dalla generazione report
        if '"tools"' not in raw:
            return self.j({"choices":[{"message":{"role":"assistant","content":"Report mock: 3 incidenti nel periodo, nessuna anomalia critica."}}]})
        seen_tool = '"role":"tool"' in raw
        if seen_tool:
            return self.j({"choices":[{"message":{"role":"assistant","content":"Fatto: dati letti e informazioni registrate."}}]})
        want_rule = 'REGOLA' in raw
        if want_rule:
            tc=[{"id":"c1","type":"function","function":{"name":"create_sensor_rule","arguments":json.dumps({"metric":"gas","op":">=","threshold":50,"etype":"sensore_critico","label":"AI agent"})}}]
        else:
            tc=[{"id":"c1","type":"function","function":{"name":"query_incidents","arguments":json.dumps({"state":"tutti","limit":3})}},
                {"id":"c2","type":"function","function":{"name":"add_diary_entry","arguments":json.dumps({"body":"ronda completata senza anomalie (AI)"})}}]
        self.j({"choices":[{"message":{"role":"assistant","content":None,"tool_calls":tc}}]})
    def j(self,o):
        b=json.dumps(o).encode(); self.send_response(200)
        self.send_header('content-type','application/json'); self.send_header('content-length',str(len(b)))
        self.end_headers(); self.wfile.write(b)
http.server.HTTPServer(('127.0.0.1',9097),H).serve_forever()
PY
python /tmp/mockagent.py & MOCK=$!; sleep 1

# --- superadmin: connessione provider=openai verso il mock ---
SJ=$(mktemp); curl -s -o /dev/null -c "$SJ" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$SJ"|tail -1)
sapi(){ curl -s -b "$SJ" -H "x-csrf-token: $SC" "$@"; }
TID=$(sapi "$BASE/api/superadmin/tenants" | jv '[t["id"] for t in d if t["slug"]=="acme"][0]')
HADC=$(sql "SELECT count(*) FROM app.ai_connections WHERE tenant_id=$TIDQ")
sapi -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants/$TID/ai" \
  -d '{"provider":"openai","base_url":"http://127.0.0.1:9097","api_key":"sk-mock","chat_path":"/v1/chat/completions"}' >/dev/null
[ "$(sapi "$BASE/api/superadmin/tenants/$TID/ai" | jv 'd["provider"]')" = "openai" ] && ok "provider selezionabile (openai)" || no "provider non impostato" "-"

# --- tenant admin: abilita feature agent ---
login(){ curl -s -o /dev/null -c "$1" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d "{\"email\":\"$2\",\"password\":\"demo\"}"; }
csrf(){ awk '$6=="yw_csrf"{print $7}' "$1"|tail -1; }
AD=$(mktemp); login "$AD" "admin@acme.loneworker.eu"; ACX=$(csrf "$AD")
aapi(){ curl -s -b "$AD" -H 'x-tenant-slug: acme' -H "x-csrf-token: $ACX" "$@"; }
aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/features" -d '{"feature":"agent","enabled":true}' | grep -q '"ok":true' && ok "feature agent abilitata" || no "abilita agent fallito" "-"

# operatore session (scrive diario come se stesso)
OPU=$(mktemp); login "$OPU" "operatore@acme.loneworker.eu"; OCX=$(csrf "$OPU")
oapi(){ curl -s -b "$OPU" -H 'x-tenant-slug: acme' -H "x-csrf-token: $OCX" "$@"; }

# 1) agente esegue scrittura a basso rischio: nota diario salvata
D0=$(sql "SELECT count(*) FROM app.diary_entries WHERE tenant_id=$TIDQ AND source='ai'")
R=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/agent" -d '{"goal":"controlla gli incidenti e registra una nota di ronda"}')
echo "$R" | grep -q '"provider":"openai"' && ok "agente risponde (provider openai)" || no "agente non risponde" "$R"
echo "$R" | grep -q 'add_diary_entry' && ok "agente ha invocato add_diary_entry" || no "nessuna azione diario" "$R"
D1=$(sql "SELECT count(*) FROM app.diary_entries WHERE tenant_id=$TIDQ AND source='ai'")
[ "$D1" -gt "$D0" ] && ok "nota diario SCRITTA su DB (source=ai)" || no "diario non scritto" "D0=$D0 D1=$D1"

# 2) azione sensibile -> pending (NON eseguita), poi confirm la esegue
RULE0=$(sql "SELECT count(*) FROM app.sensor_rules WHERE tenant_id=$TIDQ AND metric='gas'")
R2=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/agent" -d '{"goal":"REGOLA: crea una regola sensori per il gas"}')
echo "$R2" | grep -q 'create_sensor_rule' && ok "azione sensibile proposta in pending" || no "nessun pending" "$R2"
RULEm=$(sql "SELECT count(*) FROM app.sensor_rules WHERE tenant_id=$TIDQ AND metric='gas'")
[ "$RULEm" = "$RULE0" ] && ok "sensibile NON eseguita senza conferma" || no "eseguita senza conferma!" "$RULE0->$RULEm"
# confirm (admin, ruolo abilitato alle regole)
aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/agent/confirm" -d '{"name":"create_sensor_rule","args":{"metric":"gas","op":">=","threshold":50,"etype":"sensore_critico","label":"AI agent"}}' | grep -q '"ok":true' && ok "confirm esegue l'azione sensibile" || no "confirm fallito" "-"
RULE1=$(sql "SELECT count(*) FROM app.sensor_rules WHERE tenant_id=$TIDQ AND metric='gas'")
[ "$RULE1" -gt "$RULE0" ] && ok "regola sensori SCRITTA dopo conferma" || no "regola non scritta" "$RULE0->$RULE1"

# 3) confirm rifiuta una non-sensibile (anti-abuso)
R=$(aapi -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/ai/agent/confirm" -d '{"name":"add_diary_entry","args":{"body":"x"}}')
[ "$R" = "400" ] && ok "confirm rifiuta azioni non sensibili" || no "confirm accetta non-sensibile" "$R"

# 4) report AI generato e SALVATO su DB
REP0=$(sql "SELECT count(*) FROM app.ai_reports WHERE tenant_id=$TIDQ")
G=$(aapi -H 'content-type: application/json' -X POST "$BASE/api/ai/reports/generate" -d '{"prompt":"report incidenti ultimi 30 giorni","kind":"incidenti"}')
echo "$G" | grep -q '"id"' && ok "report generato" || no "report non generato" "$G"
REP1=$(sql "SELECT count(*) FROM app.ai_reports WHERE tenant_id=$TIDQ")
[ "$REP1" -gt "$REP0" ] && ok "report SALVATO su app.ai_reports" || no "report non salvato" "$REP0->$REP1"
RID=$(echo "$G" | jv 'd["id"]')
aapi "$BASE/api/ai/reports/$RID" | grep -q '"body"' && ok "report consultabile (get by id)" || no "report non leggibile" "-"
aapi "$BASE/api/ai/reports" | grep -q "$RID" && ok "report in elenco" || no "report non in elenco" "-"

# cleanup: cancella la riga feature creata (torna al default), regola/diario/report, connessione se non c'era
sql "DELETE FROM app.ai_features WHERE tenant_id=$TIDQ AND feature='agent'" >/dev/null
sql "DELETE FROM app.sensor_rules WHERE tenant_id=$TIDQ AND metric='gas' AND label='AI agent'" >/dev/null
sql "DELETE FROM app.diary_entries WHERE tenant_id=$TIDQ AND source='ai'" >/dev/null
sql "DELETE FROM app.ai_reports WHERE id='$RID'" >/dev/null
[ "$HADC" = "0" ] && sql "DELETE FROM app.ai_connections WHERE tenant_id=$TIDQ" >/dev/null
kill $MOCK 2>/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
