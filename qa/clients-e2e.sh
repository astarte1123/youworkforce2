#!/usr/bin/env bash
# Assistenza A1: anagrafica Cliente + referenti + legame ordine↔cliente.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Assistenza A1 — Clienti =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
NM="Cliente-$(python -c 'import random;print(random.randint(100000,999999))')"

say "1 crea cliente"
CID=$(curl -s $H -X POST $BASE/api/clients -d "{\"name\":\"$NM\",\"vat\":\"IT123\",\"phone\":\"0200\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$CID" ] && ok || no "no id"

say "2 aggiungi referente"
curl -s $H -X POST $BASE/api/clients/$CID/contacts -d '{"name":"Mario Rossi","role":"RSPP","phone":"333"}' -o /dev/null
NK=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/clients/$CID | jq1 "print(len(json.load(sys.stdin).get('contacts',[])))")
[ "$NK" = "1" ] && ok || no "contacts=$NK"

say "3 lista clienti mostra orders/contacts count"
LC=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/clients)
echo "$LC" | grep -q "\"name\":\"$NM\"" && ok || no "$LC"

say "4 crea intervento legato al cliente"
OID=$(curl -s $H -X POST $BASE/api/orders -d "{\"title\":\"Riparazione\",\"client_id\":\"$CID\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$OID" ] && ok || no "no order"

say "5 lista ordini espone client_name"
LO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders)
echo "$LO" | grep -q "\"client_name\":\"$NM\"" && ok || no "no client in order list"

say "6 scheda cliente elenca i suoi interventi"
NO=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/clients/$CID | jq1 "print(len(json.load(sys.stdin).get('orders',[])))")
[ "$NO" -ge 1 ] && ok || no "orders=$NO"

say "7 elimina referente"
KID=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/clients/$CID | jq1 "print(json.load(sys.stdin)['contacts'][0]['id'])")
curl -s $H -X DELETE $BASE/api/contacts/$KID -o /dev/null
NK=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/clients/$CID | jq1 "print(len(json.load(sys.stdin).get('contacts',[])))")
[ "$NK" = "0" ] && ok || no "contacts=$NK"

say "8 modulo interventi OFF -> 403"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/clients)
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
