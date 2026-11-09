#!/usr/bin/env bash
# SICUREZZA (Man Down/SOS 81/08, punto 1d) — la batteria in esaurimento puo' lasciare
# il lavoratore privo di protezione. L'admin deve poter decidere la severita':
#   battery_as = off (ignora) | warning (evento, default) | critico (allarme + escalation).
# Verifica tutte e tre le politiche sia sul percorso soglia% sia sull'alarm lowBattery.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
RUN="$(date +%s)-$$"

JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA")
setmap(){ curl -s -o /dev/null -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/traccar/tools/alarm_map" -d "$1"; }
send_batt(){ local id="$RUN-$RANDOM-$RANDOM"
  curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
    -d "{\"event\":{\"id\":\"$id\",\"type\":\"deviceMoving\"},\"device\":{\"uniqueId\":\"BADGE-001\"},\"position\":{\"latitude\":45.4,\"longitude\":9.1,\"attributes\":{\"batteryLevel\":$1}}}" \
    | python -c 'import sys,json;d=json.load(sys.stdin);print(d.get("incident_id"),"EVT" if d.get("event_id") else "NOEVT")'; }
send_alarm(){ local id="$RUN-$RANDOM-$RANDOM"
  curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
    -d "{\"event\":{\"id\":\"$id\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"$1\"}},\"device\":{\"uniqueId\":\"BADGE-001\"},\"position\":{\"latitude\":45.4,\"longitude\":9.1}}" \
    | python -c 'import sys,json;d=json.load(sys.stdin);print(d.get("incident_id"))'; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
restore(){ setmap '{"config":{"sos_as":"sos","battery_min":15,"battery_as":"warning"}}'; }
trap restore EXIT

# --- battery_as = critico -> batteria sotto soglia apre incidente + escalation ---
setmap '{"config":{"sos_as":"sos","battery_min":15,"battery_as":"critico"}}'
OUT=$(send_batt 5); IID=$(echo "$OUT" | awk '{print $1}')
if [ "$IID" != "None" ] && [ -n "$IID" ]; then ok "critico: batteria 5% -> incidente ($IID)"; else no "critico: nessun incidente" "$OUT"; fi
ET=$(docker exec ywdb psql -U postgres -d youworkforce -tAc "SELECT etype FROM app.incidents WHERE id='$IID'" 2>/dev/null)
[ "$ET" = "batteria_critica" ] && ok "critico: etype=batteria_critica (in CRITICAL)" || no "etype errato" "$ET"
GN=$(docker exec ywdb psql -U postgres -d youworkforce -tAc "SELECT count(*) FROM app.outbox WHERE tenant_id=$TIDQ AND kind='goalert_notify' AND payload->>'etype'='batteria_critica'")
[ "${GN:-0}" -ge 1 ] && ok "critico: escalation GoAlert richiesta (outbox)" || no "nessuna escalation" "goalert_notify=$GN"
# anche l'alarm lowBattery deve seguire la policy
R=$(send_alarm lowBattery); [ "$R" != "None" ] && [ -n "$R" ] && ok "critico: alarm lowBattery -> incidente" || no "lowBattery non critico sotto policy critico" "$R"

# --- battery_as = warning (default) -> evento registrato, NON critico ---
setmap '{"config":{"sos_as":"sos","battery_min":15,"battery_as":"warning"}}'
OUT=$(send_batt 5); IID=$(echo "$OUT" | awk '{print $1}'); EV=$(echo "$OUT" | awk '{print $2}')
[ "$IID" = "None" ] && ok "warning: batteria bassa -> nessun incidente" || no "warning: incidente inatteso" "$OUT"
[ "$EV" = "EVT" ] && ok "warning: evento batteria_bassa comunque registrato" || no "warning: evento non registrato" "$OUT"

# --- battery_as = off -> ignorata del tutto (nessun incidente, nessun evento batteria) ---
setmap '{"config":{"sos_as":"sos","battery_min":15,"battery_as":"off"}}'
OUT=$(send_batt 5); IID=$(echo "$OUT" | awk '{print $1}')
[ "$IID" = "None" ] && ok "off: batteria bassa -> nessun incidente" || no "off: incidente inatteso" "$OUT"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
