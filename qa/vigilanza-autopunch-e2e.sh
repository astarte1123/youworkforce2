#!/usr/bin/env bash
# Vigilanza slice dispositivi: auto-punch GPS da posizione (PWA worker/position).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Vigilanza — auto-punch da dispositivo =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in giri loneworker; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
RN="RondaGPS-$(python -c 'import random;print(random.randint(100000,999999))')"
RID=$(curl -s $H -X POST $BASE/api/patrol-routes -d "{\"name\":\"$RN\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
curl -s $H -X POST $BASE/api/patrol-routes/$RID/checkpoints -d '{"name":"GP1","kind":"gps","lat":45.5,"lon":9.2,"radius_m":100}' -o /dev/null
curl -s $H -X POST $BASE/api/patrol-routes/$RID/checkpoints -d '{"name":"GP2","kind":"gps","lat":46.0,"lon":10.0,"radius_m":100}' -o /dev/null

# mario (lavoratore) avvia la ronda e invia la posizione
curl -s -c "$J/m" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"mario@acme.loneworker.eu","password":"demo"}' -o /dev/null
CM=$(csrfh "$J/m")
HM="-b $J/m -H x-tenant-slug:acme -H x-csrf-token:$CM -H content-type:application/json"
RO=$(curl -s $HM -X POST $BASE/api/patrol-routes/$RID/rounds -d '{}' | jq1 "print(json.load(sys.stdin).get('id',''))")
say "1 ronda avviata da mario"
[ -n "$RO" ] && ok || no "no round"

say "2 posizione vicino a GP1 -> auto-punch"
curl -s $HM -X POST $BASE/api/worker/position -d '{"lat":45.5001,"lon":9.2001}' -o /dev/null
P1=$(curl -s -b "$J/m" -H 'x-tenant-slug: acme' $BASE/api/patrol-rounds/$RO | jq1 "c=next(x for x in json.load(sys.stdin)['checkpoints'] if x['name']=='GP1');print(bool(c.get('punched') and c['punched']['ok']))")
[ "$P1" = "True" ] && ok || no "GP1 punched=$P1"

say "3 GP2 (lontano) NON punzonato"
P2=$(curl -s -b "$J/m" -H 'x-tenant-slug: acme' $BASE/api/patrol-rounds/$RO | jq1 "c=next(x for x in json.load(sys.stdin)['checkpoints'] if x['name']=='GP2');print(bool(c.get('punched')))")
[ "$P2" = "False" ] && ok || no "GP2 punched=$P2"

say "4 metodo punch = gps automatico"
MT=$(curl -s -b "$J/m" -H 'x-tenant-slug: acme' $BASE/api/patrol-rounds/$RO | jq1 "c=next(x for x in json.load(sys.stdin)['checkpoints'] if x['name']=='GP1');print(c['punched']['method'])")
[ "$MT" = "gps" ] && ok || no "method=$MT"

say "5 posizione vicino a GP2 -> auto-punch GP2"
curl -s $HM -X POST $BASE/api/worker/position -d '{"lat":46.0001,"lon":10.0001}' -o /dev/null
P2b=$(curl -s -b "$J/m" -H 'x-tenant-slug: acme' $BASE/api/patrol-rounds/$RO | jq1 "c=next(x for x in json.load(sys.stdin)['checkpoints'] if x['name']=='GP2');print(bool(c.get('punched') and c['punched']['ok']))")
[ "$P2b" = "True" ] && ok || no "GP2 punched=$P2b"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
