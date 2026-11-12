#!/usr/bin/env bash
# Parità Schedules — residui minori: assegnazione di tipo ROTAZIONE, "Allow gaps", iCal per-gruppo.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="globex"; EMAIL="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
TID="(SELECT id FROM app.tenants WHERE slug='$SLUG')"
G=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"SchedMin-$RANDOM\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/oncall" -d '{"mode":"tutti","coverage_mode":"24_7","tz":"UTC"}' >/dev/null
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$G','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$G',escalation_engine='native'" >/dev/null
UA=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'rotA-$RANDOM@globex.loneworker.eu','x','operatore','+390666000001' FROM t RETURNING id" | head -1)
UB=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'rotB-$RANDOM@globex.loneworker.eu','x','operatore','+390666000002' FROM t RETURNING id" | head -1)

# --- Minor 1: assegnazione a TURNI (rotazione) -> on-call = UN solo membro (non tutti) ---
AID=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/assignments" -d '{"label":"RotA","mode":"turni","oncall_type":"weekly"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
MODE=$(sql "SELECT mode FROM app.esc_assignments WHERE id='$AID'")
[ "$MODE" = turni ] && ok "assegnazione mode=turni salvata" || no "mode turni ko" "$MODE"
api -H 'content-type: application/json' -X POST "$BASE/api/assignments/$AID/members" -d "{\"user_id\":\"$UA\",\"position\":1}" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/assignments/$AID/members" -d "{\"user_id\":\"$UB\",\"position\":2}" >/dev/null
CNT=$(api "$BASE/api/oncall-now-native" | python -c "import sys,json;d=json.load(sys.stdin);g=[x for x in d['groups'] if x['group'].startswith('SchedMin')];print(len([e for e in (g[0]['oncall'] if g else []) if 'rot' in e]))")
[ "$CNT" = 1 ] && ok "assegnazione a turni: UN solo membro on-call (rotazione)" || no "rotazione assegnazione ko" "$CNT"

# --- Minor 2: Allow gaps ---
N0=$(date -u +%Y-%m-%dT%H:%M:%SZ); N2=$(python -c "import datetime;print((datetime.datetime.utcnow()+datetime.timedelta(days=2)).strftime('%Y-%m-%dT%H:%M:%SZ'))")
# blocco 1h su finestra 2 giorni -> buco -> 400 senza allow_gaps
BAD=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/oncall-temp-schedules" -d "{\"group_id\":\"$G\",\"start\":\"$N0\",\"end\":\"$N2\",\"blocks\":[{\"user_id\":\"$UA\",\"hours\":1}]}")
[ "$BAD" = 400 ] && ok "temp schedule con buco rifiutata (400)" || no "buco non rifiutato" "$BAD"
GOOD=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/oncall-temp-schedules" -d "{\"group_id\":\"$G\",\"start\":\"$N0\",\"end\":\"$N2\",\"blocks\":[{\"user_id\":\"$UA\",\"hours\":1}],\"allow_gaps\":true}")
[ "$GOOD" = 200 ] && ok "temp schedule con buco accettata con allow_gaps" || no "allow_gaps ko" "$GOOD"

# --- Minor 3: iCal per-gruppo ---
URL=$(api "$BASE/api/groups/$G/calendar/url" | python -c "import sys,json;print(json.load(sys.stdin).get('url',''))")
[ -n "$URL" ] && ok "URL iCal del gruppo generato" || no "ical url ko" "-"
TOK="${URL##*token=}"
CODE=$(curl -s -o /tmp/gcal.ics -w '%{http_code}' "$BASE/api/group-calendar.ics?token=$TOK")
VEV=$(grep -c "BEGIN:VEVENT" /tmp/gcal.ics 2>/dev/null || echo 0)
{ [ "$CODE" = 200 ] && [ "$VEV" -ge 1 ]; } && ok "feed iCal gruppo servito con eventi ($VEV)" || no "ical feed ko" "code=$CODE vev=$VEV"

# teardown
api -X DELETE "$BASE/api/groups/$G" >/dev/null 2>&1 || true
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND (u.email LIKE 'rotA-%' OR u.email LIKE 'rotB-%')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
