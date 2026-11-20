#!/usr/bin/env bash
# Vigilanza V2: ronde pianificate + cruscotto (dovute vs svolte).
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }

echo "== Vigilanza V2 — piani + cruscotto =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
SC=$(csrfh "$J/sa")
AID=$(curl -s -b "$J/sa" $BASE/api/superadmin/tenants | jq1 "print(next((t['id'] for t in json.load(sys.stdin) if t.get('slug')=='acme'),''))")
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"giri","state":"attivo"}' -o /dev/null
curl -s -c "$J/pl" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"pianificatore@acme.loneworker.eu","password":"demo"}' -o /dev/null
CP=$(csrfh "$J/pl")
H="-b $J/pl -H x-tenant-slug:acme -H x-csrf-token:$CP -H content-type:application/json"
RN="RondaV2-$(python -c 'import random;print(random.randint(100000,999999))')"
RID=$(curl -s $H -X POST $BASE/api/patrol-routes -d "{\"name\":\"$RN\"}" | jq1 "print(json.load(sys.stdin).get('id',''))")
curl -s $H -X POST $BASE/api/patrol-routes/$RID/checkpoints -d '{"name":"C1","kind":"qr","code":"X1"}' -o /dev/null

say "1 crea piano (tutti i giorni, 00:00-23:00, ogni 60)"
SID=$(curl -s $H -X POST $BASE/api/patrol-schedules -d "{\"route_id\":\"$RID\",\"days_mask\":\"1111111\",\"from_time\":\"00:00\",\"to_time\":\"23:00\",\"every_minutes\":60}" | jq1 "print(json.load(sys.stdin).get('id',''))")
[ -n "$SID" ] && ok || no "no schedule"

say "2 lista piani contiene il nuovo"
echo "$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-schedules)" | grep -q "$RN" && ok || no "not listed"

say "3 cruscotto: slot dovuti + mancanti>0"
# DETERMINISTICO a qualsiasi ora del giorno: porta il test-clock esattamente alle 12:00:00Z di OGGI
# (stesso giorno di 'now'), così gli slot 00:00..10:00 risultano scaduti e non punzonati -> mancanti.
# Evita il caso 'now appena dopo mezzanotte' (nessuno slot ancora scaduto) del vecchio +2h.
NOW=$(curl -s -X POST $BASE/api/_test/advance -H 'content-type: application/json' -d '{"seconds":0}' | jq1 "print(json.load(sys.stdin)['now'])")
DELTA=$(python -c "import sys,datetime as d; s='$NOW'.replace('Z','+00:00'); n=d.datetime.fromisoformat(s); t=n.replace(hour=12,minute=0,second=0,microsecond=0); print(int((t-n).total_seconds()))")
curl -s -X POST $BASE/api/_test/advance -H 'content-type: application/json' -d "{\"seconds\":$DELTA}" -o /dev/null
D=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-dashboard)
NS=$(echo "$D" | jq1 "print(len(json.load(sys.stdin)['slots']))")
MA=$(echo "$D" | jq1 "print(json.load(sys.stdin)['summary']['mancanti'])")
curl -s -X POST $BASE/api/_test/advance -H 'content-type: application/json' -d "{\"seconds\":$((-DELTA))}" -o /dev/null
[ "${NS:-0}" -ge 1 ] && [ "${MA:-0}" -ge 1 ] && ok || no "slots=$NS mancanti=$MA (now=$NOW delta=$DELTA)"

say "4 giorno escluso dal mask -> nessuno slot di quel giro"
# metti mask tutto 0 tranne un giorno diverso da oggi: ricreo con mask che esclude oggi
curl -s $H -X DELETE $BASE/api/patrol-schedules/$SID -o /dev/null
# weekday in UTC: il server valuta il mask con clock UTC (clock.rs), non ora locale -> allineati anche a cavallo di mezzanotte
TODOW=$(python -c "import datetime as d;print(d.datetime.now(d.timezone.utc).weekday())")
MASK=$(python -c "i=$TODOW;print(''.join('0' if k==i else '1' for k in range(7)))")
SID2=$(curl -s $H -X POST $BASE/api/patrol-schedules -d "{\"route_id\":\"$RID\",\"days_mask\":\"$MASK\",\"from_time\":\"00:00\",\"to_time\":\"23:00\",\"every_minutes\":60}" | jq1 "print(json.load(sys.stdin).get('id',''))")
NS2=$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-dashboard | jq1 "print(len([s for s in json.load(sys.stdin)['slots'] if s['route_name']=='$RN']))")
[ "${NS2:-0}" = "0" ] && ok || no "slots oggi=$NS2 (atteso 0)"

say "5 elimina piano"
curl -s $H -X DELETE $BASE/api/patrol-schedules/$SID2 -o /dev/null
echo "$(curl -s -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-schedules)" | grep -q "$RN" && no "ancora presente" || ok

say "6 modulo giri OFF -> 403 cruscotto"
curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/tenants/$AID/modules -d '{"module_code":"giri","state":"non_abilitato"}' -o /dev/null
c=$(curl -s -o /dev/null -w '%{http_code}' -b "$J/pl" -H 'x-tenant-slug: acme' $BASE/api/patrol-dashboard)
[ "$c" = "403" ] && ok || no "got $c"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
