#!/usr/bin/env bash
# Fase 4 Tappa 2 - split di `admin` nelle capability per-area (*.manage). Prova che i ~149 endpoint
# prima `admin_only` ora rispondono alle capability d'area: default-preserving (admin ovunque 200,
# non-admin ovunque 403) + delega mirata (un ruolo custom/override con UNA area sblocca SOLO quell'area).
# Endpoint campione (uno per area): iot.manage=/connectors, users.manage=/user-groups,
# devices.admin=/ingest-tokens, escalation.manage=/safety/profiles, audit.view=/audit-log,
# gdpr.manage=/gdpr/retention.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1"|head -1|tr -d '[:space:]'; }
TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
sql "DELETE FROM app.role_capabilities WHERE tenant_id='$TID'" >/dev/null
R=$RANDOM; HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
OPM="op-sp-$R@globex.loneworker.eu"
OPU=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$OPM','$HASH','operatore') RETURNING id")
login(){ local em=$1 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADMIN"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
relog(){ OJ=$(login "$OPM"); OCS=$(awk '$6=="yw_csrf"{print $7}' "$OJ"|tail -1); }
relog
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" -o /dev/null -w '%{http_code}' "$@"; }
op(){  curl -s -b "$OJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $OCS" -o /dev/null -w '%{http_code}' "$@"; }
IOT="$BASE/api/connectors"; USERS="$BASE/api/user-groups"; DEV="$BASE/api/ingest-tokens"
ESC="$BASE/api/safety/profiles"; AUD="$BASE/api/audit-log"; GDPR="$BASE/api/gdpr/retention"

# 1) default-preserving: admin 200 ovunque
A_OK=1; for u in "$IOT" "$USERS" "$DEV" "$ESC" "$AUD" "$GDPR"; do [ "$(adm "$u")" = 200 ] || A_OK=0; done
[ "$A_OK" = 1 ] && ok "admin -> 200 su tutte le aree (default-preserving)" || no "admin areas" "-"
# 2) default-preserving: operatore 403 ovunque (prima bloccato da admin_only)
O_BLOCK=1; for u in "$IOT" "$USERS" "$DEV" "$ESC" "$AUD" "$GDPR"; do [ "$(op "$u")" = 403 ] || O_BLOCK=0; done
[ "$O_BLOCK" = 1 ] && ok "operatore -> 403 su tutte le aree (default invariato)" || no "oper baseline" "iot=$(op "$IOT") users=$(op "$USERS") dev=$(op "$DEV") esc=$(op "$ESC") aud=$(op "$AUD") gdpr=$(op "$GDPR")"

# 3) DELEGA via ruolo custom: {iot.manage} -> sblocca SOLO iot
CRID=$(curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" -H 'content-type: application/json' -X POST "$BASE/api/custom-roles" -d '{"name":"IoT mgr '"$R"'","capabilities":["iot.manage"]}' | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" -H 'content-type: application/json' -X POST "$BASE/api/custom-roles/$CRID/users" -d "{\"user_id\":\"$OPU\"}" >/dev/null
relog
[ "$(op "$IOT")" = 200 ] && ok "custom role iot.manage -> /connectors 200" || no "deleg iot" "$(op "$IOT")"
[ "$(op "$USERS")" = 403 ] && [ "$(op "$DEV")" = 403 ] && [ "$(op "$ESC")" = 403 ] && ok "iot.manage NON sblocca altre aree (isolamento)" || no "iot isolation" "users=$(op "$USERS") dev=$(op "$DEV") esc=$(op "$ESC")"

# 4) DELEGA via override ruolo: concedi escalation.manage al ruolo operatore -> sblocca SOLO escalation
curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" -H 'content-type: application/json' -X POST "$BASE/api/capabilities" -d '{"role":"operatore","capability":"escalation.manage","granted":true}' >/dev/null
relog
[ "$(op "$ESC")" = 200 ] && ok "override escalation.manage -> /safety/profiles 200" || no "deleg esc" "$(op "$ESC")"
[ "$(op "$AUD")" = 403 ] && ok "escalation.manage NON sblocca audit" || no "esc isolation" "aud=$(op "$AUD")"

# 5) RESET override -> escalation di nuovo 403; il ruolo custom iot resta
curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" -X DELETE "$BASE/api/capabilities" >/dev/null
relog
[ "$(op "$ESC")" = 403 ] && [ "$(op "$IOT")" = 200 ] && ok "dopo reset: escalation 403, iot (custom role) ancora 200" || no "post-reset" "esc=$(op "$ESC") iot=$(op "$IOT")"

# teardown
sql "DELETE FROM app.custom_roles WHERE id='${CRID:-00000000-0000-0000-0000-000000000000}'" >/dev/null 2>&1 || true
sql "DELETE FROM app.role_capabilities WHERE tenant_id='$TID'" >/dev/null
sql "DELETE FROM app.users WHERE id='$OPU'" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
