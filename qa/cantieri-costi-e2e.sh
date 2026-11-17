#!/usr/bin/env bash
# Cantieri C3: costi vs budget (materiali + costo orario manodopera, scostamento).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Cantieri C3 — costi vs budget =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"
CID=$(curl -s $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"Costi test\",\"budget_amount\":1000}" | jq1 "print(json.load(sys.stdin).get('id',''))")
cost(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | python -c "import sys,json;print(json.dumps(json.load(sys.stdin)['cost']))"; }

say "1 imposta costo orario 25"
curl -s $H -X POST $BASE/api/commesse/$CID -d '{"labor_cost_hour":25}' -o /dev/null
LR=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(json.load(sys.stdin)['labor_rate'])")
[ "$LR" = "25.0" ] && ok || no "rate=$LR"

say "2 aggiungi materiale 300"
curl -s $H -X POST $BASE/api/commesse/$CID/costi -d '{"description":"Cemento","category":"materiale","amount":300}' -o /dev/null
MC=$(cost | jq1 "print(json.load(sys.stdin)['materials_cost'])")
[ "$MC" = "300.0" ] && ok || no "materials=$MC"

say "3 aggiungi nolo 200 -> materiali 500"
curl -s $H -X POST $BASE/api/commesse/$CID/costi -d '{"description":"Nolo gru","category":"nolo","amount":200}' -o /dev/null
MC=$(cost | jq1 "print(json.load(sys.stdin)['materials_cost'])")
[ "$MC" = "500.0" ] && ok || no "materials=$MC"

say "4 scostamento = budget - totale = 500"
VAR=$(cost | jq1 "print(json.load(sys.stdin)['variance'])")
[ "$VAR" = "500.0" ] && ok || no "variance=$VAR"

say "5 elimina un costo -> materiali 300, scostamento 700"
KID=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(next(k['id'] for k in json.load(sys.stdin)['costi'] if k['description']=='Nolo gru'))")
curl -s $H -X DELETE $BASE/api/commessa-costi/$KID -o /dev/null
VAR=$(cost | jq1 "print(json.load(sys.stdin)['variance'])")
[ "$VAR" = "700.0" ] && ok || no "variance=$VAR"

say "6 totale costo include materiali (manodopera 0 senza presenze chiuse)"
TC=$(cost | jq1 "print(json.load(sys.stdin)['total_cost'])")
[ "$TC" = "300.0" ] && ok || no "total=$TC"

say "7 modulo cantieri OFF -> 403 su aggiunta costo"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/commesse/$CID/costi -d '{"description":"x","amount":1}')
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
