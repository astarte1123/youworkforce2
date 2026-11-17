#!/usr/bin/env bash
# R3: allegati di commessa (upload bytea, download, delete).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== R3 — allegati commessa =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
CODE="C-$(python -c 'import random;print(random.randint(100000,999999))')"
CID=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H content-type:application/json -X POST $BASE/api/commesse -d "{\"code\":\"$CODE\",\"name\":\"Att test\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
printf 'contenuto documento di prova' > "$J/doc.txt"

say "1 upload allegato"
AIDT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: text/plain' --data-binary @"$J/doc.txt" "$BASE/api/commesse/$CID/attachments?kind=doc&filename=prova.txt&entity_kind=fattura" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$AIDT" ] && ok || no "no id"

say "2 scheda commessa elenca l'allegato"
N=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(len(json.load(sys.stdin).get('attachments',[])))")
[ "$N" = "1" ] && ok || no "att=$N"

say "3 download restituisce il contenuto"
BODY=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commessa-attachments/$AIDT)
[ "$BODY" = "contenuto documento di prova" ] && ok || no "body=$BODY"

say "4 elimina allegato"
curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -X DELETE $BASE/api/commessa-attachments/$AIDT -o /dev/null
N=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/commesse/$CID | jq1 "print(len(json.load(sys.stdin).get('attachments',[])))")
[ "$N" = "0" ] && ok || no "att=$N"

say "5 modulo cantieri OFF -> 403 upload"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: text/plain' --data-binary @"$J/doc.txt" "$BASE/api/commesse/$CID/attachments?kind=doc&filename=x.txt")
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"cantieri","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
