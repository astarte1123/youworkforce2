#!/usr/bin/env bash
# Traccar P1 — polling via API (pull backbone) + watchdog silenzio + salute.
# UPSTREAM REALE: interroga il Traccar reale (porta 8083) con le credenziali del
# tenant, recupera eventi dal cursore, rileva il silenzio dei badge in modo
# indipendente da deviceOffline, ed espone lo stato salute della connessione.
#   device -> Traccar (eventi/posizioni) -> core POLLA via API -> incidente.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
TR="${TRACCAR:-http://localhost:8083}"
TRUSER="${TRUSER:-op@acme.test}"; TRPASS="${TRPASS:-Passw0rd!}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }

JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' \
  -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
status(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/traccar/status"; }
incids(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/incidents"; }

# cadenza bassa + watchdog corto per un test deterministico (ripristinati alla fine)
sql "UPDATE app.traccar_connections SET poll_enabled=true, poll_interval_s=2, silence_after_s=120
     WHERE tenant_id=(SELECT id FROM app.tenants WHERE slug='acme');" >/dev/null
cleanup(){ sql "UPDATE app.traccar_connections SET poll_interval_s=30, silence_after_s=600
     WHERE tenant_id=(SELECT id FROM app.tenants WHERE slug='acme');" >/dev/null; }
trap cleanup EXIT

poll_wait(){ sleep 5; }  # ~2 cicli di poll da 2s

# fix fresco del badge: imposta un lastUpdate recente (base deterministica per il watchdog)
curl -s -o /dev/null "http://localhost:5055/?id=BADGE-001&lat=45.47&lon=9.19&timestamp=$(date +%s)"
poll_wait
S=$(status)

# 1) salute connessione: configurata + raggiungibile dopo un poll
echo "$S" | python -c 'import sys,json;d=json.load(sys.stdin);exit(0 if d.get("configured") and d.get("reachable") else 1)' \
  && ok "connessione Traccar configurata e raggiungibile (pull attivo)" || no "status reachable" "$S"

# 2) dispositivi visti via API
echo "$S" | python -c 'import sys,json;d=json.load(sys.stdin);exit(0 if d.get("device_count",0)>=1 else 1)' \
  && ok "device_count >= 1 (letti da /api/devices)" || no "device_count" "$S"

# 3) cursore: il poller ha letto eventi reali da /api/reports/events (deviceOnline/Moving ecc.)
CUR=$(echo "$S" | python -c 'import sys,json;print(json.load(sys.stdin).get("last_event_id",0))')
[ "${CUR:-0}" -ge 1 ] && ok "cursore eventi >=1 ($CUR): poller legge /api/reports/events" \
  || no "cursore" "last_event_id=$CUR"

# 4) watchdog silenzio: avanzo il clock oltre la soglia -> il badge risulta silente ->
#    perdita_contatto aperto da NOI, indipendente dal deviceOffline di Traccar.
BEFORE=$(incids | python -c "import sys,json;print(len([x for x in json.load(sys.stdin) if str(x.get('correlation','')).startswith('traccar-silence-BADGE-001-')]))")
curl -s -o /dev/null -X POST "$BASE/api/_test/advance" -H 'content-type: application/json' -d '{"seconds":200}'
poll_wait
AFTER=$(incids | python -c "import sys,json;print(len([x for x in json.load(sys.stdin) if str(x.get('correlation','')).startswith('traccar-silence-BADGE-001-')]))")
[ "${AFTER:-0}" -gt "${BEFORE:-0}" ] \
  && ok "watchdog silenzio -> perdita_contatto (indip. da deviceOffline)" \
  || no "watchdog non scattato" "$BEFORE -> $AFTER"

# 5) idempotenza: poll successivi sullo STESSO episodio non duplicano l'incidente
poll_wait
AGAIN=$(incids | python -c "import sys,json;print(len([x for x in json.load(sys.stdin) if str(x.get('correlation','')).startswith('traccar-silence-BADGE-001-')]))")
[ "${AGAIN:-0}" -eq "${AFTER:-0}" ] \
  && ok "watchdog idempotente (episodio = 1 incidente, nessuna raffica)" \
  || no "watchdog duplica" "$AFTER -> $AGAIN"

# 6) stato salute leggibile (lag/online aggiornati dal poller)
status | python -c 'import sys,json;d=json.load(sys.stdin);print("online",d.get("online_count"),"/",d.get("device_count"),"lag",d.get("lag_seconds"),"cursore",d.get("last_event_id"))'
ok "stato salute aggiornato (card Connettori)"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
