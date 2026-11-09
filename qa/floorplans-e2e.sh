#!/usr/bin/env bash
# INDOOR — piantine caricabili + beacon posizionati + geofence indoor (coordinate pixel).
# Verifica upload immagine (round-trip byte identici), beacon su piantina (x,y),
# geofence indoor (polygon/circle in coordinate immagine) e isolamento per tenant.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" | tail -1; }
sql(){ docker exec ywdb psql -U postgres -d youworkforce -tAc "$1"; }
TIDQ="(SELECT id FROM app.tenants WHERE slug='acme')"
WORK="$(pwd)/.fp_e2e_$$"; mkdir -p "$WORK"
cleanup(){ sql "DELETE FROM app.floorplans WHERE tenant_id=$TIDQ AND name LIKE 'E2E %'" >/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

# genera un PNG valido (path relativo: python Windows e Git Bash concordano sul cwd)
python - "$WORK/fp.png" <<'PY'
import sys,struct,zlib
w,h=16,12
raw=bytearray()
for y in range(h):
    raw.append(0)
    for x in range(w): raw+=bytes([(x*16)%256,(y*20)%256,100])
def c(t,d): return struct.pack('>I',len(d))+t+d+struct.pack('>I',zlib.crc32(t+d)&0xffffffff)
png=b'\x89PNG\r\n\x1a\n'+c(b'IHDR',struct.pack('>IIBBBBB',w,h,8,2,0,0,0))+c(b'IDAT',zlib.compress(bytes(raw)))+c(b'IEND',b'')
open(sys.argv[1],'wb').write(png)
PY
[ -f "$WORK/fp.png" ] && ok "PNG di test generato" || { no "png non generato" "-"; echo "== $PASS passed, $((FAIL+1)) failed =="; exit 1; }

JA=$(mktemp)
curl -s -o /dev/null -c "$JA" -X POST "$BASE/api/login" -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}'
CA=$(csrfh "$JA")
api(){ curl -s -b "$JA" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CA" "$@"; }

# upload piantina
FID=$(api -H 'content-type: image/png' --data-binary @"$WORK/fp.png" "$BASE/api/floorplans?name=E2E%20Capannone&width=16&height=12" | python -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
[ -n "$FID" ] && ok "upload piantina -> id" || no "upload fallito" "-"

# dimensioni memorizzate
DIM=$(sql "SELECT width||'x'||height FROM app.floorplans WHERE id='$FID'")
[ "$DIM" = "16x12" ] && ok "dimensioni immagine memorizzate ($DIM)" || no "dimensioni errate" "$DIM"

# round-trip immagine identico
api "$BASE/api/floorplans/$FID/image" -o "$WORK/back.png" >/dev/null
cmp -s "$WORK/fp.png" "$WORK/back.png" && ok "immagine round-trip byte identici" || no "immagine alterata" "-"

# beacon posizionato sulla piantina (x,y)
api -H 'content-type: application/json' -X POST "$BASE/api/beacons" \
  -d "{\"beacon_uid\":\"e2e-fp-1\",\"zone_name\":\"E2E Zona\",\"floorplan_id\":\"$FID\",\"x\":130.5,\"y\":90}" >/dev/null
BX=$(sql "SELECT floorplan_id||'|'||x||'|'||y FROM app.beacons WHERE tenant_id=$TIDQ AND beacon_uid='e2e-fp-1'")
echo "$BX" | grep -qF "$FID|130.5|90" && ok "beacon posizionato su piantina (x,y)" || no "beacon xy errato" "$BX"

# geofence indoor (polygon in coordinate immagine)
GID=$(api -H 'content-type: application/json' -X POST "$BASE/api/geofences" \
  -d "{\"name\":\"E2E Area $RANDOM\",\"kind\":\"polygon\",\"floorplan_id\":\"$FID\",\"geom\":{\"points\":[[1,1],[10,1],[10,8],[1,8]]}}" | python -c 'import sys,json;print(json.load(sys.stdin).get("id",""))')
FP_G=$(sql "SELECT floorplan_id FROM app.geofences WHERE id='$GID'")
[ "$FP_G" = "$FID" ] && ok "geofence indoor legata alla piantina" || no "geofence indoor non legata" "$FP_G"

# la lista geofence espone floorplan_id (per filtrare lato UI)
api "$BASE/api/geofences" | python -c "import sys,json;d=json.load(sys.stdin);print('ok' if any(g.get('floorplan_id')=='$FID' for g in d) else 'no')" | grep -q ok \
  && ok "lista geofence espone floorplan_id" || no "floorplan_id non esposto" "-"

# rinomina piantina
api -H 'content-type: application/json' -X POST "$BASE/api/floorplans/$FID" -d '{"name":"E2E Rinominata"}' >/dev/null
[ "$(sql "SELECT name FROM app.floorplans WHERE id='$FID'")" = "E2E Rinominata" ] && ok "rinomina piantina" || no "rinomina fallita" "-"

# posizionamento di un beacon GIA' registrato (register -> place), non invenzione
api -H 'content-type: application/json' -X POST "$BASE/api/beacons" -d '{"beacon_uid":"e2e-reg-1","zone_name":"E2E Reg"}' >/dev/null
RBID=$(sql "SELECT id FROM app.beacons WHERE tenant_id=$TIDQ AND beacon_uid='e2e-reg-1'")
api -H 'content-type: application/json' -X POST "$BASE/api/beacons/$RBID/place" -d "{\"floorplan_id\":\"$FID\",\"x\":42,\"y\":24}" >/dev/null
PB=$(sql "SELECT floorplan_id||'|'||x||'|'||y FROM app.beacons WHERE id='$RBID'")
echo "$PB" | grep -qF "$FID|42|24" && ok "beacon registrato posizionato via /place" || no "place fallito" "$PB"
# unplace
api -H 'content-type: application/json' -X POST "$BASE/api/beacons/$RBID/place" -d '{"floorplan_id":null,"x":null,"y":null}' >/dev/null
[ "$(sql "SELECT coalesce(floorplan_id::text,'NULL') FROM app.beacons WHERE id='$RBID'")" = "NULL" ] && ok "beacon rimosso dalla piantina (resta registrato)" || no "unplace fallito" "-"
sql "DELETE FROM app.beacons WHERE id='$RBID'" >/dev/null

# delete piantina -> beacon resta ma perde la posizione (ON DELETE SET NULL)
api -H 'content-type: application/json' -X POST "$BASE/api/beacons" -d "{\"beacon_uid\":\"e2e-fp-1\",\"zone_name\":\"E2E Zona\",\"floorplan_id\":\"$FID\",\"x\":5,\"y\":5}" >/dev/null
api -X DELETE "$BASE/api/floorplans/$FID" >/dev/null
LEFT=$(sql "SELECT coalesce(floorplan_id::text,'NULL') FROM app.beacons WHERE tenant_id=$TIDQ AND beacon_uid='e2e-fp-1'")
[ "$LEFT" = "NULL" ] && ok "delete piantina -> beacon de-posizionato (SET NULL), non perso" || no "cascade errato" "$LEFT"
sql "DELETE FROM app.beacons WHERE tenant_id=$TIDQ AND beacon_uid='e2e-fp-1'" >/dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
