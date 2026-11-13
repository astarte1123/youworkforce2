#!/usr/bin/env bash
# Fase 1 — Gruppi di persone CONDIVISI (primitiva comune).
# Prove: CRUD gruppo, aggiunta/rimozione membri, IMPORT dei membri in un gruppo di allarme
# (dimostra il riuso per l'instradamento allarmi/escalation), ISOLAMENTO fra tenant
# (acme non vede/non tocca i gruppi di globex), e permessi (solo tenant_admin).
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
# escalation attiva su globex (serve a import nel gruppo allarme)
sql "INSERT INTO app.tenant_modules(tenant_id,module_code,state) VALUES('$TID','loneworker','attivo') ON CONFLICT (tenant_id,module_code) DO UPDATE SET state='attivo'" >/dev/null

# login admin globex
AJ=$(mktemp); curl -s -o /dev/null -c "$AJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"$ADMIN\",\"password\":\"demo\"}"
ACS=$(awk '$6=="yw_csrf"{print $7}' "$AJ" | tail -1)
adm(){ curl -s -b "$AJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $ACS" "$@"; }
# login admin acme (altro tenant)
AJ2=$(mktemp); curl -s -o /dev/null -c "$AJ2" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG2" -d "{\"email\":\"$ADMIN2\",\"password\":\"demo\"}"
ACS2=$(awk '$6=="yw_csrf"{print $7}' "$AJ2" | tail -1)
adm2(){ curl -s -b "$AJ2" -H "x-tenant-slug: $SLUG2" -H "x-csrf-token: $ACS2" "$@"; }

R=$RANDOM
# due utenti globex per i membri
HASH=$(one "SELECT pass_hash FROM app.users WHERE tenant_id='$TID' AND email='$ADMIN'")
U1=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','ug-a-$R@globex.loneworker.eu','$HASH','reperibile') RETURNING id")
U2=$(one "INSERT INTO app.users(tenant_id,email,pass_hash,role) VALUES('$TID','ug-b-$R@globex.loneworker.eu','$HASH','reperibile') RETURNING id")

# 1) CREATE gruppo condiviso
GID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/user-groups" -d "{\"name\":\"Squadra Nord $R\",\"kind\":\"squadra\"}" | jq "d.get('id','')")
[ -n "$GID" ] && ok "create user-group" || no "create ko" "-"

# 2) UPDATE (rinomina)
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/user-groups/$GID" -d "{\"name\":\"Squadra Nord-Est $R\",\"kind\":\"reparto\"}" | grep -q 200 && ok "update user-group" || no "update ko" "-"

# 3) ADD membri
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/user-groups/$GID/members" -d "{\"user_id\":\"$U1\"}" | grep -q 200 && \
adm -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/user-groups/$GID/members" -d "{\"user_id\":\"$U2\"}" | grep -q 200 && ok "add members" || no "add ko" "-"
N=$(adm "$BASE/api/user-groups" | jq "[len(g['members']) for g in d if g['id']=='$GID'][0]")
[ "$N" = 2 ] && ok "list mostra 2 membri + rinomina" || no "list ko" "members=$N"

# 4) DEL un membro
adm -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/user-groups/$GID/members/$U2" | grep -q 200 && ok "del member" || no "del member ko" "-"
N=$(adm "$BASE/api/user-groups" | jq "[len(g['members']) for g in d if g['id']=='$GID'][0]")
[ "$N" = 1 ] && ok "membro rimosso (1 resta)" || no "del verify ko" "members=$N"
# rimetto U2 per l'import
adm -o /dev/null -X POST "$BASE/api/user-groups/$GID/members" -H 'content-type: application/json' -d "{\"user_id\":\"$U2\"}" >/dev/null

# 5) IMPORT nel gruppo di ALLARME (instradamento/escalation)
AGID=$(adm -H 'content-type: application/json' -X POST "$BASE/api/groups" -d "{\"name\":\"Allarme Nord $R\"}" | jq "d.get('id','')")
[ -n "$AGID" ] || no "create alarm group ko" "-"
ADDED=$(adm -H 'content-type: application/json' -X POST "$BASE/api/groups/$AGID/members/import" -d "{\"user_group_id\":\"$GID\"}" | jq "d.get('added',-1)")
[ "$ADDED" = 2 ] && ok "import: 2 membri copiati nel gruppo allarme" || no "import ko" "added=$ADDED"
# verifica diretta su alarm_group_members
AM=$(one "SELECT count(*) FROM app.alarm_group_members WHERE group_id='$AGID'")
[ "$AM" = 2 ] && ok "membri presenti in alarm_group_members (routing alimentato)" || no "routing ko" "count=$AM"
# idempotente: re-import non duplica
ADDED2=$(adm -H 'content-type: application/json' -X POST "$BASE/api/groups/$AGID/members/import" -d "{\"user_group_id\":\"$GID\"}" | jq "d.get('added',-1)")
[ "$ADDED2" = 0 ] && ok "re-import idempotente (0 aggiunti)" || no "idem ko" "added=$ADDED2"

# 6) ISOLAMENTO tenant: acme non vede il gruppo di globex
SEEN=$(adm2 "$BASE/api/user-groups" | jq "len([g for g in d if g['id']=='$GID'])")
[ "$SEEN" = 0 ] && ok "isolamento: acme non vede il gruppo di globex (list)" || no "iso list ko" "seen=$SEEN"
# acme non puo' modificarlo (404, RLS/no-row)
C=$(adm2 -o /dev/null -w "%{http_code}" -H 'content-type: application/json' -X POST "$BASE/api/user-groups/$GID" -d '{"name":"hack"}')
[ "$C" = 404 ] && ok "isolamento: acme non puo' modificare (404)" || no "iso update ko" "$C"
# acme non puo' cancellarlo
C=$(adm2 -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/user-groups/$GID")
[ "$C" = 404 ] && ok "isolamento: acme non puo' cancellare (404)" || no "iso del ko" "$C"
# gruppo intatto lato globex
N=$(adm "$BASE/api/user-groups" | jq "[len(g['members']) for g in d if g['id']=='$GID'][0]")
[ "$N" = 2 ] && ok "gruppo globex intatto dopo tentativi acme" || no "intact ko" "members=$N"

# 7) PERMESSI: ruolo non-admin negato
WJ=$(mktemp); curl -s -o /dev/null -c "$WJ" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"ug-a-$R@globex.loneworker.eu\",\"password\":\"demo\"}"
WCS=$(awk '$6=="yw_csrf"{print $7}' "$WJ" | tail -1)
C=$(curl -s -b "$WJ" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $WCS" -o /dev/null -w "%{http_code}" -X POST "$BASE/api/user-groups" -H 'content-type: application/json' -d '{"name":"x"}')
[ "$C" = 403 ] && ok "permesso: non-admin negato (403)" || no "perm ko" "$C"

# 8) DELETE gruppo condiviso (cascade membri) — NON tocca il gruppo allarme gia' popolato
adm -o /dev/null -w "%{http_code}" -X DELETE "$BASE/api/user-groups/$GID" | grep -q 200 && ok "delete user-group" || no "delete ko" "-"
AM=$(one "SELECT count(*) FROM app.alarm_group_members WHERE group_id='$AGID'")
[ "$AM" = 2 ] && ok "cancellare il gruppo condiviso NON svuota il gruppo allarme (disaccoppiati)" || no "coupling ko" "count=$AM"

# teardown
sql "DELETE FROM app.alarm_groups WHERE id='$AGID'" >/dev/null
sql "DELETE FROM app.users WHERE id IN ('$U1','$U2')" >/dev/null
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" = 0 ]
