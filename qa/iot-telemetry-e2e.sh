#!/usr/bin/env bash
# IoT nativo F1: telemetria time-series (ingest nativo + storico + latest).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== IoT F1 — telemetria nativa =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"iot","state":"attivo"}' -o /dev/null
curl -s -c "$J/a" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}' -o /dev/null
CA=$(csrfh "$J/a")
H="-b $J/a -H x-tenant-slug:acme -H x-csrf-token:$CA -H content-type:application/json"
EXT="SENS-$(python -c 'import random;print(random.randint(100000,999999))')"
DID=$(curl -s $H -X POST $BASE/api/devices -d "{\"external_id\":\"$EXT\",\"source\":\"iot\",\"label\":\"Sensore test\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
say "1 device IoT creato"
[ -n "$DID" ] && ok || no "no device"

say "2 ingest telemetria (token seed + id esterno)"
c=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/ingress/telemetry?token=acme-ingest-token&id=$EXT" -H 'content-type: application/json' -d '{"values":{"temperature":21.5,"humidity":55,"status":"ok"}}')
[ "$c" = "200" ] && ok || no "ingest $c"

say "3 latest espone i valori"
L=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' $BASE/api/iot/devices/$DID/telemetry/latest)
echo "$L" | jq1 "d={x['key']:(x['num'] if x['num'] is not None else x['str']) for x in json.load(sys.stdin)};print(d.get('temperature'),d.get('status'))" | grep -q "21.5 ok" && ok || no "$L"

say "4 secondo invio -> storico cresce"
curl -s -X POST "$BASE/api/ingress/telemetry?token=acme-ingest-token&id=$EXT" -H 'content-type: application/json' -d '{"values":{"temperature":22.1}}' -o /dev/null
N=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' "$BASE/api/iot/devices/$DID/telemetry?key=temperature" | jq1 "print(len(json.load(sys.stdin)))")
[ "${N:-0}" -ge 2 ] && ok || no "serie=$N"

say "5 latest temperature aggiornata a 22.1"
T=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' $BASE/api/iot/devices/$DID/telemetry/latest | jq1 "print(next(x['num'] for x in json.load(sys.stdin) if x['key']=='temperature'))")
[ "$T" = "22.1" ] && ok || no "temp=$T"

say "6 token errato -> 401"
c=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/ingress/telemetry?token=xxx&id=$EXT" -H 'content-type: application/json' -d '{"values":{"x":1}}')
[ "$c" = "401" ] && ok || no "got $c"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
