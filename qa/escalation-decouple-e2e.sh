#!/usr/bin/env bash
# F1+F2 — Escalation nativa DISACCOPPIATA e SELF-SUFFICIENT (strada nuova), senza GoAlert.
#  F1) con "Sicurezza lavoratore" (loneworker) SPENTO ma un altro modulo che genera incidenti
#      ATTIVO (thingsboard), la centrale + l'escalation restano visibili/configurabili (no 403).
#  F1) maintenance NATIVO: persistito su tenant_oncall_cfg, sospende le consegne del runner nativo.
#  F2) override/assenze NATIVI (app.esc_overrides), verifica recapiti NATIVA (OTP via SMS/sink),
#      runner nativo che CONSEGNA senza alcuna sessione GoAlert.
# Tenant di prova: globex (isolato), stato ripristinato a fine run.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="globex"; EMAIL="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
code(){ curl -s -o /dev/null -w '%{http_code}' -b "$JAR" -H "x-tenant-slug: $SLUG" "$@"; }

JAR=$(mktemp)
curl -s -o /dev/null -c "$JAR" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$EMAIL\",\"password\":\"demo\"}"
CS=$(awk '$6=="yw_csrf"{print $7}' "$JAR" | tail -1)
api(){ curl -s -b "$JAR" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $CS" "$@"; }
TID="(SELECT id FROM app.tenants WHERE slug='$SLUG')"

# ---- setup scenario: loneworker OFF, thingsboard ON, no GoAlert ----
sql "UPDATE app.tenant_modules tm SET state='non_abilitato' FROM app.tenants t WHERE t.id=tm.tenant_id AND t.slug='$SLUG' AND tm.module_code='loneworker'" >/dev/null
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) SELECT id,'thingsboard','attivo' FROM app.tenants WHERE slug='$SLUG' ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
sql "DELETE FROM app.goalert_services gs USING app.tenants t WHERE t.id=gs.tenant_id AND t.slug='$SLUG'" >/dev/null

# ---- F1: escalation/centrale raggiungibili con loneworker OFF ----
[ "$(code "$BASE/api/incidents")" = 200 ]       && ok "F1 centrale incidenti 200 (loneworker OFF, thingsboard ON)" || no "incidenti non raggiungibili" "$(code "$BASE/api/incidents")"
[ "$(code "$BASE/api/groups")" = 200 ]          && ok "F1 gruppi 200" || no "gruppi 403" "-"
[ "$(code "$BASE/api/oncall-settings")" = 200 ] && ok "F1 on-call settings 200" || no "oncall-settings 403" "-"
[ "$(code "$BASE/api/oncall-contacts")" = 200 ] && ok "F1 recapiti 200" || no "contacts 403" "-"

# default group + membro con telefono (per consegna)
GID=$(api "$BASE/api/groups" | python -c "import sys,json;g=json.load(sys.stdin)['groups'];print(g[0]['id'] if g else '')")
[ -n "$GID" ] && ok "gruppo presente" || no "nessun gruppo" "-"
sql "INSERT INTO app.tenant_oncall_cfg(tenant_id,default_group_id,escalation_engine) SELECT $TID,'$GID','native' ON CONFLICT (tenant_id) DO UPDATE SET default_group_id='$GID',escalation_engine='native'" >/dev/null
UID1=$(sql "SELECT user_id FROM app.alarm_group_members m JOIN app.tenants t ON t.id=m.tenant_id WHERE t.slug='$SLUG' AND m.group_id='$GID' LIMIT 1")
[ -z "$UID1" ] && UID1=$(sql "SELECT id FROM app.users u JOIN app.tenants t ON t.id=u.tenant_id WHERE t.slug='$SLUG' LIMIT 1") && sql "INSERT INTO app.alarm_group_members(tenant_id,group_id,user_id,position) VALUES($TID,'$GID','$UID1',1) ON CONFLICT DO NOTHING" >/dev/null
sql "UPDATE app.users SET phone='+390000000001' WHERE id='$UID1'" >/dev/null

# ---- F2: override nativo CRUD (senza GoAlert) ----
OID=$(api -H 'content-type: application/json' -X POST "$BASE/api/oncall-overrides" -d "{\"group_id\":\"$GID\",\"start\":\"2026-12-01T08:00:00Z\",\"end\":\"2026-12-01T18:00:00Z\",\"add_user_id\":\"$UID1\"}" | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
[ -n "$OID" ] && ok "F2 override nativo creato" || no "override non creato" "-"
CNT=$(api "$BASE/api/oncall-overrides?group_id=$GID" | python -c "import sys,json;print(len(json.load(sys.stdin)['overrides']))")
[ "$CNT" -ge 1 ] && ok "F2 override nativo in lista" || no "override non listato" "$CNT"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-overrides/delete" -d "{\"id\":\"$OID\"}" >/dev/null
CNT2=$(api "$BASE/api/oncall-overrides?group_id=$GID" | python -c "import sys,json;print(len(json.load(sys.stdin)['overrides']))")
[ "$CNT2" -lt "$CNT" ] && ok "F2 override nativo eliminato" || no "override non eliminato" "$CNT2"

# ---- F2: verifica recapito nativa (OTP) ----
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/verify" -d "{\"user_id\":\"$UID1\"}" >/dev/null
OTP=$(sql "SELECT phone_verify_code FROM app.users WHERE id='$UID1'")
[ -n "$OTP" ] && ok "F2 OTP generato e memorizzato" || no "OTP assente" "-"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-contacts/code" -d "{\"user_id\":\"$UID1\",\"code\":\"$OTP\"}" >/dev/null
VER=$(sql "SELECT phone_verified_at IS NOT NULL FROM app.users WHERE id='$UID1'")
[ "$VER" = t ] && ok "F2 recapito verificato (nativo)" || no "recapito non verificato" "$VER"

# ---- F2: runner nativo CONSEGNA senza GoAlert ----
sql "DELETE FROM app.esc_rotation_state rs USING app.tenants t WHERE t.id=rs.tenant_id AND t.slug='$SLUG'" >/dev/null
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'e2e-selfsuff-'||gen_random_uuid(),now()" >/dev/null
DLV=$(api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' | python -c "import sys,json;d=json.load(sys.stdin);print(d['deliveries'])")
[ "$DLV" -ge 1 ] && ok "F2 consegna nativa SENZA GoAlert ($DLV rung)" || no "nessuna consegna nativa" "$DLV"

# ---- F1: maintenance nativo sospende la consegna ----
sql "INSERT INTO app.incidents(tenant_id,etype,state,priority,correlation,opened_at) SELECT $TID,'sos','nuovo',1,'e2e-maint-'||gen_random_uuid(),now()" >/dev/null
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-maintenance" -d '{"hours":1}' >/dev/null
INCM=$(api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' | python -c "import sys,json;print(json.load(sys.stdin)['incidents'])")
[ "$INCM" = 0 ] && ok "F1 maintenance nativo sospende la consegna" || no "maintenance non sospende" "$INCM"
api -H 'content-type: application/json' -X POST "$BASE/api/oncall-maintenance" -d '{"hours":0}' >/dev/null
INCA=$(api -H 'content-type: application/json' -X POST "$BASE/api/escalation/delivery/run" -d '{}' | python -c "import sys,json;print(json.load(sys.stdin)['incidents'])")
[ "$INCA" -ge 1 ] && ok "F1 consegna riprende a maintenance OFF" || no "consegna non ripresa" "$INCA"

# ---- teardown: ripristino stato globex ----
sql "UPDATE app.tenant_modules tm SET state='attivo' FROM app.tenants t WHERE t.id=tm.tenant_id AND t.slug='$SLUG' AND tm.module_code='loneworker'" >/dev/null
sql "UPDATE app.tenant_modules tm SET state='non_abilitato' FROM app.tenants t WHERE t.id=tm.tenant_id AND t.slug='$SLUG' AND tm.module_code='thingsboard'" >/dev/null
sql "UPDATE app.incidents i SET state='chiuso',closed_at=now(),outcome='e2e escalation-decouple' FROM app.tenants t WHERE t.id=i.tenant_id AND t.slug='$SLUG' AND i.state<>'chiuso' AND i.correlation LIKE 'e2e-%'" >/dev/null

echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
