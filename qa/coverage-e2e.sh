#!/usr/bin/env bash
# W3c — copertura minima: la verifica trova i giorni scoperti e li chiude quando si pianifica.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Copertura minima (W3c) =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"workforce","state":"attivo"}' -o /dev/null

curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
uid(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/users | jq1 "print(next((u['id'] for u in json.load(sys.stdin) if u['email']=='$1'),''))"; }
MARIO=$(uid mario@acme.loneworker.eu); OPER=$(uid operatore@acme.loneworker.eu)
P(){ curl -s -b "$J/pl" -H 'x-tenant-slug: acme' -H "x-csrf-token: $CP" -H 'content-type: application/json' "$@"; }

# pulizia requisiti residui da run precedenti (test idempotente)
for cid in $(P $BASE/api/coverage | jq1 "import json,sys;[print(c['id']) for c in json.load(sys.stdin)]"); do P -X DELETE $BASE/api/coverage/$cid -o /dev/null; done

# requisito: ovunque, tutti i giorni, 08-16, minimo 2
CID=$(P -X POST $BASE/api/coverage -d '{"site_id":null,"days_mask":"1111111","cov_start":"08:00","cov_end":"16:00","min_people":2}' | jq1 "print(json.load(sys.stdin).get('id',''))")
say "1 requisito di copertura creato"
[ -n "$CID" ] && ok || no "no id"

D=$(python -c "import datetime,random;print((datetime.date(2039,1,1)+datetime.timedelta(days=random.randint(0,3000))).isoformat())")
D1=$(python -c "import datetime;print((datetime.date.fromisoformat('$D')+datetime.timedelta(days=1)).isoformat())")
# controlla i gap SOLO del requisito indicato (evita interferenze da altri requisiti)
gaps_on(){ P "$BASE/api/coverage/check?requirement=$1&from=${D}T00:00:00Z&to=${D1}T00:00:00Z" | jq1 "import json,sys;d=json.load(sys.stdin);print(sum(1 for g in d['gaps'] if g['date']=='$D'))"; }

say "2 giorno senza turni -> scoperto (gap present)"
[ "$(gaps_on $CID)" -ge 1 ] && ok || no "no gap"

# pianifica 2 persone che coprono 08-16 quel giorno
P -X POST $BASE/api/shift-plans -d "{\"user_id\":\"$MARIO\",\"starts_at\":\"${D}T08:00:00Z\",\"ends_at\":\"${D}T16:00:00Z\"}" -o /dev/null
P -X POST $BASE/api/shift-plans -d "{\"user_id\":\"$OPER\",\"starts_at\":\"${D}T08:00:00Z\",\"ends_at\":\"${D}T16:00:00Z\"}" -o /dev/null
say "3 due persone coprono la fascia -> giorno non più scoperto"
[ "$(gaps_on $CID)" = "0" ] && ok || no "ancora gap"

say "4 requisito min 3 -> di nuovo scoperto (2<3)"
CID3=$(P -X POST $BASE/api/coverage -d '{"site_id":null,"days_mask":"1111111","cov_start":"08:00","cov_end":"16:00","min_people":3}' | jq1 "print(json.load(sys.stdin).get('id',''))")
[ "$(gaps_on $CID3)" -ge 1 ] && ok || no "min3 non scoperto"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
