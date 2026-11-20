#!/usr/bin/env bash
# PROVA di RECAPITO REALE del motore di escalation NATIVO (YWAlert) — nessun YWAlert.
#
# Dimostra che un SOS fa PARTIRE FISICAMENTE sulla rete la rung di notifica: il motore
# nativo consegna via l'adapter di consegna a un RECEIVER webhook esterno (stesso percorso
# del sink/Twilio REST, cambia solo l'URL di destinazione). Non serve un carrier a pagamento:
# il receiver fa da endpoint reale e verifichiamo il corpo HTTP effettivamente ricevuto.
#
# PREREQUISITO: core avviato con YW_ESC_SINK_URL=http://127.0.0.1:18099/ (e YW_ESC_FROM).
#   In CI/run-all questo è impostato all'avvio dei servizi. Il receiver lo avvia questo script.
# Requisiti: docker (psql via ywdb), python3, curl. Tenant di prova: globex (isolato).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="globex"; EMAIL="admin@globex.loneworker.eu"
SINK_PORT="${YW_ESC_SINK_PORT:-18099}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  (${2:-})"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }

# 0) receiver webhook. Se CI/run-all ha già un receiver condiviso in ascolto (core avviato con
#    YW_ESC_SINK_URL verso di esso), lo RIUSIAMO leggendo il suo log (YW_ESC_SINK_LOG); altrimenti
#    ne avviamo uno effimero. In entrambi i casi filtriamo per incident-id, quindi un log condiviso
#    con altre consegne non disturba.
RECV_PID=""
if curl -s -o /dev/null -m 2 -X POST "http://127.0.0.1:$SINK_PORT/" -d 'ping'; then
  SINK_LOG="${YW_ESC_SINK_LOG:?receiver condiviso attivo ma YW_ESC_SINK_LOG non impostato}"
  ok "receiver webhook condiviso già attivo su :$SINK_PORT"
else
  SINK_LOG=$(mktemp)
  python3 -c '
import http.server,sys
f=open(sys.argv[1],"ab")
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n=int(self.headers.get("content-length",0)); b=self.rfile.read(n)
        f.write(b+b"\n"); f.flush()
        self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
    def log_message(self,*a): pass
http.server.HTTPServer(("127.0.0.1",'"$SINK_PORT"'),H).serve_forever()
' "$SINK_LOG" &
  RECV_PID=$!
  sleep 1
  curl -s -o /dev/null -m 2 -X POST "http://127.0.0.1:$SINK_PORT/" -d 'ping' && ok "receiver webhook effimero attivo su :$SINK_PORT" || { no "receiver non attivo"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }
fi
cleanup(){ [ -n "$RECV_PID" ] && kill "$RECV_PID" 2>/dev/null || true; }
trap cleanup EXIT

JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
TID="(SELECT id FROM app.tenants WHERE slug='$SLUG')"

# 1) gruppo + on-call con telefono UNICO verificato (range 3988), engine NATIVO, default group
R=$RANDOM
GID=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"Delivery-$R\"}" | python3 -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
[ -n "$GID" ] && ok "gruppo creato" || { no "gruppo non creato"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$GID','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID',escalation_engine='native'" >/dev/null
UID1=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone,phone_verified_at) SELECT id,'dlv1-$R@globex.loneworker.eu','x','operatore','+393988000001',now() FROM t RETURNING id" | head -1)
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/members" -d "{\"user_id\":\"$UID1\",\"position\":1}" >/dev/null
ok "on-call con telefono verificato in gruppo"

# 2) SOS -> incidente instradato al gruppo; cattura short-id (primi 8 char uuid) per il match
IID=$(sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,forced_group_id) SELECT $TID,'sos','nuovo',1,'e2e-delivery-'||gen_random_uuid(),now(),'$GID' RETURNING id" | head -1)
SHORT=${IID:0:8}
[ -n "$IID" ] && ok "SOS: incidente aperto ($SHORT)" || no "incidente non creato" "-"

# 3) esegui il runner di consegna NATIVO (stesso codice del worker tick)
DLV=$(api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' | python3 -c "import sys,json;print(json.load(sys.stdin).get('deliveries',0))")
[ "$DLV" -ge 1 ] && ok "runner nativo: $DLV rung consegnate" || no "nessuna rung consegnata" "$DLV"

# 4) IL PUNTO: il receiver HA RICEVUTO FISICAMENTE il POST, con canale sms + engine native + incidente
GOT=""
for i in $(seq 1 15); do
  GOT=$(grep -F "$IID" "$SINK_LOG" 2>/dev/null | head -1)
  [ -n "$GOT" ] && break
  sleep 1
done
if [ -n "$GOT" ]; then
  ok "RECAPITO REALE: receiver ha ricevuto il POST per l'incidente $SHORT"
else
  no "nessun POST ricevuto dal receiver entro il timeout" "-"
fi
echo "$GOT" | grep -q '"channel":"sms"'   && ok "corpo: channel=sms (rung SMS reale)" || no "channel sms assente nel corpo" "-"
echo "$GOT" | grep -q '"engine":"native"' && ok "corpo: engine=native (motore YWAlert)" || no "engine native assente nel corpo" "-"
echo "$GOT" | grep -q '+393988000001'     && ok "corpo: destinatario = telefono on-call verificato" || no "telefono destinatario assente" "-"

# 5) persistenza: la consegna è loggata come inviata (stato sent, non dry/failed)
SENT=$(sql "SELECT count(*) FROM app.esc_delivery_log WHERE incident_id='$IID' AND state='sent'")
[ "${SENT:-0}" -ge 1 ] && ok "esc_delivery_log: consegna registrata state='sent'" || no "consegna non registrata come sent" "${SENT:-0}"

# ---- teardown: ripristino globex ----
sql "UPDATE app.incidents SET state='chiuso',closed_at=now(),outcome='e2e ywalert-delivery' WHERE id='$IID'" >/dev/null
api -X DELETE "$BASE/api/groups/$GID" >/dev/null 2>&1 || true
sql "DELETE FROM app.alarm_group_members WHERE group_id='$GID'" >/dev/null
sql "DELETE FROM app.esc_rotation_state WHERE group_id='$GID'" >/dev/null
sql "DELETE FROM app.esc_delivery_log WHERE incident_id='$IID'" >/dev/null
sql "DELETE FROM app.esc_alert_state WHERE incident_id='$IID'" >/dev/null
sql "DELETE FROM app.tenant_oncall_cfg oc USING app.tenants t WHERE t.id=oc.tenant_id AND t.slug='$SLUG' AND oc.default_group_id='$GID'" >/dev/null
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND u.email LIKE 'dlv1-%@globex%'" >/dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
