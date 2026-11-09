#!/usr/bin/env bash
# #5 Report: percorso (distanza/durata/soste) + presenza (ingressi/uscite) + export CSV.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; TOK="${INGEST_TOKEN:-acme-ingest-token}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
jget(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
RUN="$(date +%s)-$$"; EXT="REP-$RUN"

JA=$(mktemp); JM=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
curl -s -o /dev/null -c "$JM" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA"); CM=$(csrfh "$JM")
A(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" -H 'content-type: application/json' "$@"; }
pos(){ curl -s -o /dev/null -X POST "$BASE/api/ingress/traccar/position?token=$TOK" -H 'content-type: application/json' \
  -d "{\"device\":{\"uniqueId\":\"$EXT\"},\"position\":{\"id\":\"$1\",\"latitude\":$2,\"longitude\":$3,\"speed\":$4,\"fixTime\":\"$5\"}}"; }

DID=$(A -X POST "$BASE/api/devices" -d "{\"external_id\":\"$EXT\",\"source\":\"traccar\",\"label\":\"Rep badge\"}" | jget 'd["id"]')
GID=$(A -X POST "$BASE/api/geofences" -d "{\"name\":\"RepArea $RUN\",\"kind\":\"circle\",\"geom\":{\"lat\":45.4641,\"lon\":9.1919,\"radius\":300}}" | jget 'd["id"]')

# sosta dentro ~6 min (stesso punto) poi spostamento fuori
pos "r1" 45.4641 9.1919 0 "2026-11-10T10:00:00Z"
pos "r2" 45.4641 9.1919 0 "2026-11-10T10:03:00Z"
pos "r3" 45.4642 9.1920 0 "2026-11-10T10:06:00Z"
pos "r4" 45.5200 9.2500 30 "2026-11-10T10:20:00Z"   # ~7 km piu' lontano, esce

R=$(A "$BASE/api/reports/route?device=$DID&from=2026-11-10T00:00:00Z&to=2026-11-10T23:59:00Z")
# nota: il filtro #7 puo' scartare il punto duplicato (r2) -> >=3 punti
echo "$R" | python -c 'import sys,json;d=json.load(sys.stdin);assert d["points"]>=3,d;assert d["total_m"]>1000,d;assert len(d["stops"])>=1,d;print("ok")' >/dev/null 2>&1 \
  && ok "report percorso: punti>=3, distanza>1km, >=1 sosta" || no "route" "$R"

A "$BASE/api/reports/route.csv?device=$DID&from=2026-11-10T00:00:00Z&to=2026-11-10T23:59:00Z" | grep -q 'data_ora,lat,lon' && ok "export percorso CSV" || no "route csv" "-"

R=$(A "$BASE/api/reports/presence?from=2026-11-10T00:00:00Z&to=2026-11-10T23:59:00Z")
echo "$R" | python -c "import sys,json;d=json.load(sys.stdin);ev=[e for e in d['events'] if e['external_id']=='$EXT'];assert any(e['inside'] for e in ev),ev;assert any(not e['inside'] for e in ev),ev;print('ok')" >/dev/null 2>&1 \
  && ok "report presenza: ingresso + uscita registrati" || no "presence" "$R"

A "$BASE/api/reports/presence.csv?from=2026-11-10T00:00:00Z&to=2026-11-10T23:59:00Z" | grep -q 'evento' && ok "export presenza CSV" || no "presence csv" "-"

# RBAC
C=$(curl -s -o /dev/null -w '%{http_code}' -b "$JM" -H 'x-tenant-slug: acme' "$BASE/api/reports/route?device=$DID")
[ "$C" = "403" ] && ok "lavoratore non accede ai report (403)" || no "rbac" "$C"

A -X DELETE "$BASE/api/geofences/$GID" >/dev/null; A -X DELETE "$BASE/api/devices/$DID" >/dev/null 2>&1 || true
echo "== $PASS passed, $FAIL failed =="; exit $FAIL
