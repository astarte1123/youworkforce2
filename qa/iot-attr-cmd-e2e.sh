#!/usr/bin/env bash
# IoT nativo F2: attributi (client/shared/server) + comandi/RPC + pull device.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== IoT F2 — attributi + comandi =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"iot","state":"attivo"}' -o /dev/null
curl -s -c "$J/a" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}' -o /dev/null
CA=$(csrfh "$J/a")
H="-b $J/a -H x-tenant-slug:acme -H x-csrf-token:$CA -H content-type:application/json"
EXT="IOT-$(python -c 'import random;print(random.randint(100000,999999))')"
DID=$(curl -s $H -X POST $BASE/api/devices -d "{\"external_id\":\"$EXT\",\"source\":\"iot\",\"label\":\"PLC\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")

say "1 set shared attribute"
curl -s $H -X POST $BASE/api/iot/devices/$DID/attributes -d '{"scope":"shared","values":{"reportInterval":60}}' -o /dev/null
SH=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' $BASE/api/iot/devices/$DID/attributes | jq1 "print(json.load(sys.stdin)['shared'].get('reportInterval'))")
[ "$SH" = "60" ] && ok || no "shared=$SH"

say "2 scope non valido -> 400"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/iot/devices/$DID/attributes -d '{"scope":"boh","values":{"x":1}}')
[ "$c" = "400" ] && ok || no "got $c"

say "3 ingest con attributi client"
curl -s -X POST "$BASE/api/ingress/telemetry?token=acme-ingest-token&id=$EXT" -H 'content-type: application/json' -d '{"values":{"t":20},"attributes":{"fw":"1.2.0"}}' -o /dev/null
CL=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' $BASE/api/iot/devices/$DID/attributes | jq1 "print(json.load(sys.stdin)['client'].get('fw'))")
[ "$CL" = "1.2.0" ] && ok || no "client fw=$CL"

say "4 comando -> pending (device non teltonika)"
CID=$(curl -s $H -X POST $BASE/api/iot/devices/$DID/command -d '{"method":"reboot"}' | jq1 "d=json.load(sys.stdin);print(d.get('status'),d.get('id'))")
echo "$CID" | grep -q "pending" && ok || no "$CID"

say "5 lista comandi >=1"
NC=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' $BASE/api/iot/devices/$DID/commands | jq1 "print(len(json.load(sys.stdin)))")
[ "${NC:-0}" -ge 1 ] && ok || no "cmds=$NC"

say "6 pull device: shared + comandi pending -> poi sent"
P=$(curl -s "$BASE/api/ingress/pull?token=acme-ingest-token&id=$EXT")
OKP=$(echo "$P" | jq1 "d=json.load(sys.stdin);print(d['shared'].get('reportInterval')==60 and len(d['commands'])>=1)")
[ "$OKP" = "True" ] && ok || no "$P"

say "7 dopo il pull il comando è 'sent'"
STt=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' $BASE/api/iot/devices/$DID/commands | jq1 "print(json.load(sys.stdin)[0]['status'])")
[ "$STt" = "sent" ] && ok || no "status=$STt"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
