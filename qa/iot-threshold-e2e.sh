#!/usr/bin/env bash
# IoT nativo F3: soglie telemetria (sensor_rules) -> incidente nativo.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== IoT F3 — soglie -> incidenti =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in iot loneworker; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done
curl -s -c "$J/a" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}' -o /dev/null
CA=$(csrfh "$J/a")
H="-b $J/a -H x-tenant-slug:acme -H x-csrf-token:$CA -H content-type:application/json"
EXT="THR-$(python -c 'import random;print(random.randint(100000,999999))')"
MET="tempC$(python -c 'import random;print(random.randint(1000,9999))')"
DID=$(curl -s $H -X POST $BASE/api/devices -d "{\"external_id\":\"$EXT\",\"source\":\"iot\",\"label\":\"PLC T\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")

say "1 crea regola soglia ($MET >= 30 -> sensore_critico)"
RR=$(curl -s $H -X POST $BASE/api/sensor-rules -d "{\"metric\":\"$MET\",\"op\":\">=\",\"threshold\":30,\"etype\":\"sensore_critico\",\"label\":\"$MET soglia\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$RR" ] && ok || no "no rule"

say "2 telemetria sotto soglia -> nessun incidente"
R=$(curl -s -X POST "$BASE/api/ingress/telemetry?token=acme-ingest-token&id=$EXT" -H 'content-type: application/json' -d "{\"values\":{\"$MET\":20}}")
IN=$(echo "$R" | jq1 "print(json.load(sys.stdin).get('incident'))")
[ "$IN" = "None" ] && ok || no "incident=$IN"

say "3 telemetria sopra soglia -> incidente creato"
R=$(curl -s -X POST "$BASE/api/ingress/telemetry?token=acme-ingest-token&id=$EXT" -H 'content-type: application/json' -d "{\"values\":{\"$MET\":35}}")
IID=$(echo "$R" | jq1 "i=json.load(sys.stdin).get('incident');print((i.get('incident_id') or i.get('id') or '') if isinstance(i,dict) else '')")
[ -n "$IID" ] && ok || no "$R"

say "4 l'incidente compare in centrale"
GOT=$(curl -s -b "$J/a" -H 'x-tenant-slug: acme' $BASE/api/incidents | jq1 "print(any(str(i.get('id'))=='$IID' for i in json.load(sys.stdin)))")
[ "$GOT" = "True" ] && ok || no "id $IID non in centrale"

say "5 rimuovi regola"
curl -s $H -X DELETE $BASE/api/sensor-rules/$RR -o /dev/null
R=$(curl -s -X POST "$BASE/api/ingress/telemetry?token=acme-ingest-token&id=$EXT" -H 'content-type: application/json' -d "{\"values\":{\"$MET\":40}}")
IN=$(echo "$R" | jq1 "print(json.load(sys.stdin).get('incident'))")
[ "$IN" = "None" ] && ok || no "incident ancora creato=$IN"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
