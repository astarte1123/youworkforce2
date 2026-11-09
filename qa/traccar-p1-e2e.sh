#!/usr/bin/env bash
# Traccar P1 (sicurezza): mappa allarmi completa, deviceOffline critico configurabile,
# batteria bassa. Usa l'adapter reale /api/ingress/traccar con payload Traccar-shaped.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
RUN="$(date +%s)-$$"; N=0

JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA")
setmap(){ curl -s -o /dev/null -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' \
  -X POST "$BASE/api/modules/traccar/tools/alarm_map" -d "$1"; }

# invia un evento Traccar (alarm) e restituisce incident_id ("None" se non critico)
send_alarm(){ local id="$RUN-$RANDOM-$RANDOM"
  curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
    -d "{\"event\":{\"id\":\"$id\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"$1\"}},\"device\":{\"uniqueId\":\"BADGE-001\"},\"position\":{\"latitude\":45.4,\"longitude\":9.1}}" \
    | python -c 'import sys,json;d=json.load(sys.stdin);print(d.get("incident_id"))'; }
send_evt(){ local id="$RUN-$RANDOM-$RANDOM"
  curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
    -d "{\"event\":{\"id\":\"$id\",\"type\":\"$1\"},\"device\":{\"uniqueId\":\"BADGE-001\"}}" \
    | python -c 'import sys,json;d=json.load(sys.stdin);print(d.get("incident_id"))'; }
send_batt(){ local id="$RUN-$RANDOM-$RANDOM"
  curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
    -d "{\"event\":{\"id\":\"$id\",\"type\":\"deviceMoving\"},\"device\":{\"uniqueId\":\"BADGE-001\"},\"position\":{\"latitude\":45.4,\"longitude\":9.1,\"attributes\":{\"batteryLevel\":$1}}}" \
    | python -c 'import sys,json;d=json.load(sys.stdin);print(d.get("incident_id"),d.get("event_id") is not None)'; }
crit(){ [ "$1" != "None" ] && [ -n "$1" ]; }

setmap '{"config":{"sos_as":"sos","offline_critical":"no","battery_min":15}}'

# allarmi di sicurezza -> incidente critico
crit "$(send_alarm removing)"  && ok "removing -> incidente (manomissione badge)" || no "removing" "-"
crit "$(send_alarm tampering)" && ok "tampering -> incidente"                       || no "tampering" "-"
crit "$(send_alarm powerCut)"  && ok "powerCut -> incidente (perdita alimentazione)"|| no "powerCut" "-"
crit "$(send_alarm jamming)"   && ok "jamming -> incidente"                         || no "jamming" "-"
crit "$(send_alarm accident)"  && ok "accident -> incidente (man_down)"             || no "accident" "-"
crit "$(send_alarm sos)"       && ok "sos -> incidente (regressione)"               || no "sos" "-"

# lowBattery -> warning, NON critico
R=$(send_alarm lowBattery); crit "$R" && no "lowBattery dovrebbe essere non critico" "$R" || ok "lowBattery -> non critico"
# allarme sconosciuto -> device_alarm non critico
R=$(send_alarm vibration); crit "$R" && no "vibration critico?" "$R" || ok "vibration -> device_alarm non critico"

# deviceOffline: default non critico
R=$(send_evt deviceOffline); crit "$R" && no "offline critico di default?" "$R" || ok "deviceOffline default -> non critico"
# abilita offline critico
setmap '{"config":{"sos_as":"sos","offline_critical":"si","battery_min":15}}'
crit "$(send_evt deviceOffline)" && ok "offline_critical=si -> deviceOffline apre incidente (perdita contatto)" || no "offline critical on" "-"
setmap '{"config":{"sos_as":"sos","offline_critical":"no","battery_min":15}}'

# P3: deviceStopped -> immobilita (opt-in)
R=$(send_evt deviceStopped); crit "$R" && no "deviceStopped critico di default?" "$R" || ok "deviceStopped default -> non critico"
setmap '{"config":{"sos_as":"sos","stopped_as_immobilita":"si","battery_min":15}}'
crit "$(send_evt deviceStopped)" && ok "stopped_as_immobilita=si -> deviceStopped apre incidente (immobilita)" || no "stopped immobilita" "-"
setmap '{"config":{"sos_as":"sos","stopped_as_immobilita":"no","battery_min":15}}'

# batteria sotto soglia su posizione -> batteria_bassa (ingerito, non critico)
OUT=$(send_batt 5); echo "$OUT" | grep -q "None True" && ok "batteria 5% -> ingerito non critico (batteria_bassa)" || no "battery low" "$OUT"
# batteria ok -> nessun declassamento
OUT=$(send_batt 80); echo "$OUT" | grep -q "None True" && ok "batteria 80% -> ingerito (traccar_event)" || no "battery ok" "$OUT"

# config espone i nuovi campi
curl -s -b "$JA" -H 'x-tenant-slug: acme' "$BASE/api/modules/traccar/tools" | grep -q 'offline_critical' && ok "schema alarm_map aggiornato (offline_critical, battery_min)" || no "schema" "-"

echo "== $PASS passed, $FAIL failed =="
exit $FAIL
