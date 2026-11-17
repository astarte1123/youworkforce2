#!/usr/bin/env bash
# Interventi A3: dispatch — suggerimento tecnico (carico + assenze), assegnazione.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Interventi A3 — dispatch =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
for m in interventi workforce; do curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d "{\"module_code\":\"$m\",\"state\":\"attivo\"}" -o /dev/null; done
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
DAY="2040-07-0$((RANDOM%9+1))"
OID=$(curl -s $H -X POST $BASE/api/orders -d "{\"title\":\"Dispatch test\",\"scheduled_start\":\"${DAY}T09:00:00Z\",\"scheduled_end\":\"${DAY}T11:00:00Z\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")

say "1 suggerisci tecnico: candidati + giorno"
R=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders/$OID/suggest-tech)
echo "$R" | grep -q '"candidates"' && echo "$R" | grep -q "\"day\":\"$DAY\"" && ok || no "$R"

say "2 c'è almeno un candidato disponibile"
N=$(echo "$R" | jq1 "print(len([c for c in json.load(sys.stdin)['candidates'] if not c['absent']]))")
[ "${N:-0}" -ge 1 ] && ok || no "disponibili=$N"

say "3 summary presente"
echo "$R" | jq1 "print('ok' if json.load(sys.stdin).get('summary') else '')" | grep -q ok && ok || no "no summary"

# primo candidato disponibile
TOP=$(echo "$R" | jq1 "print(next(c['user_id'] for c in json.load(sys.stdin)['candidates'] if not c['absent']))")
say "4 assenza del top -> risulta assente e in fondo"
curl -s $H -X POST $BASE/api/absences -d "{\"user_id\":\"$TOP\",\"kind\":\"ferie\",\"starts_at\":\"${DAY}T00:00:00Z\",\"ends_at\":\"${DAY}T23:59:00Z\"}" -o /dev/null
R2=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders/$OID/suggest-tech)
AB=$(echo "$R2" | jq1 "print(next(c['absent'] for c in json.load(sys.stdin)['candidates'] if c['user_id']=='$TOP'))")
LAST=$(echo "$R2" | jq1 "print(json.load(sys.stdin)['candidates'][-1]['user_id'])")
[ "$AB" = "True" ] && [ "$LAST" = "$TOP" ] && ok || no "absent=$AB last==top? $([ "$LAST" = "$TOP" ] && echo yes || echo no)"

say "5 assegna un tecnico disponibile"
AVA=$(echo "$R2" | jq1 "print(next(c['user_id'] for c in json.load(sys.stdin)['candidates'] if not c['absent']))")
VER=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders | jq1 "print(next(o['version'] for o in json.load(sys.stdin) if o['id']=='$OID'))")
c=$(curl -s -o /dev/null -w '%{http_code}' $H -X POST $BASE/api/orders/$OID/assign -d "{\"version\":$VER,\"user_id\":\"$AVA\"}")
[ "$c" = "200" ] && ok || no "assign got $c"

say "6 modulo interventi OFF -> 403"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/orders/$OID/suggest-tech)
[ "$c" = "403" ] && ok || no "got $c"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"interventi","state":"attivo"}' -o /dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
