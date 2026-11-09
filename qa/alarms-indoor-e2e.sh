#!/usr/bin/env bash
# SICUREZZA — allarmi INDOOR vs OUTDOOR. In fabbrica il GPS spesso non prende: i
# dispositivi riportano la zona via BEACON. Un allarme con beacon dev'essere marcato
# indoor e localizzato per zona; senza beacon resta outdoor (GPS). Un beacon non
# ancora mappato resta indoor con zona ignota (l'admin deve poterlo registrare).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
RUN="$(date +%s)-$$"

JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA")
api(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
UIDM="900-$RANDOM"; UIDU="901-$RANDOM"   # beacon mappato / non mappato
cleanup(){ sql "DELETE FROM app.beacons WHERE tenant_id=$TIDQ AND beacon_uid IN ('$UIDM')" >/dev/null; }
trap cleanup EXIT
cleanup

# --- admin registra una zona beacon con ancora ---
BID=$(api -X POST "$BASE/api/beacons" -d "{\"beacon_uid\":\"$UIDM\",\"zone_name\":\"Reparto Test $RUN\",\"lat\":45.50,\"lon\":9.20}" | python -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$BID" ] && ok "admin registra zona beacon (CRUD)" || no "registrazione beacon fallita" "-"

# --- allarme INDOOR: SOS con beacon mappato, GPS assente ---
ii=$(curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
  -d "{\"event\":{\"id\":\"ind-$RUN\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"sos\"}},\"device\":{\"uniqueId\":\"BADGE-001\"},\"position\":{\"attributes\":{\"beaconId\":\"$UIDM\"}}}" \
  | python -c 'import sys,json;print(json.load(sys.stdin).get("event_id",""))')
R=$(sql "SELECT indoor||'|'||coalesce(zone,'')||'|'||coalesce(lat::text,'')||'|'||coalesce(beacon_uid,'') FROM app.events WHERE id='$ii'")
echo "indoor evento: $R"
[ "${R%%|*}" = "true" ] && ok "allarme con beacon -> indoor=true" || no "non marcato indoor" "$R"
echo "$R" | grep -q "Reparto Test" && ok "zona risolta dal beacon" || no "zona non risolta" "$R"
echo "$R" | grep -q "45.5" && ok "ancora del beacon applicata come posizione" || no "ancora non applicata" "$R"

# --- allarme OUTDOOR: SOS con GPS, nessun beacon ---
io=$(curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
  -d "{\"event\":{\"id\":\"out-$RUN\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"sos\"}},\"device\":{\"uniqueId\":\"BADGE-001\"},\"position\":{\"latitude\":45.40,\"longitude\":9.10}}" \
  | python -c 'import sys,json;print(json.load(sys.stdin).get("event_id",""))')
RO=$(sql "SELECT indoor FROM app.events WHERE id='$io'")
[ "$RO" = "f" ] && ok "allarme con GPS, senza beacon -> outdoor (indoor=false)" || no "outdoor marcato indoor" "$RO"

# --- beacon NON mappato: indoor con zona ignota (da registrare) ---
iu=$(curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
  -d "{\"event\":{\"id\":\"unm-$RUN\",\"type\":\"alarm\",\"attributes\":{\"alarm\":\"sos\"}},\"device\":{\"uniqueId\":\"BADGE-001\"},\"position\":{\"attributes\":{\"beaconId\":\"$UIDU\"}}}" \
  | python -c 'import sys,json;print(json.load(sys.stdin).get("event_id",""))')
RU=$(sql "SELECT indoor||'|'||coalesce(zone,'NULL')||'|'||coalesce(beacon_uid,'') FROM app.events WHERE id='$iu'")
[ "${RU%%|*}" = "true" ] && echo "$RU" | grep -qF "|NULL|$UIDU" && ok "beacon non mappato -> indoor, zona ignota, uid tracciato" || no "beacon non mappato gestito male" "$RU"

# --- endpoint mappa espone indoor/outdoor con device e beacon ---
MAP=$(api "$BASE/api/alarms/map?from=2020-01-01T00:00:00Z&to=2030-01-01T00:00:00Z&etype=sos")
echo "$MAP" | python -c 'import sys,json;a=json.load(sys.stdin)["alarms"];print("ok" if any(x["indoor"] for x in a) and any(not x["indoor"] for x in a) else "no")' | grep -q ok \
  && ok "mappa allarmi espone sia indoor sia outdoor" || no "mappa non differenzia" "-"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
