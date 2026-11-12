#!/usr/bin/env bash
# F3 — Policy di escalation multi-step per gruppo (parità GoAlert) + monthly rotation.
#  - CRUD policy (replace-all step: canale+attesa, reorder lato client, repeat)
#  - il runner nativo USA la policy custom (step0 distintivo = voice vs default sms)
#  - reset a policy vuota => torna alla ladder derivata (default)
# Tenant: globex (native). Stato ripristinato a fine run.
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

GID=$(api "$BASE/api/groups" | python -c "import sys,json;g=[x for x in json.load(sys.stdin)['groups'] if '$SLUG' in str(x['members'])];print(g[0]['id'] if g else '')")
[ -n "$GID" ] && ok "gruppo globex trovato" || no "nessun gruppo" "-"
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$GID','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID',escalation_engine='native'" >/dev/null
sql "UPDATE app.users u SET phone='+390000000009' FROM app.alarm_group_members m WHERE m.group_id='$GID' AND m.user_id=u.id AND (u.phone IS NULL OR u.phone='')" >/dev/null

# ---- CRUD policy multi-step ----
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[{"delay_min":0,"channel":"voice"},{"delay_min":3,"channel":"sms"},{"delay_min":5,"channel":"slack"}],"repeat":2}' >/dev/null
N=$(api "$BASE/api/groups/$GID/policy" | python -c "import sys,json;d=json.load(sys.stdin);print(len(d['steps']))")
[ "$N" = 3 ] && ok "policy 3 step salvata e riletta" || no "policy non riletta" "$N"
REP=$(api "$BASE/api/groups/$GID/policy" | python -c "import sys,json;print(json.load(sys.stdin)['repeat'])")
[ "$REP" = 2 ] && ok "repeat=2 persistito" || no "repeat errato" "$REP"
CH0=$(api "$BASE/api/groups/$GID/policy" | python -c "import sys,json;print(json.load(sys.stdin)['steps'][0]['channel'])")
[ "$CH0" = voice ] && ok "step0 canale=voice" || no "step0 errato" "$CH0"

# ---- il runner USA la policy custom (step0=voice, default sarebbe sms) ----
sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'f3pol-'||gen_random_uuid(),now()" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null
LCH=$(sql "SELECT channel FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' AND step=0 ORDER BY el.id DESC LIMIT 1")
[ "$LCH" = voice ] && ok "runner usa policy custom (step0=voice)" || no "runner non usa policy custom" "$LCH"

# ---- validazione ----
BAD=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[{"delay_min":0,"channel":"fax"}]}')
[ "$BAD" = 400 ] && ok "canale non valido rifiutato (400)" || no "validazione canale assente" "$BAD"

# ---- reset a vuoto => torna alla ladder derivata (default) ----
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[],"repeat":0}' >/dev/null
sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'f3def-'||gen_random_uuid(),now()" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null
DCH=$(sql "SELECT channel FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' AND step=0 ORDER BY el.id DESC LIMIT 1")
[ "$DCH" = sms ] && ok "policy vuota => ladder derivata (step0=sms)" || no "fallback default errato" "$DCH"

# ---- monthly rotation accettata ----
MC=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/oncall" -d '{"mode":"turni","oncall_type":"monthly","shift":1}')
[ "$MC" = 200 ] && ok "rotation monthly accettata" || no "monthly rifiutata" "$MC"
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/oncall" -d '{"mode":"tutti","oncall_type":"weekly","shift":1}' >/dev/null

# ---- teardown ----
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e policy' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'f3%'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
