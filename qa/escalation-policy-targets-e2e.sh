#!/usr/bin/env bash
# Parità Escalation Policies GoAlert — target PER-STEP: uno step può notificare un destinatario
# diverso (on-call del gruppo, un ALTRO gruppo, una persona, un canale Slack).
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
M1=$(sql "SELECT user_id FROM app.alarm_group_members m JOIN app.tenants t ON t.id=m.tenant_id WHERE t.slug='$SLUG' AND m.group_id='$GID' LIMIT 1")
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$GID','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID',escalation_engine='native'" >/dev/null
sql "UPDATE app.users SET phone='+390111000001' WHERE id='$M1'" >/dev/null
# utente U2 (persona target) con phone distinto
U2=$(sql "SELECT u.id FROM app.users u JOIN app.tenants t ON t.id=u.tenant_id WHERE t.slug='$SLUG' AND u.id<>'$M1' LIMIT 1")
[ -z "$U2" ] && U2=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'u2tgt-$RANDOM@globex.loneworker.eu','x','operatore','+390222000002' FROM t RETURNING id" | head -1)
sql "UPDATE app.users SET phone='+390222000002' WHERE id='$U2'" >/dev/null
# gruppo G2 con membro U3 (altro gruppo target)
G2=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"Responsabili-$RANDOM\"}" | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
U3=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'u3resp-$RANDOM@globex.loneworker.eu','x','responsabile','+390333000003' FROM t RETURNING id" | head -1)
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G2/members" -d "{\"user_id\":\"$U3\"}" >/dev/null
clean(){ sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null; sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null; }
mkinc(){ sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'$1',now()" >/dev/null; }
run(){ api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null; }
lastto(){ sql "SELECT channel||'|'||to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1"; }

# A) step target = persona (U2) -> consegna al telefono di U2, NON all'on-call del gruppo
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d "{\"steps\":[{\"channel\":\"sms\",\"delay_min\":0,\"target_kind\":\"user\",\"target_user_id\":\"$U2\"}],\"repeat\":0}" >/dev/null
clean; mkinc "tgt-user-$RANDOM"; run
[ "$(lastto)" = "sms|+390222000002" ] && ok "step target=persona consegna alla persona" || no "target persona ko" "$(lastto)"

# B) step target = altro gruppo (G2) -> consegna al membro di G2
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d "{\"steps\":[{\"channel\":\"sms\",\"delay_min\":0,\"target_kind\":\"group\",\"target_group_id\":\"$G2\"}],\"repeat\":0}" >/dev/null
clean; mkinc "tgt-group-$RANDOM"; run
[ "$(lastto)" = "sms|+390333000003" ] && ok "step target=altro gruppo consegna all'on-call di quel gruppo" || no "target gruppo ko" "$(lastto)"

# C) step target = canale Slack -> rung slack sul canale
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[{"channel":"slack","delay_min":0,"target_kind":"slack","target_slack":"C-OPS-123"}],"repeat":0}' >/dev/null
clean; mkinc "tgt-slack-$RANDOM"; run
[ "$(lastto)" = "slack|C-OPS-123" ] && ok "step target=Slack consegna al canale" || no "target slack ko" "$(lastto)"

# D) default (oncall) resta il comportamento: consegna all'on-call del gruppo
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[{"channel":"sms","delay_min":0,"target_kind":"oncall"}],"repeat":0}' >/dev/null
clean; mkinc "tgt-oncall-$RANDOM"; run
[ "$(lastto)" = "sms|+390111000001" ] && ok "step target=on-call consegna al gruppo (default)" || no "target oncall ko" "$(lastto)"

# E) validazione: target group senza group_id -> 400
BAD=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[{"channel":"sms","target_kind":"group"}]}')
[ "$BAD" = 400 ] && ok "validazione: target group senza gruppo = 400" || no "validazione target ko" "$BAD"

# F) get ritorna i target
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d "{\"steps\":[{\"channel\":\"voice\",\"target_kind\":\"user\",\"target_user_id\":\"$U2\"}],\"repeat\":0}" >/dev/null
GK=$(api "$BASE/api/groups/$GID/policy" | python -c "import sys,json;s=json.load(sys.stdin)['steps'];print(s[0]['target_kind'] if s else '')")
[ "$GK" = user ] && ok "get policy ritorna target_kind" || no "get target ko" "$GK"

# teardown
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$GID/policy" -d '{"steps":[],"repeat":0}' >/dev/null
api -X DELETE "$BASE/api/groups/$G2" >/dev/null 2>&1 || true
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND u.email LIKE 'u3resp-%'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e targets' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'tgt-%'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
