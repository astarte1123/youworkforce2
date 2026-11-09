#!/usr/bin/env bash
# SICUREZZA — watchdog della CONNESSIONE Traccar. Se il server e' irraggiungibile
# oltre la soglia, il sistema di sicurezza e' cieco: deve alzare un incidente
# critico (escalation GoAlert) e chiuderlo al ripristino. Simula il down puntando
# la connessione a una porta morta, poi ripristina.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"

JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
conndown(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/incidents" \
  | python -c "import sys,json;d=json.load(sys.stdin);print(json.dumps([x for x in d if str(x.get('correlation','')).startswith('traccar-conndown-')]))"; }

# salva lo stato reale della connessione e forza un down
ORIG=$(sql "SELECT base_url FROM app.traccar_connections WHERE tenant_id=$TIDQ")
restore(){ sql "UPDATE app.traccar_connections SET base_url='$ORIG', poll_interval_s=30, conn_down_after_s=180, poll_enabled=true WHERE tenant_id=$TIDQ" >/dev/null
  sql "UPDATE app.traccar_poll_state SET down_since=NULL, conn_incident_open=false, conn_incident_corr=NULL WHERE tenant_id=$TIDQ" >/dev/null; }
trap restore EXIT

# baseline pulito: chiudi eventuali incidenti conndown lasciati da altri test
sql "UPDATE app.incidents SET state='chiuso',closed_at=now() WHERE correlation LIKE 'traccar-conndown-%' AND state<>'chiuso' AND tenant_id=$TIDQ" >/dev/null
sql "UPDATE app.traccar_connections SET base_url='http://127.0.0.1:9', poll_interval_s=2, conn_down_after_s=1 WHERE tenant_id=$TIDQ" >/dev/null
# reset stato precedente; last_poll_at nel passato cosi' il poll riparte subito
# (il clock di test puo' lasciare timestamp nel futuro tra restart del core).
sql "UPDATE app.traccar_poll_state SET down_since=NULL, conn_incident_open=false, conn_incident_corr=NULL, last_poll_at=now()-interval '1 hour' WHERE tenant_id=$TIDQ" >/dev/null

echo "attendo rilevamento down..."
sleep 9
OPEN=$(conndown | python -c 'import sys,json;d=json.load(sys.stdin);print(len([x for x in d if x.get("state")!="chiuso"]))')
[ "${OPEN:-0}" -ge 1 ] && ok "Traccar down -> incidente critico traccar_conn_down aperto" || no "nessun incidente su down" "open=$OPEN"

# escalation richiesta? outbox goalert_notify per quell'incidente
GN=$(sql "SELECT count(*) FROM app.outbox o WHERE o.tenant_id=$TIDQ AND o.kind='goalert_notify' AND o.payload->>'etype'='traccar_conn_down'")
[ "${GN:-0}" -ge 1 ] && ok "escalation GoAlert richiesta per il down (outbox goalert_notify)" || no "nessuna escalation" "goalert_notify=$GN"

# idempotenza: niente raffica (un episodio = un incidente)
sleep 4
CNT=$(conndown | python -c 'import sys,json;print(len([x for x in json.load(sys.stdin) if x.get("state")!="chiuso"]))')
[ "${CNT:-0}" -le 1 ] && ok "un episodio di down = un incidente (no raffica: $CNT)" || no "down duplica" "$CNT"

# ripristino connessione -> recovery automatico (incidente chiuso + goalert_close)
sql "UPDATE app.traccar_connections SET base_url='$ORIG' WHERE tenant_id=$TIDQ" >/dev/null
sql "UPDATE app.traccar_poll_state SET last_poll_at=now()-interval '1 hour' WHERE tenant_id=$TIDQ" >/dev/null
echo "attendo recovery..."
sleep 8
STILL=$(conndown | python -c 'import sys,json;print(len([x for x in json.load(sys.stdin) if x.get("state")!="chiuso"]))')
[ "${STILL:-1}" -eq 0 ] && ok "ripristino connessione -> incidente chiuso (recovery automatico)" || no "non chiuso al ripristino" "aperti=$STILL"
GC=$(sql "SELECT count(*) FROM app.outbox o WHERE o.tenant_id=$TIDQ AND o.kind='goalert_close' AND o.payload->>'etype'='traccar_conn_down'")
[ "${GC:-0}" -ge 1 ] && ok "chiusura alert GoAlert richiesta (outbox goalert_close)" || no "nessuna chiusura GoAlert" "goalert_close=$GC"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
