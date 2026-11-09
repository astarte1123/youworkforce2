#!/usr/bin/env bash
# SICUREZZA (review #2) — un Traccar che AUTENTICA ma degrada (200 su /api/session,
# 5xx su /api/devices) NON deve risultare "reachable": entrambi i watchdog
# resterebbero ciechi. Deve invece scattare il watchdog connessione.
# Usa un mock HTTP che risponde 200+cookie al login e 503 ai dati.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"

MOCK=$(mktemp /tmp/trmock.XXXX.py)
cat > "$MOCK" <<'PY'
import http.server, socketserver
class H(http.server.BaseHTTPRequestHandler):
    def _h(self, code, body=b'[]', ck=False):
        self.send_response(code)
        self.send_header('Content-Type','application/json')
        if ck: self.send_header('Set-Cookie','JSESSIONID=mock; Path=/')
        self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        if self.path.startswith('/api/session'): self._h(200, b'{"id":1}', ck=True)   # login OK
        else: self._h(503)
    def do_GET(self):
        self._h(503)   # /api/devices, /api/positions ... tutto degradato
    def log_message(self,*a): pass
socketserver.TCPServer(('127.0.0.1',18083), H).serve_forever()
PY
python "$MOCK" &
MPID=$!
ORIG=$(sql "SELECT base_url FROM app.traccar_connections WHERE tenant_id=$TIDQ")
restore(){ kill $MPID 2>/dev/null; rm -f "$MOCK"
  sql "UPDATE app.traccar_connections SET base_url='$ORIG', poll_interval_s=30, conn_down_after_s=180 WHERE tenant_id=$TIDQ" >/dev/null
  sql "UPDATE app.traccar_poll_state SET down_since=NULL, conn_incident_open=false, conn_incident_corr=NULL WHERE tenant_id=$TIDQ" >/dev/null
  # chiudi l'incidente di test per non contaminare altri e2e
  sql "UPDATE app.incidents SET state='chiuso',closed_at=now() WHERE correlation LIKE 'traccar-conndown-%' AND state<>'chiuso' AND tenant_id=$TIDQ" >/dev/null; }
trap restore EXIT
sleep 1

sql "UPDATE app.traccar_connections SET base_url='http://127.0.0.1:18083', poll_interval_s=2, conn_down_after_s=1 WHERE tenant_id=$TIDQ" >/dev/null
sql "UPDATE app.traccar_poll_state SET down_since=NULL, conn_incident_open=false, conn_incident_corr=NULL, last_poll_at=now()-interval '1 hour' WHERE tenant_id=$TIDQ" >/dev/null

echo "attendo rilevamento degrado..."
sleep 9
R=$(sql "SELECT reachable FROM app.traccar_poll_state WHERE tenant_id=$TIDQ")
[ "$R" = "f" ] && ok "Traccar che autentica ma 5xx sui dati -> reachable=false (non cieco)" || no "reachable sbagliato" "reachable=$R"
OPEN=$(sql "SELECT count(*) FROM app.incidents WHERE correlation LIKE 'traccar-conndown-%' AND state<>'chiuso' AND tenant_id=$TIDQ")
[ "${OPEN:-0}" -ge 1 ] && ok "watchdog connessione scatta sul degrado (traccar_conn_down)" || no "nessun allarme sul degrado" "open=$OPEN"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
