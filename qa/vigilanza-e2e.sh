#!/usr/bin/env bash
# Vigilanza V1: tempi per tratta + anomalie (saltato, non completato) alla chiusura ronda.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Vigilanza V1 — tempi + anomalie =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"giri","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
RN="Ronda-$(python -c 'import random;print(random.randint(100000,999999))')"

say "1 crea giro (mezzo, tolleranza 2)"
RID=$(curl -s $H -X POST $BASE/api/patrol-routes -d "{\"name\":\"$RN\",\"mode\":\"mezzo\",\"grace_min\":2}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$RID" ] && ok || no "no route"

say "2 route espone mode/grace"
echo "$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-routes)" | python -c "import sys,json;r=next(x for x in json.load(sys.stdin) if x['id']=='$RID');print('ok' if r.get('mode')=='mezzo' and r.get('grace_min')==2 else 'no')" | grep -q ok && ok || no "mode/grace"

say "3 aggiungi 3 checkpoint (cp2 max 5 min)"
curl -s $H -X POST $BASE/api/patrol-routes/$RID/checkpoints -d '{"name":"P1","kind":"qr","code":"A1"}' -o /dev/null
curl -s $H -X POST $BASE/api/patrol-routes/$RID/checkpoints -d '{"name":"P2","kind":"qr","code":"A2","max_min":5}' -o /dev/null
curl -s $H -X POST $BASE/api/patrol-routes/$RID/checkpoints -d '{"name":"P3","kind":"qr","code":"A3"}' -o /dev/null
NC=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-routes | jq1 "print(len(next(x for x in json.load(sys.stdin) if x['id']=='$RID')['checkpoints']))")
[ "$NC" = "3" ] && ok || no "checkpoints=$NC"

say "4 checkpoint espone max_min"
MX=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-routes | jq1 "cps=next(x for x in json.load(sys.stdin) if x['id']=='$RID')['checkpoints'];print(next(c['max_min'] for c in cps if c['name']=='P2'))")
[ "$MX" = "5" ] && ok || no "max_min=$MX"

say "5 avvia ronda + punzona solo P1"
RO=$(curl -s $H -X POST $BASE/api/patrol-routes/$RID/rounds -d '{}' | jq1 "print(json.load(sys.stdin).get('id',''))")
CID1=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-rounds/$RO | jq1 "print(next(c['id'] for c in json.load(sys.stdin)['checkpoints'] if c['name']=='P1'))")
curl -s $H -X POST $BASE/api/patrol-rounds/$RO/punch -d "{\"checkpoint_id\":\"$CID1\",\"code\":\"A1\"}" -o /dev/null
[ -n "$RO" ] && ok || no "no round"

say "6 chiudi ronda -> incompleto + anomalie"
CLOSE=$(curl -s $H -X POST $BASE/api/patrol-rounds/$RO/close -d '{}')
STT=$(echo "$CLOSE" | jq1 "print(json.load(sys.stdin)['status'])")
NA=$(echo "$CLOSE" | jq1 "print(json.load(sys.stdin)['anomalies'])")
[ "$STT" = "incompleto" ] && [ "${NA:-0}" -ge 3 ] && ok || no "status=$STT anomalie=$NA"

say "7 anomalie: 2 saltato + 1 non_completato"
KINDS=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-rounds/$RO | jq1 "a=json.load(sys.stdin)['anomalies'];ks=[x['kind'] for x in a];print(ks.count('saltato'),ks.count('non_completato'))")
[ "$KINDS" = "2 1" ] && ok || no "kinds=$KINDS"

say "8 modulo giri OFF -> 403"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"giri","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-routes)
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"giri","state":"non_abilitato"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
