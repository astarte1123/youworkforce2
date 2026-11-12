#!/usr/bin/env bash
# Parità Rotations GoAlert — R1 Set Active: imposta subito il reperibile di turno.
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
# gruppo a turni dedicato con 2 membri (posizione 1 e 2) con telefoni distinti
G=$(api -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"Turni-SA-$RANDOM\"}" | python -c "import sys,json;print(json.load(sys.stdin)['id'])")
M1=$(sql "SELECT u.id FROM app.users u JOIN app.tenants t ON t.id=u.tenant_id WHERE t.slug='$SLUG' LIMIT 1")
M2=$(sql "WITH t AS (SELECT id FROM app.tenants WHERE slug='$SLUG') INSERT INTO app.users(tenant_id,email,pass_hash,role,phone) SELECT id,'sa2-$RANDOM@globex.loneworker.eu','x','operatore','+390444000002' FROM t RETURNING id" | head -1)
sql "UPDATE app.users SET phone='+390444000001' WHERE id='$M1'" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/members" -d "{\"user_id\":\"$M1\",\"position\":1}" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/members" -d "{\"user_id\":\"$M2\",\"position\":2}" >/dev/null
# modalita' a turni + routing di default su questo gruppo
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/oncall" -d '{"mode":"turni","oncall_type":"weekly","shift":1}' >/dev/null
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$G','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$G',escalation_engine='native'" >/dev/null
sql "DELETE FROM app.esc_rotation_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null

# Set Active su M2 (posizione 1 nell'indice 0-based)
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/rotation/set-active" -d "{\"user_id\":\"$M2\"}" >/dev/null
POS=$(sql "SELECT position||'/'||seeded_from FROM app.esc_rotation_state rs JOIN app.tenants t ON t.id=rs.tenant_id WHERE t.slug='$SLUG' AND rs.group_id='$G'")
[ "$POS" = "1/manual" ] && ok "Set Active imposta posizione=1 seeded_from=manual" || no "set active stato ko" "$POS"

# la consegna va a M2 (il reperibile appena attivato)
sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'sa-'||gen_random_uuid(),now()" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null
TO=$(sql "SELECT to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$TO" = "+390444000002" ] && ok "consegna al reperibile attivato (M2)" || no "consegna non a M2" "$TO"

# Set Active su M1 (indice 0) -> cambia il reperibile corrente
api -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/rotation/set-active" -d "{\"user_id\":\"$M1\"}" >/dev/null
sql "DELETE FROM app.esc_alert_state s USING app.tenants t WHERE t.id=s.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "DELETE FROM app.esc_delivery_log el USING app.tenants t WHERE t.id=el.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'sa2-'||gen_random_uuid(),now()" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' >/dev/null
TO2=$(sql "SELECT to_value FROM app.esc_delivery_log el JOIN app.tenants t ON t.id=el.tenant_id WHERE t.slug='$SLUG' ORDER BY el.id DESC LIMIT 1")
[ "$TO2" = "+390444000001" ] && ok "Set Active cambia il reperibile corrente (M1)" || no "cambio reperibile ko" "$TO2"

# validazione: utente non membro -> 400
BAD=$(api -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/groups/$G/rotation/set-active" -d "{\"user_id\":\"$(python -c 'import uuid;print(uuid.uuid4())')\"}")
[ "$BAD" = 400 ] && ok "Set Active su non-membro = 400" || no "validazione set active ko" "$BAD"

# teardown
api -X DELETE "$BASE/api/groups/$G" >/dev/null 2>&1 || true
sql "DELETE FROM app.users u USING app.tenants t WHERE t.id=u.tenant_id AND t.slug='$SLUG' AND u.email LIKE 'sa2-%'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e setactive' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'sa%-%'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
