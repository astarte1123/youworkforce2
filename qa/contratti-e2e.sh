#!/usr/bin/env bash
# Interventi A4: contratti/SLA + piani di manutenzione ricorrente (generazione + avanzamento).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Interventi A4 — contratti + manutenzione =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
NM="Contratto-$(python -c 'import random;print(random.randint(100000,999999))')"

say "1 crea contratto (SLA 8h)"
CT=$(curl -s $H -X POST $BASE/api/contracts -d "{\"name\":\"$NM\",\"sla_hours\":8,\"hours_bundle\":40}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$CT" ] && ok || no "no id"

say "2 lista contratti mostra SLA"
echo "$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/contracts)" | grep -q "\"name\":\"$NM\"" && ok || no "not listed"

say "3 crea piano manutenzione (ogni 30 gg, dal 2040-01-01)"
PL=$(curl -s $H -X POST $BASE/api/maintenance-plans -d '{"title":"Caldaia","every_days":30,"next_date":"2040-01-01"}' | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$PL" ] && ok || no "no plan"

say "3b modifica piano (titolo + ogni 45 gg + prossima data)"
curl -s $H -X POST $BASE/api/maintenance-plans/$PL -d '{"title":"Caldaia rev","every_days":45,"next_date":"2040-01-01"}' -o /dev/null
R=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/maintenance-plans | jq1 "p=next(p for p in json.load(sys.stdin) if p['id']=='$PL');print(p['title'],p['every_days'])")
[ "$R" = "Caldaia rev 45" ] && ok || no "$R"

say "4 genera intervento -> prossima data 2040-02-15 (dopo rev a 45gg)"
ND=$(curl -s $H -X POST $BASE/api/maintenance-plans/$PL/generate -d '{}' | jq1 "print(json.load(sys.stdin).get('next_date'))")
[ "$ND" = "2040-02-15" ] && ok || no "next=$ND"

say "5 l'intervento generato esiste (schedulato 2040-01-01)"
GOT=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders | jq1 "print(any((o.get('scheduled_start') or '').startswith('2040-01-01') and 'manutenzione' in o['title'] for o in json.load(sys.stdin)))")
[ "$GOT" = "True" ] && ok || no "order non trovato"

say "6 modulo interventi OFF -> 403"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/contracts)
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
