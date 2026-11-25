#!/usr/bin/env bash
# TA-7 (decisione PO 26/11/2026): gruppi di escalation e profili di protezione prendono i membri dai
# gruppi di persone, sempre allineati; le persone aggiunte a mano non vengono toccate. Tenant usa-e-getta.
set -uo pipefail
BASE="${BASE:-http://localhost:8080}"
PASS=0; FAIL=0
ok(){ echo "PASS  $1"; PASS=$((PASS+1)); }
no(){ echo "FAIL  $1  ($2)"; FAIL=$((FAIL+1)); }
py(){ python -c "import sys,json;d=json.load(sys.stdin);print($1)"; }
J=$(mktemp -d)
curl -s -o /dev/null -c "$J/sa" -X POST "$BASE/api/superadmin/login" -H 'content-type: application/json' -d '{"email":"admin@loneworker.eu","password":"admin"}'
SC=$(awk '$6=="yw_csrf"{print $7}' "$J/sa" | tail -1)
R=$(python -c 'import random;print(random.randint(100000,999999))'); SLUG="ms$R"
TID=$(curl -s -b "$J/sa" -H "x-csrf-token: $SC" -H 'content-type: application/json' -X POST "$BASE/api/superadmin/tenants" \
  -d "{\"slug\":\"$SLUG\",\"name\":\"Ms $R\",\"admin_email\":\"a@$SLUG.test\",\"admin_password\":\"DirectPass9\",\"modules\":[\"loneworker\"]}" | py "d.get('id','')")
trap 'curl -s -o /dev/null -b "$J/sa" -H "x-csrf-token: $SC" -H "content-type: application/json" -X DELETE "$BASE/api/superadmin/tenants/$TID" -d "{\"slug\":\"$SLUG\"}"; rm -rf "$J"' EXIT
[ -n "$TID" ] || { echo "FAIL crea tenant"; exit 1; }
curl -s -o /dev/null -c "$J/A" -X POST "$BASE/api/login" -H 'content-type: application/json' -H "x-tenant-slug: $SLUG" -d "{\"email\":\"a@$SLUG.test\",\"password\":\"DirectPass9\"}"
call(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
code(){ local c; c=$(awk '$6=="yw_csrf"{print $7}' "$J/A" | tail -1)
  curl -s -o /dev/null -w "%{http_code}" -b "$J/A" -H "x-tenant-slug: $SLUG" -H "x-csrf-token: $c" -H 'content-type: application/json' -X "$1" "$BASE$2" ${3:+-d "$3"}; }
mk(){ call POST /api/users "{\"email\":\"$1@$SLUG.test\",\"role\":\"reperibile\",\"password\":\"WorkPass9\"}" | py "d['id']"; }
U1=$(mk u1); U2=$(mk u2); U3=$(mk u3); UM=$(mk manuale)
PG=$(call POST /api/user-groups '{"name":"Squadra notte","kind":"squadra"}' | py "d['id']")
call POST "/api/user-groups/$PG/members" "{\"user_id\":\"$U1\"}" >/dev/null; call POST "/api/user-groups/$PG/members" "{\"user_id\":\"$U2\"}" >/dev/null
AG=$(call POST /api/groups '{"name":"Reperibili notte"}' | py "d['id']")
call POST "/api/groups/$AG/members" "{\"user_id\":\"$UM\"}" >/dev/null
mem(){ call GET /api/groups | py "sorted(m['email'].split('@')[0]+('*' if m.get('via_user_group') else '') for g in d['groups'] if g['id']=='$AG' for m in g['members'])"; }

C=$(code POST "/api/groups/$AG/sources" "{\"user_group_id\":\"$PG\"}"); [ "$C" = "200" ] && ok "gruppo di persone collegato al gruppo di escalation" || no "collega" "$C"
[ "$(mem)" = "['manuale', 'u1*', 'u2*']" ] && ok "i membri del gruppo di persone entrano (segnati), quello a mano resta" || no "membri" "$(mem)"
call POST "/api/user-groups/$PG/members" "{\"user_id\":\"$U3\"}" >/dev/null
[ "$(mem)" = "['manuale', 'u1*', 'u2*', 'u3*']" ] && ok "chi entra nel gruppo di persone entra anche nell'escalation" || no "entra" "$(mem)"
call DELETE "/api/user-groups/$PG/members/$U1" >/dev/null
[ "$(mem)" = "['manuale', 'u2*', 'u3*']" ] && ok "chi esce dal gruppo di persone esce anche dall'escalation" || no "esce" "$(mem)"
C=$(code DELETE "/api/groups/$AG/members/$U2"); [ "$C" = "409" ] && ok "togliere a mano chi arriva dal gruppo: 409 con spiegazione" || no "rimozione a mano" "$C"
call GET /api/groups | py "[m['position'] for g in d['groups'] if g['id']=='$AG' for m in g['members']]" | python -c "import sys,ast;p=ast.literal_eval(sys.stdin.read());print(len(p)==len(set(p)))" | grep -q True && ok "posizioni nella rotazione senza doppioni" || no "posizioni" "-"
call DELETE "/api/groups/$AG/sources/$PG" >/dev/null
[ "$(mem)" = "['manuale']" ] && ok "scollegando il gruppo restano solo le persone aggiunte a mano" || no "scollega" "$(mem)"

# profili di protezione
P1=$(call POST /api/safety/profiles '{"name":"Notte"}' | py "d['id']"); P2=$(call POST /api/safety/profiles '{"name":"Giorno"}' | py "d['id']")
call POST "/api/safety/profiles/$P2/members" "{\"user_id\":\"$U3\"}" >/dev/null
call POST "/api/safety/profiles/$P1/sources" "{\"user_group_id\":\"$PG\"}" >/dev/null
pm(){ call GET /api/safety/profiles | py "sorted(m['email'].split('@')[0] for p in d['profiles'] if p['id']=='$1' for m in p['members'])" 2>/dev/null || call GET /api/safety/profiles | py "sorted(m['email'].split('@')[0] for p in d if p['id']=='$1' for m in p['members'])"; }
[ "$(pm "$P1")" = "['u2']" ] && ok "profilo: entrano i membri del gruppo; chi ha già un altro profilo resta lì" || no "profilo" "$(pm "$P1") / $(pm "$P2")"
call POST "/api/user-groups/$PG/members" "{\"user_id\":\"$U1\"}" >/dev/null
[ "$(pm "$P1")" = "['u1', 'u2']" ] && ok "profilo sempre allineato al gruppo di persone" || no "profilo allineato" "$(pm "$P1")"
echo "member-sources: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
