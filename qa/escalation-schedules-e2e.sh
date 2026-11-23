#!/usr/bin/env bash
# Parità Schedules YWAlert — G-A assegnazioni multiple (coperture con finestra) + G-B vista Shifts.
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
# gruppo base 'tutti' con M1; assegnazione extra con M2
R=$RANDOM
# ISOLAMENTO: utenti dedicati con telefoni UNICI (range 3955), mai admin/LIMIT 1
G=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"Sched-$R\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
M1=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'asg1-$R@globex.loneworker.eu','x','operatore','+393955000001' FROM t RETURNING id" | head -1)
M2=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'asg2-$R@globex.loneworker.eu','x','operatore','+393955000002' FROM t RETURNING id" | head -1)
EM2=$(sql "SELECT email FROM app.users WHERE id='$M2'")
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/members" -d "{\"user_id\":\"$M1\",\"position\":1}" >/dev/null
# tz=UTC così la finestra giorni combacia con ISODOW (altrimenti il default Europe/Rome sposta il giorno)
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/oncall" -d '{"mode":"tutti","coverage_mode":"24_7","tz":"UTC"}' >/dev/null
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id) SELECT $TID,'$G' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$G'" >/dev/null

# --- G-A: assegnazione 24h con M2 -> on-call UNISCE M2 ---
AID=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/assignments" -d '{"label":"Extra","cov_days":"1111111"}' | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
[ -n "$AID" ] && ok "assegnazione creata" || no "assegnazione non creata" "-"
api -H 'content-type: application/json' -X POST "$BASE/api/assignments/$AID/members" -d "{\"user_id\":\"$M2\"}" >/dev/null
ONC=$(api "$BASE/api/oncall-now-native" | python -c "import sys,json;d=json.load(sys.stdin);print(1 if any('$EM2' in (x.get('oncall') or []) for x in d['groups']) else 0)")
[ "$ONC" = 1 ] && ok "G-A: assegnazione 24h aggiunge il membro all'on-call" || no "G-A unione ko" "$ONC"
# consegna: M2 riceve (oltre a M1)
sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null
# forced_group_id: pinna al gruppo di test (immune a routing/gruppi residui di altre suite)
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at,forced_group_id) SELECT $TID,'sos','nuovo',1,'asg-'||gen_random_uuid(),now(),'$G'" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null
GOT2=$(sql "SELECT count(*) FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' AND el.to_value='+393955000002'")
[ "$GOT2" -ge 1 ] && ok "G-A: consegna include il membro dell'assegnazione" || no "G-A consegna ko" "$GOT2"

# --- G-A: finestra che ESCLUDE oggi -> M2 non in on-call ---
TODAY=$(sql "SELECT EXTRACT(ISODOW FROM now())::int")   # 1=lun..7=dom
# cov_days con solo oggi a 0
MASK=$(python -c "d=$TODAY-1;print(''.join('0' if i==d else '1' for i in range(7)))")
api -H 'content-type: application/json' -X POST "$BASE/api/assignments/$AID" -X DELETE >/dev/null 2>&1 || true
api -X DELETE "$BASE/api/assignments/$AID" >/dev/null
AID2=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/assignments" -d "{\"label\":\"NonOggi\",\"cov_days\":\"$MASK\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
api -H 'content-type: application/json' -X POST "$BASE/api/assignments/$AID2/members" -d "{\"user_id\":\"$M2\"}" >/dev/null
ONC2=$(api "$BASE/api/oncall-now-native" | python -c "import sys,json;d=json.load(sys.stdin);print(1 if any('$EM2' in (x.get('oncall') or []) for x in d['groups']) else 0)")
[ "$ONC2" = 0 ] && ok "G-A: finestra che esclude oggi -> membro NON on-call" || no "G-A finestra ko" "$ONC2"

# --- G-B: vista Shifts ---
SHN=$(api "$BASE/api/groups/$G/shifts?days=7" | python -c "import sys,json;print(len(json.load(sys.stdin)['shifts']))")
[ "$SHN" -ge 1 ] && ok "G-B: shifts calcolati ($SHN segmenti)" || no "G-B shifts ko" "$SHN"

# teardown COMPLETO: nessun residuo
api -X DELETE "$BASE/api/groups/$G" >/dev/null 2>&1 || true
sql "DELETE FROM app.alarm_group_members WHERE group_id='$G'" >/dev/null
sql "DELETE FROM app.esc_rotation_state WHERE group_id='$G'" >/dev/null
sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "DELETE FROM app.tenant_oncall_cfg oc USING app.tenants t WHERE t.id=oc.tenant_id AND t.slug='$SLUG' AND oc.default_group_id='$G'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e sched' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'asg-%'" >/dev/null
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND u.email LIKE 'asg1-%@globex%'" >/dev/null
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND u.email LIKE 'asg2-%@globex%'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
