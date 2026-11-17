#!/usr/bin/env bash
# R1: anagrafiche più ricche — cliente (PEC/SDI/pagamenti) + commessa legata al cliente + CIG/CUP.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== R1 — anagrafiche ricche =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in interventi cantieri; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
NM="Cli-$(python -c 'import random;print(random.randint(100000,999999))')"

say "1 cliente con PEC/SDI/pagamenti"
CID=$(curl -s $H -X POST $BASE/api/clients -d "{\"name\":\"$NM\",\"pec\":\"a@pec.it\",\"sdi\":\"ABC1234\",\"payment_terms\":\"30 gg\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
GOT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/clients/$CID | jq1 "d=json.load(sys.stdin);print(d.get('pec'),d.get('sdi'),d.get('payment_terms'))")
[ "$GOT" = "a@pec.it ABC1234 30 gg" ] && ok || no "$GOT"

say "2 commessa legata al cliente + CIG/CUP"
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"
COM=$(curl -s $H -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"Appalto\",\"client_id\":\"$CID\",\"cig\":\"CIG123\",\"cup\":\"CUP999\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
GOT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$COM | jq1 "d=json.load(sys.stdin);print(d.get('client_name'),d.get('cig'),d.get('cup'))")
[ "$GOT" = "$NM CIG123 CUP999" ] && ok || no "$GOT"

say "3 lista commesse mostra il nome cliente"
LC=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse)
echo "$LC" | grep -q "\"client_name\":\"$NM\"" && ok || no "no client in list"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
