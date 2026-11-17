#!/usr/bin/env bash
# Cantieri C2: cronoprogramma — date e dipendenze sulle lavorazioni + rilevamento ritardo.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Cantieri C2 — cronoprogramma =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"
CID=$(curl -s $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"Crono test\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
getlav(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | python -c "import sys,json;d=json.load(sys.stdin);print(json.dumps(d['lavorazioni']))"; }

say "1 lavorazione con date (cronoprogramma)"
L1=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"Fondazioni","weight":2,"start_date":"2040-03-01","end_date":"2040-03-10"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
SD=$(getlav | jq1 "import sys,json;print(next(l['start_date'] for l in json.load(sys.stdin) if l['id']=='$L1'))" 2>/dev/null)
[ "$SD" = "2040-03-01" ] && ok || no "start=$SD"

say "2 fine prima dell'inizio -> 400"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"X","start_date":"2040-03-10","end_date":"2040-03-01"}')
[ "$c" = "400" ] && ok || no "got $c"

say "3 seconda lavorazione dipende dalla prima"
L2=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"Struttura","weight":3}' | jq1 "print(json.load(sys.stdin).get('id',''))")
curl -s $H -X POST $BASE/api/lavorazioni/$L2 -d "{\"start_date\":\"2040-03-11\",\"end_date\":\"2040-03-20\",\"depends_on\":\"$L1\"}" -o /dev/null
DEP=$(getlav | jq1 "import sys,json;print(next(l.get('depends_on') for l in json.load(sys.stdin) if l['id']=='$L2'))")
[ "$DEP" = "$L1" ] && ok || no "dep=$DEP"

say "4 dipendenza da sé stessa -> 400"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/lavorazioni/$L2 -d "{\"depends_on\":\"$L2\"}")
[ "$c" = "400" ] && ok || no "got $c"

say "5 rimozione dipendenza (clear_dep)"
curl -s $H -X POST $BASE/api/lavorazioni/$L2 -d '{"clear_dep":true}' -o /dev/null
DEP=$(getlav | jq1 "import sys,json;print(next(l.get('depends_on') for l in json.load(sys.stdin) if l['id']=='$L2'))")
[ "$DEP" = "None" ] && ok || no "dep=$DEP"

say "6 rilevamento ritardo (fine passata, progress<100)"
L3=$(curl -s $H -X POST $BASE/api/commesse/$CID/lavorazioni -d '{"name":"Scaduta","start_date":"2020-01-01","end_date":"2020-01-10"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
LATE=$(getlav | jq1 "import sys,json;print(next(l.get('late') for l in json.load(sys.stdin) if l['id']=='$L3'))")
[ "$LATE" = "True" ] && ok || no "late=$LATE"

say "7 lavorazione completata non è in ritardo"
curl -s $H -X POST $BASE/api/lavorazioni/$L3 -d '{"status":"fatto"}' -o /dev/null
LATE=$(getlav | jq1 "import sys,json;print(next(l.get('late') for l in json.load(sys.stdin) if l['id']=='$L3'))")
[ "$LATE" = "False" ] && ok || no "late=$LATE"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
