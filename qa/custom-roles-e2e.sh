#!/usr/bin/env bash
# Fase 3 — Ruoli personalizzati per tenant. Bundle di capability assegnabile a UTENTI e/o GRUPPI
# condivisi; SOLO additivo (aggiunge capability sopra il ruolo base). Caps ricalcolate a ogni
# richiesta -> effetto immediato. Isolamento tenant. Cascade su delete.
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
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','loneworker','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null
R=$RANDOM
HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
U1M="cr-u1-$R@globex.loneworker.eu"; U2M="cr-u2-$R@globex.loneworker.eu"
U1=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$U1M','$HASH','operatore') RETURNING id")
U2=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','$U2M','$HASH','operatore') RETURNING id")
SID=$(one "INSERT INTO app.sites(tenant_id,name) VALUES('$TID','CR site $R') RETURNING id")

login(){ local em=$1 slug=$2 j=$(mktemp); curl -s -o /dev/null -c "$j" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $slug" -d "{\"email\":\"$em\",\"password\":\"demo\"}"; echo "$j"; }
AJ=$(login "$ADMIN" "$SLUG"); ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ"|tail -1)
A2J=$(login "$ADMIN2" "$SLUG2"); A2CS=$(awk '$6=="yw_csrf"{print $7}' "$A2J"|tail -1)
U1J=$(login "$U1M" "$SLUG"); U1CS=$(awk '$6=="yw_csrf"{print $7}' "$U1J"|tail -1)
U2J=$(login "$U2M" "$SLUG"); U2CS=$(awk '$6=="yw_csrf"{print $7}' "$U2J"|tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
adm2(){ curl -s -b "$A2J" -H "x-tenant-slug: $SLUG2" -H "x-csrf-token: $A2CS" "$@"; }
u1(){ curl -s -b "$U1J" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $U1CS" "$@"; }
u2(){ curl -s -b "$U2J" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $U2CS" "$@"; }
mkorder(){ local who=$1; $who -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/orders" -d "{\"title\":\"T $R\",\"site_id\":\"$SID\"}"; }

# baseline: operatori non pianificano
[ "$(mkorder u1)" = 403 ] && [ "$(mkorder u2)" = 403 ] && ok "baseline: operatori senza plan (403)" || no "baseline ko" "-"

# 1) CREATE ruolo custom "Capo turno" con capability plan
RID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/custom-roles" -d '{"name":"Capo turno '"$R"'","description":"pianifica","capabilities":["plan"]}' | jq "d.get('id','')")
[ -n "$RID" ] && ok "create ruolo custom (plan)" || no "create ko" "-"

# 2) ASSEGNA a utente U1 -> U1 ora pianifica (subito, stessa sessione)
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/custom-roles/$RID/users" -d "{\"user_id\":\"$U1\"}" | grep -q 200 && ok "assegna ruolo a utente" || no "assign user ko" "-"
[ "$(mkorder u1)" = 200 ] && ok "utente con ruolo custom pianifica subito (additivo)" || no "user cap ko" "$(mkorder u1)"
[ "$(mkorder u2)" = 403 ] && ok "altro operatore NON tocco (ancora 403)" || no "u2 leak ko" "$(mkorder u2)"

# 3) ASSEGNA a GRUPPO condiviso contenente U2 -> U2 eredita via gruppo
GID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/user-groups" -d '{"name":"CR grp '"$R"'"}' | jq "d.get('id','')")
adm -o /dev/null -X POST "$BASE/api/user-groups/$GID/members" -H 'content-type: application/json' -d "{\"user_id\":\"$U2\"}" >/dev/null
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/custom-roles/$RID/groups" -d "{\"group_id\":\"$GID\"}" | grep -q 200 && ok "assegna ruolo a gruppo condiviso" || no "assign group ko" "-"
[ "$(mkorder u2)" = 200 ] && ok "membro del gruppo eredita capability (via gruppo)" || no "group cap ko" "$(mkorder u2)"

# 4) /api/me espone le caps
u1 "$BASE/api/me" | jq "'plan' in d['caps']" | grep -qi true && ok "/api/me espone caps dell'utente" || no "me ko" "-"

# 5) RIMUOVI assegnazione gruppo -> U2 torna senza plan
adm -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/custom-roles/$RID/groups/$GID" | grep -q 200 && ok "rimuovi ruolo da gruppo" || no "unassign grp ko" "-"
[ "$(mkorder u2)" = 403 ] && ok "tolto il gruppo: U2 torna senza capability" || no "u2 revert ko" "$(mkorder u2)"

# 6) ISOLAMENTO: acme non vede il ruolo custom di globex
SEEN=$(adm2 "$BASE/api/custom-roles" | jq "len([r for r in d['roles'] if r['id']=='$RID'])")
[ "$SEEN" = 0 ] && ok "isolamento: acme non vede il ruolo di globex" || no "iso ko" "seen=$SEEN"
# acme non puo' assegnarlo
C=$(adm2 -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/custom-roles/$RID")
[ "$C" = 404 ] && ok "isolamento: acme non puo' cancellarlo (404)" || no "iso del ko" "$C"

# 7) VALIDAZIONE: capability sconosciuta -> 400
[ "$(adm -o /dev/null -w '%{http_code}' -H 'content-type: application/json' -X POST "$BASE/api/custom-roles" -d '{"name":"X '"$R"'","capabilities":["ignota"]}')" = 400 ] && ok "capability sconosciuta -> 400" || no "val ko" "-"

# 8) DELETE ruolo -> cascade assegnazioni, U1 torna senza plan
adm -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/custom-roles/$RID" | grep -q 200 && ok "delete ruolo custom" || no "delete ko" "-"
[ "$(mkorder u1)" = 403 ] && ok "dopo delete: U1 torna senza capability (cascade)" || no "cascade ko" "$(mkorder u1)"
N=$(one "SELECT count(*) FROM app.user_custom_roles WHERE tenant_id='$TID'")
[ "$N" = 0 ] && ok "assegnazioni utente rimosse in cascade" || no "cascade residue ko" "count=$N"

# teardown
sql "DELETE FROM app.work_orders WHERE site_id='$SID'" >/dev/null
sql "DELETE FROM app.sites WHERE id='$SID'" >/dev/null
sql "DELETE FROM app.user_groups WHERE id='$GID'" >/dev/null
sql "DELETE FROM app.users WHERE email IN ('$U1M','$U2M')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
