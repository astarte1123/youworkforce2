#!/usr/bin/env bash
# Traccar P2 — posizioni & presenza: position forwarding -> ultima posizione + storico,
# idempotenza, no-regressione su posizione vecchia, geofence enter/exit come presenza.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"; EXT="BADGE-P2-$RUN"

JA=$(mktemp); JO=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JO" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"operatore@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA")
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }

# registra device traccar
DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"$EXT\",\"source\":\"traccar\",\"label\":\"Badge P2\"}" | jget 'd["id"]')
[ -n "$DID" ] && ok "device traccar registrato ($DID)" || no "create device" "$DID"

pos(){ # id lat lon fixtime battery
  curl -s -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
    -d "{\"device\":{\"uniqueId\":\"$EXT\"},\"position\":{\"id\":\"$1\",\"latitude\":$2,\"longitude\":$3,\"speed\":1.2,\"course\":90,\"fixTime\":\"$4\",\"attributes\":{\"batteryLevel\":$5}}}"; }

# invia posizione
pos "p-$RUN-1" 45.470 9.190 "2026-11-10T10:00:00Z" 88 | grep -q '"ok":true' && ok "posizione accettata" || no "position ingest" "-"
# ultima posizione sul device
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');assert abs(x['last_lat']-45.470)<1e-6,x;assert x['last_battery']==88,x;assert x['last_pos_at'],x;print('ok')" >/dev/null 2>&1 \
  && ok "ultima posizione aggiornata su device (lat+batteria)" || no "last pos on device" "-"
# storico
A "$BASE/api/devices/$DID/positions" | python -c "import sys,json;d=json.load(sys.stdin);assert len(d)>=1;assert abs(d[0]['lat']-45.470)<1e-6;print('ok')" >/dev/null 2>&1 \
  && ok "storico posizioni leggibile" || no "positions list" "-"
# idempotenza: stesso id -> dedup
# stesso id + stessa posizione: non re-inserita (dedup per id OPPURE scartata dal filtro #7)
pos "p-$RUN-1" 45.470 9.190 "2026-11-10T10:00:00Z" 88 | grep -qE '"deduplicated":true|"filtered"' && ok "stessa posizione -> non re-inserita (dedup/filtro)" || no "dedup" "-"
# posizione piu' recente aggiorna
pos "p-$RUN-2" 45.500 9.200 "2026-11-10T11:00:00Z" 80 >/dev/null
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');assert abs(x['last_lat']-45.500)<1e-6,x;print('ok')" >/dev/null 2>&1 \
  && ok "posizione piu' recente aggiorna ultima nota" || no "newer pos" "-"
# posizione piu' VECCHIA non regredisce
pos "p-$RUN-0" 10.0 10.0 "2026-11-10T09:00:00Z" 70 >/dev/null
A "$BASE/api/devices" | python -c "import sys,json;d=json.load(sys.stdin);x=next(v for v in d if v['id']=='$DID');assert abs(x['last_lat']-45.500)<1e-6,x;print('ok')" >/dev/null 2>&1 \
  && ok "posizione vecchia NON sovrascrive l'ultima" || no "old pos regress" "-"
# device non registrato -> ignorato
curl -s -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
  -d '{"device":{"uniqueId":"IGNOTO-xyz"},"position":{"id":"z1","latitude":1,"longitude":1}}' | grep -q 'ignored' && ok "device non registrato -> ignorato" || no "unknown device" "-"

# geofence come presenza (non critico)
G1=$(curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
  -d "{\"event\":{\"id\":\"gf-$RUN-1\",\"type\":\"geofenceEnter\"},\"device\":{\"uniqueId\":\"$EXT\"}}")
echo "$G1" | python -c 'import sys,json;d=json.load(sys.stdin);assert d.get("incident_id") is None;assert d.get("event_id");print("ok")' >/dev/null 2>&1 \
  && ok "geofenceEnter -> presenza (ingresso_sito, non critico)" || no "geofenceEnter" "$G1"
G2=$(curl -s -X POST "$BASE/api/ingress/traccar?token=$TOK" -H 'content-type: application/json' \
  -d "{\"event\":{\"id\":\"gf-$RUN-2\",\"type\":\"geofenceExit\"},\"device\":{\"uniqueId\":\"$EXT\"}}")
echo "$G2" | python -c 'import sys,json;d=json.load(sys.stdin);assert d.get("event_id");print("ok")' >/dev/null 2>&1 \
  && ok "geofenceExit -> presenza (uscita_sito)" || no "geofenceExit" "$G2"

# RBAC: lavoratore non legge le posizioni
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JM" -H 'x-tenant-slug: acme' "$BASE/api/devices/$DID/positions")
[ "$C" = "403" ] && ok "lavoratore non legge posizioni (403)" || no "rbac positions" "$C"
# operatore sì
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JO" -H 'x-tenant-slug: acme' "$BASE/api/devices/$DID/positions")
[ "$C" = "200" ] && ok "operatore legge posizioni (200)" || no "operator positions" "$C"

# cleanup
A -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
echo "== $PASS passed, $FAIL failed =="
exit $FAIL
