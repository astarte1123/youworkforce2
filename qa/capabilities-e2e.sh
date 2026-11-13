#!/usr/bin/env bash
# Fase 2 — Capability layer. Default IDENTICO ai ruoli; override per-tenant aggiungono/tolgono
# capability; isolamento fra tenant; anti-lockout; reset. Le caps si ricalcolano a ogni richiesta
# (session_of), quindi un override ha effetto subito sulla sessione esistente.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
SLUG="globex"; ADMIN="admin@globex.loneworker.eu"
SLUG2="acme";  ADMIN2="admin@acme.loneworker.eu"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
sql(){ docker exec -i ywdb psql -U postgres -d youworkforce -tAc "$1"; }
one(){ sql "$1" | head -1 | tr -d '[:space:]'; }
jq(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

TID=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG'")
TID2=$(one "SELECT id FROM app.tenants WHERE slug='$SLUG2'")
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','interventi','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
sql "DELETE FROM app.role_capabilities WHERE tenant_id IN ('$TID','$TID2')" >/dev/null
R=$RANDOM
HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
OPM="oper-$R@globex.loneworker.eu"; PLM="plan-$R@globex.loneworker.eu"
OPU=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$OPM','$HASH','operatore') RETURNING id")
PLU=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$PLM','$HASH','pianificatore') RETURNING id")
SID=$(one "INSERT INTO app.sites(tenant_id,name) VALUES('$TID','Cap site $R') RETURNING id")

login(){ local em=$1 slug=$2 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $slug" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADMIN" "$SLUG"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
OJ=$(login "$OPM" "$SLUG"); OCS=$(awk '$6=="yw_csrf"{print $7}' "$OJ"|tail -1)
PJ=$(login "$PLM" "$SLUG"); PCS=$(awk '$6=="yw_csrf"{print $7}' "$PJ"|tail -1)
A2J=$(login "$ADMIN2" "$SLUG2"); A2CS=$(awk '$6=="yw_csrf"{print $7}' "$A2J"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
op(){  curl -s -b "$OJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $OCS" "$@"; }
pl(){  curl -s -b "$PJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $PCS" "$@"; }
adm2(){ curl -s -b "$A2J" -H "x-tenant-slug: $SLUG2" -H "x-csrf-token: $A2CS" "$@"; }
mkorder(){ local who=$1; $who -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/orders" -d "{\"title\":\"T $R\",\"site_id\":\"$SID\"}"; }

# --- DEFAULT (identico ai ruoli)
adm "$BASE/api/capabilities" | jq "'admin' in d['matrix']['tenant_admin'][0].values() or [x for x in d['matrix']['tenant_admin'] if x['cap']=='admin'][0]['granted']" | grep -qi true && ok "matrice: tenant_admin ha admin (default)" || no "matrix ko" "-"
[ "$(mkorder pl)" = 200 ] && ok "default: pianificatore crea ordine (plan)" || no "plan default ko" "$(mkorder pl)"
[ "$(mkorder op)" = 403 ] && ok "default: operatore NON crea ordine (no plan)" || no "op default ko" "$(mkorder op)"
[ "$(op -o /dev/null -w '%{http_code}' "$BASE/api/user-groups")" = 403 ] && ok "default: operatore NON amministra (no admin)" || no "op admin default ko" "-"

# --- OVERRIDE: concedi 'admin' all'operatore -> ora amministra
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/capabilities" -d '{"role":"operatore","capability":"admin","granted":true}' | grep -q 200 && ok "override set (operatore+admin)" || no "set ko" "-"
[ "$(op -o /dev/null -w '%{http_code}' "$BASE/api/user-groups")" = 200 ] && ok "override attivo subito: operatore amministra (200)" || no "override apply ko" "$(op -o /dev/null -w '%{http_code}' "$BASE/api/user-groups")"
# matrice segnala override
adm "$BASE/api/capabilities" | jq "[x for x in d['matrix']['operatore'] if x['cap']=='admin'][0]['override']" | grep -qi true && ok "matrice segnala override operatore.admin" || no "matrix override ko" "-"

# --- OVERRIDE: togli 'plan' al pianificatore -> non crea piu' ordini
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/capabilities" -d '{"role":"pianificatore","capability":"plan","granted":false}' | grep -q 200 && ok "override set (pianificatore-plan)" || no "set2 ko" "-"
[ "$(mkorder pl)" = 403 ] && ok "override attivo: pianificatore NON crea ordine" || no "revoke plan ko" "$(mkorder pl)"

# --- ISOLAMENTO: override su globex non tocca acme
C=$(adm2 -o /dev/null -w "%{http_code}" "$BASE/api/capabilities")
[ "$C" = 200 ] && ok "acme legge la propria matrice" || no "acme read ko" "$C"
# su acme l'operatore resta senza admin: creo operatore acme e verifico 403
HASH2=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID2' AND email='$ADMIN2'")
OPM2="oper2-$R@acme.loneworker.eu"
one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID2','$OPM2','$HASH2','operatore') RETURNING id" >/dev/null
O2J=$(login "$OPM2" "$SLUG2"); O2CS=$(awk '$6=="yw_csrf"{print $7}' "$O2J"|tail -1)
C=$(curl -s -b "$O2J" -H "x-tenant-slug: $SLUG2" -H "x-csrf-token: $O2CS" -o /dev/null -w "%{http_code}" "$BASE/api/user-groups")
[ "$C" = 403 ] && ok "isolamento: operatore acme NON eredita override di globex (403)" || no "iso ko" "$C"

# --- ANTI-LOCKOUT: non si toglie admin a tenant_admin
C=$(adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/capabilities" -d '{"role":"tenant_admin","capability":"admin","granted":false}')
[ "$C" = 400 ] && ok "anti-lockout: no togliere admin a tenant_admin (400)" || no "lockout ko" "$C"

# --- VALIDAZIONE input
[ "$(adm -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/capabilities" -d '{"role":"ignoto","capability":"admin","granted":true}')" = 400 ] && ok "ruolo sconosciuto -> 400" || no "role val ko" "-"
[ "$(adm -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/capabilities" -d '{"role":"operatore","capability":"ignota","granted":true}')" = 400 ] && ok "capability sconosciuta -> 400" || no "cap val ko" "-"

# --- RESET: torna tutto al default
adm -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/capabilities" | grep -q 200 && ok "reset capabilities" || no "reset ko" "-"
[ "$(op -o /dev/null -w '%{http_code}' "$BASE/api/user-groups")" = 403 ] && [ "$(mkorder pl)" = 200 ] && ok "dopo reset: comportamento di default ripristinato" || no "post-reset ko" "-"
N=$(one "SELECT count(*) FROM app.role_capabilities WHERE tenant_id='$TID'")
[ "$N" = 0 ] && ok "nessun override residuo dopo reset" || no "residue ko" "count=$N"

# teardown
sql "DELETE FROM app.work_orders WHERE site_id='$SID'" >/dev/null
sql "DELETE FROM app.sites WHERE id='$SID'" >/dev/null
sql "DELETE FROM app.users WHERE email IN ('$OPM','$PLM','$OPM2')" >/dev/null
sql "DELETE FROM app.role_capabilities WHERE tenant_id IN ('$TID','$TID2')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
