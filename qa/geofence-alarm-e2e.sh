#!/usr/bin/env bash
# Geofence -> ALLARME su attraversamento (calcolato da YW): area con alert_on='exit' (area sicura)
# genera un incidente quando il device ESCE; con alert_on='enter' (zona vietata) quando ENTRA.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
RUN="$(date +%s)-$$"; EXT="GF-$RUN"

JA=$(mktemp); curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
A -X POST "$BASE/api/modules/traccar/tools/alarm_map" -d '{"config":{"sos_as":"sos","max_jump_kmh":300,"min_move_m":5}}' >/dev/null
DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"$EXT\",\"source\":\"traccar\",\"label\":\"Geo $RUN\"}" | jget 'd["id"]')
pos(){ curl -s -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
  -d "{\"device\":{\"uniqueId\":\"$EXT\"},\"position\":{\"id\":\"$1\",\"latitude\":$2,\"longitude\":$3,\"fixTime\":\"$4\"}}"; }
cnt_uscita(){ sql "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE i.tenant_id=$TIDQ AND i.etype='geofence_uscita' AND e.source='geofence'"; }
cnt_ingresso(){ sql "SELECT count(*) FROM app.incidents i WHERE i.tenant_id=$TIDQ AND i.etype='geofence_ingresso'"; }
cleanup(){
  sql "DELETE FROM app.geofences WHERE tenant_id=$TIDQ AND name LIKE 'E2E-GF-$RUN%'" >/dev/null 2>&1 || true
  # chiudi gli incidenti di test (il device resta): evita carico sul worker escalation
  sql "UPDATE app.incidents SET state='chiuso' WHERE tenant_id=$TIDQ AND event_id IN (SELECT id FROM app.events WHERE source='geofence' AND device_id=(SELECT id FROM app.devices WHERE tenant_id=$TIDQ AND external_id='$EXT'))" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- AREA SICURA (alert su USCITA) ---
GID=$(A -X POST "$BASE/api/geofences" -d "{\"name\":\"E2E-GF-$RUN-safe\",\"kind\":\"circle\",\"geom\":{\"lat\":45.50,\"lon\":9.20,\"radius\":300}}" | jget 'd["id"]')
[ -n "$GID" ] && ok "geofence creata" || no "no geofence" "-"
A -X PUT "$BASE/api/geofences/$GID/alert" -d '{"alert_on":"exit"}' | grep -q '"ok":true' && ok "alert_on=exit impostato" || no "set alert fallito" "-"

B=$(cnt_uscita)
# dentro l'area (primo fix): nessun allarme di uscita
pos "in-$RUN" 45.5000 9.2000 "2026-11-10T10:00:00Z" >/dev/null
sleep 1
[ "$(cnt_uscita)" = "$B" ] && ok "ingresso in area sicura: nessun allarme di uscita" || no "allarme spurio all'ingresso" "-"

# esce dall'area (~11km, 1h dopo -> velocità plausibile): allarme di USCITA
pos "out-$RUN" 45.6000 9.2000 "2026-11-10T11:00:00Z" >/dev/null
sleep 1
[ "$(cnt_uscita)" -eq "$((B+1))" ] && ok "uscita da area sicura -> allarme geofence_uscita (incidente)" || no "allarme uscita non generato" "$B -> $(cnt_uscita)"

# l'incidente porta la geofence nella zona (posizione esplicativa)
Z=$(sql "SELECT e.zone FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE i.etype='geofence_uscita' AND e.source='geofence' ORDER BY i.opened_at DESC LIMIT 1")
echo "$Z" | grep -q "E2E-GF-$RUN-safe" && ok "incidente geofence riporta il nome area nella zona" || no "zona non valorizzata" "$Z"

# --- ZONA VIETATA (alert su INGRESSO) con etype configurato (sos) ---
GID2=$(A -X POST "$BASE/api/geofences" -d "{\"name\":\"E2E-GF-$RUN-vietata\",\"kind\":\"circle\",\"geom\":{\"lat\":46.00,\"lon\":9.00,\"radius\":300}}" | jget 'd["id"]')
A -X PUT "$BASE/api/geofences/$GID2/alert" -d '{"alert_on":"enter"}' >/dev/null
# fuori -> dentro: ingresso in zona vietata
BSOS=$(sql "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE i.etype='geofence_ingresso' AND e.source='geofence'")
pos "far-$RUN" 45.0000 9.0000 "2026-11-10T12:00:00Z" >/dev/null   # lontano da entrambe
sleep 1
pos "enter-$RUN" 46.0000 9.0000 "2026-11-10T20:00:00Z" >/dev/null # dentro la vietata (8h dopo)
sleep 1
A2=$(sql "SELECT count(*) FROM app.incidents i JOIN app.events e ON e.id=i.event_id WHERE i.etype='geofence_ingresso' AND e.source='geofence'")
[ "$A2" -eq "$((BSOS+1))" ] && ok "ingresso in zona vietata -> allarme geofence_ingresso" || no "allarme ingresso non generato" "$BSOS -> $A2"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
