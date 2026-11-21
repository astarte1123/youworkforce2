#!/usr/bin/env bash
# Superadmin: monitoring sistema (/system: hardware/DB/worker/code) + storico alert watchdog.
BASE="${BASE:-http://localhost:8080}"
J=$(mktemp -d); PASS=0; FAIL=0
say(){ printf '%-62s' "$1"; }
ok(){ echo "PASS"; PASS=$((PASS+1)); }
no(){ echo "FAIL  ($1)"; FAIL=$((FAIL+1)); }
csrfh(){ awk '$6=="yw_csrf"{print $7}' "$1" 2>/dev/null | tail -1; }
jq1(){ python -c "import sys,json;$1" 2>/dev/null; }
code(){ curl -s -o /dev/null -w '%{http_code}' "$@"; }

echo "== Superadmin — monitoring sistema =="
curl -s -c "$J/sa" -X POST $BASE/api/superadmin/login -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}' -o /dev/null
S="-b $J/sa"

say "1 /system: struttura completa (status/server/db/worker/checks)"
D=$(curl -s $S $BASE/api/superadmin/system)
echo "$D" | jq1 "d=json.load(sys.stdin);
import sys as _s
ok = d.get('status') in ('ok','warn','bad') and 'cpus' in d.get('server',{}) and 'reachable' in d.get('db',{}) and isinstance(d.get('checks'),list) and len(d['checks'])>=4 and 'heartbeat_age_secs' in d.get('worker',{})
print('OK' if ok else 'NO')" | grep -q OK && ok || no "struttura /system"

say "2 /system: metriche hardware presenti (mem_total>0, disk_total>0)"
echo "$D" | jq1 "d=json.load(sys.stdin)['server'];print(d.get('mem_total',0)>0 and d.get('disk_total',0)>0)" | grep -q True && ok || no "hw metrics"

say "3 /system: DB raggiungibile + dimensione > 0"
echo "$D" | jq1 "d=json.load(sys.stdin)['db'];print(d.get('reachable') and (d.get('size_bytes') or 0)>0)" | grep -q True && ok || no "db"

say "4 /system: worker heartbeat recente (< 120s)"
echo "$D" | jq1 "w=json.load(sys.stdin)['worker'];a=w.get('heartbeat_age_secs',-1);print(0<=a<120)" | grep -q True && ok || no "worker heartbeat"

say "5 /system/alerts: ritorna un array"
echo "$(curl -s $S $BASE/api/superadmin/system/alerts)" | jq1 "print(isinstance(json.load(sys.stdin),list))" | grep -q True && ok || no "alerts non-array"

say "6 settings: POST soglia mem=88 -> effetto su /system/thresholds"
SC=$(csrfh "$J/sa")
curl -s $S -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/system/settings -d '{"mem_max_pct":88,"alert_email":"ops@test.eu"}' -o /dev/null
echo "$(curl -s $S $BASE/api/superadmin/system)" | jq1 "print(json.load(sys.stdin)['thresholds']['mem_max_pct'])" | grep -q "88" && ok || no "soglia non applicata"

say "7 settings: soglia fuori range -> 400"
c=$(code $S -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/system/settings -d '{"disk_max_pct":150}')
[ "$c" = "400" ] && ok || no "got $c"

say "8 settings: reset (null) -> torna al default"
curl -s $S -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST $BASE/api/superadmin/system/settings -d '{}' -o /dev/null
echo "$(curl -s $S $BASE/api/superadmin/system)" | jq1 "print(json.load(sys.stdin)['thresholds']['mem_max_pct'])" | grep -q "90" && ok || no "reset non riuscito"

say "9 gating: tenant admin NON vede /system -> 403"
curl -s -c "$J/t" -X POST $BASE/api/login -H 'content-type: application/json' -H 'x-tenant-slug: acme' -d '{"email":"admin@acme.loneworker.eu","password":"demo"}' -o /dev/null
c=$(code -b "$J/t" -H 'x-tenant-slug: acme' $BASE/api/superadmin/system)
{ [ "$c" = "403" ] || [ "$c" = "401" ]; } && ok || no "got $c"

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
