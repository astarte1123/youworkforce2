#!/usr/bin/env bash
# Fase 4 Tappa 3 - gruppi di persone come CONSUMATORI in Campo. Prova: assegnazione di ordini e giri
# a un GRUPPO condiviso + ACL per gruppo (i membri vedono/eseguono, i non-membri no; NULL = aperto come
# prima). Endpoint: /orders (create+assign-group+list+arrive), /patrol-routes (create+list+rounds).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1"|head -1|tr -d '[:space:]'; }
jid(){ python -c "import sys,json;print(json.load(sys.stdin).get('$1',''))" 2>/dev/null; }
TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
for m in interventi giri; do sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','$m','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null; done
R=$RANDOM; HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
T1M="t1-$R@globex.loneworker.eu"; T2M="t2-$R@globex.loneworker.eu"
T1=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$T1M','$HASH','lavoratore') RETURNING id")
T2=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$T2M','$HASH','lavoratore') RETURNING id")
login(){ local em=$1 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADMIN"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
J1=$(login "$T1M"); C1=$(awk '$6=="yw_csrf"{print $7}' "$J1"|tail -1)
J2=$(login "$T2M"); C2=$(awk '$6=="yw_csrf"{print $7}' "$J2"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
t1(){ curl -s -b "$J1" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $C1" "$@"; }
t2(){ curl -s -b "$J2" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $C2" "$@"; }
code(){ "$@" -o /dev/null -w '%{http_code}'; }

# gruppo condiviso G con SOLO T1 membro
GID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/user-groups" -d '{"name":"Squadra Nord '"$R"'","kind":"squadra"}' | jid id)
adm -o /dev/null -X POST "$BASE/api/user-groups/$GID/members" -H 'content-type: application/json' -d "{\"user_id\":\"$T1\"}"

# --- ORDINI ---
# 1) crea ordine assegnato al gruppo G
OID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/orders" -d '{"title":"Riparazione cancello '"$R"'","assigned_group":"'"$GID"'"}' | jid id)
ST=$(one "SELECT state FROM app.work_orders WHERE id='$OID'"); AG=$(one "SELECT (assigned_group='$GID') FROM app.work_orders WHERE id='$OID'")
[ "$ST" = assegnato ] && [ "$AG" = t ] && ok "ordine assegnato a gruppo -> state=assegnato" || no "order group create" "state=$ST ag=$AG"
# 2) T1 (membro) vede l'ordine
t1 "$BASE/api/orders" | python -c "import sys,json;print(any(o['id']=='$OID' for o in json.load(sys.stdin)))" | grep -q True && ok "T1 (membro) vede l'ordine di gruppo" || no "t1 list" "-"
# 3) T2 (non membro) NON vede l'ordine
t2 "$BASE/api/orders" | python -c "import sys,json;print(any(o['id']=='$OID' for o in json.load(sys.stdin)))" | grep -q False && ok "T2 (non membro) NON vede l'ordine" || no "t2 list" "-"
# 4) ACL esecuzione: T1 puo' arrivare (200), T2 no (403)
[ "$(code t1 -X POST "$BASE/api/orders/$OID/arrive")" = 200 ] && ok "T1 puo' eseguire (arrive 200)" || no "t1 arrive" "$(code t1 -X POST "$BASE/api/orders/$OID/arrive")"
[ "$(code t2 -X POST "$BASE/api/orders/$OID/arrive")" = 403 ] && ok "T2 non puo' eseguire (arrive 403)" || no "t2 arrive" "-"
# 5) assign-group su bozza -> assegnato; 6) non-planner 403
OID2=$(adm -H 'content-type: application/json' -X POST "$BASE/api/orders" -d '{"title":"Bozza '"$R"'"}' | jid id)
V=$(one "SELECT version FROM app.work_orders WHERE id='$OID2'")
adm -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/orders/$OID2/assign-group" -d '{"version":'"$V"',"group_id":"'"$GID"'"}' | grep -q 200 && ok "assign-group bozza -> 200" || no "assign-group" "-"
[ "$(one "SELECT state FROM app.work_orders WHERE id='$OID2'")" = assegnato ] && ok "assign-group -> state assegnato" || no "assign-group state" "-"
V2=$(one "SELECT version FROM app.work_orders WHERE id='$OID2'")
[ "$(code t1 -H 'content-type: application/json' -X POST "$BASE/api/orders/$OID2/assign-group" -d '{"version":'"$V2"',"group_id":"'"$GID"'"}')" = 403 ] && ok "assign-group non-planner -> 403" || no "assign-group acl" "-"

# --- GIRI/RONDE ---
# 7) giro assegnato al gruppo G + 8) visibilita'
RID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes" -d '{"name":"Giro Nord '"$R"'","assigned_group":"'"$GID"'"}' | jid id)
t1 "$BASE/api/patrol-routes" | python -c "import sys,json;print(any(r['id']=='$RID' for r in json.load(sys.stdin)))" | grep -q True && ok "T1 vede il giro di gruppo" || no "t1 route" "-"
t2 "$BASE/api/patrol-routes" | python -c "import sys,json;print(any(r['id']=='$RID' for r in json.load(sys.stdin)))" | grep -q False && ok "T2 NON vede il giro di gruppo" || no "t2 route" "-"
# 9) start_round: T2 403, T1 200
[ "$(code t2 -X POST "$BASE/api/patrol-routes/$RID/rounds")" = 403 ] && ok "T2 non avvia ronda su giro di gruppo (403)" || no "t2 round" "$(code t2 -X POST "$BASE/api/patrol-routes/$RID/rounds")"
[ "$(code t1 -X POST "$BASE/api/patrol-routes/$RID/rounds")" = 200 ] && ok "T1 avvia ronda su giro di gruppo (200)" || no "t1 round" "-"
# 10) giro SENZA gruppo (NULL) -> entrambi vedono + entrambi avviano (default invariato)
RID0=$(adm -H 'content-type: application/json' -X POST "$BASE/api/patrol-routes" -d '{"name":"Giro aperto '"$R"'"}' | jid id)
t2 "$BASE/api/patrol-routes" | python -c "import sys,json;print(any(r['id']=='$RID0' for r in json.load(sys.stdin)))" | grep -q True && [ "$(code t2 -X POST "$BASE/api/patrol-routes/$RID0/rounds")" = 200 ] && ok "giro senza gruppo: aperto a tutti (default preservato)" || no "open route" "-"

# teardown
sql "DELETE FROM app.patrol_routes WHERE id IN ('${RID:-0}','${RID0:-0}')" >/dev/null 2>&1 || true
sql "DELETE FROM app.work_orders WHERE id IN ('${OID:-0}','${OID2:-0}')" >/dev/null 2>&1 || true
sql "DELETE FROM app.user_groups WHERE id='${GID:-0}'" >/dev/null 2>&1 || true
sql "DELETE FROM app.users WHERE id IN ('$T1','$T2')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
