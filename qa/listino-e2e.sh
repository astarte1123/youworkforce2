#!/usr/bin/env bash
# Interventi A2: listino prezzi + valorizzazione della visita (righe + totale).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Interventi A2 — listino + valorizzazione =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
NM="Manodopera-$(python -c 'import random;print(random.randint(100000,999999))')"

say "1 crea voce di listino (50/ora)"
PI=$(curl -s $H -X POST $BASE/api/price-items -d "{\"name\":\"$NM\",\"category\":\"manodopera\",\"unit\":\"ora\",\"unit_price\":50}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$PI" ] && ok || no "no id"

say "2 lista listino espone prezzo"
LP=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/price-items)
echo "$LP" | grep -q "\"name\":\"$NM\"" && ok || no "$LP"

say "3 aggiorna prezzo a 60"
curl -s $H -X POST $BASE/api/price-items/$PI -d "{\"name\":\"$NM\",\"unit_price\":60}" -o /dev/null
UP=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/price-items | jq1 "import sys,json;print(next(p['unit_price'] for p in json.load(sys.stdin) if p['id']=='$PI'))")
[ "$UP" = "60.0" ] && ok || no "price=$UP"

OID=$(curl -s $H -X POST $BASE/api/orders -d '{"title":"Riparazione caldaia"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
say "4 valorizza: 2 ore dal listino -> 120"
curl -s $H -X POST $BASE/api/orders/$OID/lines -d "{\"description\":\"Intervento\",\"item_id\":\"$PI\",\"category\":\"manodopera\",\"unit\":\"ora\",\"qty\":2,\"unit_price\":60}" -o /dev/null
TOT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders/$OID/lines | jq1 "print(json.load(sys.stdin)['total'])")
[ "$TOT" = "120.0" ] && ok || no "total=$TOT"

say "5 riga libera (materiale 15.50) -> totale 135.5"
curl -s $H -X POST $BASE/api/orders/$OID/lines -d '{"description":"Guarnizione","category":"materiale","unit":"pezzo","qty":1,"unit_price":15.5}' -o /dev/null
TOT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders/$OID/lines | jq1 "print(json.load(sys.stdin)['total'])")
[ "$TOT" = "135.5" ] && ok || no "total=$TOT"

say "6 elimina una riga -> totale 15.5"
LID=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders/$OID/lines | jq1 "print(next(l['id'] for l in json.load(sys.stdin)['lines'] if l['description']=='Intervento'))")
curl -s $H -X DELETE $BASE/api/order-lines/$LID -o /dev/null
TOT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders/$OID/lines | jq1 "print(json.load(sys.stdin)['total'])")
[ "$TOT" = "15.5" ] && ok || no "total=$TOT"

say "7 modulo interventi OFF -> 403 sul listino"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/price-items)
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
