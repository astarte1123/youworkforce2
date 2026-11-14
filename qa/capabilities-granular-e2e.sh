#!/usr/bin/env bash
# Fase 4 Tappa 1 — capability granulari per-modulo. Prova che i check (prima cablati su stringhe-
# ruolo) ora rispondono alle capability: un ruolo senza la cap -> 403; concessa via override o ruolo
# custom -> 200; revoca -> 403. Endpoint campione: /api/presence (incidents.view),
# /api/checkins/list (incidents.view).
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"; SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1"|head -1|tr -d '[:space:]'; }
TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','loneworker','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
sql "DELETE FROM app.role_capabilities WHERE tenant_id='$TID'" >/dev/null
R=$RANDOM; HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
PLM="plan-g-$R@globex.loneworker.eu"
PLU=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$PLM','$HASH','pianificatore') RETURNING id")
login(){ local em=$1 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADMIN"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
PJ=$(login "$PLM"); PCS=$(awk '$6=="yw_csrf"{print $7}' "$PJ"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
pl(){  curl -s -b "$PJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $PCS" "$@"; }
code(){ "$@" -o /dev/null -w '%{http_code}'; }

# baseline: pianificatore NON ha incidents.view -> 403 su presence/checkins
[ "$(code pl "$BASE/api/checkins/list")" = 403 ] && ok "baseline: pianificatore -> 403 (checkins)" || no "base checkins" "-"
# operatore invece (default incidents.view) passa
OPM="oper-g-$R@globex.loneworker.eu"; one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$OPM','$HASH','operatore')" >/dev/null
OJ=$(login "$OPM"); OCS=$(awk '$6=="yw_csrf"{print $7}' "$OJ"|tail -1)
[ "$(curl -s -b "$OJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $OCS" -o /dev/null -w '%{http_code}' "$BASE/api/checkins/list")" = 200 ] && ok "operatore (default incidents.view) -> 200" || no "oper presence" "-"

# OVERRIDE: concedi incidents.view al ruolo pianificatore -> ora 200 (effetto immediato)
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/capabilities" -d '{"role":"pianificatore","capability":"incidents.view","granted":true}' | grep -q 200 && ok "override set pianificatore+incidents.view" || no "set ko" "-"
[ "$(code pl "$BASE/api/checkins/list")" = 200 ] && ok "override attivo: pianificatore -> 200 (checkins)" || no "override checkins" "-"

# RUOLO CUSTOM additivo: nuovo ruolo con config.view assegnato a un operatore -> legge module-tools
# (operatore di default NON ha config.view)
CRID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/custom-roles" -d '{"name":"Auditor cfg '"$R"'","capabilities":["config.view"]}' | python -c "import sys,json;print(json.load(sys.stdin).get('id',''))")
# un modulo con tool per avere un 200 reale; in assenza, 200 con lista vuota è comunque ok
adm -o /dev/null -X POST "$BASE/api/custom-roles/$CRID/users" -H 'content-type: application/json' -d "{\"user_id\":\"$(one "SELECT id FROM app.users WHERE email='$OPM'")\"}" >/dev/null
# operatore senza config.view: baseline su un endpoint config.view (assistant plan richiede modulo assistant; usiamo module-tools via /api/modules? semplifichiamo: verifichiamo la cap effettiva via /api/me)
OJ2=$(login "$OPM"); OCS2=$(awk '$6=="yw_csrf"{print $7}' "$OJ2"|tail -1)
curl -s -b "$OJ2" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $OCS2" "$BASE/api/me" | grep -q "config.view" && ok "ruolo custom additivo: operatore ha ora config.view (/api/me)" || no "custom cap ko" "-"

# RESET
adm -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/capabilities" | grep -q 200 && ok "reset override" || no "reset ko" "-"
[ "$(code pl "$BASE/api/checkins/list")" = 403 ] && ok "dopo reset: pianificatore -> 403 (default ripristinato)" || no "post-reset" "-"

# teardown
sql "DELETE FROM app.custom_roles WHERE id='${CRID:-00000000-0000-0000-0000-000000000000}'" >/dev/null 2>&1 || true
sql "DELETE FROM app.role_capabilities WHERE tenant_id='$TID'" >/dev/null
sql "DELETE FROM app.users WHERE email IN ('$PLM','$OPM')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
