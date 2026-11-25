#!/usr/bin/env bash
# Cantieri C4 (giornale) + C5 (fatturazione da SAL).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Cantieri C4+C5 — giornale + fatture =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"
CID=$(curl -s $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"GF test\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
get(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID; }

say "1 aggiungi annotazione giornale"
curl -s $H -X POST $BASE/api/commesse/$CID/diario -d '{"body":"Getto fondazioni","weather":"sereno","present_count":5}' -o /dev/null
ND=$(get | jq1 "print(len(json.load(sys.stdin).get('diario',[])))")
[ "$ND" = "1" ] && ok || no "diario=$ND"

say "2 elimina annotazione"
GID=$(get | jq1 "print(json.load(sys.stdin)['diario'][0]['id'])")
curl -s $H -X DELETE $BASE/api/commessa-diario/$GID -o /dev/null
ND=$(get | jq1 "print(len(json.load(sys.stdin).get('diario',[])))")
[ "$ND" = "0" ] && ok || no "diario=$ND"

say "3 registra SAL 50% importo 5000"
curl -s $H -X POST $BASE/api/commesse/$CID/sal -d '{"pct":50,"amount":5000}' -o /dev/null
SID=$(get | jq1 "print(json.load(sys.stdin)['sal'][0]['id'])")
[ -n "$SID" ] && ok || no "no sal"

say "4 emetti fattura da SAL (5000)"
curl -s $H -X POST $BASE/api/commesse/$CID/fatture -d "{\"number\":\"2026/$$-$RANDOM\",\"amount\":5000,\"sal_id\":\"$SID\"}" -o /dev/null
FT=$(get | jq1 "print(len(json.load(sys.stdin).get('fatture',[])))")
[ "$FT" = "1" ] && ok || no "fatture=$FT"

say "5 fattura collegata al SAL + importo"
OKF=$(get | jq1 "f=json.load(sys.stdin)['fatture'][0];print(f['sal_id']=='$SID' and f['amount']==5000.0)")
[ "$OKF" = "True" ] && ok || no "$OKF"

say "6 segna pagata"
FID=$(get | jq1 "print(json.load(sys.stdin)['fatture'][0]['id'])")
curl -s $H -X POST $BASE/api/commessa-fatture/$FID -d '{"status":"pagata"}' -o /dev/null
STt=$(get | jq1 "print(json.load(sys.stdin)['fatture'][0]['status'])")
[ "$STt" = "pagata" ] && ok || no "status=$STt"

say "7 stato fattura non valido -> 400"
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/commessa-fatture/$FID -d '{"status":"boh"}')
[ "$c" = "400" ] && ok || no "got $c"

say "8 modulo cantieri OFF -> 403 (giornale)"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/commesse/$CID/diario -d '{"body":"x"}')
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
